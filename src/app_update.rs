//! Update checks and installs (see `crate::updater`).

use super::{App, post, s};
use crate::updater::{self, InstallKind, Release};

impl App {
    pub(super) fn load_update_settings(&mut self) {
        let b = self.backend();
        b.set_update_auto(self.db.lock().unwrap().setting_bool("update_auto", true));
        b.set_install_kind(s(updater::install_kind().label()));
        log::info!("{} copy of trax", updater::install_kind().label());
        if b.get_update_auto() {
            self.check_updates(false);
        }
    }

    pub(super) fn set_update_auto(&mut self, on: bool) {
        self.backend().set_update_auto(on);
        self.db.lock().unwrap().set_setting_bool("update_auto", on);
    }

    /// `manual`: the user clicked "Check now", so also report "up to date" and errors.
    pub(super) fn check_updates(&mut self, manual: bool) {
        let b = self.backend();
        if b.get_update_busy() {
            return;
        }
        b.set_update_busy(true);
        b.set_update_status(s("Checking for updates…"));
        std::thread::spawn(move || {
            let res = updater::check().map_err(|e| format!("{e:#}"));
            post(move |a| a.on_update_checked(res, manual));
        });
    }

    fn on_update_checked(&mut self, res: Result<Option<Release>, String>, manual: bool) {
        let b = self.backend();
        b.set_update_busy(false);
        match res {
            Ok(Some(r)) => {
                b.set_update_available(true);
                b.set_update_version(s(&r.version));
                b.set_update_notes(s(&r.notes));
                b.set_update_status(s(format!("trax {} is available.", r.version)));
                b.set_update_banner(true);
                self.update = Some(r);
            }
            Ok(None) => {
                b.set_update_available(false);
                b.set_update_status(s(format!("trax {} is up to date.", env!("CARGO_PKG_VERSION"))));
                if manual {
                    self.toast("trax is up to date");
                }
            }
            Err(e) => {
                b.set_update_status(s(format!("Couldn't check for updates: {e}")));
                if manual {
                    self.toast("Couldn't check for updates");
                }
            }
        }
    }

    /// Download, verify, then quit so the update can be applied.
    pub(super) fn install_update(&mut self) {
        let Some(release) = self.update.clone() else { return };
        let b = self.backend();
        if b.get_update_busy() {
            return;
        }
        let kind = updater::install_kind();
        b.set_update_busy(true);
        b.set_update_progress(0.0);
        b.set_update_status(s(format!("Downloading trax {}…", release.version)));
        std::thread::spawn(move || {
            let last = std::sync::Mutex::new(-1i32);
            let res = updater::download(&release, kind, |p| {
                // Only post whole-percent changes to the UI.
                let pct = (p * 100.0) as i32;
                let mut l = last.lock().unwrap();
                if *l != pct {
                    *l = pct;
                    post(move |a| a.backend().set_update_progress(pct as f32 / 100.0));
                }
            });
            let res = res.and_then(|path| updater::stage(&path, kind)).map_err(|e| format!("{e:#}"));
            post(move |a| a.on_update_ready(res, kind));
        });
    }

    fn on_update_ready(&mut self, res: Result<(), String>, kind: InstallKind) {
        let b = self.backend();
        b.set_update_busy(false);
        match res {
            Ok(()) => {
                b.set_update_status(s(match kind {
                    InstallKind::Installed => "Installing the update…",
                    InstallKind::Portable => "Restarting with the new version…",
                }));
                // The update is applied once trax has saved everything and exited.
                let _ = slint::quit_event_loop();
            }
            Err(e) => {
                b.set_update_status(s(format!("Update failed: {e}")));
                self.toast(format!("Update failed: {e}"));
            }
        }
    }

    pub(super) fn open_release_page(&self) {
        let page = self
            .update
            .as_ref()
            .map(|r| r.page.clone())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| format!("https://github.com/{}/releases", updater::REPO));
        self.open_url(&page);
    }
}
