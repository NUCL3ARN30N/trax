//! Audio output (rodio) and the play queue.

use crate::eq::{EqParams, EqSource};
use crate::fx::{DeckTap, FxParams, FxSource};
use crate::model::Track;
use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use rodio::source::SeekError;
use rodio::{ChannelCount, Decoder, MixerDeviceSink, Player as RodioPlayer, SampleRate, Source};
use std::f32::consts::FRAC_PI_2;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Shape of a fade.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FadeCurve {
    Linear,
    /// Constant perceived loudness during a crossfade.
    EqualPower,
    /// Slow start and end.
    SCurve,
}

impl FadeCurve {
    pub fn from_int(i: i32) -> FadeCurve {
        match i {
            0 => FadeCurve::Linear,
            2 => FadeCurve::SCurve,
            _ => FadeCurve::EqualPower,
        }
    }

    fn as_u32(self) -> u32 {
        match self {
            FadeCurve::Linear => 0,
            FadeCurve::EqualPower => 1,
            FadeCurve::SCurve => 2,
        }
    }
}

/// Gain ramp shared with the audio thread: the source moves to `target` over `ms`.
struct FadeCtl {
    target: AtomicU32,
    ms: AtomicU32,
    curve: AtomicU32,
    version: AtomicU64,
}

impl FadeCtl {
    fn new() -> Arc<Self> {
        Arc::new(FadeCtl {
            target: AtomicU32::new(1f32.to_bits()),
            ms: AtomicU32::new(0),
            curve: AtomicU32::new(0),
            version: AtomicU64::new(0),
        })
    }

    fn ramp(&self, target: f32, dur: Duration, curve: FadeCurve) {
        self.target.store(target.to_bits(), Ordering::Relaxed);
        self.ms.store(dur.as_millis() as u32, Ordering::Relaxed);
        self.curve.store(curve.as_u32(), Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Release);
    }
}

/// Sample-accurate fade in/out wrapper.
struct Fader<S: Source> {
    inner: S,
    ctl: Arc<FadeCtl>,
    version: u64,
    gain: f32,
    from: f32,
    target: f32,
    progress: f32,
    step: f32,
    curve: u32,
}

impl<S: Source> Fader<S> {
    fn new(inner: S, ctl: Arc<FadeCtl>, initial: f32) -> Self {
        Fader { inner, ctl, version: 0, gain: initial, from: initial, target: initial, progress: 1.0, step: 0.0, curve: 0 }
    }

    fn shaped(&self) -> f32 {
        let p = self.progress;
        let rising = self.target > self.from;
        let s = match self.curve {
            1 if rising => (p * FRAC_PI_2).sin(),
            1 => 1.0 - (p * FRAC_PI_2).cos(),
            2 => p * p * (3.0 - 2.0 * p),
            _ => p,
        };
        self.from + (self.target - self.from) * s
    }
}

impl<S: Source> Iterator for Fader<S> {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        let v = self.ctl.version.load(Ordering::Acquire);
        if v != self.version {
            self.version = v;
            self.from = self.gain;
            self.target = f32::from_bits(self.ctl.target.load(Ordering::Relaxed));
            self.curve = self.ctl.curve.load(Ordering::Relaxed);
            let ms = self.ctl.ms.load(Ordering::Relaxed) as f32;
            let samples = ms / 1000.0 * self.inner.sample_rate().get() as f32 * self.inner.channels().get() as f32;
            if samples < 1.0 {
                self.progress = 1.0;
                self.gain = self.target;
            } else {
                self.progress = 0.0;
                self.step = 1.0 / samples;
            }
        }
        let s = self.inner.next()?;
        if self.progress < 1.0 {
            self.progress = (self.progress + self.step).min(1.0);
            self.gain = self.shaped();
        }
        Some(s * self.gain)
    }
}

impl<S: Source> Source for Fader<S> {
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
        self.inner.try_seek(pos)
    }
}

