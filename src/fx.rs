//! Audio effects after the equalizer: tempo and pitch (phase vocoder), filters, echo, reverb,
//! sidechain pump, 8D rotation, stereo width/balance, vocal remover and a limiter.
//! Also feeds the visualizer and tracks the playback position in *track* time, which stays
//! correct when the tempo changes.

use pitch_shift::{RawState, Shifter, TOTAL_F32};
use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Phase vocoder block size (fixed by the pitch_shift crate).
const BLOCK: usize = 128;
/// How many recent mono samples the visualizer can read.
pub const TAP_LEN: usize = 4096;

/// Low-end mode: skip all effects and the visualizer feed (only decoding + EQ remain).
static BYPASS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_bypass(on: bool) {
    BYPASS.store(on, Ordering::Relaxed);
}

macro_rules! params {
    ($($name:ident: $default:expr),* $(,)?) => {
        /// Effect settings shared with the audio thread.
        pub struct FxParams {
            $(pub $name: AtomicU32,)*
        }

        impl FxParams {
            pub fn new() -> Arc<Self> {
                Arc::new(FxParams { $($name: AtomicU32::new(($default as f32).to_bits()),)* })
            }

            /// Every setting as (name, value), for saving.
            pub fn all(&self) -> Vec<(&'static str, f32)> {
                vec![$((stringify!($name), f32::from_bits(self.$name.load(Ordering::Relaxed))),)*]
            }

            /// Set a setting by name (used when loading). Unknown names are ignored.
            pub fn set_named(&self, name: &str, v: f32) {
                match name {
                    $(stringify!($name) => self.$name.store(v.to_bits(), Ordering::Relaxed),)*
                    _ => {}
                }
            }

            pub fn reset(&self) {
                $(self.$name.store(($default as f32).to_bits(), Ordering::Relaxed);)*
            }
        }
    };
}

params! {
    tempo: 1.0,          // 0.5 .. 2.0, pitch preserved
    pitch: 0.0,          // semitones -12 .. 12
    reverb_mix: 0.0,     // 0 .. 1
    reverb_size: 0.6,    // 0 .. 1
    reverb_damp: 0.4,    // 0 .. 1
    echo_mix: 0.0,       // 0 .. 1
    echo_ms: 350.0,      // 50 .. 1000
    echo_feedback: 0.35, // 0 .. 0.9
    pump_depth: 0.0,     // 0 .. 1 (sidechain-style ducking)
    pump_bpm: 120.0,     // 60 .. 180
    width: 1.0,          // 0 (mono) .. 2 (extra wide)
    balance: 0.0,        // -1 (left) .. 1 (right)
    lowpass: 20000.0,    // Hz; >= 19000 is off
    highpass: 20.0,      // Hz; <= 25 is off
    rotate: 0.0,         // Hz; 8D rotation speed, 0 is off
    karaoke: 0.0,        // 0 / 1: vocal remover
    limiter: 0.0,        // 0 / 1
}

impl FxParams {
    pub fn get(a: &AtomicU32) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }
}

/// Per-deck state readable from the UI thread.
pub struct DeckTap {
    /// Input frames consumed (track time).
    pub frames: AtomicU64,
    pub rate: AtomicU32,
    /// Recent mono samples for the visualizer (newest at the end).
    pub samples: Mutex<VecDeque<f32>>,
}

impl DeckTap {
    pub fn new() -> Arc<Self> {
        Arc::new(DeckTap {
            frames: AtomicU64::new(0),
            rate: AtomicU32::new(44100),
            samples: Mutex::new(VecDeque::with_capacity(TAP_LEN)),
        })
    }

