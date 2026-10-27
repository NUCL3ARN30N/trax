//! Sharing playlists as a code, and importing shared codes (see `crate::share`).

use super::{App, PlaylistView, post, s};
use crate::connectors::{soundcloud, youtube};
use crate::model::SourceKind;
use crate::share;

impl App {
    /// Make the share code for the playlist that is open.
    pub(super) fn share_playlist(&mut self) {
        let b = self.backend();
        let tracks = self.playlist.tracks();
        let (code, included, skipped) = share::encode(&b.get_playlist_name(), &b.get_playlist_description(), &tracks);
        let info = match (included, skipped) {
            (0, _) => "This playlist has no YouTube or SoundCloud songs, so there's nothing to share. Songs from your PC or your Nextcloud can't be played by others.".to_string(),
            (n, 0) => format!("{n} song(s). Send this code to someone; they import it with + next to Playlists → Import shared."),
            (n, k) => format!(
                "{n} song(s). {k} song(s) from your PC or Nextcloud were left out, because others can't play them. Send this code to someone; they import it with + next to Playlists → Import shared."
            ),
        };
        b.set_share_code(s(if included > 0 { code } else { String::new() }));
        b.set_share_info(s(info));
    }

    pub(super) fn copy_share_code(&mut self) {
        let code = self.backend().get_share_code().to_string();
        match copy_text(&code) {
            Ok(()) => self.toast("Code copied"),
            Err(e) => self.toast(format!("Couldn't copy: {e}")),
        }
    }

    /// Create a playlist from a pasted code. Returns false (and shows why) if the code is bad.
    pub(super) fn import_playlist(&mut self, code: &str) -> bool {
        let shared = match share::decode(code) {
            Ok(p) => p,
            Err(e) => {
                self.backend().set_share_error(s(format!("{e:#}")));
                return false;
            }
        };
        self.backend().set_share_error(s(""));
        let res = (|| -> anyhow::Result<(i64, Vec<(i64, SourceKind, String)>)> {
            let db = self.db.lock().unwrap();
            let id = db.create_playlist(&shared.name)?;
            db.set_playlist_details(id, &shared.name, &shared.description, None)?;
            let mut ids = vec![];
            let mut need_cover = vec![];
            for t in &shared.tracks {
                // Songs in a playlist are part of the library, like when you add them yourself.
                let tid = db.upsert_track(t, true)?;
                if db.track(tid).map(|t| t.cover.is_none()).unwrap_or(false) {
                    need_cover.push((tid, t.source, t.uri.clone()));
                }
                ids.push(tid);
            }
            db.add_to_playlist(id, &ids)?;
            Ok((id, need_cover))
        })();
        let (id, need_cover) = match res {
            Ok(r) => r,
            Err(e) => {
                self.backend().set_share_error(s(format!("Import failed: {e:#}")));
                return false;
            }
        };
        self.toast(format!("Imported “{}” with {} song(s)", shared.name, shared.tracks.len()));
        self.library_dirty = true;
        self.refresh_playlists();
        self.open_playlist(id);
        self.fetch_covers(id, need_cover);
        true
    }

    /// Thumbnails / artwork for imported songs, in the background.
    fn fetch_covers(&self, playlist: i64, todo: Vec<(i64, SourceKind, String)>) {
        if todo.is_empty() {
            return;
        }
        let db = self.db.clone();
        let bin = self.backend().get_ytdlp_path().to_string();
        std::thread::spawn(move || {
            for (id, source, uri) in todo {
                let cover = match source {
                    SourceKind::YouTube => youtube::fetch_thumbnail(&uri),
                    SourceKind::SoundCloud => soundcloud::fetch_thumbnail(&bin, &uri, None),
                    _ => continue,
                };
                if let Some(c) = cover {
                    let _ = db.lock().unwrap().set_found_cover(id, &c, None, None);
                }
            }
            post(move |a| {
                a.refresh_playlists();
                if a.view == PlaylistView::User(playlist) {
                    a.reload_playlist_view();
                }
            });
        });
    }
}

#[cfg(windows)]
pub(super) fn copy_text(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    // Share codes are plain ASCII, which `clip` takes as is.
    let mut child = crate::connectors::hidden_command("clip").stdin(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn copy_text(text: &str) -> anyhow::Result<()> {
    // On Linux the copying program serves the clipboard itself, so the handle stays open.
    thread_local! {
        static CLIPBOARD: std::cell::RefCell<Option<arboard::Clipboard>> = const { std::cell::RefCell::new(None) };
    }
    CLIPBOARD.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = Some(arboard::Clipboard::new()?);
        }
        c.as_mut().unwrap().set_text(text)?;
        Ok(())
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(super) fn copy_text(_text: &str) -> anyhow::Result<()> {
    anyhow::bail!("select the code and copy it")
}
