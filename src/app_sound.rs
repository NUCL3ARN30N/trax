//! Sound features: effects and presets, the spectrum visualizer, favourites, favourite song
//! sections and the audio cache limit.

use super::{App, s};
use crate::connectors::cache;
use crate::fx::FxParams;
use crate::model::{Track, format_duration};
use rustfft::num_complex::Complex32;
use slint::{ComponentHandle, Model};
use std::time::Duration;

/// Effect presets: (name, settings). Settings not listed are reset to neutral first.
pub(super) const FX_PRESETS: &[(&str, &[(&str, f32)])] = &[
    ("Nightcore", &[("tempo", 1.2), ("pitch", 3.0)]),
    ("Slowed + reverb", &[("tempo", 0.85), ("pitch", -2.5), ("reverb_mix", 0.35), ("reverb_size", 0.85)]),
    ("Vaporwave", &[("tempo", 0.8), ("pitch", -4.0), ("reverb_mix", 0.2), ("lowpass", 9000.0)]),
    ("8D audio", &[("rotate", 0.12), ("reverb_mix", 0.2)]),
    ("Club pump", &[("pump_depth", 0.45), ("pump_bpm", 124.0), ("width", 1.3)]),
    ("Lo-fi", &[("lowpass", 3800.0), ("highpass", 140.0), ("reverb_mix", 0.12), ("width", 0.8)]),
    ("Concert hall", &[("reverb_mix", 0.45), ("reverb_size", 0.95), ("reverb_damp", 0.3)]),
    ("Karaoke", &[("karaoke", 1.0)]),
];

/// Number of visualizer bars and the FFT size.
pub(super) const BARS: usize = 40;
const FFT_LEN: usize = 2048;

/// "m:ss" / "h:mm:ss" / plain seconds -> milliseconds. Empty or invalid -> 0.
fn parse_time(text: &str) -> i64 {
    let mut total = 0.0f64;
    for part in text.trim().split(':') {
        let Ok(v) = part.trim().replace(',', ".").parse::<f64>() else { return 0 };
        total = total * 60.0 + v;
    }
    (total.max(0.0) * 1000.0) as i64
}

fn time_text(ms: i64) -> String {
    if ms <= 0 { String::new() } else { format_duration(ms) }
}

impl App {
    // ------------------------------------------------------------------ effects

