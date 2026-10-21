//! "Keep offline": playlists whose YouTube, SoundCloud and Nextcloud songs are downloaded
//! up front and pinned in the cache, so they play without a connection.

use super::{App, cache_file, fetch_remote, post, ready_file, s};
use crate::model::Track;
use std::collections::HashSet;
use std::path::PathBuf;

impl App {
    /// Cache files that belong to a playlist kept offline.
    pub(super) fn offline_pins(&self) -> HashSet<PathBuf> {
        self.db.lock().unwrap().offline_tracks().iter().filter_map(cache_file).collect()
    }

    pub(super) fn set_playlist_offline(&mut self, id: i64, on: bool) {
        let res = self.db.lock().unwrap().set_playlist_offline(id, on);
        if let Err(e) = res {
            self.toast(format!("Error: {e}"));
            return;
        }
        if !on && self.backend().get_offline_all() {
            // "Always keep playlists offline" can't stand while one is left out.
            self.db.lock().unwrap().set_setting_bool("offline_all", false);
            self.backend().set_offline_all(false);
        }
        self.refresh_playlists();
        self.reload_playlist_view();
        if on {
            self.sync_offline();
        } else {
            self.backend().set_offline_status(s(""));
            self.toast("Not kept offline any more — the downloads are ordinary cache again");
            self.trim_cache();
        }
    }

    /// "Always keep playlists offline": applies to every playlist there is, and new ones
    /// start out kept offline (see `Db::create_playlist`).
    pub(super) fn set_offline_all(&mut self, on: bool) {
        {
            let db = self.db.lock().unwrap();
            db.set_setting_bool("offline_all", on);
            let _ = if on { db.set_all_playlists_offline() } else { db.clear_offline_playlists() };
        }
        self.backend().set_offline_all(on);
        self.refresh_playlists();
        self.reload_playlist_view();
        if on {
            self.sync_offline();
        } else {
            self.backend().set_offline_status(s(""));
            self.toast("Playlists are no longer kept offline");
            self.trim_cache();
        }
    }

    /// Download whatever the playlists kept offline are still missing.
    pub(super) fn sync_offline(&mut self) {
        if self.offline_busy {
            return;
        }
        let todo: Vec<Track> =
            self.db.lock().unwrap().offline_tracks().into_iter().filter(|t| ready_file(t).is_none()).collect();
        if todo.is_empty() {
            return;
        }
        self.offline_busy = true;
        let cfg = self.nc_config();
        let bin = self.backend().get_ytdlp_path().to_string();
        std::thread::spawn(move || {
            let total = todo.len();
            let mut failed = 0;
            for (i, t) in todo.iter().enumerate() {
                post(move |a| a.backend().set_offline_status(s(format!("Keeping offline: {} of {total}…", i + 1))));
                if let Err(e) = fetch_remote(&cfg, &bin, t) {
                    log::info!("offline download of {} failed: {e:#}", t.uri);
                    failed += 1;
                }
            }
            post(move |a| a.finish_offline(total - failed, failed));
        });
    }

    fn finish_offline(&mut self, done: usize, failed: usize) {
        self.offline_busy = false;
        self.backend().set_offline_status(s(""));
        if failed > 0 {
            self.toast(format!("{done} songs kept offline, {failed} could not be downloaded"));
        } else if done > 0 {
            self.toast(format!("{done} songs are now kept offline"));
        }
        self.trim_cache();
    }
}