struct Deck {
    player: RodioPlayer,
    fade: Arc<FadeCtl>,
    tap: Arc<DeckTap>,
    loaded: bool,
    /// Volume normalization for the track on this deck (1.0 = play it as it is).
    gain: f32,
}

impl Deck {
    fn new(sink: &MixerDeviceSink) -> Deck {
        let player = RodioPlayer::connect_new(sink.mixer());
        player.pause();
        Deck { player, fade: FadeCtl::new(), tap: DeckTap::new(), loaded: false, gain: 1.0 }
    }

    fn clear(&mut self) {
        if self.loaded {
            self.player.clear();
        }
        self.loaded = false;
    }

    fn start(&mut self, decoder: Decoder<std::io::BufReader<std::fs::File>>, eq: &Arc<EqParams>, fx: &Arc<FxParams>, initial: f32) {
        self.clear();
        self.fade = FadeCtl::new();
        self.tap = DeckTap::new();
        let chain = FxSource::new(EqSource::new(decoder, eq.clone()), fx.clone(), self.tap.clone());
        self.player.append(Fader::new(chain, self.fade.clone(), initial));
        self.player.play();
        self.loaded = true;
    }
}

/// Two decks so the next track can fade in while the current one fades out.
pub struct AudioOutput {
    _sink: MixerDeviceSink,
    decks: [Deck; 2],
    active: usize,
    /// The previous deck is fading out until this instant.
    fading_until: Option<Instant>,
    pub eq: Arc<EqParams>,
    pub fx: Arc<FxParams>,
    volume: f32,
    muted: bool,
}

fn open_decoder(path: &Path) -> Result<Decoder<std::io::BufReader<std::fs::File>>> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    Decoder::builder()
        .with_data(std::io::BufReader::new(file))
        .with_byte_len(len)
        .with_seekable(true)
        .build()
        .with_context(|| format!("unsupported audio format: {}", path.display()))
}

impl AudioOutput {
    pub fn new() -> Result<Self> {
        let mut sink = rodio::DeviceSinkBuilder::open_default_sink().context("no audio output device")?;
        sink.log_on_drop(false);
        let decks = [Deck::new(&sink), Deck::new(&sink)];
        Ok(AudioOutput {
            _sink: sink,
            decks,
            active: 0,
            fading_until: None,
            eq: EqParams::new(),
            fx: FxParams::new(),
            volume: 0.7,
            muted: false,
        })
    }

    fn cur(&self) -> &Deck {
        &self.decks[self.active]
    }

    /// Load a file and start playing it immediately (no crossfade). `gain` is the volume
    /// normalization factor for this track.
    pub fn load(&mut self, path: &Path, gain: f32) -> Result<Option<Duration>> {
        let decoder = open_decoder(path)?;
        let duration = decoder.total_duration();
        self.end_fade();
        let (eq, fx) = (self.eq.clone(), self.fx.clone());
        self.decks[self.active].gain = gain;
        self.decks[self.active].start(decoder, &eq, &fx, 1.0);
        self.apply_volume();
        Ok(duration)
    }

    /// Start `path` on the other deck, fading it in while the current track fades out.
    pub fn crossfade_to(&mut self, path: &Path, dur: Duration, curve: FadeCurve, gain: f32) -> Result<Option<Duration>> {
        let decoder = open_decoder(path)?;
        let duration = decoder.total_duration();
        self.end_fade();
        let next = 1 - self.active;
        let (eq, fx) = (self.eq.clone(), self.fx.clone());
        self.decks[next].gain = gain;
        self.decks[next].start(decoder, &eq, &fx, 0.0);
        self.decks[next].fade.ramp(1.0, dur, curve);
        self.decks[self.active].fade.ramp(0.0, dur, curve);
        self.active = next;
        self.fading_until = Some(Instant::now() + dur + Duration::from_millis(100));
        self.apply_volume();
        Ok(duration)
    }

    pub fn is_crossfading(&self) -> bool {
        self.fading_until.is_some()
    }

    /// Called periodically: releases the deck that finished fading out.
    pub fn tick(&mut self) {
        if let Some(until) = self.fading_until {
            if Instant::now() >= until {
                self.end_fade();
            }
        }
    }

