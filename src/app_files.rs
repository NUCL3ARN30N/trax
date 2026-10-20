//! Songs opened from anywhere on the PC ("Open with trax", double-click when trax is the
//! default player). Each one joins the library on its own (source "file"); it leaves the
//! library again when it is gone at the moment it is played.

use super::{App, post};
use crate::connectors::{self, is_audio};
use crate::model::{SourceKind, Track};
use std::path::PathBuf;

impl App {
    /// Add the files to the library and play them (the rest of the queue stays after them).
    pub(super) fn open_files(&mut self, files: Vec<PathBuf>) {
        self.show_window();
        if files.is_empty() {
            return;
        }
        let db = self.db.clone();
        std::thread::spawn(move || {
            let mut tracks = vec![];
            let mut skipped = 0;
            for path in files {
                let path = std::fs::canonicalize(&path).map(strip_verbatim).unwrap_or(path);
                let p = path.to_string_lossy().into_owned();
                if !path.is_file() || !is_audio(&p) {
                    skipped += 1;
                    continue;
                }
                let existing = db.lock().unwrap().disk_track(&p);
                let track = match existing {
                    Some(t) => {
                        // Bring an opened file that was removed earlier back into the library.
                        let _ = db.lock().unwrap().set_in_library(t.id, true);
                        Some(t)
                    }
                    None => {
                        let mut t = Track::new(SourceKind::File, p.clone());
                        connectors::guess_from_path(&mut t, &p);
                        connectors::read_tags(&path, &mut t);
                        let db = db.lock().unwrap();
                        db.upsert_track(&t, true).ok().and_then(|id| db.track(id))
                    }
                };
                tracks.extend(track);
            }
            post(move |a| a.play_opened(tracks, skipped));
        });
    }

    fn play_opened(&mut self, tracks: Vec<Track>, skipped: usize) {
        // In a party as a guest, opened files are added to the library but don't play.
        if self.party_intercept(super::party_app::Intercept::PlayOther) {
            self.library_dirty = true;
            return;
        }
        if skipped > 0 {
            self.toast(format!("{skipped} file(s) couldn't be opened (not found or not audio)"));
        }
        if tracks.is_empty() {
            return;
        }
        self.resume_pos = None;
        let start = match self.queue.index {
            Some(i) => {
                for (n, t) in tracks.into_iter().enumerate() {
                    self.queue.tracks.insert(i + 1 + n, t);
                }
                i + 1
            }
            None => {
                self.queue.set(tracks, 0);
                0
            }
        };
        self.queue.index = Some(start);
        self.start_current_with(false);
        self.library_dirty = true;
        self.refresh_library();
        self.refresh_home();
    }

    /// Called before playing: an opened file that is gone leaves the library and the queue.
    /// Returns true if the track was missing.
    pub(super) fn drop_if_missing(&mut self, track: &Track) -> bool {
        if track.source != SourceKind::File || std::path::Path::new(&track.uri).exists() {
            return false;
        }
        if track.id > 0 {
            let _ = self.db.lock().unwrap().set_in_library(track.id, false);
        }
        if let Some(i) = self.queue.index {
            self.queue.remove(i);
        }
        self.toast(format!("“{}” wasn't found any more and was removed from your library", track.title));
        self.library_dirty = true;
        self.refresh_library();
        self.refresh_queue();
        true
    }
}

/// `canonicalize` returns `\\?\C:\…` paths on Windows; library paths use the normal form.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => s.strip_prefix(r"\\?\").map(PathBuf::from).unwrap_or_else(|| p.clone()),
    }
}
