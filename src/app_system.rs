//! Settings → System: trax as the default music player.

use super::{App, s};
use crate::system;

impl App {
    /// At start: keep an existing registration pointing at this trax.exe (portable copies move).
    pub(super) fn load_system_integration(&mut self) {
        if system::registered_here() == Some(false) {
            if let Err(e) = system::register() {
                log::warn!("couldn't update the file type registration: {e:#}");
            }
        }
        self.refresh_default_status();
    }

    pub(super) fn refresh_default_status(&self) {
        let b = self.backend();
        let registered = system::registered_here() == Some(true);
        b.set_player_registered(registered);
        let defaults = system::default_for();
        let status = if !registered {
            "trax isn't registered as a music player yet.".to_string()
        } else if defaults.len() == system::FILE_TYPES.len() {
            "trax is the default player for all supported audio files.".to_string()
        } else if defaults.is_empty() {
            "trax shows up in “Open with”, but isn't the default player yet.".to_string()
        } else {
            let list: Vec<String> = defaults.iter().map(|e| e.trim_start_matches('.').to_uppercase()).collect();
            format!("trax is the default for {}.", list.join(", "))
        };
        b.set_player_default_status(s(status));
    }

    pub(super) fn make_default_player(&mut self) {
        match system::register() {
            Ok(()) => {
                system::open_default_apps();
                self.toast("In Default apps, choose trax for the file types you want");
            }
            Err(e) => self.toast(format!("Couldn't register trax: {e:#}")),
        }
        self.refresh_default_status();
    }

    /// Show a document shipped next to trax.exe (LICENSE, THIRD_PARTY_LICENSES.txt), or the
    /// copy in the repository when running from elsewhere.
    pub(super) fn open_legal(&self, name: &str) {
        let local = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).filter(|p| p.exists());
        match local {
            Some(path) => {
                #[cfg(windows)]
                let _ = crate::connectors::hidden_command("explorer").arg(path).spawn();
                #[cfg(not(windows))]
                let _ = std::process::Command::new("xdg-open").arg(path).spawn();
            }
            None => self.open_url(&format!("https://github.com/{}/blob/main/{name}", crate::updater::REPO)),
        }
    }

    /// Show the folder where users can put yt-dlp themselves, then check for it again.
    pub(super) fn open_ytdlp_folder(&self) {
        let dir = crate::paths::bin_dir();
        #[cfg(windows)]
        let _ = crate::connectors::hidden_command("explorer").arg(dir).spawn();
        #[cfg(not(windows))]
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        self.check_ytdlp();
    }

    pub(super) fn unregister_player(&mut self) {
        match system::unregister() {
            Ok(()) => self.toast("trax was removed from “Open with” and the default apps"),
            Err(e) => self.toast(format!("Couldn't remove the registration: {e:#}")),
        }
        self.refresh_default_status();
    }
}
