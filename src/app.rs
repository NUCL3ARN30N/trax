//! Application controller: owns the UI, player, queue and database, and wires them together.

use crate::connectors::{self, local, nextcloud, soundcloud, youtube};
use crate::db::{AUTO_PLAYLISTS, Db};
use crate::eq;
use crate::media_keys::{MediaAction, MediaKeys};
use crate::model::{SourceKind, Track, format_duration, format_hours};
use crate::paths;
use crate::player::{AudioOutput, Queue, Repeat};
use crate::tray::{Tray, TrayAction};
use crate::ui_models::{ImageCache, TrackModel};
use crate::{AppWindow, ArtistStat, Backend, PlaylistItem, TrayPopup, TrayState};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "app_artists.rs"]
mod artists;
#[path = "app_sound.rs"]
mod sound;
#[path = "app_files.rs"]
mod files;
#[path = "app_system.rs"]
mod system_settings;
#[path = "app_share.rs"]
mod share_ui;
#[path = "app_party.rs"]
mod party_app;
#[path = "app_social.rs"]
mod social_app;
#[path = "app_update.rs"]
mod update;
#[path = "app_theme.rs"]
mod theme;
#[path = "app_offline.rs"]
mod offline;
#[path = "app_lyrics.rs"]
mod lyrics_ui;

const TICK: Duration = Duration::from_millis(250);

/// The local file a track plays from, if it is there already (local or cached download).
fn ready_file(t: &Track) -> Option<PathBuf> {
    let p = match t.source {
        SourceKind::Local | SourceKind::File => PathBuf::from(&t.uri),
        SourceKind::Nextcloud => nextcloud::cache_path(&t.uri),
        SourceKind::YouTube => youtube::cache_path(&t.uri),
        SourceKind::SoundCloud => return soundcloud::cached(&t.uri),
    };
    (t.source.is_on_disk() || p.exists()).then_some(p)
}

/// Where a remote track's download lives in the cache (None for files on this PC).
fn cache_file(t: &Track) -> Option<PathBuf> {
    match t.source {
        SourceKind::Local | SourceKind::File => None,
        SourceKind::Nextcloud => Some(nextcloud::cache_path(&t.uri)),
        SourceKind::YouTube => Some(youtube::cache_path(&t.uri)),
        SourceKind::SoundCloud => Some(soundcloud::cache_path(&t.uri)),
    }
}

/// Download a remote track into the cache.
fn fetch_remote(cfg: &nextcloud::Config, ytdlp: &str, t: &Track) -> anyhow::Result<PathBuf> {
    match t.source {
        SourceKind::Local | SourceKind::File => Ok(PathBuf::from(&t.uri)),
        SourceKind::Nextcloud => nextcloud::fetch(cfg, &t.uri),
        SourceKind::YouTube => youtube::fetch(ytdlp, &t.uri),
        SourceKind::SoundCloud => soundcloud::fetch(ytdlp, &t.uri),
    }
}

/// One step in the back/forward history.
#[derive(Clone, PartialEq)]
enum NavEntry {
    Page(i32),
    Playlist(i64),
    Auto(String),
    Artist(String),
}

/// What the edit dialog is editing.
enum EditTarget {
    None,
    Track(Track),
    Playlist(i64),
}

#[derive(Clone, PartialEq)]
enum PlaylistView {
    None,
    User(i64),
    Auto(String),
}

