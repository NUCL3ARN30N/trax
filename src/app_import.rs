//! Importing playlists from YouTube / YouTube Music and Spotify links (see `crate::import`).

use super::{App, post, s};
use crate::import::{self, Imported, Link};

impl App {
    /// Start importing a playlist link in the background. Returns false (and shows why) if it
    /// can't start.
    pub(super) fn import_link(&mut self, link: Link) -> bool {
        let error = if self.import_busy {
            "Another playlist is still being imported. Wait until it's done."
        } else if !self.backend().get_yt_enabled() {
            "Imported songs play from YouTube. Switch YouTube on in Settings → Sources first."
        } else {
            ""
        };
        self.backend().set_share_error(s(error));
        if !error.is_empty() {
            return false;
        }
        self.import_busy = true;
        let bin = self.backend().get_ytdlp_path().to_string();
        self.toast(match link {
            Link::YouTube(_) => "Reading the YouTube playlist…",
            Link::Spotify { .. } => "Reading the Spotify playlist…",
        });
        std::thread::spawn(move || {
            let res = import::fetch(&bin, &link, |done, total| {
                post(move |a| a.toast(format!("Finding the songs on YouTube: {done} of {total}")));
            });
            post(move |a| a.finish_import(res));
        });
        true
    }

    fn finish_import(&mut self, res: anyhow::Result<Imported>) {
        self.import_busy = false;
        let p = match res {
            Ok(p) => p,
            Err(e) => {
                self.toast(format!("Import failed: {e:#}"));
                return;
            }
        };
        if let Err(e) = self.save_imported(&p.name, &p.description, p.cover.as_deref(), &p.tracks) {
            self.toast(format!("Import failed: {e:#}"));
            return;
        }
        for song in &p.missing {
            log::info!("import: not found on YouTube: {song}");
        }
        self.toast(match p.missing.len() {
            0 => format!("Imported “{}” with {} song(s)", p.name, p.tracks.len()),
            n => format!("Imported “{}” with {} song(s); {n} weren't found on YouTube", p.name, p.tracks.len()),
        });
    }
}
