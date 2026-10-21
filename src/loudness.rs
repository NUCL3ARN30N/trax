//! Loudness measurement for volume normalization (ITU-R BS.1770 / EBU R 128).
//!
//! A track is decoded once in the background and its integrated loudness stored, so every
//! song can be played at the same perceived level instead of at whatever level it was
//! mastered to. The measurement is the standard one: K-weighting, 400 ms blocks with 75 %
//! overlap, an absolute gate at -70 LUFS and a relative gate 10 LU below the ungated mean.

use anyhow::{Context, Result};
use rodio::{Decoder, Source};
use std::path::Path;

/// Everything is normalized to this level; -14 LUFS is what streaming services target.
pub const TARGET_LUFS: f32 = -14.0;
/// How far a track may be turned up or down, so a quiet recording is not blown up.
pub const MAX_ADJUST_DB: f32 = 12.0;
/// Stop measuring after this much audio: enough for a stable number on any song.
const MAX_SECONDS: usize = 600;

/// Gain (as an amplitude factor) that brings `lufs` to the target.
pub fn gain_for(lufs: f32) -> f32 {
    let db = (TARGET_LUFS - lufs).clamp(-MAX_ADJUST_DB, MAX_ADJUST_DB);
    10f32.powf(db / 20.0)
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

#[derive(Clone, Copy, Default)]
struct State {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    /// Stage 1 of the K-weighting: the shelving filter for the head's acoustic effect.
    fn shelf(rate: f64) -> Biquad {
        let f0 = 1681.974450955533;
        let g = 3.999843853973347;
        let q = 0.7071752369554196;
        let k = (std::f64::consts::PI * f0 / rate).tan();
        let vh = 10f64.powf(g / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / q + k * k;
        Biquad {
            b0: (vh + vb * k / q + k * k) / a0,
            b1: 2.0 * (k * k - vh) / a0,
            b2: (vh - vb * k / q + k * k) / a0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / q + k * k) / a0,
        }
    }

    /// Stage 2: the RLB high-pass that removes the rumble below ~40 Hz.
    fn highpass(rate: f64) -> Biquad {
        let f0 = 38.13547087602444;
        let q = 0.5003270373238773;
        let k = (std::f64::consts::PI * f0 / rate).tan();
        let den = 1.0 + k / q + k * k;
        Biquad {
            b0: 1.0,
            b1: -2.0,
            b2: 1.0,
            a1: 2.0 * (k * k - 1.0) / den,
            a2: (1.0 - k / q + k * k) / den,
        }
    }

    #[inline]
    fn run(&self, s: &mut State, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * s.x1 + self.b2 * s.x2 - self.a1 * s.y1 - self.a2 * s.y2;
        s.x2 = s.x1;
        s.x1 = x;
        s.y2 = s.y1;
        s.y1 = y;
        y
    }
}

/// Running BS.1770 measurement fed with interleaved samples.
pub struct Meter {
    channels: usize,
    shelf: Biquad,
    highpass: Biquad,
    states: Vec<(State, State)>,
    /// Sum of squares of the current 100 ms step, per channel.
    step_sum: Vec<f64>,
    step_len: usize,
    step_pos: usize,
    /// The last four 100 ms steps make one 400 ms block (75 % overlap).
    recent: Vec<Vec<f64>>,
    /// Loudness of every gated-in block, as mean square sums per channel.
    blocks: Vec<f64>,
}

impl Meter {
    pub fn new(rate: u32, channels: u16) -> Meter {
        let channels = channels.max(1) as usize;
        Meter {
            channels,
            shelf: Biquad::shelf(rate as f64),
            highpass: Biquad::highpass(rate as f64),
            states: vec![(State::default(), State::default()); channels],
            step_sum: vec![0.0; channels],
            step_len: (rate as usize / 10).max(1),
            step_pos: 0,
            recent: vec![],
            blocks: vec![],
        }
    }

    /// Feed interleaved samples; call as often as you like.
    pub fn push(&mut self, samples: &[f32]) {
        for (i, s) in samples.iter().enumerate() {
            let ch = i % self.channels;
            let (s1, s2) = &mut self.states[ch];
            let weighted = self.highpass.run(s2, self.shelf.run(s1, *s as f64));
            self.step_sum[ch] += weighted * weighted;
            if ch == self.channels - 1 {
                self.step_pos += 1;
                if self.step_pos >= self.step_len {
                    self.close_step();
                }
            }
        }
    }

    fn close_step(&mut self) {
        let mean: Vec<f64> = self.step_sum.iter().map(|s| s / self.step_len as f64).collect();
        self.step_sum.iter_mut().for_each(|s| *s = 0.0);
        self.step_pos = 0;
        self.recent.push(mean);
        if self.recent.len() > 4 {
            self.recent.remove(0);
        }
        if self.recent.len() == 4 {
            // Mean square of the 400 ms block, summed over the channels (all weight 1.0;
            // the surround weights of BS.1770 only matter above 5 channels).
            let z: f64 = (0..self.channels)
                .map(|ch| self.recent.iter().map(|m| m[ch]).sum::<f64>() / 4.0)
                .sum();
            self.blocks.push(z);
        }
    }

    /// Integrated loudness in LUFS, or None when the track is silent or too short.
    pub fn integrated(&self) -> Option<f32> {
        if self.blocks.is_empty() {
            return None;
        }
        let loudness = |z: f64| -0.691 + 10.0 * z.max(1e-12).log10();
        // Absolute gate at -70 LUFS.
        let loud: Vec<f64> = self.blocks.iter().copied().filter(|z| loudness(*z) > -70.0).collect();
        if loud.is_empty() {
            return None;
        }
        // Relative gate: 10 LU below the mean of what is left.
        let mean = loud.iter().sum::<f64>() / loud.len() as f64;
        let threshold = loudness(mean) - 10.0;
        let kept: Vec<f64> = loud.into_iter().filter(|z| loudness(*z) > threshold).collect();
        if kept.is_empty() {
            return None;
        }
        let mean = kept.iter().sum::<f64>() / kept.len() as f64;
        Some(loudness(mean) as f32)
    }
}

/// Decode `path` and measure its integrated loudness. Takes a second or two per song, so
/// call it from a worker thread.
pub fn analyze(path: &Path) -> Result<f32> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let decoder = Decoder::builder()
        .with_data(std::io::BufReader::new(file))
        .with_byte_len(len)
        .with_seekable(true)
        .build()
        .with_context(|| format!("cannot decode {}", path.display()))?;
    let rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let mut meter = Meter::new(rate, channels);
    let limit = MAX_SECONDS * rate as usize * channels as usize;
    let mut buf = Vec::with_capacity(8192);
    let mut total = 0usize;
    for s in decoder {
        buf.push(s);
        if buf.len() == 8192 {
            meter.push(&buf);
            buf.clear();
        }
        total += 1;
        if total >= limit {
            break;
        }
    }
    meter.push(&buf);
    meter.integrated().context("track is silent or too short to measure")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// A 1 kHz sine, where K-weighting is ~0 dB, measures its own RMS level.
    #[test]
    fn measures_a_sine_at_its_rms_level() {
        let rate = 48_000;
        let amp = 0.1f32; // -20 dBFS peak, so -23 dBFS RMS
        let mut meter = Meter::new(rate, 1);
        let samples: Vec<f32> =
            (0..rate * 5).map(|i| amp * (TAU * 1000.0 * i as f32 / rate as f32).sin()).collect();
        meter.push(&samples);
        let lufs = meter.integrated().expect("a sine has a loudness");
        assert!((lufs - -23.0).abs() < 1.0, "expected about -23 LUFS, got {lufs}");
    }

    #[test]
    fn silence_has_no_loudness() {
        let mut meter = Meter::new(48_000, 2);
        meter.push(&vec![0.0; 48_000 * 2]);
        assert_eq!(meter.integrated(), None);
    }

    #[test]
    fn quiet_tracks_are_turned_up_and_loud_ones_down() {
        assert!(gain_for(-24.0) > 1.0);
        assert!(gain_for(-6.0) < 1.0);
        assert!((gain_for(TARGET_LUFS) - 1.0).abs() < 0.001);
        // Nothing is turned up more than MAX_ADJUST_DB.
        let most = 10f32.powf(MAX_ADJUST_DB / 20.0);
        assert!((gain_for(-60.0) - most).abs() < 0.001);
        assert!((gain_for(30.0) - 1.0 / most).abs() < 0.001);
    }

    /// Two blocks of very different level: the quiet half is gated out, so the number
    /// follows the loud part instead of averaging the two.
    #[test]
    fn the_relative_gate_ignores_the_quiet_part() {
        let rate = 48_000;
        let mut meter = Meter::new(rate, 1);
        let loud: Vec<f32> = (0..rate * 3).map(|i| 0.5 * (TAU * 1000.0 * i as f32 / rate as f32).sin()).collect();
        let quiet: Vec<f32> = loud.iter().map(|s| s * 0.001).collect();
        meter.push(&loud);
        meter.push(&quiet);
        let lufs = meter.integrated().unwrap();
        let mut only_loud = Meter::new(rate, 1);
        only_loud.push(&loud);
        assert!((lufs - only_loud.integrated().unwrap()).abs() < 1.0);
    }
}