pub struct App {
    window: AppWindow,
    popup: TrayPopup,
    popup_visible: bool,
    popup_shown_at: Instant,
    popup_last_hover: Instant,
    db: Arc<Mutex<Db>>,
    audio: Option<AudioOutput>,
    queue: Queue,
    library: Rc<TrackModel>,
    recent: Rc<TrackModel>,
    search_local: Rc<TrackModel>,
    search_yt: Rc<TrackModel>,
    search_sc: Rc<TrackModel>,
    playlist: Rc<TrackModel>,
    queue_model: Rc<TrackModel>,
    view: PlaylistView,
    play_token: u64,
    search_token: u64,
    loading: bool,
    listened_ms: i64,
    last_tick: Instant,
    duration_ms: i64,
    toast_until: Option<Instant>,
    /// A newer release found by the update check.
    update: Option<crate::updater::Release>,
    /// A running listening party (host or guest).
    party: Option<party_app::PartyState>,
    party_ids: u64,
    /// Friends, while switched on.
    social: Option<social_app::Social>,
    social_ids: u64,
    /// Profile keys (see `crate::social::profile`), loaded on first use.
    profile: Option<nostr::prelude::Keys>,
    /// Set by `refresh_queue`; the party sends the change on the next tick.
    party_queue_dirty: std::cell::Cell<bool>,
    tray: Option<Tray>,
    media: Option<MediaKeys>,
    images: Rc<ImageCache>,
    artist_model: Rc<TrackModel>,
    /// Artist shown on the artist page.
    artist_view: Option<String>,
    artist_albums: Vec<Vec<Track>>,
    artist_fetching: Arc<Mutex<HashSet<String>>>,
    cover_fetching: Arc<Mutex<HashSet<String>>>,
    match_candidates: Vec<artists::MatchCandidate>,
    match_token: u64,
    nav: Vec<NavEntry>,
    nav_pos: usize,
    /// Set while going back/forward so the move isn't recorded as a new step.
    nav_restoring: bool,
    timer: slint::Timer,
    library_dirty: bool,
    eq_gains: Rc<VecModel<f32>>,
    /// Playlist the queue was started from (new tracks added to it are queued too).
    queue_context: Option<i64>,
    edit: EditTarget,
    /// Pending artwork change in the edit dialog: None = unchanged, Some(None) = removed.
    edit_cover: Option<Option<String>>,
    /// Remote tracks currently being preloaded.
    prefetching: Arc<Mutex<HashSet<String>>>,
    /// A download run for the playlists kept offline is in progress.
    offline_busy: bool,
    favorites: crate::ui_models::Favorites,
    spectrum: Rc<VecModel<f32>>,
    spectrum_peaks: Rc<VecModel<f32>>,
    spectrum_smooth: Vec<f32>,
    visual_timer: slint::Timer,
    fft: Arc<dyn rustfft::Fft<f32>>,
    /// A pause fade-out is running; the player pauses when it ends.
    pause_pending: bool,
    /// Position to continue from when the restored song is first played.
    resume_pos: Option<Duration>,
    ticks: u32,
    /// Lyrics of the song that is playing and the song they belong to.
    lyrics: crate::lyrics::Lyrics,
    lyrics_key: String,
    lyrics_index: i32,
    lyrics_fetching: Arc<Mutex<HashSet<String>>>,
    /// Tracks whose loudness is being measured for volume normalization.
    loudness_busy: Arc<Mutex<HashSet<String>>>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` with the application state. Silently skipped if the state is already borrowed
/// (re-entrant call) or not yet initialised.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => {
            log::debug!("re-entrant UI callback ignored");
            None
        }
    })
}

/// Run `f` later on the UI thread (use from worker threads).
fn post(f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with_app(f);
    });
}

fn s(v: impl AsRef<str>) -> SharedString {
    SharedString::from(v.as_ref())
}

/// Index in the preset list shown in the UI ("Custom" first, then the built-in presets).
fn eq_preset_index(name: &str) -> i32 {
    eq::PRESETS.iter().position(|(n, _)| *n == name).map(|i| i as i32 + 1).unwrap_or(0)
}

fn eq_preset_name(index: i32) -> &'static str {
    if index <= 0 { "Custom" } else { eq::PRESETS.get(index as usize - 1).map(|(n, _)| *n).unwrap_or("Custom") }
}

/// How long the launch animation runs; the timeline in `ui/splash.slint` is as long.
const SPLASH: Duration = Duration::from_millis(2200);

/// Centres the launch animation on its monitor. Windows has already put it on the right
/// one (the screen trax was started from, where the main window opens too), just not in
/// the middle. Called right after `show`, before the first frame is drawn.
#[cfg(windows)]
fn center_splash(splash: &crate::SplashWindow) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
    let window = splash.window();
    let (pos, size) = (window.position(), window.size());
    let (w, h) = (size.width as i32, size.height as i32);
    let mid = POINT { x: pos.x + w / 2, y: pos.y + h / 2 };
    // SAFETY: plain Win32 queries that only write into the struct passed to them.
    let work = unsafe {
        let monitor = MonitorFromPoint(mid, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        info.rcWork
    };
    window.set_position(slint::PhysicalPosition::new(
        work.left + (work.right - work.left - w) / 2,
        work.top + (work.bottom - work.top - h) / 2,
    ));
}

/// Elsewhere the monitor's full area has to do. The native window only appears once the event
/// loop is running, so this checks back until it is there; the animation starts on a blank
/// frame, so the move isn't seen. Wayland compositors place windows themselves and ignore this.
#[cfg(not(windows))]
fn center_splash(splash: &crate::SplashWindow) {
    use slint::winit_030::WinitWindowAccessor;
    fn attempt(splash: slint::Weak<crate::SplashWindow>, tries: u32) {
        slint::Timer::single_shot(Duration::from_millis(5), move || {
            let Some(sp) = splash.upgrade() else { return };
            let window = sp.window();
            match window.with_winit_window(|w| w.current_monitor()) {
                Some(Some(monitor)) => {
                    let (pos, area, size) = (monitor.position(), monitor.size(), window.size());
                    window.set_position(slint::PhysicalPosition::new(
                        pos.x + (area.width as i32 - size.width as i32) / 2,
                        pos.y + (area.height as i32 - size.height as i32) / 2,
                    ));
                }
                Some(None) => {}
                None if tries > 0 => attempt(splash, tries - 1),
                None => {}
            }
        });
    }
    attempt(splash.as_weak(), 100);
}

pub fn run(db: Db, files: Vec<PathBuf>) -> anyhow::Result<()> {
    // Wayland app id and X11 window class, which tie the windows to trax.desktop (icon,
    // taskbar grouping). Has to be set before the first window is created.
    #[cfg(target_os = "linux")]
    let _ = slint::set_xdg_app_id("trax");
    // Up before anything else, so there is something to look at while the library opens.
    let splash = match db.setting_bool("splash", true) {
        true => crate::SplashWindow::new().map_err(|e| log::error!("launch animation failed: {e}")).ok(),
        false => None,
    };
    if let Some(sp) = &splash {
        sp.set_version(s(concat!("v", env!("CARGO_PKG_VERSION"))));
        let _ = sp.show();
        center_splash(sp);
    }
    let window = AppWindow::new()?;
    let popup = TrayPopup::new()?;
    let images = Rc::new(ImageCache::default());
    let favorites: crate::ui_models::Favorites = Default::default();

    let audio = match AudioOutput::new() {
        Ok(a) => Some(a),
        Err(e) => {
            log::error!("audio init failed: {e:#}");
            None
        }
    };

    let app = App {
        library: TrackModel::new(images.clone(), false, favorites.clone()),
        recent: TrackModel::new(images.clone(), false, favorites.clone()),
        search_local: TrackModel::new(images.clone(), false, favorites.clone()),
        search_yt: TrackModel::new(images.clone(), false, favorites.clone()),
        search_sc: TrackModel::new(images.clone(), false, favorites.clone()),
        playlist: TrackModel::new(images.clone(), false, favorites.clone()),
        queue_model: TrackModel::new(images.clone(), true, favorites.clone()),
        window,
        popup,
        popup_visible: false,
        popup_shown_at: Instant::now(),
        popup_last_hover: Instant::now(),
        db: Arc::new(Mutex::new(db)),
        audio,
        queue: Queue::new(),

        view: PlaylistView::None,
        play_token: 0,
        search_token: 0,
        loading: false,
        listened_ms: 0,
        last_tick: Instant::now(),
        duration_ms: 0,
        toast_until: None,
        update: None,
        party: None,
        party_ids: 0,
        social: None,
        social_ids: 0,
        profile: None,
        party_queue_dirty: std::cell::Cell::new(false),
        tray: None,
        media: None,
        artist_model: TrackModel::new(images.clone(), false, favorites.clone()),
        images: images.clone(),
        favorites: favorites.clone(),
        spectrum: Rc::new(VecModel::from(vec![0.0f32; sound::BARS])),
        spectrum_peaks: Rc::new(VecModel::from(vec![0.0f32; sound::BARS])),
        spectrum_smooth: vec![0.0; sound::BARS],
        visual_timer: slint::Timer::default(),
        fft: rustfft::FftPlanner::new().plan_fft_forward(2048),
        pause_pending: false,
        resume_pos: None,
        ticks: 0,
        artist_view: None,
        artist_albums: vec![],
        artist_fetching: Arc::new(Mutex::new(HashSet::new())),
        cover_fetching: Arc::new(Mutex::new(HashSet::new())),
        match_candidates: vec![],
        match_token: 0,
        nav: vec![NavEntry::Page(0)],
        nav_pos: 0,
        nav_restoring: false,
        timer: slint::Timer::default(),
        library_dirty: true,
        eq_gains: Rc::new(VecModel::from(vec![0.0f32; 10])),
        queue_context: None,
        edit: EditTarget::None,
        edit_cover: None,
        prefetching: Arc::new(Mutex::new(HashSet::new())),
        offline_busy: false,
        lyrics: Default::default(),
        lyrics_key: String::new(),
        lyrics_index: -1,
        lyrics_fetching: Arc::new(Mutex::new(HashSet::new())),
        loudness_busy: Arc::new(Mutex::new(HashSet::new())),
    };

    let window = app.window.clone_strong();
    APP.with(|cell| *cell.borrow_mut() = Some(app));

    with_app(|a| {
        a.bind_models();
        a.load_settings();
        a.refresh_all();
        a.restore_resume();
    });
    wire_callbacks(&window);
    wire_tray_popup();

    window.window().on_close_requested(|| {
        let to_tray = with_app(|a| a.window.global::<Backend>().get_close_to_tray() && a.tray.is_some()).unwrap_or(false);
        if !to_tray {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });

    // The window appears once the launch animation is over.
    let open_window = move || {
        if let Err(e) = window.show() {
            log::error!("cannot show the window: {e}");
        }
        start_window(files);
    };
    match splash {
        Some(sp) => slint::Timer::single_shot(SPLASH, move || {
            let _ = sp.hide();
            open_window();
        }),
        None => open_window(),
    }

    // Later starts hand their files to this instance.
    crate::instance::listen(|files| post(move |a| a.open_files(files)));

    slint::run_event_loop_until_quit()?;
    crate::instance::release();

    with_app(|a| {
        // End or leave a running party first (a guest gets its own queue back).
        a.party_leave(None);
        // Friends see you go offline.
        a.social_stop(true);
        a.finish_listening();
        a.save_session();
    });
    // Release the database before an update starts the new version.
    APP.with(|cell| drop(cell.borrow_mut().take()));
    crate::updater::run_after_exit();
    Ok(())
}

/// Everything that needs a live event loop and a native window.
fn start_window(files: Vec<PathBuf>) {
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        with_app(|a| {
            let scale = a.window.window().scale_factor() as f64;
            set_native_window_icon(a.window.window(), scale);
            install_nav_input(a.window.window());
            theme::set_window_corners(a.window.window(), a.backend().get_corner_style());
            match Tray::new(on_tray_action, scale) {
                Ok(t) => a.tray = Some(t),
                Err(e) => log::error!("tray icon failed: {e:#}"),
            }
            a.media = MediaKeys::new(a.window.window(), on_media_action);
            a.timer.start(slint::TimerMode::Repeated, TICK, || {
                with_app(|a| a.tick());
            });
            a.startup_scan();
            a.update_tray();
            // Files trax was started with ("Open with", double-click on a song).
            if !files.is_empty() {
                a.open_files(files);
            }
            #[cfg(debug_assertions)]
            a.social_debug_autostart();
            #[cfg(debug_assertions)]
            a.party_debug_autostart();
        });
    });
}

impl App {
    fn backend(&self) -> Backend<'_> {
        self.window.global::<Backend>()
    }

    fn bind_models(&self) {
        let b = self.backend();
        b.set_library(ModelRc::from(self.library.clone()));
        b.set_recent(ModelRc::from(self.recent.clone()));
        b.set_search_local(ModelRc::from(self.search_local.clone()));
        b.set_search_yt(ModelRc::from(self.search_yt.clone()));
        b.set_search_sc(ModelRc::from(self.search_sc.clone()));
        b.set_playlist_tracks(ModelRc::from(self.playlist.clone()));
        b.set_queue(ModelRc::from(self.queue_model.clone()));
        b.set_artist_tracks(ModelRc::from(self.artist_model.clone()));
        b.set_spectrum(ModelRc::from(self.spectrum.clone()));
        b.set_spectrum_peaks(ModelRc::from(self.spectrum_peaks.clone()));
        b.set_fx_presets(ModelRc::new(VecModel::from(
            sound::FX_PRESETS.iter().map(|(n, _)| s(n)).collect::<Vec<_>>(),
        )));
        let presets: Vec<SharedString> =
            std::iter::once(s("Custom")).chain(eq::PRESETS.iter().map(|(n, _)| s(n))).collect();
        b.set_eq_presets(ModelRc::new(VecModel::from(presets)));
        b.set_eq_gains(ModelRc::from(self.eq_gains.clone()));
        b.set_eq_labels(ModelRc::new(VecModel::from(eq::BAND_LABELS.iter().map(s).collect::<Vec<_>>())));
        b.set_data_dir(s(paths::data_dir().to_string_lossy()));
        b.set_version(s(env!("CARGO_PKG_VERSION")));
    }

    // ------------------------------------------------------------------ settings

    fn load_settings(&mut self) {
        let db = self.db.lock().unwrap();
        let b = self.window.global::<Backend>();
        b.set_theme_mode(db.setting_or("theme", "0").parse().unwrap_or(0));
        b.set_close_to_tray(db.setting_bool("close_to_tray", true));
        b.set_local_enabled(db.setting_bool("local_enabled", true));
        b.set_nc_enabled(db.setting_bool("nc_enabled", false));
        b.set_yt_enabled(db.setting_bool("yt_enabled", false));
        b.set_sc_enabled(db.setting_bool("sc_enabled", false));
        b.set_search_scope(db.setting_or("search_scope", "0").parse().unwrap_or(0));
        b.set_spotify_enabled(db.setting_bool("spotify_enabled", false));
        b.set_spotify_auto(db.setting_bool("spotify_auto", true));
        b.set_nc_url(s(db.setting_or("nc_url", "")));
        b.set_nc_user(s(db.setting_or("nc_user", "")));
        b.set_nc_password(s(db.setting_or("nc_password", "")));
        b.set_nc_folder(s(db.setting_or("nc_folder", "Music")));
        b.set_ytdlp_path(s(db.setting_or("ytdlp_path", "")));
        b.set_ytdlp_help(s(youtube::setup_help(&paths::bin_dir().display().to_string())));
        b.set_sp_client_id(s(db.setting_or("sp_client_id", "")));
        b.set_sp_client_secret(s(db.setting_or("sp_client_secret", "")));
        let folders: Vec<SharedString> =
            db.setting_or("local_folders", "").lines().filter(|l| !l.trim().is_empty()).map(s).collect();
        b.set_local_folders(ModelRc::new(VecModel::from(folders)));

        let volume: f32 = db.setting_or("volume", "0.7").parse().unwrap_or(0.7);
        self.queue.shuffle = db.setting_bool("shuffle", false);
        self.queue.repeat = Repeat::from_int(db.setting_or("repeat", "0").parse().unwrap_or(0));
        let eq_on = db.setting_bool("eq_enabled", false);
        let gains: Vec<f32> = db
            .setting_or("eq_gains", "")
            .split(',')
            .filter_map(|g| g.trim().parse().ok())
            .collect();
        let preset = db.setting_or("eq_preset", "Custom");
        let auto_on = db.setting_bool("auto_playlists", true);
        let preload = db.setting_bool("preload", true);
        let crossfade = db.setting_bool("crossfade", false);
        let crossfade_secs: f32 = db.setting_or("crossfade_secs", "2").parse().unwrap_or(2.0);
        drop(db);

        if let Some(audio) = &mut self.audio {
            audio.set_volume(volume);
            audio.eq.set_enabled(eq_on);
            for (i, g) in gains.iter().enumerate().take(10) {
                audio.eq.set_gain(i, *g);
            }
        }
        b.set_volume(volume);
        b.set_shuffle(self.queue.shuffle);
        b.set_repeat_mode(self.queue.repeat.as_int());
        b.set_eq_enabled(eq_on);
        b.set_eq_preset_index(eq_preset_index(&preset));
        b.set_auto_playlists_enabled(auto_on);
        b.set_preload_enabled(preload);
        b.set_crossfade_enabled(crossfade);
        b.set_crossfade_secs(crossfade_secs.clamp(0.5, 5.0));
        self.push_eq_gains();
        {
            let db = self.db.lock().unwrap();
            b.set_crossfade_curve(db.setting_or("crossfade_curve", "1").parse().unwrap_or(1));
            b.set_crossfade_on_skip(db.setting_bool("crossfade_on_skip", false));
            b.set_pause_fade(db.setting_bool("pause_fade", true));
            b.set_resume_enabled(db.setting_bool("resume_enabled", true));
            b.set_sections_enabled(db.setting_bool("sections", true));
            b.set_cache_limit_mb(db.setting_or("cache_limit_mb", "4096").parse().unwrap_or(4096));
            b.set_offline_all(db.setting_bool("offline_all", false));
            b.set_fs_background(db.setting_or("fs_background", "0").parse().unwrap_or(0));
            b.set_visual_style(db.setting_or("visual_style", "0").parse().unwrap_or(0));
            b.set_lyrics_enabled(db.setting_bool("lyrics", true));
            b.set_normalize_enabled(db.setting_bool("normalize", false));
            b.set_splash_enabled(db.setting_bool("splash", true));
        }
        self.update_normalize_info();
        let low_end = self.db.lock().unwrap().setting_bool("low_end", false);
        self.set_low_end(low_end);
        let visualizer = self.db.lock().unwrap().setting_bool("visualizer", false);
        self.set_visualizer(visualizer);
        self.fx_load();
        self.load_favorites();
        self.load_appearance();
        self.apply_theme();
        self.trim_cache();
        self.sync_offline();
        self.check_ytdlp();
        self.load_update_settings();
        self.load_party_settings();
        self.load_social_settings();
        self.load_system_integration();
    }

    fn save_session(&self) {
        if let Some(audio) = &self.audio {
            self.db.lock().unwrap().set_setting("volume", &format!("{:.3}", audio.volume()));
        }
        self.save_resume();
    }

    fn enabled_sources(&self) -> Vec<SourceKind> {
        let b = self.backend();
        let mut v = vec![];
        if b.get_local_enabled() {
            v.push(SourceKind::Local);
        }
        if b.get_nc_enabled() {
            v.push(SourceKind::Nextcloud);
        }
        if b.get_yt_enabled() {
            v.push(SourceKind::YouTube);
        }
        if b.get_sc_enabled() {
            v.push(SourceKind::SoundCloud);
        }
        // Files opened on their own were picked by the user, so they always show.
        v.push(SourceKind::File);
        v
    }

    fn nc_config(&self) -> nextcloud::Config {
        let b = self.backend();
        nextcloud::Config {
            url: b.get_nc_url().to_string(),
            user: b.get_nc_user().to_string(),
            password: b.get_nc_password().to_string(),
            folder: b.get_nc_folder().to_string(),
        }
    }

    fn check_ytdlp(&self) {
        let path = self.backend().get_ytdlp_path().to_string();
        self.backend().set_ytdlp_status(s("checking…"));
        std::thread::spawn(move || {
            let status = match youtube::version(&path) {
                Some(v) => format!("yt-dlp {v} ✓"),
                None => youtube::not_found_hint(&path).to_string(),
            };
            post(move |a| a.backend().set_ytdlp_status(s(status)));
        });
    }

    // ------------------------------------------------------------------ refresh

    fn refresh_all(&mut self) {
        self.refresh_library();
        self.refresh_playlists();
        self.refresh_home();
        self.refresh_queue();
    }

    fn refresh_library(&mut self) {
        let b = self.backend();
        let source = match b.get_library_source() {
            1 => Some(SourceKind::Local),
            2 => Some(SourceKind::Nextcloud),
            3 => Some(SourceKind::YouTube),
            4 => Some(SourceKind::SoundCloud),
            _ => None,
        };
        if b.get_library_view() == 1 {
            let artists = self.artist_items(&b.get_library_filter());
            b.set_artists(ModelRc::new(VecModel::from(artists)));
        }
        let tracks =
            self.db.lock().unwrap().library(&self.enabled_sources(), b.get_library_sort(), &b.get_library_filter(), source);
        b.set_library_count(tracks.len() as i32);
        self.library.set(tracks);
        self.library_dirty = false;
        self.mark_playing();
    }

    fn refresh_playlists(&self) {
        let lists = self.db.lock().unwrap().playlists();
        let items: Vec<PlaylistItem> = lists
            .iter()
            .map(|p| {
                let cover = p.cover.as_deref().and_then(ImageCache::full);
                PlaylistItem {
                    id: p.id as i32,
                    key: s(""),
                    name: s(&p.name),
                    subtitle: s(if p.description.is_empty() {
                        format!("{} tracks · {}", p.count, format_hours(p.duration_ms))
                    } else {
                        p.description.clone()
                    }),
                    count: p.count as i32,
                    has_cover: cover.is_some(),
                    cover: cover.unwrap_or_default(),
                    favorites: p.favorites,
                }
            })
            .collect();
        self.backend().set_playlists(ModelRc::new(VecModel::from(items)));
        let autos: Vec<PlaylistItem> = if self.backend().get_auto_playlists_enabled() {
            AUTO_PLAYLISTS
                .iter()
                .map(|p| PlaylistItem {
                    id: 0,
                    key: s(p.key),
                    name: s(p.name),
                    subtitle: s(p.description),
                    count: 0,
                    cover: Default::default(),
                    has_cover: false,
                    favorites: false,
                })
                .collect()
        } else {
            vec![]
        };
        self.backend().set_auto_playlists(ModelRc::new(VecModel::from(autos)));
    }

    fn refresh_home(&self) {
        let db = self.db.lock().unwrap();
        let st = db.stats();
        let recent: Vec<Track> = db.auto_playlist("recent", &self.enabled_sources()).into_iter().take(8).collect();
        drop(db);
        let b = self.backend();
        b.set_hours_week(s(format_hours(st.ms_week)));
        b.set_hours_year(s(format_hours(st.ms_year)));
        b.set_hours_total(s(format_hours(st.ms_total)));
        b.set_plays_week(s(st.plays_week.to_string()));
        b.set_track_count(s(st.track_count.to_string()));
        // Merge "A, B" / "A feat. B" into the main artist, as on the artist pages.
        let mut merged: Vec<(String, i64)> = vec![];
        for (name, ms) in &st.top_artists_year {
            let main = self.main_artist(name);
            match merged.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(&main)) {
                Some((_, total)) => *total += ms,
                None => merged.push((main, *ms)),
            }
        }
        merged.sort_by(|a, b| b.1.cmp(&a.1));
        let artists: Vec<ArtistStat> = merged
            .iter()
            .take(5)
            .map(|(name, ms)| {
                let img = self.db.lock().unwrap().artist_art(name).and_then(|p| self.images.thumb(&p));
                ArtistStat {
                    name: s(name),
                    hours: s(format_hours(*ms)),
                    has_image: img.is_some(),
                    image: img.unwrap_or_default(),
                }
            })
            .collect();
        b.set_top_artists(ModelRc::new(VecModel::from(artists)));
        // Fetch pictures for top artists that don't have one yet.
        for a in merged.iter().take(5) {
            if self.db.lock().unwrap().artist_art(&a.0).is_none() {
                self.auto_artist_info(&a.0);
            }
        }
        self.recent.set(recent);
        self.mark_playing();
    }

    fn refresh_queue(&self) {
        self.party_queue_dirty.set(true);
        self.queue_model.set(self.queue.tracks.clone());
        self.backend().set_queue_index(self.queue.index.map(|i| i as i32).unwrap_or(-1));
        self.mark_playing();
    }

    fn reload_playlist_view(&self) {
        let mut cover = None;
        let mut description = String::new();
        let mut offline = false;
        let (name, subtitle, editable, tracks) = match &self.view {
            PlaylistView::User(id) => {
                let db = self.db.lock().unwrap();
                let tracks = db.playlist_tracks(*id);
                let info = db.playlist(*id);
                cover = info.as_ref().and_then(|p| p.cover.as_deref()).and_then(ImageCache::full);
                description = info.as_ref().map(|p| p.description.clone()).unwrap_or_default();
                offline = info.as_ref().is_some_and(|p| p.offline);
                (info.map(|p| p.name).unwrap_or_default(), String::new(), true, tracks)
            }
            PlaylistView::Auto(key) => {
                let info = AUTO_PLAYLISTS.iter().find(|p| p.key == key);
                let tracks = self.db.lock().unwrap().auto_playlist(key, &self.enabled_sources());
                (
                    info.map(|i| i.name).unwrap_or("").to_string(),
                    info.map(|i| i.description).unwrap_or("").to_string(),
                    false,
                    tracks,
                )
            }
            PlaylistView::None => return,
        };
        let total: i64 = tracks.iter().map(|t| t.duration_ms).sum();
        let summary = format!("{} tracks · {}", tracks.len(), format_hours(total));
        let b = self.backend();
        b.set_playlist_name(s(name));
        b.set_playlist_subtitle(s(if subtitle.is_empty() { summary } else { format!("{subtitle} · {summary}") }));
        b.set_playlist_editable(editable);
        let fav_id = self.db.lock().unwrap().favorites_id().ok();
        b.set_playlist_favorites(matches!(self.view, PlaylistView::User(id) if Some(id) == fav_id));
        b.set_playlist_description(s(description));
        b.set_playlist_offline(offline);
        b.set_playlist_has_cover(cover.is_some());
        b.set_playlist_cover(cover.unwrap_or_default());
        b.set_playlist_id(match &self.view {
            PlaylistView::User(id) => *id as i32,
            _ => 0,
        });
        b.set_playlist_key(s(match &self.view {
            PlaylistView::Auto(k) => k.as_str(),
            _ => "",
        }));
        self.playlist.set(tracks);
        self.mark_playing();
    }

    fn all_models(&self) -> [&Rc<TrackModel>; 8] {
        [&self.library, &self.recent, &self.search_local, &self.search_yt, &self.search_sc, &self.playlist, &self.queue_model, &self.artist_model]
    }

    fn mark_playing(&self) {
        let key = self.queue.current().map(|t| (t.source, t.uri.clone()));
        for m in [&self.library, &self.recent, &self.search_local, &self.search_yt, &self.search_sc, &self.playlist, &self.artist_model] {
            m.set_playing(key.clone(), None);
        }
        self.queue_model.set_playing(None, self.queue.index);
    }

    fn set_edit_cover_preview(&self, cover: Option<&str>) {
        let img = cover.and_then(ImageCache::full);
        let b = self.backend();
        b.set_edit_has_cover(img.is_some());
        b.set_edit_cover(img.unwrap_or_default());
    }

    fn toast(&mut self, msg: impl AsRef<str>) {
        self.backend().set_toast(s(msg));
        self.toast_until = Some(Instant::now() + Duration::from_millis(3500));
    }

    // ------------------------------------------------------------------ lists

    fn model_for(&self, list: &str) -> Option<&Rc<TrackModel>> {
        Some(match list {
            "library" => &self.library,
            "recent" => &self.recent,
            "search-local" => &self.search_local,
            "search-yt" => &self.search_yt,
            "search-sc" => &self.search_sc,
            "playlist" => &self.playlist,
            "queue" => &self.queue_model,
            "artist" => &self.artist_model,
            _ => return None,
        })
    }

    fn track_at(&self, list: &str, index: i32) -> Option<Track> {
        self.model_for(list)?.get(index.max(0) as usize)
    }

    /// Make sure a track has a database id (YouTube results aren't stored until used).
    fn ensure_id(&self, t: &mut Track) -> Option<i64> {
        if t.id > 0 {
            return Some(t.id);
        }
        let id = self.db.lock().unwrap().ensure_track(t).ok()?;
        t.id = id;
        Some(id)
    }

    // ------------------------------------------------------------------ playback

    /// The user playlist a list belongs to, if any (used to keep the queue in sync).
    fn context_for(&self, list: &str) -> Option<i64> {
        match (list, &self.view) {
            ("playlist", PlaylistView::User(id)) => Some(*id),
            _ => None,
        }
    }

    fn play_from(&mut self, list: &str, index: i32) {
        if self.party_intercept(party_app::Intercept::PlayOther) {
            return;
        }
        self.resume_pos = None;
        if list == "queue" {
            if (index as usize) < self.queue.tracks.len() {
                self.queue.index = Some(index as usize);
                let cf = self.skip_crossfade();
                self.start_current_with(cf);
            }
            return;
        }
        let Some(model) = self.model_for(list) else { return };
        let tracks = model.tracks().clone();
        if tracks.is_empty() {
            return;
        }
        self.queue.set(tracks.clone(), index.max(0) as usize);
        self.queue_context = self.context_for(list);
        if self.queue.shuffle {
            // `set` shuffled everything after the start index; keep the clicked track first.
            if let Some(pos) = self.queue.tracks.iter().position(|t| t.uri == tracks[index as usize].uri) {
                self.queue.index = Some(pos);
            }
        }
        let cf = self.skip_crossfade();
        self.start_current_with(cf);
    }

    fn play_all(&mut self, list: &str, shuffle: bool) {
        if self.party_intercept(party_app::Intercept::PlayOther) {
            return;
        }
        self.resume_pos = None;
        let Some(model) = self.model_for(list) else { return };
        let tracks = model.tracks().clone();
        if tracks.is_empty() {
            return;
        }
        if shuffle != self.queue.shuffle {
            self.queue.shuffle = shuffle;
            self.backend().set_shuffle(shuffle);
            self.db.lock().unwrap().set_setting_bool("shuffle", shuffle);
        }
        let start = if shuffle { rand::random_range(0..tracks.len()) } else { 0 };
        self.queue.set(tracks, start);
        self.queue_context = self.context_for(list);
        if shuffle {
            // Put the random start track first so the whole list follows.
            if let Some(i) = self.queue.index {
                let t = self.queue.tracks.remove(i);
                self.queue.tracks.insert(0, t);
                self.queue.index = Some(0);
            }
        }
        self.start_current();
    }

    fn start_current(&mut self) {
        self.start_current_with(false);
    }

    /// Start the queue's current track; with `crossfade` it fades in over the previous one.
    fn start_current_with(&mut self, crossfade: bool) {
        self.finish_listening();
        let Some(track) = self.queue.current().cloned() else {
            self.stop();
            return;
        };
        // Opened files are only checked when they are played.
        if self.drop_if_missing(&track) {
            if self.queue.current().is_some() {
                self.start_current_with(false);
            } else {
                self.stop();
            }
            return;
        }
        self.play_token += 1;
        let token = self.play_token;
        self.listened_ms = 0;
        self.duration_ms = track.duration_ms;
        self.show_now_playing(&track);
        self.refresh_queue();
        self.party_track_started();

        if let Some(file) = ready_file(&track) {
            self.load_file(token, file, crossfade);
        } else {
            self.set_loading(true);
            let cfg = self.nc_config();
            let bin = self.backend().get_ytdlp_path().to_string();
            std::thread::spawn(move || {
                let res = fetch_remote(&cfg, &bin, &track).map_err(|e| format!("{e:#}"));
                post(move |a| a.on_fetched(token, res));
            });
        }
    }

    fn on_fetched(&mut self, token: u64, res: Result<PathBuf, String>) {
        if token != self.play_token {
            return;
        }
        match res {
            Ok(path) => {
                self.load_file(token, path, false);
                self.trim_cache();
            }
            Err(e) => {
                self.set_loading(false);
                if self.party_is_guest() {
                    // Stay in the party, silent; the page says why.
                    self.party_track_failed(&e);
                } else {
                    self.toast(format!("Can't play: {e}"));
                }
                self.backend().set_playing(false);
            }
        }
    }

    fn set_loading(&mut self, on: bool) {
        self.loading = on;
        self.backend().set_loading(on);
    }

    fn load_file(&mut self, token: u64, path: PathBuf, crossfade: bool) {
        self.set_loading(false);
        if self.audio.is_none() {
            self.toast("No audio output device available");
            return;
        }
        let b = self.window.global::<Backend>();
        let fade = Duration::from_secs_f32(b.get_crossfade_secs());
        let curve = crate::player::FadeCurve::from_int(b.get_crossfade_curve());
        // Least-recently-played cache eviction keys off the file time.
        connectors::cache::touch(&path);
        let path_for_loudness = path.clone();
        let gain = self.queue.current().map(|t| self.track_gain(t)).unwrap_or(1.0);
        let Some(audio) = &mut self.audio else { return };
        let res = if crossfade { audio.crossfade_to(&path, fade, curve, gain) } else { audio.load(&path, gain) };
        match res {
            Ok(dur) => {
                if let Some(d) = dur {
                    if d.as_millis() > 0 {
                        self.duration_ms = d.as_millis() as i64;
                    }
                }
            }
            Err(e) => {
                self.toast(format!("{e:#}"));
                self.backend().set_playing(false);
                return;
            }
        }
        self.pause_pending = false;
        // Favourite sections don't apply to a party guest: it plays what the host plays.
        if !self.party_is_guest() {
            self.apply_section_start();
        }
        if let Some(p) = self.resume_pos.take() {
            if let Some(a) = &mut self.audio {
                a.seek(p);
            }
        }
        self.save_resume();
        self.backend().set_playing(true);
        self.update_position();
        self.update_tray();
        self.update_media(true);
        self.update_now_favorite();
        self.party_track_loaded();

        // Store remote tracks and read their real tags now that the file is local.
        let Some(mut track) = self.queue.current().cloned() else { return };
        self.ensure_id(&mut track);
        if let Some(cur) = self.queue.current_mut() {
            cur.id = track.id;
        }
        if !track.source.is_on_disk() {
            let db = self.db.clone();
            let bin = self.backend().get_ytdlp_path().to_string();
            std::thread::spawn(move || {
                let mut t = track.clone();
                match t.source {
                    SourceKind::Nextcloud => connectors::read_tags(&path, &mut t),
                    SourceKind::YouTube if t.cover.is_none() => t.cover = youtube::fetch_thumbnail(&t.uri),
                    SourceKind::SoundCloud if t.cover.is_none() => t.cover = soundcloud::fetch_thumbnail(&bin, &t.uri, None),
                    _ => {}
                }
                if t.duration_ms == 0 {
                    let mut probe = t.clone();
                    connectors::read_tags(&path, &mut probe);
                    t.duration_ms = probe.duration_ms;
                }
                let t = {
                    let db = db.lock().unwrap();
                    let _ = db.update_track_meta(&t);
                    // User edits win over file tags.
                    db.track(t.id).unwrap_or(t)
                };
                post(move |a| a.on_track_updated(token, t));
            });
        }
        self.prefetch_next();
        if let Some(t) = self.queue.current().cloned() {
            self.auto_metadata_for(&t);
            self.ensure_loudness(&t, path_for_loudness);
            self.update_normalize_info();
        }
    }

    fn on_track_updated(&mut self, token: u64, t: Track) {
        for m in [&self.library, &self.recent, &self.search_local, &self.search_yt, &self.search_sc, &self.playlist, &self.queue_model, &self.artist_model] {
            m.update_track(&t);
        }
        for q in self.queue.tracks.iter_mut() {
            if q.source == t.source && q.uri == t.uri {
                *q = t.clone();
            }
        }
        if token == self.play_token {
            self.show_now_playing(&t);
            self.update_tray();
            self.update_media(self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false));
        }
    }

    /// Download the next few remote tracks of the queue in the background so they start
    /// instantly (and can crossfade). Controlled by the "Preload upcoming songs" setting.
    fn prefetch_next(&self) {
        const AHEAD: usize = 3;
        if !self.backend().get_preload_enabled() {
            return;
        }
        let Some(cur) = self.queue.index else { return };
        let len = self.queue.tracks.len();
        let wrap = self.queue.repeat == Repeat::All;
        let mut todo = vec![];
        for step in 1..=AHEAD.min(len.saturating_sub(1)) {
            let i = cur + step;
            if i >= len && !wrap {
                break;
            }
            let t = &self.queue.tracks[i % len];
            if ready_file(t).is_none() && self.prefetching.lock().unwrap().insert(t.uri.clone()) {
                todo.push(t.clone());
            }
        }
        // Songs that are already on disk can be measured for normalization ahead of time.
        for step in 1..=AHEAD.min(len.saturating_sub(1)) {
            let i = cur + step;
            if i >= len && !wrap {
                break;
            }
            let t = self.queue.tracks[i % len].clone();
            if let Some(path) = ready_file(&t) {
                self.ensure_loudness(&t, path);
            }
        }
        if todo.is_empty() {
            return;
        }
        let cfg = self.nc_config();
        let bin = self.backend().get_ytdlp_path().to_string();
        let busy = self.prefetching.clone();
        std::thread::spawn(move || {
            for t in todo {
                if let Err(e) = fetch_remote(&cfg, &bin, &t) {
                    log::info!("preload of {} failed: {e:#}", t.uri);
                }
                busy.lock().unwrap().remove(&t.uri);
            }
        });
    }

    /// Path of the queue's next track if it can start right now (local or already cached).
    fn next_ready_for_crossfade(&self) -> Option<usize> {
        let i = self.queue.next_index(false)?;
        if Some(i) == self.queue.index {
            return None; // repeat-one: restart without fading into itself
        }
        let t = self.queue.tracks.get(i)?;
        ready_file(t).map(|_| i)
    }

    fn show_now_playing(&mut self, t: &Track) {
        let b = self.window.global::<Backend>();
        b.set_has_track(true);
        b.set_now_title(s(if t.title.is_empty() { &t.uri } else { &t.title }));
        b.set_now_artist(s(t.display_artist()));
        b.set_now_album(s(&t.album));
        b.set_now_source(s(t.source.label()));
        let cover = t.cover.as_deref().and_then(ImageCache::full);
        b.set_has_now_cover(cover.is_some());
        b.set_now_cover(cover.clone().unwrap_or_default());
        b.set_duration((self.duration_ms as f32 / 1000.0).max(0.0));
        b.set_duration_text(s(format_duration(self.duration_ms)));

        let tp = self.popup.global::<TrayState>();
        tp.set_has_track(true);
        tp.set_title(s(if t.title.is_empty() { &t.uri } else { &t.title }));
        tp.set_artist(s(t.display_artist()));
        tp.set_has_cover(cover.is_some());
        tp.set_cover(cover.unwrap_or_default());

        self.update_fullscreen_art();
        self.mark_playing();
        self.load_lyrics(t);
    }

    fn stop(&mut self) {
        self.finish_listening();
        if let Some(a) = &mut self.audio {
            a.stop();
        }
        let b = self.backend();
        b.set_playing(false);
        b.set_position(0.0);
        b.set_position_text(s("0:00"));
        if let Some(m) = &mut self.media {
            m.stop();
        }
        self.update_tray();
        self.party_stopped();
    }

    fn toggle_play(&mut self) {
        if self.audio.is_none() || self.party_intercept(party_app::Intercept::TogglePlay) {
            return;
        }
        if self.queue.current().is_none() {
            // Nothing queued yet: play the library.
            self.play_all("library", false);
            return;
        }
        let Some(audio) = &self.audio else { return };
        if audio.finished() || !audio.is_loaded() {
            // Ended or never loaded: (re)start the current track.
            self.start_current();
            return;
        }
        // The UI state is the truth: during a pause fade-out the player is still running.
        let playing = self.backend().get_playing();
        let fade = if self.backend().get_pause_fade() { Duration::from_millis(220) } else { Duration::ZERO };
        if playing {
            if fade.is_zero() {
                audio.pause();
            } else {
                audio.fade_out_for_pause(fade);
                self.pause_pending = true;
                slint::Timer::single_shot(fade + Duration::from_millis(20), || {
                    with_app(|a| {
                        if a.pause_pending {
                            a.pause_pending = false;
                            if let Some(au) = &a.audio {
                                au.pause();
                            }
                        }
                    });
                });
            }
        } else if self.pause_pending {
            self.pause_pending = false;
            audio.restore_gain();
        } else {
            audio.play_faded(fade);
        }
        self.backend().set_playing(!playing);
        self.update_tray();
        self.update_media(!playing);
        self.party_play_state();
    }

    fn set_play_state(&mut self, play: bool) {
        let playing = self.backend().get_playing();
        if playing != play {
            self.toggle_play();
        }
    }

    /// Crossfade a manual track change too (setting), if something is playing.
    fn skip_crossfade(&self) -> bool {
        let b = self.backend();
        b.get_crossfade_enabled() && b.get_crossfade_on_skip() && b.get_playing()
    }

    fn next(&mut self, user: bool) {
        if user && self.party_intercept(party_app::Intercept::Next) {
            return;
        }
        self.resume_pos = None;
        match self.queue.next_index(user) {
            Some(i) => {
                self.queue.index = Some(i);
                self.party_note_skip(user);
                let cf = user && self.skip_crossfade();
                self.start_current_with(cf);
            }
            None => {
                // End of queue.
                self.stop();
                self.refresh_queue();
            }
        }
    }

    fn previous(&mut self) {
        if self.party_intercept(party_app::Intercept::Previous) {
            return;
        }
        self.resume_pos = None;
        let pos = self.audio.as_ref().map(|a| a.position()).unwrap_or_default();
        if pos > Duration::from_secs(3) {
            self.seek(0.0);
            return;
        }
        if let Some(i) = self.queue.prev_index() {
            self.queue.index = Some(i);
            let cf = self.skip_crossfade();
            self.start_current_with(cf);
        }
    }

    fn seek(&mut self, secs: f32) {
        if self.party_intercept(party_app::Intercept::Seek(secs)) {
            return;
        }
        if !self.audio.as_ref().map(|a| a.is_loaded()).unwrap_or(false) {
            // Restored song not loaded yet: remember where to start.
            self.resume_pos = Some(Duration::from_secs_f32(secs.max(0.0)));
            self.update_position();
            return;
        }
        if let Some(a) = &mut self.audio {
            a.seek(Duration::from_secs_f32(secs.max(0.0)));
        }
        self.update_position();
        self.update_media(self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false));
        self.party_seeked();
    }

    fn set_volume(&mut self, v: f32) {
        if let Some(a) = &mut self.audio {
            a.set_volume(v);
            if a.muted() && v > 0.0 {
                a.set_muted(false);
            }
        }
        self.sync_volume();
        self.db.lock().unwrap().set_setting("volume", &format!("{v:.3}"));
    }

    fn toggle_mute(&mut self) {
        if let Some(a) = &mut self.audio {
            let m = !a.muted();
            a.set_muted(m);
        }
        self.sync_volume();
    }

    fn sync_volume(&self) {
        let (v, m) = self.audio.as_ref().map(|a| (a.volume(), a.muted())).unwrap_or((0.0, false));
        let b = self.backend();
        b.set_volume(v);
        b.set_muted(m);
        let t = self.popup.global::<TrayState>();
        t.set_volume(v);
        t.set_muted(m);
    }

    /// Record the play of the current track if it was listened to long enough.
    fn finish_listening(&mut self) {
        let listened = std::mem::take(&mut self.listened_ms);
        if listened < 5_000 {
            return;
        }
        let threshold = if self.duration_ms > 0 { (self.duration_ms / 2).min(30_000) } else { 30_000 };
        if listened < threshold {
            return;
        }
        let Some(mut t) = self.queue.current().cloned() else { return };
        if let Some(id) = self.ensure_id(&mut t) {
            if let Err(e) = self.db.lock().unwrap().record_play(id, listened) {
                log::warn!("cannot record play: {e}");
            }
            self.refresh_home();
        }
    }

    fn update_position(&self) {
        let loaded = self.audio.as_ref().map(|a| a.is_loaded()).unwrap_or(false);
        let pos = if loaded {
            self.audio.as_ref().map(|a| a.position()).unwrap_or_default()
        } else {
            self.resume_pos.unwrap_or_default()
        };
        let b = self.backend();
        b.set_position(pos.as_secs_f32());
        b.set_position_text(s(format_duration(pos.as_millis() as i64).replace("--:--", "0:00")));
        if self.duration_ms > 0 {
            b.set_duration(self.duration_ms as f32 / 1000.0);
            b.set_duration_text(s(format_duration(self.duration_ms)));
        }
    }

    /// Highlight the lyric line that belongs to the position (called from the tick).
    fn update_lyrics(&mut self) {
        if self.lyrics.synced {
            let pos = self.audio.as_ref().map(|a| a.position().as_millis() as i64).unwrap_or(0);
            self.update_lyrics_line(pos);
        }
    }

    fn update_tray(&self) {
        // The UI state, not the audio device: during a pause fade-out the audio still runs.
        let playing = self.backend().get_playing();
        let (title, artist) = match self.queue.current() {
            Some(t) => (t.title.clone(), t.display_artist().to_string()),
            None => ("Nothing playing".to_string(), String::new()),
        };
        if let Some(tray) = &self.tray {
            tray.update(&title, &artist, playing);
        }
        self.popup.global::<TrayState>().set_playing(playing);
    }

    fn update_media(&mut self, playing: bool) {
        let pos = self.audio.as_ref().map(|a| a.position()).unwrap_or_default();
        let current = self.queue.current().cloned();
        let duration = Duration::from_millis(self.duration_ms.max(0) as u64);
        if let (Some(m), Some(t)) = (&mut self.media, current) {
            m.set_metadata(&t.title, t.display_artist(), &t.album, t.cover.as_deref(), duration);
            m.set_playing(playing, pos);
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;

        if let Some(audio) = &mut self.audio {
            audio.tick();
        }
        if let Some(audio) = &self.audio {
            let playing = audio.is_playing() && self.backend().get_playing();
            if playing {
                self.listened_ms += elapsed.as_millis() as i64;
            }
            let finished = audio.finished();
            let crossfading = audio.is_crossfading();
            let pos = audio.position().as_millis() as i64;
            // End of the track, or of its favourite section.
            let end = self.effective_end_ms();
            let section_end = end > 0 && end < self.duration_ms;
            let (ui_playing, cf_on, cf_secs) = {
                let b = self.backend();
                (b.get_playing(), b.get_crossfade_enabled(), b.get_crossfade_secs())
            };
            // A party guest never moves on by itself; the host says what plays next.
            let guest = self.party_is_guest();
            if guest {
            } else if finished && !self.loading && ui_playing {
                self.next(false);
            } else if playing && !crossfading {
                let mut started = false;
                if cf_on && end > 0 {
                    // Fade length in song time (the tempo effect speeds the song up).
                    let fade_ms = (cf_secs * 1000.0 * self.tempo()) as i64;
                    let remaining = end - pos;
                    if remaining <= fade_ms && remaining > 0 && end > fade_ms * 2 {
                        if let Some(i) = self.next_ready_for_crossfade() {
                            self.queue.index = Some(i);
                            self.start_current_with(true);
                            started = true;
                        }
                    }
                }
                if !started && section_end && pos >= end {
                    self.next(false);
                }
            }
        }
        self.party_tick();
        self.social_tick();
        self.update_narrow_nav();
        self.update_position();
        self.update_lyrics();
        // Save the resume point every 10 seconds while playing.
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks % 40 == 0 && self.backend().get_playing() {
            self.save_resume();
        }

        if let Some(until) = self.toast_until {
            if now > until {
                self.toast_until = None;
                self.backend().set_toast(s(""));
            }
        }

        // Hide the tray popup when the pointer has left it for a while.
        if self.popup_visible {
            if self.popup.get_hovered() {
                self.popup_last_hover = now;
            }
            let idle = now.duration_since(self.popup_last_hover.max(self.popup_shown_at));
            if idle > Duration::from_secs(4) {
                self.hide_popup();
            }
        }
    }

    // ------------------------------------------------------------------ queue

    fn queue_add(&mut self, list: &str, index: i32, next: bool) {
        let Some(t) = self.track_at(list, index) else { return };
        if self.party_intercept(party_app::Intercept::QueueAdd(t.clone(), next)) {
            return;
        }
        let was_empty = self.queue.current().is_none();
        let title = t.title.clone();
        if next { self.queue.insert_next(t) } else { self.queue.push(t) }
        self.refresh_queue();
        if was_empty {
            self.start_current();
        } else {
            self.toast(if next { format!("“{title}” plays next") } else { format!("Added “{title}” to queue") });
            self.prefetch_next();
        }
    }

    // ------------------------------------------------------------------ playlists

    fn open_playlist(&mut self, id: i64) {
        self.view = PlaylistView::User(id);
        self.reload_playlist_view();
        self.backend().set_page(3);
        self.nav_push(NavEntry::Playlist(id));
    }

    fn open_auto(&mut self, key: &str) {
        self.view = PlaylistView::Auto(key.to_string());
        self.reload_playlist_view();
        self.backend().set_page(3);
        self.nav_push(NavEntry::Auto(key.to_string()));
    }

    // ------------------------------------------------------------------ navigation

    /// Switch to one of the main pages (0 home, 1 search, 2 library, 4 settings).
    fn show_page(&mut self, page: i32) {
        self.backend().set_page(page);
        match page {
            0 => self.refresh_home(),
            2 if self.library_dirty => self.refresh_library(),
            // The user may have changed the default apps in Windows meanwhile.
            4 => self.refresh_default_status(),
            _ => {}
        }
        self.nav_push(NavEntry::Page(page));
    }

    fn nav_push(&mut self, entry: NavEntry) {
        if self.nav_restoring {
            return;
        }
        if self.nav.get(self.nav_pos) != Some(&entry) {
            self.nav.truncate(self.nav_pos + 1);
            self.nav.push(entry);
            if self.nav.len() > 100 {
                self.nav.remove(0);
            }
            self.nav_pos = self.nav.len() - 1;
        }
        self.update_nav_buttons();
    }

    fn update_narrow_nav(&self) {
        let w = self.window.window();
        let width = w.size().width as f32 / w.scale_factor();
        let narrow = (width > 0.0 && width < 1100.0) || self.backend().get_sidebar_always_compact();
        if self.backend().get_narrow_nav() != narrow {
            log::debug!("window {width:.0} px wide: narrow layout {narrow}");
            self.backend().set_narrow_nav(narrow);
        }
        // The player bar needs its own threshold: it spans the whole window, and the sidebar
        // setting says nothing about how much room its right-hand buttons have.
        let narrow_player = width > 0.0 && width < 920.0;
        if self.backend().get_narrow_player() != narrow_player {
            self.backend().set_narrow_player(narrow_player);
        }
    }

    fn update_nav_buttons(&self) {
        let b = self.backend();
        b.set_can_go_back(self.nav_pos > 0);
        b.set_can_go_forward(self.nav_pos + 1 < self.nav.len());
    }

    /// Move `step` entries through the history (-1 back, +1 forward). Entries that no longer
    /// exist (a deleted playlist) are dropped and skipped.
    fn nav_go(&mut self, step: isize) {
        loop {
            let target = self.nav_pos as isize + step;
            if target < 0 || target as usize >= self.nav.len() {
                break;
            }
            let entry = self.nav[target as usize].clone();
            if let NavEntry::Playlist(id) = entry {
                if self.db.lock().unwrap().playlist(id).is_none() {
                    self.nav.remove(target as usize);
                    if step < 0 {
                        self.nav_pos -= 1;
                    }
                    continue;
                }
            }
            self.nav_pos = target as usize;
            self.nav_restoring = true;
            match entry {
                NavEntry::Page(p) => self.show_page(p),
                NavEntry::Playlist(id) => self.open_playlist(id),
                NavEntry::Auto(key) => self.open_auto(&key),
                NavEntry::Artist(name) => self.open_artist(&name),
            }
            self.nav_restoring = false;
            break;
        }
        self.update_nav_buttons();
    }

    fn add_to_playlist(&mut self, list: &str, index: i32, playlist: i64) {
        let Some(mut t) = self.track_at(list, index) else { return };
        let Some(id) = self.ensure_id(&mut t) else { return };
        let res = {
            let db = self.db.lock().unwrap();
            // Saving a YouTube track to a playlist also adds it to the library.
            let _ = db.set_in_library(id, true);
            db.add_to_playlist(playlist, &[id]).map(|_| db.playlist_name(playlist).unwrap_or_default())
        };
        // Listening to this playlist right now: the new track joins the queue as well.
        let queued = res.is_ok() && self.queue_context == Some(playlist) && self.queue.current().is_some();
        if queued {
            let mut qt = t.clone();
            qt.id = id;
            self.queue.push(qt);
            self.refresh_queue();
            self.prefetch_next();
        }
        let added = res.is_ok();
        match res {
            Ok(name) if queued => self.toast(format!("Added to {name} and the queue")),
            Ok(name) => self.toast(format!("Added to {name}")),
            Err(e) => self.toast(format!("Error: {e}")),
        }
        self.refresh_playlists();
        if self.view == PlaylistView::User(playlist) {
            self.reload_playlist_view();
        }
        if t.source.is_online() {
            self.library_dirty = true;
        }
        // A playlist kept offline downloads what was just added to it.
        let offline = self.db.lock().unwrap().playlist(playlist).is_some_and(|p| p.offline);
        if added && offline {
            self.sync_offline();
        }
    }

    fn current_playlist_ids(&self) -> Vec<i64> {
        self.playlist.tracks().iter().map(|t| t.id).collect()
    }

    fn write_playlist_order(&mut self, ids: Vec<i64>) {
        let PlaylistView::User(pid) = self.view else { return };
        let res = self.db.lock().unwrap().set_playlist_order(pid, &ids);
        if let Err(e) = res {
            self.toast(format!("Error: {e}"));
        }
        self.reload_playlist_view();
        self.refresh_playlists();
    }

    // ------------------------------------------------------------------ search

    fn search(&mut self, query: &str) {
        let q = query.trim().to_string();
        self.search_token += 1;
        let token = self.search_token;
        let b = self.backend();
        b.set_search_error(s(""));
        if q.is_empty() {
            self.search_local.set(vec![]);
            self.search_yt.set(vec![]);
            self.search_sc.set(vec![]);
            b.set_yt_searching(false);
            b.set_sc_searching(false);
            return;
        }
        // Search filter; a filter for a source that was switched off means everywhere.
        let mut scope = b.get_search_scope();
        if (scope == 2 && !b.get_yt_enabled()) || (scope == 3 && !b.get_sc_enabled()) {
            scope = 0;
            b.set_search_scope(0);
        }
        self.db.lock().unwrap().set_setting("search_scope", &scope.to_string());
        let local = if matches!(scope, 0 | 1) { self.db.lock().unwrap().search(&self.enabled_sources(), &q, 50) } else { vec![] };
        self.search_local.set(local);
        self.mark_playing();

        let yt = b.get_yt_enabled() && matches!(scope, 0 | 2);
        let sc = b.get_sc_enabled() && matches!(scope, 0 | 3);
        if !yt {
            self.search_yt.set(vec![]);
            b.set_yt_searching(false);
        }
        if !sc {
            self.search_sc.set(vec![]);
            b.set_sc_searching(false);
        }
        if !(yt || sc) || q.chars().count() < 2 {
            return;
        }
        b.set_yt_searching(yt);
        b.set_sc_searching(sc);
        // Debounce: only search online once typing pauses.
        slint::Timer::single_shot(Duration::from_millis(650), move || {
            with_app(|a| {
                if a.search_token != token {
                    return;
                }
                let bin = a.backend().get_ytdlp_path().to_string();
                if yt {
                    let (bin, q) = (bin.clone(), q.clone());
                    std::thread::spawn(move || {
                        let res = youtube::search(&bin, &q, 20);
                        post(move |a| a.on_yt_results(token, res.map_err(|e| format!("{e:#}"))));
                    });
                }
                if sc {
                    let q = q.clone();
                    std::thread::spawn(move || {
                        let res = soundcloud::search(&bin, &q, 20);
                        post(move |a| a.on_sc_results(token, bin, res.map_err(|e| format!("{e:#}"))));
                    });
                }
            });
        });
    }

    fn on_sc_results(&mut self, token: u64, bin: String, res: Result<Vec<(Track, Option<String>)>, String>) {
        if token != self.search_token {
            return;
        }
        self.backend().set_sc_searching(false);
        match res {
            Ok(results) => {
                let missing: Vec<(String, Option<String>)> =
                    results.iter().filter(|(t, _)| t.cover.is_none()).map(|(t, art)| (t.uri.clone(), art.clone())).collect();
                self.search_sc.set(results.into_iter().map(|(t, _)| t).collect());
                self.mark_playing();
                std::thread::spawn(move || {
                    for (uri, art) in missing {
                        // Tracks without artwork are skipped rather than asking yt-dlp per result.
                        let Some(art) = art else { continue };
                        if let Some(cover) = soundcloud::fetch_thumbnail(&bin, &uri, Some(&art)) {
                            post(move |a| {
                                let found = a.search_sc.tracks().iter().find(|t| t.uri == uri).cloned();
                                if let Some(mut t) = found {
                                    t.cover = Some(cover);
                                    a.search_sc.update_track(&t);
                                }
                            });
                        }
                    }
                });
            }
            Err(e) => {
                self.search_sc.set(vec![]);
                self.backend().set_search_error(s(e));
            }
        }
    }

    fn on_yt_results(&mut self, token: u64, res: Result<Vec<Track>, String>) {
        if token != self.search_token {
            return;
        }
        self.backend().set_yt_searching(false);
        match res {
            Ok(tracks) => {
                let missing: Vec<String> = tracks.iter().filter(|t| t.cover.is_none()).map(|t| t.uri.clone()).collect();
                self.search_yt.set(tracks);
                self.mark_playing();
                std::thread::spawn(move || {
                    for id in missing {
                        if let Some(cover) = youtube::fetch_thumbnail(&id) {
                            post(move |a| {
                                let found = a.search_yt.tracks().iter().find(|t| t.uri == id).cloned();
                                if let Some(mut t) = found {
                                    t.cover = Some(cover);
                                    a.search_yt.update_track(&t);
                                }
                            });
                        }
                    }
                });
            }
            Err(e) => {
                self.search_yt.set(vec![]);
                self.backend().set_search_error(s(e));
            }
        }
    }

    // ------------------------------------------------------------------ equalizer

    /// Update the slider values in place (replacing the model would recreate the slider
    /// the user is dragging).
    fn push_eq_gains(&self) {
        let gains: Vec<f32> = self.audio.as_ref().map(|a| a.eq.gains().to_vec()).unwrap_or(vec![0.0; 10]);
        for (i, g) in gains.into_iter().enumerate() {
            if self.eq_gains.row_data(i) != Some(g) {
                self.eq_gains.set_row_data(i, g);
            }
        }
    }

    fn save_eq(&self) {
        let Some(a) = &self.audio else { return };
        let gains: Vec<String> = a.eq.gains().iter().map(|g| format!("{g:.1}")).collect();
        let db = self.db.lock().unwrap();
        db.set_setting("eq_gains", &gains.join(","));
        db.set_setting_bool("eq_enabled", a.eq.enabled());
        let preset = eq_preset_name(self.backend().get_eq_preset_index());
        db.set_setting("eq_preset", preset);
        if preset == "Custom" {
            db.set_setting("eq_custom_gains", &gains.join(","));
        }
    }

    // ------------------------------------------------------------------ scanning

    fn startup_scan(&mut self) {
        let b = self.backend();
        if b.get_local_enabled() && b.get_local_folders().row_count() > 0 {
            self.rescan_local(true);
        }
    }

    fn local_folders(&self) -> Vec<String> {
        self.backend().get_local_folders().iter().map(|f| f.to_string()).collect()
    }

    fn save_local_folders(&self, folders: &[String]) {
        self.db.lock().unwrap().set_setting("local_folders", &folders.join("\n"));
        let model: Vec<SharedString> = folders.iter().map(s).collect();
        self.backend().set_local_folders(ModelRc::new(VecModel::from(model)));
    }

    fn rescan_local(&mut self, quiet: bool) {
        let folders = self.local_folders();
        let b = self.backend();
        if b.get_scanning() {
            return;
        }
        b.set_scanning(true);
        b.set_scan_status(s("Scanning local folders…"));
        let db = self.db.clone();
        std::thread::spawn(move || {
            let res = local::scan(&db, &folders, |n| {
                post(move |a| a.backend().set_scan_status(s(format!("Scanning local folders… {n} files"))));
            });
            post(move |a| {
                let b = a.backend();
                b.set_scanning(false);
                match res {
                    Ok(n) => {
                        b.set_scan_status(s(format!("Local library: {n} files")));
                        if !quiet {
                            a.toast(format!("Local scan finished: {n} files"));
                        }
                    }
                    Err(e) => {
                        b.set_scan_status(s(format!("Local scan failed: {e:#}")));
                        a.toast(format!("Local scan failed: {e:#}"));
                    }
                }
                a.refresh_library();
                a.refresh_home();
            });
        });
    }

    fn scan_nextcloud(&mut self) {
        let cfg = self.nc_config();
        if !cfg.is_complete() {
            self.toast("Fill in server URL, username and app password first");
            return;
        }
        let b = self.backend();
        if b.get_scanning() {
            return;
        }
        b.set_scanning(true);
        b.set_scan_status(s("Scanning Nextcloud…"));
        let db = self.db.clone();
        std::thread::spawn(move || {
            let res = nextcloud::scan(&db, &cfg, |n| {
                post(move |a| a.backend().set_scan_status(s(format!("Scanning Nextcloud… {n} files"))));
            });
            post(move |a| {
                let b = a.backend();
                b.set_scanning(false);
                match res {
                    Ok(n) => {
                        b.set_scan_status(s(format!("Nextcloud: {n} files")));
                        a.toast(format!("Nextcloud scan finished: {n} files"));
                    }
                    Err(e) => {
                        b.set_scan_status(s(format!("Nextcloud scan failed: {e:#}")));
                        a.toast(format!("Nextcloud: {e:#}"));
                    }
                }
                a.refresh_library();
                a.refresh_home();
            });
        });
    }

    // ------------------------------------------------------------------ data

    fn export_data(&mut self) {
        let name = format!("trax-backup-{}.traxbak", chrono::Local::now().format("%Y-%m-%d"));
        let Some(dest) = rfd::FileDialog::new()
            .set_title("Export trax data")
            .set_file_name(&name)
            .add_filter("trax backup", &["traxbak"])
            .save_file()
        else {
            return;
        };
        self.toast("Exporting…");
        let db = self.db.clone();
        std::thread::spawn(move || {
            let res = crate::backup::export(&db, &dest);
            post(move |a| match res {
                Ok(()) => a.toast(format!("Exported to {}", dest.display())),
                Err(e) => a.toast(format!("Export failed: {e:#}")),
            });
        });
    }

    fn import_data(&mut self) {
        let Some(src) = rfd::FileDialog::new()
            .set_title("Import trax data")
            .add_filter("trax backup", &["traxbak", "zip"])
            .pick_file()
        else {
            return;
        };
        let confirm = rfd::MessageDialog::new()
            .set_title("Import trax data")
            .set_description(
                "This replaces your current library, playlists, play history and settings with the backup.\n\
                 Your current database is kept as a copy in the data folder.\n\nContinue?",
            )
            .set_buttons(rfd::MessageButtons::YesNo)
            .set_level(rfd::MessageLevel::Warning)
            .show();
        if confirm != rfd::MessageDialogResult::Yes {
            return;
        }
        self.stop();
        self.queue = Queue::new();
        // Close the live database while files are swapped.
        let res = crate::backup::restore(&self.db, &src);
        match res {
            Ok(old_dir) => {
                log::info!("imported backup from {old_dir}");
                self.toast("Import complete. If your music folders moved, use Re-map in Settings.");
            }
            Err(e) => self.toast(format!("Import failed: {e:#}")),
        }
        self.load_settings();
        self.view = PlaylistView::None;
        self.refresh_all();
        let b = self.backend();
        b.set_has_track(false);
        b.set_now_title(s("Nothing playing"));
        b.set_now_artist(s(""));
        b.set_has_now_cover(false);
        self.show_page(0);
    }

    fn open_data_dir(&self) {
        let dir = paths::data_dir();
        #[cfg(windows)]
        let cmd = "explorer";
        #[cfg(target_os = "macos")]
        let cmd = "open";
        #[cfg(all(unix, not(target_os = "macos")))]
        let cmd = "xdg-open";
        let _ = std::process::Command::new(cmd).arg(dir).spawn();
    }

    /// Figures for the "clear audio cache" dialog.
    fn prepare_clear_cache(&self) {
        let keep = self.offline_pins();
        let (bytes, files, pinned) = connectors::cache::usage(&keep);
        let lists = self.db.lock().unwrap().playlists().iter().filter(|p| p.offline).count();
        let total = connectors::cache::format_size(bytes + pinned);
        let b = self.backend();
        b.set_cache_offline_count(lists as i32);
        b.set_cache_clear_info(s(if lists == 0 {
            format!("{files} downloaded songs ({total}) will be removed. Your library and playlists stay as they are.")
        } else {
            format!(
                "{files} downloaded songs ({total}) are on disk. {} of that belongs to {lists} playlist{} kept offline.",
                connectors::cache::format_size(pinned),
                if lists == 1 { "" } else { "s" }
            )
        }));
    }

    /// Delete the downloaded songs. With `include_offline`, the playlists kept offline lose
    /// their downloads too and stop being kept offline.
    fn clear_cache(&mut self, include_offline: bool) {
        let current = self.queue.current().and_then(cache_file);
        let keep = if include_offline { HashSet::new() } else { self.offline_pins() };
        let mut freed = 0u64;
        if let Ok(entries) = std::fs::read_dir(paths::cache_dir()) {
            for e in entries.filter_map(|e| e.ok()) {
                let path = e.path();
                if Some(&path) == current.as_ref() || keep.contains(&path) {
                    continue;
                }
                freed += e.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = std::fs::remove_file(&path);
            }
        }
        if include_offline {
            // Without this the playlists would download themselves again straight away.
            let cleared = self.db.lock().unwrap().clear_offline_playlists().unwrap_or(0);
            if cleared > 0 {
                self.refresh_playlists();
                self.reload_playlist_view();
            }
        }
        self.toast(format!("Freed {:.1} MB", freed as f64 / 1_048_576.0));
        self.trim_cache();
    }

    // ------------------------------------------------------------------ tray popup

    fn show_window(&self) {
        let _ = self.window.show();
        self.window.window().set_minimized(false);
        use slint::winit_030::WinitWindowAccessor;
        self.window.window().with_winit_window(|w| w.focus_window());
    }

    fn toggle_popup(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if self.popup_visible {
            self.hide_popup();
            return;
        }
        let scale = self.window.window().scale_factor() as f64;
        let (pw, ph) = (360.0 * scale, 148.0 * scale);
        let px = (x + w / 2.0 - pw / 2.0).max(0.0);
        let py = if y > ph + 12.0 { y - ph - 12.0 } else { y + h + 12.0 };
        let _ = self.popup.show();
        theme::set_window_corners(self.popup.window(), self.backend().get_corner_style());
        self.popup.window().set_position(slint::PhysicalPosition::new(px as i32, py as i32));
        self.popup_visible = true;
        self.popup_shown_at = Instant::now();
        self.popup_last_hover = Instant::now();
        use slint::winit_030::WinitWindowAccessor;
        self.popup.window().with_winit_window(|w| {
            #[cfg(windows)]
            {
                use slint::winit_030::winit::platform::windows::WindowExtWindows;
                w.set_skip_taskbar(true);
            }
            w.focus_window();
        });
    }

    fn hide_popup(&mut self) {
        let _ = self.popup.hide();
        self.popup_visible = false;
    }
}

/// Mouse side buttons (back/forward), the keyboard's browser keys and Alt+Left/Right move
/// through the navigation history.
fn install_nav_input(window: &slint::Window) {
    use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent};
    use slint::winit_030::winit::keyboard::{Key, NamedKey};
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    let alt = std::cell::Cell::new(false);
    window.on_winit_window_event(move |_, ev| {
        let step = match ev {
            WindowEvent::ModifiersChanged(m) => {
                alt.set(m.state().alt_key());
                None
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Back, .. } => Some(-1),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Forward, .. } => Some(1),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat => {
                match &event.logical_key {
                    Key::Named(NamedKey::BrowserBack) => Some(-1),
                    Key::Named(NamedKey::BrowserForward) => Some(1),
                    Key::Named(NamedKey::ArrowLeft) if alt.get() => Some(-1),
                    Key::Named(NamedKey::ArrowRight) if alt.get() => Some(1),
                    _ => None,
                }
            }
            _ => None,
        };
        match step {
            Some(step) => {
                // Defer: this runs inside winit's event dispatch.
                slint::Timer::single_shot(Duration::ZERO, move || {
                    with_app(|a| a.nav_go(step));
                });
                EventResult::PreventDefault
            }
            None => EventResult::Propagate,
        }
    });
}

/// Use the exe's multi-size icon resource for the title bar and taskbar, so Windows picks a
/// frame drawn for that size instead of downscaling one large image.
fn set_native_window_icon(window: &slint::Window, scale: f64) {
    #[cfg(windows)]
    {
        use slint::winit_030::WinitWindowAccessor;
        use slint::winit_030::winit::dpi::PhysicalSize;
        use slint::winit_030::winit::platform::windows::{IconExtWindows, WindowExtWindows};
        use slint::winit_030::winit::window::Icon;
        let px = |base: f64| {
            let p = (base * scale).round() as u32;
            Some(PhysicalSize::new(p, p))
        };
        window.with_winit_window(|w| {
            if let Ok(small) = Icon::from_resource(1, px(16.0)) {
                w.set_window_icon(Some(small));
            }
            if let Ok(big) = Icon::from_resource(1, px(32.0)) {
                w.set_taskbar_icon(Some(big));
            }
        });
    }
    // X11 takes the icon from the window; Wayland from the desktop entry (see `system`).
    #[cfg(target_os = "linux")]
    {
        use slint::winit_030::WinitWindowAccessor;
        use slint::winit_030::winit::window::Icon;
        let _ = scale;
        let Ok(img) = image::load_from_memory(include_bytes!("../assets/logo/AppIcon.iconset/icon_32x32@2x.png")) else {
            return;
        };
        let img = img.into_rgba8();
        let (w, h) = img.dimensions();
        if let Ok(icon) = Icon::from_rgba(img.into_raw(), w, h) {
            window.with_winit_window(|win| win.set_window_icon(Some(icon)));
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = (window, scale);
}

fn on_tray_action(action: TrayAction) {
    with_app(|a| match action {
        TrayAction::TogglePopup { x, y, w, h } => a.toggle_popup(x, y, w, h),
        TrayAction::ShowApp => a.show_window(),
        TrayAction::PlayPause => a.toggle_play(),
        TrayAction::Next => a.next(true),
        TrayAction::Previous => a.previous(),
        TrayAction::Quit => {
            let _ = slint::quit_event_loop();
        }
    });
}

fn on_media_action(action: MediaAction) {
    with_app(|a| match action {
        MediaAction::PlayPause => a.toggle_play(),
        MediaAction::Play => a.set_play_state(true),
        MediaAction::Pause => a.set_play_state(false),
        MediaAction::Next => a.next(true),
        MediaAction::Previous => a.previous(),
        MediaAction::Seek(p) => a.seek(p.as_secs_f32()),
        MediaAction::Raise => a.show_window(),
    });
}

fn wire_tray_popup() {
    with_app(|a| {
        let t = a.popup.global::<TrayState>();
        t.on_play_pause(|| {
            with_app(|a| a.toggle_play());
        });
        t.on_next(|| {
            with_app(|a| a.next(true));
        });
        t.on_previous(|| {
            with_app(|a| a.previous());
        });
        t.on_set_volume(|v| {
            with_app(|a| a.set_volume(v));
        });
        t.on_toggle_mute(|| {
            with_app(|a| a.toggle_mute());
        });
        t.on_show_app(|| {
            with_app(|a| {
                a.hide_popup();
                a.show_window();
            });
        });
        a.sync_volume();
        use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
        a.popup.window().on_winit_window_event(|_, ev| {
            if let WindowEvent::Focused(false) = ev {
                // Defer: we may be inside a borrow of the app state.
                slint::Timer::single_shot(Duration::from_millis(10), || {
                    with_app(|a| a.hide_popup());
                });
            }
            EventResult::Propagate
        });
    });
}

fn wire_callbacks(window: &AppWindow) {
    let b = window.global::<Backend>();

    b.on_navigate(|page| {
        with_app(|a| a.show_page(page));
    });
    b.on_toggle_favorite(|list, i| {
        with_app(|a| a.toggle_favorite(&list, i));
    });
    b.on_toggle_favorite_current(|| {
        with_app(|a| a.toggle_favorite_current());
    });
    b.on_fx_set(|name, v| {
        with_app(|a| a.fx_set(&name, v));
    });
    b.on_fx_preset(|name| {
        with_app(|a| a.fx_preset(&name));
    });
    b.on_fx_reset(|| {
        with_app(|a| a.fx_reset());
    });
    b.on_set_crossfade_curve(|c| {
        with_app(|a| {
            a.backend().set_crossfade_curve(c);
            a.db.lock().unwrap().set_setting("crossfade_curve", &c.to_string());
        });
    });
    b.on_set_crossfade_on_skip(|on| {
        with_app(|a| {
            a.backend().set_crossfade_on_skip(on);
            a.db.lock().unwrap().set_setting_bool("crossfade_on_skip", on);
        });
    });
    b.on_set_resume(|on| {
        with_app(|a| {
            a.backend().set_resume_enabled(on);
            a.db.lock().unwrap().set_setting_bool("resume_enabled", on);
            if on {
                a.save_resume();
            }
        });
    });
    b.on_set_pause_fade(|on| {
        with_app(|a| {
            a.backend().set_pause_fade(on);
            a.db.lock().unwrap().set_setting_bool("pause_fade", on);
        });
    });
    b.on_set_sections_enabled(|on| {
        with_app(|a| {
            a.backend().set_sections_enabled(on);
            a.db.lock().unwrap().set_setting_bool("sections", on);
        });
    });
    b.on_edit_section_from_position(|start| {
        with_app(|a| a.section_from_position(start));
    });
    b.on_set_visualizer(|on| {
        with_app(|a| a.set_visualizer(on));
    });
    b.on_set_normalize(|on| {
        with_app(|a| a.set_normalize(on));
    });
    b.on_set_lyrics_enabled(|on| {
        with_app(|a| a.set_lyrics_enabled(on));
    });
    b.on_refresh_lyrics(|| {
        with_app(|a| a.refresh_lyrics());
    });
    b.on_seek_to_line(|i| {
        with_app(|a| a.seek_to_lyric(i));
    });
    b.on_set_splash(|on| {
        with_app(|a| {
            a.backend().set_splash_enabled(on);
            a.db.lock().unwrap().set_setting_bool("splash", on);
        });
    });
    b.on_set_low_end(|on| {
        with_app(|a| a.set_low_end(on));
    });
    b.on_set_fs_background(|m| {
        with_app(|a| a.set_fs_background(m));
    });
    b.on_set_visual_style(|st| {
        with_app(|a| a.set_visual_style(st));
    });
    b.on_set_cache_limit(|mb| {
        with_app(|a| {
            a.backend().set_cache_limit_mb(mb);
            a.db.lock().unwrap().set_setting("cache_limit_mb", &mb.to_string());
            a.trim_cache();
        });
    });
    b.on_set_accent(|hex| {
        with_app(|a| a.set_accent(&hex));
    });
    b.on_set_corner_style(|c| {
        with_app(|a| a.set_corner_style(c));
    });
    b.on_set_density(|d| {
        with_app(|a| a.set_density(d));
    });
    b.on_set_ui_scale(|p| {
        with_app(|a| a.set_ui_scale(p));
    });
    b.on_set_font_family(|| {
        with_app(|a| a.set_font_family());
    });
    b.on_set_ui_flag(|name, on| {
        with_app(|a| a.set_ui_flag(&name, on));
    });
    b.on_theme_select(|name| {
        with_app(|a| a.theme_select(&name));
    });
    b.on_theme_import(|| {
        with_app(|a| a.theme_import());
    });
    b.on_theme_export(|| {
        with_app(|a| a.theme_export());
    });
    b.on_theme_delete(|| {
        with_app(|a| a.theme_delete());
    });
    party_app::wire(&b);
    social_app::wire(&b);
    b.on_reset_design(|| {
        with_app(|a| a.reset_design());
    });
    b.on_share_playlist(|| {
        with_app(|a| a.share_playlist());
    });
    b.on_copy_share_code(|| {
        with_app(|a| a.copy_share_code());
    });
    b.on_import_playlist(|code| with_app(|a| a.import_playlist(&code)).unwrap_or(false));
    b.on_open_legal(|name| {
        with_app(|a| a.open_legal(&name));
    });
    b.on_make_default_player(|| {
        with_app(|a| a.make_default_player());
    });
    b.on_unregister_player(|| {
        with_app(|a| a.unregister_player());
    });
    b.on_refresh_default_status(|| {
        with_app(|a| a.refresh_default_status());
    });
    b.on_set_update_auto(|on| {
        with_app(|a| a.set_update_auto(on));
    });
    b.on_check_updates(|| {
        with_app(|a| a.check_updates(true));
    });
    b.on_install_update(|| {
        with_app(|a| a.install_update());
    });
    b.on_open_release_page(|| {
        with_app(|a| a.open_release_page());
    });
    b.on_go_back(|| {
        with_app(|a| a.nav_go(-1));
    });
    b.on_go_forward(|| {
        with_app(|a| a.nav_go(1));
    });
    b.on_play_pause(|| {
        with_app(|a| a.toggle_play());
    });
    b.on_next(|| {
        with_app(|a| a.next(true));
    });
    b.on_previous(|| {
        with_app(|a| a.previous());
    });
    b.on_seek(|v| {
        with_app(|a| a.seek(v));
    });
    b.on_set_volume(|v| {
        with_app(|a| a.set_volume(v));
    });
    b.on_toggle_mute(|| {
        with_app(|a| a.toggle_mute());
    });
    b.on_toggle_shuffle(|| {
        with_app(|a| {
            if a.party_intercept(party_app::Intercept::QueueEdit) {
                return;
            }
            let on = !a.queue.shuffle;
            a.queue.set_shuffle(on);
            a.backend().set_shuffle(on);
            a.db.lock().unwrap().set_setting_bool("shuffle", on);
            a.refresh_queue();
        });
    });
    b.on_cycle_repeat(|| {
        with_app(|a| {
            if a.party_intercept(party_app::Intercept::QueueEdit) {
                return;
            }
            a.queue.repeat = a.queue.repeat.next();
            a.backend().set_repeat_mode(a.queue.repeat.as_int());
            a.db.lock().unwrap().set_setting("repeat", &a.queue.repeat.as_int().to_string());
        });
    });

    b.on_play_from(|list, i| {
        with_app(|a| a.play_from(&list, i));
    });
    b.on_queue_add(|list, i| {
        with_app(|a| a.queue_add(&list, i, false));
    });
    b.on_queue_play_next(|list, i| {
        with_app(|a| a.queue_add(&list, i, true));
    });
    b.on_add_to_playlist(|list, i, pid| {
        with_app(|a| a.add_to_playlist(&list, i, pid as i64));
    });
    b.on_new_playlist_with(|list, i, name| {
        with_app(|a| {
            let id = a.db.lock().unwrap().create_playlist(name.trim());
            match id {
                Ok(id) => a.add_to_playlist(&list, i, id),
                Err(e) => a.toast(format!("Error: {e}")),
            }
        });
    });
    b.on_play_all(|list, shuffle| {
        with_app(|a| a.play_all(&list, shuffle));
    });

    b.on_queue_remove(|i| {
        with_app(|a| {
            let i = i as usize;
            if a.party_intercept(party_app::Intercept::QueueRemove(i)) {
                return;
            }
            let was_current = a.queue.index == Some(i);
            a.queue.remove(i);
            if was_current {
                if a.queue.current().is_some() { a.start_current() } else { a.stop() }
            }
            a.refresh_queue();
        });
    });
    b.on_queue_move(|from, to| {
        with_app(|a| {
            if a.party_intercept(party_app::Intercept::QueueMove(from as usize, to as usize)) {
                a.refresh_queue();
                return;
            }
            a.queue.move_item(from as usize, to as usize);
            a.refresh_queue();
        });
    });
    b.on_queue_clear(|| {
        with_app(|a| {
            if a.party_intercept(party_app::Intercept::QueueEdit) {
                return;
            }
            a.queue.clear_upcoming();
            a.refresh_queue();
        });
    });
    b.on_playlist_remove(|i| {
        with_app(|a| {
            let mut ids = a.current_playlist_ids();
            if (i as usize) < ids.len() {
                ids.remove(i as usize);
                a.write_playlist_order(ids);
            }
        });
    });
    b.on_playlist_move(|from, to| {
        with_app(|a| {
            let mut ids = a.current_playlist_ids();
            let (from, to) = (from as usize, to as usize);
            if from < ids.len() && to < ids.len() {
                let id = ids.remove(from);
                ids.insert(to, id);
                a.write_playlist_order(ids);
            }
        });
    });
    b.on_open_playlist(|id| {
        with_app(|a| a.open_playlist(id as i64));
    });
    b.on_open_auto(|key| {
        with_app(|a| a.open_auto(&key));
    });
    b.on_create_playlist(|name| {
        with_app(|a| {
            let res = a.db.lock().unwrap().create_playlist(name.trim());
            match res {
                Ok(id) => {
                    a.refresh_playlists();
                    a.open_playlist(id);
                }
                Err(e) => a.toast(format!("Error: {e}")),
            }
        });
    });
    b.on_edit_track_begin(|list, index| {
        with_app(|a| {
            let Some(mut t) = a.track_at(&list, index) else { return };
            a.ensure_id(&mut t);
            let b = a.backend();
            b.set_edit_title(s(&t.title));
            b.set_edit_artist(s(&t.artist));
            b.set_edit_album(s(&t.album));
            a.section_fields_for(&t);
            a.set_edit_cover_preview(t.cover.as_deref());
            a.edit_cover = None;
            a.edit = EditTarget::Track(t);
        });
    });
    b.on_edit_playlist_begin(|| {
        with_app(|a| {
            let PlaylistView::User(id) = a.view else { return };
            let Some(p) = a.db.lock().unwrap().playlist(id) else { return };
            let b = a.backend();
            b.set_edit_title(s(&p.name));
            b.set_edit_description(s(&p.description));
            a.set_edit_cover_preview(p.cover.as_deref());
            a.edit_cover = None;
            a.edit = EditTarget::Playlist(id);
        });
    });
    b.on_edit_pick_cover(|| {
        with_app(|a| {
            let Some(file) = rfd::FileDialog::new()
                .set_title("Choose artwork")
                .add_filter("Images", &["jpg", "jpeg", "png", "webp", "bmp"])
                .pick_file()
            else {
                return;
            };
            match std::fs::read(&file) {
                Ok(bytes) => {
                    // Stored in the covers folder so it is part of every export.
                    let key = format!("user:{}:{}", file.display(), chrono::Utc::now().timestamp_millis());
                    match connectors::save_cover(&key, &bytes) {
                        Some(path) => {
                            a.set_edit_cover_preview(Some(&path));
                            a.edit_cover = Some(Some(path));
                        }
                        None => a.toast("That image could not be read"),
                    }
                }
                Err(e) => a.toast(format!("Can't open image: {e}")),
            }
        });
    });
    b.on_edit_clear_cover(|| {
        with_app(|a| {
            a.set_edit_cover_preview(None);
            a.edit_cover = Some(None);
        });
    });
    b.on_edit_track_save(|| {
        with_app(|a| {
            let EditTarget::Track(mut t) = std::mem::replace(&mut a.edit, EditTarget::None) else { return };
            let b = a.backend();
            t.title = b.get_edit_title().trim().to_string();
            t.artist = b.get_edit_artist().trim().to_string();
            t.album = b.get_edit_album().trim().to_string();
            if let Some(cover) = a.edit_cover.take() {
                t.cover = cover;
            }
            let res = a.db.lock().unwrap().edit_track(t.id, &t.title, &t.artist, &t.album, t.cover.as_deref());
            if let Err(e) = res {
                a.toast(format!("Error: {e}"));
                return;
            }
            let (start, end) = a.save_section(t.id);
            t.start_ms = start;
            t.end_ms = end;
            let token = if a.queue.current().map(|c| c.source == t.source && c.uri == t.uri).unwrap_or(false) {
                a.play_token
            } else {
                u64::MAX
            };
            a.on_track_updated(token, t);
            a.reload_playlist_view();
            a.refresh_home();
            a.toast("Track info saved");
        });
    });
    b.on_edit_playlist_save(|| {
        with_app(|a| {
            let EditTarget::Playlist(id) = std::mem::replace(&mut a.edit, EditTarget::None) else { return };
            let b = a.backend();
            let name = b.get_edit_title().trim().to_string();
            let description = b.get_edit_description().trim().to_string();
            let cover = match a.edit_cover.take() {
                Some(c) => c,
                None => a.db.lock().unwrap().playlist(id).and_then(|p| p.cover),
            };
            let res = a.db.lock().unwrap().set_playlist_details(id, &name, &description, cover.as_deref());
            if let Err(e) = res {
                a.toast(format!("Error: {e}"));
            }
            a.refresh_playlists();
            a.reload_playlist_view();
        });
    });
    b.on_set_auto_playlists(|on| {
        with_app(|a| {
            a.backend().set_auto_playlists_enabled(on);
            a.db.lock().unwrap().set_setting_bool("auto_playlists", on);
            a.refresh_playlists();
            if !on && matches!(a.view, PlaylistView::Auto(_)) && a.backend().get_page() == 3 {
                a.view = PlaylistView::None;
                a.show_page(0);
            }
        });
    });
    b.on_set_preload(|on| {
        with_app(|a| {
            a.backend().set_preload_enabled(on);
            a.db.lock().unwrap().set_setting_bool("preload", on);
            if on {
                a.prefetch_next();
            }
        });
    });
    b.on_set_crossfade(|on| {
        with_app(|a| {
            a.backend().set_crossfade_enabled(on);
            a.db.lock().unwrap().set_setting_bool("crossfade", on);
        });
    });
    b.on_set_crossfade_secs(|secs| {
        with_app(|a| {
            let secs = secs.clamp(0.5, 5.0);
            a.backend().set_crossfade_secs(secs);
            a.db.lock().unwrap().set_setting("crossfade_secs", &format!("{secs:.1}"));
        });
    });
    b.on_delete_playlist(|| {
        with_app(|a| {
            if let PlaylistView::User(id) = a.view {
                let _ = a.db.lock().unwrap().delete_playlist(id);
                a.view = PlaylistView::None;
                a.refresh_playlists();
                a.show_page(0);
                a.refresh_home();
                a.toast("Playlist deleted");
            }
        });
    });
    b.on_save_auto_as_playlist(|| {
        with_app(|a| {
            let name = format!("{} · {}", a.backend().get_playlist_name(), chrono::Local::now().format("%b %Y"));
            let ids = a.current_playlist_ids();
            let res = {
                let db = a.db.lock().unwrap();
                db.create_playlist(&name).and_then(|id| db.add_to_playlist(id, &ids).map(|_| id))
            };
            match res {
                Ok(id) => {
                    a.refresh_playlists();
                    a.open_playlist(id);
                    a.toast(format!("Saved as “{name}”"));
                }
                Err(e) => a.toast(format!("Error: {e}")),
            }
        });
    });

    b.on_search(|q| {
        with_app(|a| a.search(&q));
    });
    b.on_library_changed(|| {
        with_app(|a| a.refresh_library());
    });

    // Dragging a band switches to the "Custom" preset and turns the EQ on.
    b.on_eq_set(|band, gain| {
        with_app(|a| {
            if let Some(audio) = &a.audio {
                audio.eq.set_gain(band as usize, gain);
                if !audio.eq.enabled() {
                    audio.eq.set_enabled(true);
                }
            }
            let b = a.backend();
            b.set_eq_enabled(true);
            b.set_eq_preset_index(0);
            a.push_eq_gains();
        });
    });
    b.on_eq_commit(|| {
        with_app(|a| a.save_eq());
    });
    b.on_eq_toggle(|on| {
        with_app(|a| {
            if let Some(audio) = &a.audio {
                audio.eq.set_enabled(on);
            }
            a.backend().set_eq_enabled(on);
            a.save_eq();
        });
    });
    b.on_eq_apply_preset(|name| {
        with_app(|a| {
            let gains: Vec<f32> = if name == "Custom" {
                // Restore the user's own curve.
                a.db.lock()
                    .unwrap()
                    .setting_or("eq_custom_gains", "")
                    .split(',')
                    .filter_map(|g| g.trim().parse().ok())
                    .collect()
            } else {
                match eq::PRESETS.iter().find(|(n, _)| *n == name.as_str()) {
                    Some((_, g)) => g.to_vec(),
                    None => return,
                }
            };
            if let Some(audio) = &a.audio {
                for i in 0..10 {
                    audio.eq.set_gain(i, gains.get(i).copied().unwrap_or(0.0));
                }
                audio.eq.set_enabled(true);
            }
            let b = a.backend();
            b.set_eq_enabled(true);
            b.set_eq_preset_index(eq_preset_index(&name));
            a.push_eq_gains();
            a.save_eq();
        });
    });

    b.on_set_theme(|mode| {
        with_app(|a| {
            a.backend().set_theme_mode(mode);
            a.db.lock().unwrap().set_setting("theme", &mode.to_string());
            a.apply_theme();
        });
    });
    b.on_set_close_to_tray(|on| {
        with_app(|a| {
            a.backend().set_close_to_tray(on);
            a.db.lock().unwrap().set_setting_bool("close_to_tray", on);
        });
    });
    b.on_set_connector(|name, on| {
        with_app(|a| {
            let b = a.backend();
            let key = match name.as_str() {
                "local" => {
                    b.set_local_enabled(on);
                    "local_enabled"
                }
                "nextcloud" => {
                    b.set_nc_enabled(on);
                    "nc_enabled"
                }
                "youtube" => {
                    b.set_yt_enabled(on);
                    if on {
                        a.check_ytdlp();
                    }
                    "yt_enabled"
                }
                "soundcloud" => {
                    b.set_sc_enabled(on);
                    if on {
                        a.check_ytdlp();
                    }
                    "sc_enabled"
                }
                "spotify" => {
                    b.set_spotify_enabled(on);
                    "spotify_enabled"
                }
                _ => return,
            };
            a.db.lock().unwrap().set_setting_bool(key, on);
            a.backend().set_library_source(0);
            a.refresh_library();
            a.refresh_home();
        });
    });
    b.on_add_local_folder(|| {
        // Show the dialog outside of the app borrow: it runs a nested event loop.
        let Some(dir) = rfd::FileDialog::new().set_title("Add music folder").pick_folder() else { return };
        with_app(|a| {
            let mut folders = a.local_folders();
            let d = dir.to_string_lossy().into_owned();
            if !folders.contains(&d) {
                folders.push(d);
                a.save_local_folders(&folders);
                a.rescan_local(false);
            }
        });
    });
    b.on_remove_local_folder(|i| {
        with_app(|a| {
            let mut folders = a.local_folders();
            if (i as usize) < folders.len() {
                folders.remove(i as usize);
                a.save_local_folders(&folders);
                a.rescan_local(true);
            }
        });
    });
    b.on_rescan_local(|| {
        with_app(|a| a.rescan_local(false));
    });
    b.on_save_nextcloud(|| {
        with_app(|a| {
            let cfg = a.nc_config();
            {
                let db = a.db.lock().unwrap();
                db.set_setting("nc_url", cfg.url.trim());
                db.set_setting("nc_user", cfg.user.trim());
                db.set_setting("nc_password", &cfg.password);
                db.set_setting("nc_folder", cfg.folder.trim());
            }
            a.toast("Nextcloud settings saved");
        });
    });
    b.on_test_nextcloud(|| {
        with_app(|a| {
            let cfg = a.nc_config();
            if !cfg.is_complete() {
                a.toast("Fill in server URL, username and app password first");
                return;
            }
            a.toast("Connecting…");
            std::thread::spawn(move || {
                let res = nextcloud::test(&cfg);
                post(move |a| match res {
                    Ok(n) => a.toast(format!("Connected ✓ — {n} items in the music folder")),
                    Err(e) => a.toast(format!("Nextcloud: {e:#}")),
                });
            });
        });
    });
    b.on_scan_nextcloud(|| {
        with_app(|a| a.scan_nextcloud());
    });
    b.on_save_ytdlp(|| {
        with_app(|a| {
            let p = a.backend().get_ytdlp_path().to_string();
            a.db.lock().unwrap().set_setting("ytdlp_path", p.trim());
            a.check_ytdlp();
        });
    });
    b.on_open_ytdlp_folder(|| {
        with_app(|a| a.open_ytdlp_folder());
    });
    b.on_save_spotify(|| {
        with_app(|a| {
            let b = a.backend();
            {
                let db = a.db.lock().unwrap();
                db.set_setting("sp_client_id", b.get_sp_client_id().trim());
                db.set_setting("sp_client_secret", b.get_sp_client_secret().trim());
            }
            a.toast("Spotify settings saved");
        });
    });
    // Fullscreen buttons act on the playing track's artist.
    b.on_fetch_artist_art(|| {
        with_app(|a| {
            if let Some(name) = a.current_primary_artist() {
                a.ensure_artist_info(&name, true, true);
            }
        });
    });
    b.on_pick_artist_art(|| {
        with_app(|a| {
            if let Some(name) = a.current_primary_artist() {
                a.pick_artist_image(Some(name));
            }
        });
    });
    b.on_clear_artist_art(|| {
        with_app(|a| {
            if let Some(name) = a.current_primary_artist() {
                a.clear_artist_image(Some(name));
            }
        });
    });
    b.on_open_artist(|name| {
        with_app(|a| a.open_artist(&name));
    });
    b.on_artist_refresh(|| {
        with_app(|a| {
            if let Some(name) = a.artist_view.clone() {
                a.ensure_artist_info(&name, true, true);
            }
        });
    });
    b.on_artist_pick_image(|| {
        with_app(|a| a.pick_artist_image(None));
    });
    b.on_artist_clear_image(|| {
        with_app(|a| a.clear_artist_image(None));
    });
    b.on_artist_edit_begin(|| {
        with_app(|a| a.artist_edit_begin());
    });
    b.on_artist_edit_save(|| {
        with_app(|a| a.artist_edit_save());
    });
    b.on_play_album(|i| {
        with_app(|a| a.play_album(i.max(0) as usize));
    });
    b.on_open_url(|url| {
        with_app(|a| a.open_url(&url));
    });
    b.on_match_begin_artist(|| {
        with_app(|a| a.match_begin_artist());
    });
    b.on_match_begin_track(|| {
        with_app(|a| a.match_begin_track());
    });
    b.on_match_search(|| {
        with_app(|a| a.match_search());
    });
    b.on_match_pick(|i| {
        with_app(|a| a.match_pick(i.max(0) as usize));
    });
    b.on_set_spotify_auto(|on| {
        with_app(|a| {
            a.backend().set_spotify_auto(on);
            a.db.lock().unwrap().set_setting_bool("spotify_auto", on);
        });
    });
    b.on_fill_missing_artwork(|| {
        with_app(|a| a.fill_missing_artwork());
    });
    b.on_update_all_artists(|| {
        with_app(|a| a.update_all_artists());
    });
    b.on_export_data(|| {
        with_app(|a| a.export_data());
    });
    b.on_import_data(|| {
        with_app(|a| a.import_data());
    });
    b.on_remap_paths(|from, to| {
        with_app(|a| {
            let res = a.db.lock().unwrap().remap_paths(from.trim(), to.trim());
            match res {
                Ok(n) => {
                    a.toast(format!("Re-mapped {n} tracks"));
                    a.load_settings();
                    a.refresh_library();
                }
                Err(e) => a.toast(format!("Error: {e}")),
            }
        });
    });
    b.on_open_data_dir(|| {
        with_app(|a| a.open_data_dir());
    });
    b.on_prepare_clear_cache(|| {
        with_app(|a| a.prepare_clear_cache());
    });
    b.on_clear_cache(|include_offline| {
        with_app(|a| a.clear_cache(include_offline));
    });
    b.on_set_offline_all(|on| {
        with_app(|a| a.set_offline_all(on));
    });
    b.on_set_playlist_offline(|on| {
        with_app(|a| {
            if let PlaylistView::User(id) = a.view {
                a.set_playlist_offline(id, on);
            }
        });
    });
    b.on_toggle_fullscreen(|| {
        with_app(|a| {
            let b = a.backend();
            let on = !b.get_fullscreen();
            b.set_fullscreen(on);
            a.window.window().set_fullscreen(on);
            if on {
                a.update_fullscreen_art();
            }
            a.update_visual_timer();
        });
    });
}