    pub fn position(&self) -> Duration {
        let rate = self.rate.load(Ordering::Relaxed).max(1) as f64;
        Duration::from_secs_f64(self.frames.load(Ordering::Relaxed) as f64 / rate)
    }
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn set(&mut self, high: bool, freq: f32, rate: f32) {
        let w0 = 2.0 * PI * freq.min(rate * 0.45) / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * 0.707);
        let a0 = 1.0 + alpha;
        let (b0, b1, b2) = if high {
            ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0)
        } else {
            ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0)
        };
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = -2.0 * cos / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Freeverb (Jezar's public-domain design).
struct Reverb {
    combs: [Vec<(Vec<f32>, usize, f32)>; 2],
    allpasses: [Vec<(Vec<f32>, usize)>; 2],
}

impl Reverb {
    fn new(rate: f32) -> Reverb {
        let scale = rate / 44100.0;
        let comb_t = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
        let ap_t = [556, 441, 341, 225];
        let mk = |spread: usize| {
            (
                comb_t.iter().map(|t| (vec![0.0; ((t + spread) as f32 * scale) as usize + 1], 0, 0.0)).collect(),
                ap_t.iter().map(|t| (vec![0.0; ((t + spread) as f32 * scale) as usize + 1], 0)).collect(),
            )
        };
        let (c0, a0) = mk(0);
        let (c1, a1) = mk(23);
        Reverb { combs: [c0, c1], allpasses: [a0, a1] }
    }

    #[inline]
    fn run(&mut self, ch: usize, input: f32, size: f32, damp: f32) -> f32 {
        let feedback = size * 0.28 + 0.7;
        let damp1 = damp * 0.4;
        let damp2 = 1.0 - damp1;
        let x = input * 0.015;
        let mut out = 0.0;
        for (buf, idx, store) in self.combs[ch].iter_mut() {
            let y = buf[*idx];
            *store = y * damp2 + *store * damp1;
            buf[*idx] = x + *store * feedback;
            *idx = (*idx + 1) % buf.len();
            out += y;
        }
        for (buf, idx) in self.allpasses[ch].iter_mut() {
            let b = buf[*idx];
            buf[*idx] = out + b * 0.5;
            *idx = (*idx + 1) % buf.len();
            out = b - out;
        }
        out * 3.0
    }
}

pub struct FxSource<S: Source> {
    inner: S,
    params: Arc<FxParams>,
    tap: Arc<DeckTap>,
    channels: usize,
    rate: f32,
    out: VecDeque<f32>,
    done: bool,
    // tempo / pitch
    shifters: Vec<Shifter<Box<RawState>>>,
    stretch_active: bool,
    // effects state
    hp: [Biquad; 2],
    lp: [Biquad; 2],
    filter_freqs: (f32, f32),
    echo: [Vec<f32>; 2],
    echo_pos: usize,
    reverb: Reverb,
    pump_phase: f32,
    rotate_phase: f32,
    limiter_env: f32,
    tap_chunk: Vec<f32>,
}

fn new_shifter() -> Shifter<Box<RawState>> {
    let state: Box<RawState> = vec![0.0f32; TOTAL_F32].into_boxed_slice().try_into().expect("state size");
    Shifter::new(state)
}

impl<S: Source> FxSource<S> {
    pub fn new(inner: S, params: Arc<FxParams>, tap: Arc<DeckTap>) -> Self {
        let channels = inner.channels().get() as usize;
        let rate = inner.sample_rate().get() as f32;
        tap.rate.store(rate as u32, Ordering::Relaxed);
        tap.frames.store(0, Ordering::Relaxed);
        tap.samples.lock().unwrap().clear();
        FxSource {
            inner,
            params,
            tap,
            channels,
            rate,
            out: VecDeque::with_capacity(BLOCK * 8),
            done: false,
            shifters: vec![],
            stretch_active: false,
            hp: Default::default(),
            lp: Default::default(),
            filter_freqs: (0.0, 0.0),
            echo: [vec![0.0; (rate * 2.0) as usize], vec![0.0; (rate * 2.0) as usize]],
            echo_pos: 0,
            reverb: Reverb::new(rate),
            pump_phase: 0.0,
            rotate_phase: 0.0,
            limiter_env: 0.0,
            tap_chunk: Vec::with_capacity(512),
        }
    }

    /// Decode one block, run it through the effects and queue the output.
    fn fill(&mut self) {
        let ch = self.channels.max(1);
        let mut block = vec![0.0f32; BLOCK * ch];
        let mut got = 0;
        for s in block.iter_mut() {
            match self.inner.next() {
                Some(v) => {
                    *s = v;
                    got += 1;
                }
                None => {
                    self.done = true;
                    break;
                }
            }
        }
        if got == 0 {
            return;
        }
        self.tap.frames.fetch_add((got / ch) as u64, Ordering::Relaxed);

        if BYPASS.load(Ordering::Relaxed) {
            self.stretch_active = false;
            self.out.extend(block[..got - got % ch].iter().copied());
            return;
        }

        let p = &self.params;
        let tempo = FxParams::get(&p.tempo).clamp(0.5, 2.0);
        let pitch = FxParams::get(&p.pitch).clamp(-12.0, 12.0);
        let stretch = (tempo - 1.0).abs() > 0.01 || pitch.abs() > 0.01;

        // Tempo / pitch.
        let frames: Vec<f32> = if stretch {
            if !self.stretch_active || self.shifters.len() != ch {
                self.shifters = (0..ch).map(|_| new_shifter()).collect();
            }
            self.stretch_active = true;
            let out_n = ((BLOCK as f32 / tempo).round() as usize).clamp(32, 512);
            let mut out = vec![0.0f32; out_n * ch];
            let mut mono = vec![0.0f32; BLOCK];
            for c in 0..ch {
                for i in 0..BLOCK {
                    mono[i] = block[i * ch + c];
                }
                let shifted = self.shifters[c].shift(&mono, pitch, out_n, self.rate);
                for i in 0..out_n {
                    out[i * ch + c] = shifted[i];
                }
            }
            out
        } else {
            self.stretch_active = false;
            block.truncate(got - got % ch);
            block
        };

        // Per-frame effects.
        let lowpass = FxParams::get(&p.lowpass);
        let highpass = FxParams::get(&p.highpass);
        if (lowpass, highpass) != self.filter_freqs {
            self.filter_freqs = (lowpass, highpass);
            for b in &mut self.lp {
                b.set(false, lowpass, self.rate);
            }
            for b in &mut self.hp {
                b.set(true, highpass, self.rate);
            }
        }
        let use_lp = lowpass < 19000.0;
        let use_hp = highpass > 25.0;
        let karaoke = FxParams::get(&p.karaoke) > 0.5;
        let echo_mix = FxParams::get(&p.echo_mix);
        let echo_len = ((FxParams::get(&p.echo_ms).clamp(50.0, 1000.0) / 1000.0) * self.rate) as usize;
        let echo_fb = FxParams::get(&p.echo_feedback).clamp(0.0, 0.9);
        let rev_mix = FxParams::get(&p.reverb_mix);
        let rev_size = FxParams::get(&p.reverb_size);
        let rev_damp = FxParams::get(&p.reverb_damp);
        let pump = FxParams::get(&p.pump_depth);
        let pump_step = FxParams::get(&p.pump_bpm).clamp(40.0, 240.0) / 60.0 / self.rate;
        let rotate = FxParams::get(&p.rotate);
        let width = FxParams::get(&p.width).clamp(0.0, 2.0);
        let balance = FxParams::get(&p.balance).clamp(-1.0, 1.0);
        let limiter = FxParams::get(&p.limiter) > 0.5;
        let echo_buf_len = self.echo[0].len();

        for frame in frames.chunks(ch) {
            let mut l = frame[0];
            let mut r = if ch > 1 { frame[1] } else { frame[0] };
            if use_hp {
                l = self.hp[0].run(l);
                r = self.hp[1].run(r);
            }
            if use_lp {
                l = self.lp[0].run(l);
                r = self.lp[1].run(r);
            }
            if karaoke {
                // Centre-panned vocals cancel out in the side signal.
                let side = (l - r) * 0.7;
                l = side;
                r = -side;
            }
            if echo_mix > 0.001 {
                let read = (self.echo_pos + echo_buf_len - echo_len) % echo_buf_len;
                let (dl, dr) = (self.echo[0][read], self.echo[1][read]);
                self.echo[0][self.echo_pos] = l + dl * echo_fb;
                self.echo[1][self.echo_pos] = r + dr * echo_fb;
                self.echo_pos = (self.echo_pos + 1) % echo_buf_len;
                l += dl * echo_mix;
                r += dr * echo_mix;
            }
            if rev_mix > 0.001 {
                let input = (l + r) * 0.5;
                let wl = self.reverb.run(0, input, rev_size, rev_damp);
                let wr = self.reverb.run(1, input, rev_size, rev_damp);
                l = l * (1.0 - rev_mix * 0.5) + wl * rev_mix;
                r = r * (1.0 - rev_mix * 0.5) + wr * rev_mix;
            }
            if pump > 0.001 {
                self.pump_phase = (self.pump_phase + pump_step) % 1.0;
                let gain = 1.0 - pump * (-self.pump_phase * 8.0).exp();
                l *= gain;
                r *= gain;
            }
            if rotate > 0.001 {
                self.rotate_phase = (self.rotate_phase + rotate / self.rate) % 1.0;
                let pan = (self.rotate_phase * 2.0 * PI).sin();
                let m = (l + r) * 0.5;
                l = m * ((1.0 - pan) * 0.5).sqrt() * 1.414;
                r = m * ((1.0 + pan) * 0.5).sqrt() * 1.414;
            }
            if (width - 1.0).abs() > 0.01 {
                let mid = (l + r) * 0.5;
                let side = (l - r) * 0.5 * width;
                l = mid + side;
                r = mid - side;
            }
            if balance.abs() > 0.01 {
                l *= (1.0 - balance).min(1.0);
                r *= (1.0 + balance).min(1.0);
            }
            if limiter {
                let peak = l.abs().max(r.abs());
                self.limiter_env = if peak > self.limiter_env { peak } else { self.limiter_env * 0.9995 };
                if self.limiter_env > 0.8 {
                    let g = 0.8 / self.limiter_env;
                    l *= g;
                    r *= g;
                }
            }

            if ch == 1 {
                self.out.push_back((l + r) * 0.5);
            } else {
                self.out.push_back(l);
                self.out.push_back(r);
                for &extra in &frame[2..] {
                    self.out.push_back(extra);
                }
            }

            self.tap_chunk.push((l + r) * 0.5);
        }

        if self.tap_chunk.len() >= 256 {
            let mut ring = self.tap.samples.lock().unwrap();
            ring.extend(self.tap_chunk.drain(..));
            let excess = ring.len().saturating_sub(TAP_LEN);
            ring.drain(..excess);
        }
    }
}

impl<S: Source> Iterator for FxSource<S> {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        while self.out.is_empty() {
            if self.done {
                return None;
            }
            self.fill();
        }
        self.out.pop_front()
    }
}

