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

    /// Register, then open the Default apps page for trax, where the user confirms trax as
    /// the default. Returns what to tell the user.
    pub fn make_default() -> Result<&'static str> {
        register()?;
        let _ = crate::connectors::hidden_command("cmd")
            .args(["/C", "start", "", "ms-settings:defaultapps?registeredAppUser=trax"])
            .spawn();
        Ok("In Default apps, choose trax for the file types you want")
    }

    fn notify_shell() {
        use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        // SAFETY: documented call with null item pointers for SHCNE_ASSOCCHANGED.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    }
}

/// Linux: a desktop entry and icons in `~/.local/share`, defaults in `~/.config/mimeapps.list`
/// (the freedesktop.org specs every desktop follows). Unlike Windows, apps may set
/// themselves as the default, so no confirmation step is needed.
#[cfg(target_os = "linux")]
mod imp {
    use super::FILE_TYPES;
    use anyhow::{Context, Result};
    use std::path::PathBuf;

    const DESKTOP_ID: &str = "trax.desktop";

    /// MIME types per file type; the first is the one file managers report.
    const MIME_TYPES: &[(&str, &[&str])] = &[
        (".mp3", &["audio/mpeg", "audio/mp3", "audio/x-mp3", "audio/x-mpeg"]),
        (".flac", &["audio/flac", "audio/x-flac"]),
        (".m4a", &["audio/mp4", "audio/x-m4a"]),
        (".aac", &["audio/aac", "audio/x-aac"]),
        (".ogg", &["audio/x-vorbis+ogg", "audio/ogg", "audio/vorbis"]),
        (".oga", &["audio/ogg"]),
        (".wav", &["audio/x-wav", "audio/wav", "audio/vnd.wave"]),
    ];

    const ICONS: &[(u32, &[u8])] = &[
        (16, include_bytes!("../assets/logo/AppIcon.iconset/icon_16x16.png")),
        (32, include_bytes!("../assets/logo/AppIcon.iconset/icon_32x32.png")),
        (64, include_bytes!("../assets/logo/AppIcon.iconset/icon_32x32@2x.png")),
        (128, include_bytes!("../assets/logo/AppIcon.iconset/icon_128x128.png")),
        (256, include_bytes!("../assets/logo/AppIcon.iconset/icon_256x256.png")),
        (512, include_bytes!("../assets/logo/AppIcon.iconset/icon_512x512.png")),
    ];

