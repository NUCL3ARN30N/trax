//! System tray icon with a native menu; left click opens the mini player popup.

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum TrayAction {
    TogglePopup { x: f64, y: f64, w: f64, h: f64 },
    ShowApp,
    PlayPause,
    Next,
    Previous,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    now: MenuItem,
    play_pause: MenuItem,
    ids: Vec<(tray_icon::menu::MenuId, fn() -> TrayAction)>,
}

/// On Windows the icon comes from the exe's multi-size .ico resource, so the shell gets a
/// frame drawn for the exact tray size instead of a downscaled image.
fn load_icon(scale: f64) -> Result<tray_icon::Icon> {
    #[cfg(windows)]
    {
        let px = (16.0 * scale).round().max(16.0) as u32;
        if let Ok(icon) = tray_icon::Icon::from_resource(1, Some((px, px))) {
            return Ok(icon);
        }
    }
    let _ = scale;
    let img = image::load_from_memory(include_bytes!("../assets/logo/AppIcon.iconset/icon_32x32@2x.png"))?.into_rgba8();
    let (w, h) = img.dimensions();
    Ok(tray_icon::Icon::from_rgba(img.into_raw(), w, h)?)
}

impl Tray {
    /// Create the tray icon. `handler` is invoked on the UI thread for every action.
    pub fn new(handler: fn(TrayAction), scale: f64) -> Result<Tray> {
        let now = MenuItem::new("Nothing playing", false, None);
        let play_pause = MenuItem::new("Play", true, None);
        let next = MenuItem::new("Next", true, None);
        let prev = MenuItem::new("Previous", true, None);
        let show = MenuItem::new("Show trax", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &now,
            &PredefinedMenuItem::separator(),
            &play_pause,
            &next,
            &prev,
            &PredefinedMenuItem::separator(),
            &show,
            &quit,
        ])?;

        let icon = TrayIconBuilder::new()
            .with_icon(load_icon(scale)?)
            .with_tooltip("trax")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        let ids: Vec<(tray_icon::menu::MenuId, fn() -> TrayAction)> = vec![
            (play_pause.id().clone(), || TrayAction::PlayPause),
            (next.id().clone(), || TrayAction::Next),
            (prev.id().clone(), || TrayAction::Previous),
            (show.id().clone(), || TrayAction::ShowApp),
            (quit.id().clone(), || TrayAction::Quit),
        ];
        let menu_ids = ids.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if let Some((_, make)) = menu_ids.iter().find(|(id, _)| *id == e.id) {
                let make = *make;
                let _ = slint::invoke_from_event_loop(move || handler(make()));
            }
        }));

        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
            TrayIconEvent::Click { rect, button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => {
                let (x, y) = (rect.position.x, rect.position.y);
                let (w, h) = (rect.size.width as f64, rect.size.height as f64);
                let _ = slint::invoke_from_event_loop(move || handler(TrayAction::TogglePopup { x, y, w, h }));
            }
            TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => {
                let _ = slint::invoke_from_event_loop(move || handler(TrayAction::ShowApp));
            }
            _ => {}
        }));

        Ok(Tray { icon, now, play_pause, ids })
    }

    pub fn update(&self, title: &str, artist: &str, playing: bool) {
        let label = if artist.is_empty() { title.to_string() } else { format!("{title} — {artist}") };
        let short: String = label.chars().take(60).collect();
        self.now.set_text(&short);
        self.play_pause.set_text(if playing { "Pause" } else { "Play" });
        let tip: String = format!("trax\n{label}").chars().take(127).collect();
        let _ = self.icon.set_tooltip(Some(tip));
        let _ = &self.ids;
    }
}
