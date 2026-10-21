//! Lyrics for the song that is playing: looked up once per song, cached in the database and
//! followed along line by line when the words are timed.

use super::{App, post, s};
use crate::lyrics::{self, Lyrics};
use crate::model::Track;
use slint::{ModelRc, SharedString, VecModel};

/// Look again for a song nothing was found for after this long.
const RETRY_AFTER: i64 = 14 * 24 * 3600;

impl App {
    /// Lyrics for a song that just started. Does nothing when the song has not changed.
    pub(super) fn load_lyrics(&mut self, t: &Track) {
        let key = lyrics::key(&t.artist, &t.title);
        if key == self.lyrics_key && !self.lyrics.lines.is_empty() {
            return;
        }
        self.lyrics_key = key.clone();
        self.show_lyrics(Lyrics::default(), "");
        if t.title.trim().is_empty() {
            return;
        }
        if !self.backend().get_lyrics_enabled() {
            self.backend().set_lyrics_status(s("Lyrics are switched off in Settings → Playback."));
            return;
        }
        let cached = self.db.lock().unwrap().lyrics(&key);
        if let Some((synced, plain, instrumental, at)) = cached {
            let found = lyrics::from_strings(&synced, &plain, instrumental);
            let stale = found.is_empty() && crate::db::now() - at > RETRY_AFTER;
            if !stale {
                let status = if found.is_empty() { "No lyrics found for this song." } else { "" };
                self.show_lyrics(found, status);
                return;
            }
        }
        self.fetch_lyrics(t, key);
    }

    fn fetch_lyrics(&mut self, t: &Track, key: String) {
        if !self.lyrics_fetching.lock().unwrap().insert(key.clone()) {
            return; // already on its way
        }
        self.backend().set_lyrics_status(s("Looking for lyrics…"));
        let db = self.db.clone();
        let busy = self.lyrics_fetching.clone();
        let (artist, title, album, duration) = (t.artist.clone(), t.title.clone(), t.album.clone(), t.duration_ms);
        std::thread::spawn(move || {
            let found = lyrics::fetch(&artist, &title, &album, duration);
            busy.lock().unwrap().remove(&key);
            {
                let db = db.lock().unwrap();
                match &found {
                    Some(f) => db.set_lyrics(&key, &f.synced, &f.plain, f.instrumental),
                    // Remember the miss too, so every play does not search again.
                    None => db.set_lyrics(&key, "", "", false),
                }
            }
            post(move |a| a.on_lyrics(key, found));
        });
    }

    fn on_lyrics(&mut self, key: String, found: Option<lyrics::Found>) {
        if key != self.lyrics_key {
            return; // another song is playing by now
        }
        let lyrics = match found {
            Some(f) => lyrics::from_strings(&f.synced, &f.plain, f.instrumental),
            None => Lyrics::default(),
        };
        let status = if lyrics.is_empty() { "No lyrics found for this song." } else { "" };
        self.show_lyrics(lyrics, status);
    }

    fn show_lyrics(&mut self, lyrics: Lyrics, status: &str) {
        let b = self.backend();
        let text: Vec<SharedString> = if lyrics.instrumental {
            vec![s("♪ Instrumental ♪")]
        } else {
            lyrics.lines.iter().map(|l| s(&l.text)).collect()
        };
        b.set_lyrics_lines(ModelRc::new(VecModel::from(text)));
        b.set_lyrics_synced(lyrics.synced);
        b.set_lyrics_index(-1);
        b.set_lyrics_status(s(status));
        self.lyrics_index = -1;
        self.lyrics = lyrics;
    }

    /// Highlight the line that is playing (synced lyrics only). Called from the tick.
    pub(super) fn update_lyrics_line(&mut self, pos_ms: i64) {
        if !self.lyrics.synced {
            return;
        }
        let index = lyrics::line_at(&self.lyrics.lines, pos_ms).map(|i| i as i32).unwrap_or(-1);
        if index != self.lyrics_index {
            self.lyrics_index = index;
            self.backend().set_lyrics_index(index);
        }
    }

    /// Jump to a line (clicking a line of synced lyrics).
    pub(super) fn seek_to_lyric(&mut self, index: i32) {
        if !self.lyrics.synced {
            return;
        }
        let Some(line) = self.lyrics.lines.get(index.max(0) as usize) else { return };
        self.seek(line.ms as f32 / 1000.0);
    }

    /// "Look again": drops what was stored for this song and asks LRCLIB once more.
    pub(super) fn refresh_lyrics(&mut self) {
        let Some(t) = self.queue.current().cloned() else { return };
        let key = lyrics::key(&t.artist, &t.title);
        self.db.lock().unwrap().forget_lyrics(&key);
        self.lyrics_key = key.clone();
        self.show_lyrics(Lyrics::default(), "");
        if !self.backend().get_lyrics_enabled() {
            self.backend().set_lyrics_status(s("Lyrics are switched off in Settings → Playback."));
            return;
        }
        self.fetch_lyrics(&t, key);
    }

    pub(super) fn set_lyrics_enabled(&mut self, on: bool) {
        self.backend().set_lyrics_enabled(on);
        self.db.lock().unwrap().set_setting_bool("lyrics", on);
        self.lyrics_key.clear();
        if let Some(t) = self.queue.current().cloned() {
            self.load_lyrics(&t);
        }
    }
}
