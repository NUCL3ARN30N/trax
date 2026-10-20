//! Integration with the operating system: registering trax as a music player so it shows up
//! in "Open with" and can be chosen as the default app for audio files.
//!
//! Windows doesn't let programs make themselves the default; trax registers its file types
//! (per user, no admin rights) and opens the Default apps page where the user confirms.

/// Audio types trax can play.
pub const FILE_TYPES: &[&str] = &[".mp3", ".flac", ".m4a", ".aac", ".ogg", ".oga", ".wav"];

#[cfg(windows)]
mod imp {
    use super::FILE_TYPES;
    use anyhow::{Context, Result};
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const PROG_ID: &str = "trax.audio";
    const CAPABILITIES: &str = r"Software\trax\Capabilities";

    fn exe() -> Result<String> {
        Ok(std::env::current_exe().context("can't find trax.exe")?.to_string_lossy().into_owned())
    }

    fn open_command(exe: &str) -> String {
        format!("\"{exe}\" \"%1\"")
    }

    /// Register trax for its file types (current user). Safe to call again.
    pub fn register() -> Result<()> {
        let exe = exe()?;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let classes = hkcu.create_subkey(r"Software\Classes")?.0;

        let (prog, _) = classes.create_subkey(PROG_ID)?;
        prog.set_value("", &"Audio file (trax)")?;
        prog.create_subkey("DefaultIcon")?.0.set_value("", &format!("\"{exe}\",0"))?;
        let (open, _) = prog.create_subkey(r"shell\open")?;
        open.set_value("", &"Play with trax")?;
        // Several selected files arrive in one start instead of one start per file.
        open.set_value("MultiSelectModel", &"Player")?;
        open.create_subkey("command")?.0.set_value("", &open_command(&exe))?;

        let (app, _) = classes.create_subkey(r"Applications\trax.exe")?;
        app.set_value("FriendlyAppName", &"trax")?;
        app.create_subkey(r"shell\open\command")?.0.set_value("", &open_command(&exe))?;
        let (types, _) = app.create_subkey("SupportedTypes")?;
        for ext in FILE_TYPES {
            types.set_value(ext, &"")?;
            classes.create_subkey(format!(r"{ext}\OpenWithProgids"))?.0.set_value(PROG_ID, &"")?;
        }

        let (caps, _) = hkcu.create_subkey(CAPABILITIES)?;
        caps.set_value("ApplicationName", &"trax")?;
        caps.set_value("ApplicationDescription", &"Music player for local, Nextcloud, YouTube and SoundCloud music")?;
        caps.set_value("ApplicationIcon", &format!("\"{exe}\",0"))?;
        let (assoc, _) = caps.create_subkey("FileAssociations")?;
        for ext in FILE_TYPES {
            assoc.set_value(ext, &PROG_ID)?;
        }
        hkcu.create_subkey(r"Software\RegisteredApplications")?.0.set_value("trax", &CAPABILITIES)?;
        notify_shell();
        Ok(())
    }

    /// Remove everything `register` added (also run by the uninstaller).
    pub fn unregister() -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let classes = hkcu.open_subkey_with_flags(r"Software\Classes", winreg::enums::KEY_ALL_ACCESS)?;
        let _ = classes.delete_subkey_all(PROG_ID);
        // Windows also uses Applications\trax.exe for a choice made through "Open with", so
        // only what trax added there is removed; the open command stays.
        if let Ok(app) = classes.open_subkey_with_flags(r"Applications\trax.exe", winreg::enums::KEY_ALL_ACCESS) {
            let _ = app.delete_subkey_all("SupportedTypes");
            let _ = app.delete_value("FriendlyAppName");
        }
        for ext in FILE_TYPES {
            if let Ok(k) = classes.open_subkey_with_flags(format!(r"{ext}\OpenWithProgids"), winreg::enums::KEY_ALL_ACCESS) {
                let _ = k.delete_value(PROG_ID);
            }
        }
        let _ = hkcu.delete_subkey_all(r"Software\trax");
        if let Ok(k) = hkcu.open_subkey_with_flags(r"Software\RegisteredApplications", winreg::enums::KEY_ALL_ACCESS) {
            let _ = k.delete_value("trax");
        }
        notify_shell();
        Ok(())
    }

    /// Registered, and pointing at this trax.exe (a moved portable copy needs registering again).
    pub fn registered_here() -> Option<bool> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let cmd: String = hkcu.open_subkey(format!(r"Software\Classes\{PROG_ID}\shell\open\command")).ok()?.get_value("").ok()?;
        Some(exe().map(|e| cmd == open_command(&e)).unwrap_or(false))
    }

    /// File types for which the user picked trax as the default app.
    pub fn default_for() -> Vec<&'static str> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let choice = |ext: &str| -> Option<String> {
            let base = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}");
            // Newer Windows 11 builds keep the effective choice in UserChoiceLatest.
            hkcu.open_subkey(format!(r"{base}\UserChoiceLatest\ProgId"))
                .and_then(|k| k.get_value::<String, _>("ProgId"))
                .or_else(|_| hkcu.open_subkey(format!(r"{base}\UserChoice")).and_then(|k| k.get_value("ProgId")))
                .ok()
        };
        FILE_TYPES
            .iter()
            .copied()
            .filter(|ext| {
                choice(ext).map(|p| p == PROG_ID || p.eq_ignore_ascii_case(r"Applications\trax.exe")).unwrap_or(false)
            })
            .collect()
    }

    /// The Default apps page for trax, where the user confirms trax as the default.
    pub fn open_default_apps() {
        let _ = crate::connectors::hidden_command("cmd")
            .args(["/C", "start", "", "ms-settings:defaultapps?registeredAppUser=trax"])
            .spawn();
    }

    fn notify_shell() {
        use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        // SAFETY: documented call with null item pointers for SHCNE_ASSOCCHANGED.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, bail};
    pub fn register() -> Result<()> {
        bail!("not supported on this system yet")
    }
    pub fn unregister() -> Result<()> {
        Ok(())
    }
    pub fn registered_here() -> Option<bool> {
        None
    }
    pub fn default_for() -> Vec<&'static str> {
        vec![]
    }
    pub fn open_default_apps() {}
}

pub use imp::*;
