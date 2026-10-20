//! 10-band graphic equalizer implemented as a rodio `Source` adapter.

use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

pub const BAND_FREQS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
pub const BAND_LABELS: [&str; 10] = ["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];
pub const MAX_GAIN_DB: f32 = 12.0;

pub const PRESETS: &[(&str, [f32; 10])] = &[
    ("Flat", [0.0; 10]),
    ("Bass boost", [6.0, 5.0, 4.0, 2.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Bass reducer", [-6.0, -5.0, -4.0, -2.5, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Treble boost", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.5, 4.0, 5.0, 6.0]),
    ("Rock", [4.5, 3.5, 2.0, 0.5, -1.0, -0.5, 1.0, 2.5, 3.5, 4.0]),
    ("Pop", [-1.0, 0.0, 1.5, 3.0, 4.0, 3.0, 1.5, 0.0, -0.5, -1.0]),
    ("Jazz", [3.0, 2.0, 1.0, 2.0, -1.5, -1.5, 0.0, 1.5, 3.0, 3.5]),
    ("Classical", [4.0, 3.0, 2.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.5]),
    ("Electronic", [4.5, 4.0, 1.5, 0.0, -2.0, 1.5, 1.0, 1.5, 4.0, 5.0]),
    ("Hip-hop", [5.0, 4.0, 1.5, 3.0, -1.0, -1.0, 1.5, -0.5, 2.0, 3.0]),
    ("Vocal", [-2.0, -3.0, -3.0, 1.5, 3.5, 3.5, 3.0, 1.5, 0.0, -1.5]),
    ("Loudness", [6.0, 4.0, 0.0, 0.0, -2.0, 0.0, -1.0, -5.0, 5.0, 1.0]),
    ("Acoustic", [4.5, 4.5, 3.5, 1.0, 2.0, 1.5, 3.0, 3.5, 3.0, 2.0]),
];

/// Equalizer state shared between the UI thread and the audio thread.
pub struct EqParams {
    gains: [AtomicU32; 10],
    enabled: AtomicBool,
    version: AtomicU64,
}

impl EqParams {
    pub fn new() -> Arc<Self> {
        Arc::new(EqParams {
            gains: Default::default(),
            enabled: AtomicBool::new(false),
            version: AtomicU64::new(1),
        })
    }

    pub fn set_gain(&self, band: usize, db: f32) {
        if band < 10 {
            self.gains[band].store(db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB).to_bits(), Ordering::Relaxed);
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    pub fn gain(&self, band: usize) -> f32 {
        f32::from_bits(self.gains[band].load(Ordering::Relaxed))
    }

    pub fn gains(&self) -> [f32; 10] {
        std::array::from_fn(|i| self.gain(i))
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Release);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    /// RBJ audio-EQ-cookbook peaking filter.
    fn peaking(freq: f32, gain_db: f32, q: f32, rate: f32) -> Option<Biquad> {
        if gain_db.abs() < 0.05 || freq >= rate * 0.45 {
            return None;
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Some(Biquad {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / a) / a0,
        })
    }
}

#[derive(Clone, Copy, Default)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

pub struct EqSource<S: Source> {
    inner: S,
    params: Arc<EqParams>,
    version: u64,
    filters: Vec<Biquad>,
    /// One state per (filter, channel).
    state: Vec<BiquadState>,
    channels: usize,
    rate: u32,
    channel_idx: usize,
    preamp: f32,
    active: bool,
    check_counter: u32,
}

impl<S: Source> EqSource<S> {
    pub fn new(inner: S, params: Arc<EqParams>) -> Self {
        let mut s = EqSource {
            channels: inner.channels().get() as usize,
            rate: inner.sample_rate().get(),
            inner,
            params,
            version: 0,
            filters: vec![],
            state: vec![],
            channel_idx: 0,
            preamp: 1.0,
            active: false,
            check_counter: 0,
        };
        s.rebuild();
        s
    }

    fn rebuild(&mut self) {
        self.version = self.params.version.load(Ordering::Acquire);
        self.channels = self.inner.channels().get() as usize;
        self.rate = self.inner.sample_rate().get();
        self.active = self.params.enabled();
        let gains = self.params.gains();
        self.filters = if self.active {
            BAND_FREQS
                .iter()
                .zip(gains.iter())
                .filter_map(|(f, g)| Biquad::peaking(*f, *g, 1.41, self.rate as f32))
                .collect()
        } else {
            vec![]
        };
        // Leave headroom for boosts so the EQ doesn't clip.
        let max_boost = gains.iter().cloned().fold(0.0f32, f32::max);
        self.preamp = if self.active { 10f32.powf(-max_boost * 0.8 / 20.0) } else { 1.0 };
        let needed = self.filters.len() * self.channels.max(1);
        if self.state.len() != needed {
            self.state = vec![BiquadState::default(); needed];
        }
    }
}

impl<S: Source> Iterator for EqSource<S> {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;
        self.check_counter += 1;
        if self.check_counter >= 1024 && self.channel_idx == 0 {
            self.check_counter = 0;
            if self.params.version.load(Ordering::Acquire) != self.version
                || self.inner.channels().get() as usize != self.channels
                || self.inner.sample_rate().get() != self.rate
            {
                self.rebuild();
            }
        }
        let ch = self.channel_idx;
        self.channel_idx = (self.channel_idx + 1) % self.channels.max(1);
        if !self.active || self.filters.is_empty() {
            return Some(sample);
        }
        let mut x = sample * self.preamp;
        for (i, f) in self.filters.iter().enumerate() {
            let st = &mut self.state[i * self.channels + ch];
            let y = f.b0 * x + f.b1 * st.x1 + f.b2 * st.x2 - f.a1 * st.y1 - f.a2 * st.y2;
            st.x2 = st.x1;
            st.x1 = x;
            st.y2 = st.y1;
            st.y1 = y;
            x = y;
        }
        Some(x.clamp(-1.0, 1.0))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for EqSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
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
        for s in &mut self.state {
            *s = BiquadState::default();
        }
        self.channel_idx = 0;
        r
    }
}