    /// Load saved effect settings into the player and the UI.
    pub(super) fn fx_load(&self) {
        let saved = self.db.lock().unwrap().setting_or("fx", "");
        if let Some(audio) = &self.audio {
            audio.fx.reset();
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&saved) {
                for (k, v) in map {
                    if let Some(v) = v.as_f64() {
                        audio.fx.set_named(&k, v as f32);
                    }
                }
            }
        }
        self.fx_push_ui();
    }

    fn fx_save(&self) {
        let Some(audio) = &self.audio else { return };
        let map: serde_json::Map<String, serde_json::Value> =
            audio.fx.all().into_iter().map(|(k, v)| (k.to_string(), serde_json::json!(v))).collect();
        self.db.lock().unwrap().set_setting("fx", &serde_json::Value::Object(map).to_string());
    }

    pub(super) fn fx_push_ui(&self) {
        let Some(audio) = &self.audio else { return };
        let f = &audio.fx;
        let b = self.backend();
        b.set_fx_tempo(FxParams::get(&f.tempo));
        b.set_fx_pitch(FxParams::get(&f.pitch));
        b.set_fx_reverb_mix(FxParams::get(&f.reverb_mix));
        b.set_fx_reverb_size(FxParams::get(&f.reverb_size));
        b.set_fx_reverb_damp(FxParams::get(&f.reverb_damp));
        b.set_fx_echo_mix(FxParams::get(&f.echo_mix));
        b.set_fx_echo_ms(FxParams::get(&f.echo_ms));
        b.set_fx_echo_feedback(FxParams::get(&f.echo_feedback));
        b.set_fx_pump_depth(FxParams::get(&f.pump_depth));
        b.set_fx_pump_bpm(FxParams::get(&f.pump_bpm));
        b.set_fx_width(FxParams::get(&f.width));
        b.set_fx_balance(FxParams::get(&f.balance));
        b.set_fx_lowpass(FxParams::get(&f.lowpass));
        b.set_fx_highpass(FxParams::get(&f.highpass));
        b.set_fx_rotate(FxParams::get(&f.rotate));
        b.set_fx_karaoke(FxParams::get(&f.karaoke) > 0.5);
        b.set_fx_limiter(FxParams::get(&f.limiter) > 0.5);
    }

    pub(super) fn fx_set(&self, name: &str, value: f32) {
        if let Some(audio) = &self.audio {
            audio.fx.set_named(name, value);
        }
        self.fx_push_ui();
        self.fx_save();
    }

    pub(super) fn fx_preset(&mut self, name: &str) {
        let Some((_, values)) = FX_PRESETS.iter().find(|(n, _)| *n == name) else { return };
        if let Some(audio) = &self.audio {
            audio.fx.reset();
            for (k, v) in values.iter() {
                audio.fx.set_named(k, *v);
            }
        }
        self.fx_push_ui();
        self.fx_save();
        self.toast(format!("{name} on"));
    }

    pub(super) fn fx_reset(&self) {
        if let Some(audio) = &self.audio {
            audio.fx.reset();
        }
        self.fx_push_ui();
        self.fx_save();
    }

    /// Tempo factor of the effects (song time runs this much faster than wall time).
    pub(super) fn tempo(&self) -> f32 {
        self.audio.as_ref().map(|a| FxParams::get(&a.fx.tempo).clamp(0.5, 2.0)).unwrap_or(1.0)
    }

    // ------------------------------------------------------------------ visualizer

    pub(super) fn set_visualizer(&mut self, on: bool) {
        self.backend().set_visualizer(on);
        self.db.lock().unwrap().set_setting_bool("visualizer", on);
        self.update_visual_timer();
    }

    /// Low-end mode: no animations, shadows, visualizer or sound effects. The individual
    /// settings are kept and apply again when the mode is switched off.
    pub(super) fn set_low_end(&mut self, on: bool) {
        self.backend().set_low_end(on);
        self.window.global::<crate::Theme>().set_low_end(on);
        self.popup.global::<crate::Theme>().set_low_end(on);
        crate::fx::set_bypass(on);
        self.db.lock().unwrap().set_setting_bool("low_end", on);
        self.update_visual_timer();
    }

    pub(super) fn set_fs_background(&mut self, mode: i32) {
        self.backend().set_fs_background(mode);
        self.db.lock().unwrap().set_setting("fs_background", &mode.to_string());
        self.update_visual_timer();
    }

    pub(super) fn set_visual_style(&mut self, style: i32) {
        self.backend().set_visual_style(style);
        self.db.lock().unwrap().set_setting("visual_style", &style.to_string());
    }

    /// Run the analyser only while something shows it (player bar or fullscreen).
    pub(super) fn update_visual_timer(&mut self) {
        let b = self.backend();
        let needed = !b.get_low_end() && (b.get_visualizer() || (b.get_fullscreen() && b.get_fs_background() != 0));
        if needed {
            if !self.visual_timer.running() {
                self.visual_timer.start(slint::TimerMode::Repeated, Duration::from_millis(33), || {
                    super::with_app(|a| a.visual_tick());
                });
            }
        } else {
            self.visual_timer.stop();
            for i in 0..BARS {
                self.spectrum.set_row_data(i, 0.0);
                self.spectrum_peaks.set_row_data(i, 0.0);
            }
            self.spectrum_smooth = vec![0.0; BARS];
        }
    }

    /// Compute the bar heights from the most recent samples.
    fn visual_tick(&mut self) {
        let visible = self.window.window().is_visible() && !self.window.window().is_minimized();
        let playing = self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false);
        let mut target = [0.0f32; BARS];
        if visible && playing {
            if let Some(audio) = &self.audio {
                let (samples, rate) = audio.recent_samples(FFT_LEN);
                if samples.len() == FFT_LEN {
                    self.update_shape_paths(&samples);
                    let mut buf: Vec<Complex32> = samples
                        .iter()
                        .enumerate()
                        .map(|(i, &x)| {
                            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT_LEN - 1) as f32).cos();
                            Complex32::new(x * w, 0.0)
                        })
                        .collect();
                    self.fft.process(&mut buf);
                    let hz_per_bin = rate as f32 / FFT_LEN as f32;
                    let (lo, hi) = (40.0f32, 16000.0f32);
                    for (b, t) in target.iter_mut().enumerate() {
                        let f0 = lo * (hi / lo).powf(b as f32 / BARS as f32);
                        let f1 = lo * (hi / lo).powf((b + 1) as f32 / BARS as f32);
                        let i0 = ((f0 / hz_per_bin) as usize).clamp(1, FFT_LEN / 2 - 1);
                        let i1 = ((f1 / hz_per_bin) as usize).clamp(i0 + 1, FFT_LEN / 2);
                        let mag = buf[i0..i1].iter().map(|c| c.norm()).fold(0.0f32, f32::max) / (FFT_LEN as f32 / 4.0);
                        let db = 20.0 * (mag + 1e-9).log10();
                        *t = ((db + 58.0) / 52.0).clamp(0.0, 1.0);
                    }
                }
            }
        }
        // Fast attack, slow release; peak caps fall slowly.
        for i in 0..BARS {
            let old = self.spectrum_smooth[i];
            let new = if target[i] > old { target[i] } else { old * 0.88 + target[i] * 0.12 };
            self.spectrum_smooth[i] = new;
            if (self.spectrum.row_data(i).unwrap_or(0.0) - new).abs() > 0.002 {
                self.spectrum.set_row_data(i, new);
            }
            let peak = self.spectrum_peaks.row_data(i).unwrap_or(0.0);
            let next_peak = if new >= peak { new } else { (peak - 0.012).max(new) };
            if (peak - next_peak).abs() > 0.001 {
                self.spectrum_peaks.set_row_data(i, next_peak);
            }
        }
        if !playing && self.backend().get_fullscreen() {
            let b = self.backend();
            if !b.get_wave_path().is_empty() {
                b.set_wave_path(s("M 0 200 L 1000 200"));
            }
        }
        if self.backend().get_fullscreen() && self.backend().get_visual_style() == 3 {
            self.update_circle_path();
        }
    }

    /// Oscilloscope path (viewbox 1000 x 400) from the latest samples.
    fn update_shape_paths(&self, samples: &[f32]) {
        let b = self.backend();
        if !b.get_fullscreen() || b.get_visual_style() != 2 {
            return;
        }
        let window = &samples[samples.len().saturating_sub(1024)..];
        let peak = window.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(0.15);
        let points = 200;
        let mut path = String::with_capacity(points * 14);
        for p in 0..points {
            let s_ = window[p * window.len() / points] / peak;
            let x = p as f32 * 1000.0 / (points - 1) as f32;
            let y = 200.0 - s_ * 170.0;
            path.push_str(&format!("{} {x:.1} {y:.1} ", if p == 0 { "M" } else { "L" }));
        }
        b.set_wave_path(s(path));
    }

    /// Radial bars (viewbox 1000 x 1000) mirrored left/right from the spectrum.
    fn update_circle_path(&self) {
        let n = BARS * 2;
        let mut path = String::with_capacity(n * 40);
        for k in 0..n {
            let band = if k < BARS { k } else { n - 1 - k };
            let v = self.spectrum_smooth[band];
            let angle = (k as f32 / n as f32) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let (sin, cos) = angle.sin_cos();
            let (r0, r1) = (200.0, 220.0 + v * 260.0);
            path.push_str(&format!(
                "M {:.1} {:.1} L {:.1} {:.1} ",
                500.0 + cos * r0,
                500.0 + sin * r0,
                500.0 + cos * r1,
                500.0 + sin * r1
            ));
        }
        self.backend().set_circle_path(s(path));
    }

    // ------------------------------------------------------------------ favourites

    pub(super) fn load_favorites(&self) {
        *self.favorites.borrow_mut() = self.db.lock().unwrap().favorite_ids();
    }

    pub(super) fn toggle_favorite(&mut self, list: &str, index: i32) {
        let Some(mut t) = self.track_at(list, index) else { return };
        self.toggle_favorite_track(&mut t);
    }

    pub(super) fn toggle_favorite_current(&mut self) {
        let Some(mut t) = self.queue.current().cloned() else { return };
        self.toggle_favorite_track(&mut t);
    }

    fn toggle_favorite_track(&mut self, t: &mut Track) {
        let Some(id) = self.ensure_id(t) else { return };
        let on = !self.favorites.borrow().contains(&id);
        let res = {
            let db = self.db.lock().unwrap();
            // Starring a YouTube / SoundCloud result also keeps it in the library.
            if on {
                let _ = db.set_in_library(id, true);
            }
            db.set_favorite(id, on)
        };
        if let Err(e) = res {
            self.toast(format!("Error: {e}"));
            return;
        }
        if on {
            self.favorites.borrow_mut().insert(id);
        } else {
            self.favorites.borrow_mut().remove(&id);
        }
        for m in self.all_models() {
            m.favorite_changed(id);
        }
        self.update_now_favorite();
        self.refresh_playlists();
        if let super::PlaylistView::User(pid) = self.view {
            if self.db.lock().unwrap().favorites_id().ok() == Some(pid) {
                self.reload_playlist_view();
            }
        }
        self.toast(if on { "Added to Favourites" } else { "Removed from Favourites" });
    }

    pub(super) fn update_now_favorite(&self) {
        let fav = self.queue.current().map(|t| t.id > 0 && self.favorites.borrow().contains(&t.id)).unwrap_or(false);
        self.backend().set_now_favorite(fav);
    }

    // ------------------------------------------------------------------ song sections

    pub(super) fn sections_on(&self) -> bool {
        self.backend().get_sections_enabled()
    }

    /// Where the current track should stop (its favourite section end, or the real end).
    pub(super) fn effective_end_ms(&self) -> i64 {
        let end = self.queue.current().map(|t| t.end_ms).unwrap_or(0);
        if self.sections_on() && end > 0 && (self.duration_ms == 0 || end < self.duration_ms) {
            end
        } else {
            self.duration_ms
        }
    }

    /// Jump to the favourite section start of a track that just started.
    pub(super) fn apply_section_start(&mut self) {
        let start = self.queue.current().map(|t| t.start_ms).unwrap_or(0);
        if self.sections_on() && start > 0 {
            if let Some(a) = &mut self.audio {
                a.seek(Duration::from_millis(start as u64));
            }
        }
    }

    pub(super) fn section_fields_for(&self, t: &Track) {
        let b = self.backend();
        b.set_edit_start(s(time_text(t.start_ms)));
        b.set_edit_end(s(time_text(t.end_ms)));
        let current = self.queue.current().map(|c| c.source == t.source && c.uri == t.uri).unwrap_or(false);
        b.set_edit_is_current(current);
    }

    pub(super) fn section_from_position(&self, start: bool) {
        let pos = self.audio.as_ref().map(|a| a.position()).unwrap_or_default();
        let text = s(format_duration(pos.as_millis() as i64));
        if start {
            self.backend().set_edit_start(text);
        } else {
            self.backend().set_edit_end(text);
        }
    }

    /// Store the section typed in the edit dialog. Returns (start, end) in ms.
    pub(super) fn save_section(&self, id: i64) -> (i64, i64) {
        let b = self.backend();
        let start = parse_time(&b.get_edit_start());
        let mut end = parse_time(&b.get_edit_end());
        if end > 0 && end <= start {
            end = 0;
        }
        let _ = self.db.lock().unwrap().set_section(id, start, end);
        (start, end)
    }

    // ------------------------------------------------------------------ resume

    /// Remember the queue, the current song and its position for the next start.
    pub(super) fn save_resume(&self) {
        if !self.backend().get_resume_enabled() {
            return;
        }
        let ids: Vec<i64> = self.queue.tracks.iter().map(|t| t.id).filter(|id| *id > 0).collect();
        let index = self
            .queue
            .current()
            .and_then(|c| ids.iter().position(|id| *id == c.id))
            .map(|i| i as i64)
            .unwrap_or(-1);
        let pos = match self.resume_pos {
            Some(p) => p.as_millis() as i64,
            None => self.audio.as_ref().map(|a| a.position().as_millis() as i64).unwrap_or(0),
        };
        let state = serde_json::json!({ "ids": ids, "index": index, "pos_ms": pos });
        self.db.lock().unwrap().set_setting("resume", &state.to_string());
    }

    /// Restore the saved queue and song (paused, at the saved position).
    pub(super) fn restore_resume(&mut self) {
        if !self.backend().get_resume_enabled() {
            return;
        }
        let saved = self.db.lock().unwrap().setting_or("resume", "");
        let Ok(state) = serde_json::from_str::<serde_json::Value>(&saved) else { return };
        let tracks: Vec<Track> = {
            let db = self.db.lock().unwrap();
            state["ids"].as_array().into_iter().flatten().filter_map(|v| v.as_i64()).filter_map(|id| db.track(id)).collect()
        };
        let index = state["index"].as_i64().unwrap_or(-1);
        if tracks.is_empty() || index < 0 || index as usize >= tracks.len() {
            return;
        }
        self.queue.tracks = tracks;
        self.queue.index = Some(index as usize);
        let t = self.queue.tracks[index as usize].clone();
        self.duration_ms = t.duration_ms;
        let pos = Duration::from_millis(state["pos_ms"].as_i64().unwrap_or(0).max(0) as u64);
        self.resume_pos = Some(pos);
        self.show_now_playing(&t);
        self.refresh_queue();
        self.update_now_favorite();
        let b = self.backend();
        b.set_playing(false);
        b.set_position(pos.as_secs_f32());
        b.set_position_text(s(format_duration(pos.as_millis() as i64)));
        self.update_tray();
    }

    // ------------------------------------------------------------------ cache

    pub(super) fn cache_limit_bytes(&self) -> u64 {
        self.backend().get_cache_limit_mb().max(0) as u64 * 1_048_576
    }

    /// Enforce the cache limit in the background and refresh the usage text.
    pub(super) fn trim_cache(&self) {
        let limit = self.cache_limit_bytes();
        std::thread::spawn(move || {
            let removed = cache::enforce(limit);
            if removed > 0 {
                log::info!("cache: removed {removed} least recently played files");
            }
            let (bytes, files) = cache::usage();
            super::post(move |a| a.show_cache_usage(bytes, files));
        });
    }

    fn show_cache_usage(&self, bytes: u64, files: usize) {
        let limit = self.cache_limit_bytes();
        let text = if limit == 0 {
            format!("Using {} for {files} songs (no limit)", cache::format_size(bytes))
        } else {
            format!("Using {} of {} for {files} songs", cache::format_size(bytes), cache::format_size(limit))
        };
        self.backend().set_cache_usage(s(text));
    }
}

#[cfg(test)]
mod tests {
    use super::parse_time;

    #[test]
    fn times() {
        assert_eq!(parse_time("1:30"), 90_000);
        assert_eq!(parse_time("45"), 45_000);
        assert_eq!(parse_time("1:02:03"), 3_723_000);
        assert_eq!(parse_time(""), 0);
        assert_eq!(parse_time("abc"), 0);
    }
}