    fn end_fade(&mut self) {
        if self.fading_until.take().is_some() {
            self.decks[1 - self.active].clear();
        }
    }

    pub fn stop(&mut self) {
        self.fading_until = None;
        for d in &mut self.decks {
            d.clear();
        }
    }

    pub fn play(&self) {
        for (i, d) in self.decks.iter().enumerate() {
            if d.loaded && (i == self.active || self.fading_until.is_some()) {
                d.player.play();
            }
        }
    }

    pub fn pause(&self) {
        for d in &self.decks {
            d.player.pause();
        }
    }

    /// Start fading out for a pause; call `pause()` once `dur` has passed.
    pub fn fade_out_for_pause(&self, dur: Duration) {
        self.cur().fade.ramp(0.0, dur, FadeCurve::Linear);
    }

    /// Resume with a short fade-in.
    pub fn play_faded(&self, dur: Duration) {
        let d = self.cur();
        d.fade.ramp(0.0, Duration::ZERO, FadeCurve::Linear);
        self.play();
        d.fade.ramp(1.0, dur, FadeCurve::Linear);
    }

    /// Undo a pause fade-out (e.g. play pressed again before the pause happened).
    pub fn restore_gain(&self) {
        self.cur().fade.ramp(1.0, Duration::from_millis(80), FadeCurve::Linear);
    }

    pub fn is_playing(&self) -> bool {
        let d = self.cur();
        d.loaded && !d.player.is_paused() && !d.player.empty()
    }

    /// True once the loaded track has played to the end.
    /// A track is loaded (playing or paused).
    pub fn is_loaded(&self) -> bool {
        self.cur().loaded
    }

    pub fn finished(&self) -> bool {
        let d = self.cur();
        d.loaded && d.player.empty()
    }

    /// Position in the track (unaffected by the tempo setting).
    pub fn position(&self) -> Duration {
        let d = self.cur();
        if d.loaded { d.tap.position() } else { Duration::ZERO }
    }

    /// Recent mono samples of the playing track and their sample rate (for the visualizer).
    pub fn recent_samples(&self, n: usize) -> (Vec<f32>, u32) {
        let d = self.cur();
        let ring = d.tap.samples.lock().unwrap();
        let start = ring.len().saturating_sub(n);
        (ring.range(start..).copied().collect(), d.tap.rate.load(Ordering::Relaxed))
    }

    pub fn seek(&mut self, pos: Duration) {
        self.end_fade();
        let d = self.cur();
        if d.loaded {
            if let Err(e) = d.player.try_seek(pos) {
                log::warn!("seek failed: {e}");
            }
        }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        self.apply_volume();
    }

    pub fn set_muted(&mut self, m: bool) {
        self.muted = m;
        self.apply_volume();
    }

    /// Volume normalization for the track that is playing (used when its loudness was only
    /// measured after it started, and when the setting is switched on or off).
    pub fn set_gain(&mut self, gain: f32) {
        self.decks[self.active].gain = gain;
        self.apply_volume();
    }