impl<S: Source> Source for FxSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let r = self.inner.try_seek(pos);
        if r.is_ok() {
            self.tap.frames.store((pos.as_secs_f64() * self.rate as f64) as u64, Ordering::Relaxed);
            self.out.clear();
            self.done = false;
            self.shifters.clear();
            self.stretch_active = false;
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::source::SineWave;

    fn run(params: &Arc<FxParams>) -> (Vec<f32>, Arc<DeckTap>) {
        let tap = DeckTap::new();
        let src = SineWave::new(440.0).take_duration(Duration::from_secs(1));
        let rate = src.sample_rate().get() as usize;
        let out: Vec<f32> = FxSource::new(src, params.clone(), tap.clone()).collect();
        assert!(rate > 0);
        (out, tap)
    }

    #[test]
    fn tempo_changes_length_but_not_track_position() {
        let p = FxParams::new();
        let (normal, _) = run(&p);
        p.set_named("tempo", 2.0);
        let (fast, tap) = run(&p);
        let ratio = fast.len() as f32 / normal.len() as f32;
        assert!((ratio - 0.5).abs() < 0.05, "ratio {ratio}");
        // Position is song time: the whole second was consumed.
        assert!((tap.position().as_secs_f32() - 1.0).abs() < 0.01);
    }

    #[test]
    fn effects_produce_finite_audio() {
        let p = FxParams::new();
        for (k, v) in [
            ("pitch", 5.0),
            ("reverb_mix", 0.5),
            ("echo_mix", 0.5),
            ("pump_depth", 0.6),
            ("rotate", 0.2),
            ("width", 1.8),
            ("balance", -0.5),
            ("lowpass", 2000.0),
            ("highpass", 300.0),
            ("karaoke", 1.0),
            ("limiter", 1.0),
        ] {
            p.set_named(k, v);
        }
        let (out, _) = run(&p);
        assert!(!out.is_empty());
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 4.0));
    }
}
