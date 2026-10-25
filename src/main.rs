// Hide the console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

mod app;
mod backup;
mod connectors;
mod db;
mod eq;
mod fx;
mod instance;
mod loudness;
mod lyrics;
mod media_keys;
mod model;
mod net;
mod party;
mod paths;
mod player;
mod share;
mod social;
mod tray;
mod ui_models;
mod system;
mod updater;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // `--register` is the same as Settings → Make trax the default player (without opening
    // Default apps); `--unregister` is run by the uninstaller.
    if std::env::args().any(|a| a == "--register") {
        return system::register();
    }
    if std::env::args().any(|a| a == "--unregister") {
        return system::unregister();
    }
    // Audio files to open ("Open with trax", double-click when trax is the default player).
    let files: Vec<std::path::PathBuf> =
        std::env::args_os().skip(1).filter(|a| !a.to_string_lossy().starts_with("--")).map(Into::into).collect();
    if instance::forward(&files) {
        return Ok(()); // the running trax took them
    }
    log::info!("trax {} — data in {}", env!("CARGO_PKG_VERSION"), paths::data_dir().display());
    updater::cleanup();
    let db = db::Db::open(&paths::db_file())?;
    // Interface size (Settings → Appearance) is applied when the window is created.
    let scale = db.setting_or("scale_override", "");
    if let Ok(f) = scale.parse::<f32>() {
        if f > 0.0 && std::env::var_os("SLINT_SCALE_FACTOR").is_none() {
            // SAFETY: set before any other thread is started.
            unsafe { std::env::set_var("SLINT_SCALE_FACTOR", format!("{f}")) };
        }
    }
    db.set_setting("ui_scale_started", &db.setting_or("ui_scale", "100"));
    app::run(db, files)
}