    fn mime_types() -> impl Iterator<Item = &'static str> + Clone {
        MIME_TYPES.iter().flat_map(|(_, m)| m.iter().copied())
    }

    fn data_home() -> Result<PathBuf> {
        Ok(directories::BaseDirs::new().context("no home folder")?.data_dir().to_path_buf())
    }

    fn desktop_file() -> Result<PathBuf> {
        Ok(data_home()?.join("applications").join(DESKTOP_ID))
    }

    fn icon_file(size: u32) -> Result<PathBuf> {
        Ok(data_home()?.join(format!("icons/hicolor/{size}x{size}/apps/trax.png")))
    }

    /// Installed from the .deb, which brings its own desktop entry and icons in `/usr/share`.
    fn packaged() -> bool {
        crate::updater::install_kind() == crate::updater::InstallKind::Installed
            && std::path::Path::new("/usr/share/applications").join(DESKTOP_ID).exists()
    }

    /// `Exec=` line starting this trax. The path is quoted for the Exec rules, then escaped
    /// once more as a desktop entry string.
    fn exec_line() -> Result<String> {
        let exe = std::env::current_exe().context("can't find trax")?;
        let mut quoted = String::new();
        for c in exe.to_string_lossy().chars() {
            match c {
                '"' | '`' | '$' | '\\' => quoted.extend(['\\', c]),
                '%' => quoted.push_str("%%"),
                _ => quoted.push(c),
            }
        }
        Ok(format!("Exec=\"{}\" %F", quoted.replace('\\', "\\\\")))
    }

    /// The desktop entry; the .deb ships the same one with `Exec=trax %F`
    /// (`assets/linux/trax.desktop`, kept in step by a test).
    fn desktop_entry(exec_line: &str) -> String {
        let mut mimes = String::new();
        for m in mime_types() {
            if !mimes.split(';').any(|known| known == m) {
                mimes.push_str(&format!("{m};"));
            }
        }
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=trax\n\
             GenericName=Music Player\n\
             Comment=Music player for local, Nextcloud, YouTube and SoundCloud music\n\
             {exec_line}\n\
             Icon=trax\n\
             Terminal=false\n\
             StartupWMClass=trax\n\
             Categories=AudioVideo;Audio;Player;\n\
             Keywords=music;player;audio;playlist;\n\
             MimeType={mimes}\n"
        )
    }

    /// Let the desktop pick up new or removed entries ("Open with" reads this cache).
    fn refresh_desktop_database() {
        if let Ok(dir) = data_home() {
            let _ = std::process::Command::new("update-desktop-database").arg(dir.join("applications")).output();
        }
    }

    /// Register trax for its file types (current user): launcher entry, icon, "Open with".
    /// The package already has the entry and icons, so then only the associations are added.
    pub fn register() -> Result<()> {
        if !packaged() {
            let file = desktop_file()?;
            std::fs::create_dir_all(file.parent().unwrap())?;
            std::fs::write(&file, desktop_entry(&exec_line()?)).context("can't write the desktop entry")?;
            for (size, png) in ICONS {
                let path = icon_file(*size)?;
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(path, png)?;
            }
        }
        mimeapps::edit(|section, mime, apps| {
            if section == mimeapps::ADDED && mime_types().any(|m| m == mime) && !apps.iter().any(|a| a == DESKTOP_ID) {
                apps.insert(0, DESKTOP_ID.into());
            }
        }, mime_types())?;
        refresh_desktop_database();
        Ok(())
    }

    /// Remove everything `register` and `make_default` added.
    pub fn unregister() -> Result<()> {
        mimeapps::edit(|_, _, apps| apps.retain(|a| a != DESKTOP_ID), std::iter::empty())?;
        let _ = std::fs::remove_file(desktop_file()?);
        for (size, _) in ICONS {
            let _ = std::fs::remove_file(icon_file(*size)?);
        }
        refresh_desktop_database();
        Ok(())
    }

    /// Registered, and pointing at this trax (a moved copy needs registering again).
    pub fn registered_here() -> Option<bool> {
        if packaged() {
            return Some(true);
        }
        let entry = std::fs::read_to_string(desktop_file().ok()?).ok()?;
        let exec = exec_line().ok()?;
        Some(entry.lines().any(|l| l == exec))
    }

    /// File types for which trax is the default app.
    pub fn default_for() -> Vec<&'static str> {
        FILE_TYPES
            .iter()
            .copied()
            .filter(|ext| {
                MIME_TYPES
                    .iter()
                    .find(|(e, _)| e == ext)
                    .and_then(|(_, m)| mimeapps::default_app(m[0]))
                    .is_some_and(|app| app == DESKTOP_ID)
            })
            .collect()
    }

    /// Register and make trax the default for all its file types. Returns what to tell the user.
    pub fn make_default() -> Result<&'static str> {
        register()?;
        mimeapps::edit(|section, mime, apps| {
            if section == mimeapps::DEFAULT && mime_types().any(|m| m == mime) {
                *apps = vec![DESKTOP_ID.into()];
            }
        }, mime_types())?;
        refresh_desktop_database();
        Ok("trax is now the default music player")
    }

    /// Reading and writing `mimeapps.list`. Desktop-specific files such as
    /// `kde-mimeapps.list` take precedence, so they are read first and edited too if present.
    mod mimeapps {
        use anyhow::{Context, Result};
        use std::path::PathBuf;

        pub const DEFAULT: &str = "Default Applications";
        pub const ADDED: &str = "Added Associations";

        /// Most important first.
        fn files() -> Vec<PathBuf> {
            let Some(config) = directories::BaseDirs::new().map(|d| d.config_dir().to_path_buf()) else { return vec![] };
            let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
            let mut files: Vec<PathBuf> = desktops
                .split(':')
                .filter(|d| !d.is_empty())
                .map(|d| config.join(format!("{}-mimeapps.list", d.to_lowercase())))
                .collect();
            files.push(config.join("mimeapps.list"));
            files
        }

        /// The app the user chose for `mime`, if any.
        pub fn default_app(mime: &str) -> Option<String> {
            files().into_iter().find_map(|f| {
                let text = std::fs::read_to_string(f).ok()?;
                let mut section = "";
                text.lines().find_map(|l| {
                    let l = l.trim();
                    if let Some(name) = l.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                        section = name;
                        return None;
                    }
                    let (key, value) = l.split_once('=')?;
                    (section == DEFAULT && key.trim() == mime)
                        .then(|| value.split(';').map(str::trim).find(|a| !a.is_empty()).map(String::from))
                        .flatten()
                })
            })
        }

        /// Apply `f(section, mime, apps)` to every association in both sections, in every
        /// existing file. `ensure` lists types that get an (empty) entry in the main file
        /// first, so `f` can fill them in. Entries left without apps are dropped.
        pub fn edit<'a>(f: impl Fn(&str, &str, &mut Vec<String>), ensure: impl Iterator<Item = &'a str> + Clone) -> Result<()> {
            let all = files();
            let main = all.last().cloned().context("no config folder")?;
            for file in all {
                let is_main = file == main;
                let text = match std::fs::read_to_string(&file) {
                    Ok(t) => t,
                    Err(_) if is_main => String::new(),
                    Err(_) => continue,
                };
                let edited = edit_text(&text, &f, if is_main { Some(ensure.clone()) } else { None });
                if edited != text {
                    if let Some(dir) = file.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&file, edited).with_context(|| format!("can't write {}", file.display()))?;
                }
            }
            Ok(())
        }

        pub(super) fn edit_text<'a>(
            text: &str,
            f: &impl Fn(&str, &str, &mut Vec<String>),
            ensure: Option<impl Iterator<Item = &'a str> + Clone>,
        ) -> String {
            // Sections in order, each with its lines; the part before any header has no name.
            let mut sections: Vec<(String, Vec<String>)> = vec![(String::new(), vec![])];
            for l in text.lines() {
                match l.trim().strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    Some(name) => sections.push((name.to_string(), vec![])),
                    None => sections.last_mut().unwrap().1.push(l.to_string()),
                }
            }
            if let Some(ensure) = ensure {
                for name in [DEFAULT, ADDED] {
                    if !sections.iter().any(|(n, _)| n == name) {
                        sections.push((name.to_string(), vec![]));
                    }
                    let lines = &mut sections.iter_mut().find(|(n, _)| n == name).unwrap().1;
                    for mime in ensure.clone() {
                        if !lines.iter().any(|l| l.split_once('=').is_some_and(|(k, _)| k.trim() == mime)) {
                            lines.push(format!("{mime}="));
                        }
                    }
                }
            }
            for (name, lines) in &mut sections {
                if name != DEFAULT && name != ADDED {
                    continue;
                }
                lines.retain_mut(|l| {
                    let Some((key, value)) = l.split_once('=') else { return true };
                    let key = key.trim().to_string();
                    let mut apps: Vec<String> =
                        value.split(';').map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect();
                    let before = apps.clone();
                    f(name, &key, &mut apps);
                    if apps == before && !apps.is_empty() {
                        return true; // untouched lines keep their exact spelling
                    }
                    *l = format!("{key}={}", apps.iter().map(|a| format!("{a};")).collect::<String>());
                    !apps.is_empty()
                });
            }
            let mut out = String::new();
            for (i, (name, lines)) in sections.iter().enumerate() {
                if i > 0 {
                    if !out.is_empty() && !out.ends_with("\n\n") {
                        out.push('\n');
                    }
                    out.push_str(&format!("[{name}]\n"));
                }
                for l in lines {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            out
        }
    }

    #[cfg(test)]
    mod tests {
        use super::mimeapps::{ADDED, DEFAULT, edit_text};

        #[test]
        fn packaged_desktop_entry_is_current() {
            let packaged = include_str!("../assets/linux/trax.desktop").replace("\r\n", "\n");
            assert_eq!(packaged, super::desktop_entry("Exec=trax %F"));
        }

        #[test]
        fn mimeapps_set_and_remove() {
            let text = "[Default Applications]\naudio/mpeg=vlc.desktop;\ntext/plain=gedit.desktop;\n";
            let set = edit_text(
                text,
                &|section: &str, mime: &str, apps: &mut Vec<String>| {
                    if mime.starts_with("audio/") {
                        if section == DEFAULT {
                            *apps = vec!["trax.desktop".into()];
                        } else if section == ADDED {
                            apps.insert(0, "trax.desktop".into());
                        }
                    }
                },
                Some(["audio/mpeg", "audio/flac"].into_iter()),
            );
            assert_eq!(
                set,
                "[Default Applications]\naudio/mpeg=trax.desktop;\ntext/plain=gedit.desktop;\naudio/flac=trax.desktop;\n\n\
                 [Added Associations]\naudio/mpeg=trax.desktop;\naudio/flac=trax.desktop;\n"
            );
            let removed = edit_text(
                &set,
                &|_: &str, _: &str, apps: &mut Vec<String>| apps.retain(|a| a != "trax.desktop"),
                None::<std::iter::Empty<&str>>,
            );
            assert_eq!(removed, "[Default Applications]\ntext/plain=gedit.desktop;\n\n[Added Associations]\n");
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
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
    pub fn make_default() -> Result<&'static str> {
        bail!("not supported on this system yet")
    }
}

pub use imp::*;