    fn apply_volume(&self) {
        // Perceptual curve: slider position -> amplitude, times the track's normalization.
        let amp = if self.muted { 0.0 } else { self.volume * self.volume };
        for d in &self.decks {
            d.player.set_volume(amp * d.gain);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeat {
    Off,
    All,
    One,
}

impl Repeat {
    pub fn next(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    pub fn as_int(self) -> i32 {
        match self {
            Repeat::Off => 0,
            Repeat::All => 1,
            Repeat::One => 2,
        }
    }

    pub fn from_int(i: i32) -> Repeat {
        match i {
            1 => Repeat::All,
            2 => Repeat::One,
            _ => Repeat::Off,
        }
    }
}

/// Spotify-like queue: an ordered list with a cursor. Shuffle reorders the upcoming part
/// and remembers the original order so it can be restored.
pub struct Queue {
    pub tracks: Vec<Track>,
    pub index: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    original: Option<Vec<Track>>,
}

impl Queue {
    pub fn new() -> Self {
        Queue { tracks: vec![], index: None, shuffle: false, repeat: Repeat::Off, original: None }
    }

    pub fn current(&self) -> Option<&Track> {
        self.index.and_then(|i| self.tracks.get(i))
    }

    pub fn current_mut(&mut self) -> Option<&mut Track> {
        self.index.and_then(|i| self.tracks.get_mut(i))
    }

    /// Replace the queue with `tracks` and start at `start`.
    pub fn set(&mut self, tracks: Vec<Track>, start: usize) {
        self.tracks = tracks;
        self.index = if self.tracks.is_empty() { None } else { Some(start.min(self.tracks.len() - 1)) };
        self.original = None;
        if self.shuffle {
            self.shuffle_upcoming();
        }
    }

    pub fn push(&mut self, t: Track) {
        self.tracks.push(t);
        if self.index.is_none() {
            self.index = Some(self.tracks.len() - 1);
        }
    }

    pub fn insert_next(&mut self, t: Track) {
        match self.index {
            Some(i) => self.tracks.insert(i + 1, t),
            None => self.push(t),
        }
    }

    pub fn remove(&mut self, idx: usize) {
        if idx >= self.tracks.len() {
            return;
        }
        self.tracks.remove(idx);
        if let Some(i) = self.index {
            if idx < i {
                self.index = Some(i - 1);
            } else if self.tracks.is_empty() {
                self.index = None;
            } else if i >= self.tracks.len() {
                self.index = Some(self.tracks.len() - 1);
            }
        }
    }

    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.tracks.len() || to >= self.tracks.len() || from == to {
            return;
        }
        let t = self.tracks.remove(from);
        self.tracks.insert(to, t);
        if let Some(i) = self.index {
            self.index = Some(if i == from {
                to
            } else if from < i && to >= i {
                i - 1
            } else if from > i && to <= i {
                i + 1
            } else {
                i
            });
        }
    }

    /// Keep only the current track.
    pub fn clear_upcoming(&mut self) {
        if let Some(i) = self.index {
            self.tracks.truncate(i + 1);
        } else {
            self.tracks.clear();
        }
        self.original = None;
    }

    pub fn next_index(&self, user_initiated: bool) -> Option<usize> {
        let i = self.index?;
        if self.repeat == Repeat::One && !user_initiated {
            return Some(i);
        }
        if i + 1 < self.tracks.len() {
            Some(i + 1)
        } else if self.repeat != Repeat::Off && !self.tracks.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    pub fn prev_index(&self) -> Option<usize> {
        let i = self.index?;
        if i > 0 {
            Some(i - 1)
        } else if self.repeat == Repeat::All && !self.tracks.is_empty() {
            Some(self.tracks.len() - 1)
        } else {
            Some(0)
        }
    }

    pub fn set_shuffle(&mut self, on: bool) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        if on {
            self.original = Some(self.tracks.clone());
            self.shuffle_upcoming();
        } else if let Some(orig) = self.original.take() {
            // Restore the original order, keeping the current track playing.
            let current_id = self.current().map(|t| (t.source, t.uri.clone()));
            // Tracks added while shuffled are appended in queue order.
            let mut restored = orig.clone();
            for t in &self.tracks {
                if !orig.iter().any(|o| o.source == t.source && o.uri == t.uri) {
                    restored.push(t.clone());
                }
            }
            restored.retain(|o| self.tracks.iter().any(|t| t.source == o.source && t.uri == o.uri));
            self.tracks = restored;
            self.index = current_id
                .and_then(|(s, u)| self.tracks.iter().position(|t| t.source == s && t.uri == u))
                .or(if self.tracks.is_empty() { None } else { Some(0) });
        }
    }

    fn shuffle_upcoming(&mut self) {
        let start = self.index.map(|i| i + 1).unwrap_or(0);
        if start < self.tracks.len() {
            self.tracks[start..].shuffle(&mut rand::rng());
        }
    }
}
