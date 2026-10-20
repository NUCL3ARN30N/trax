//! Appearance: colour palettes (from an accent colour or a custom JSON theme), corners,
//! density, font, interface scale and layout toggles.

use super::{App, s};
use crate::{AccentItem, Theme};
use serde::{Deserialize, Serialize};
use slint::{Color, ComponentHandle, ModelRc, VecModel};
use std::collections::BTreeMap;

pub(super) const DEFAULT_ACCENT: &str = "#6750a4";

const ACCENTS: &[&str] = &["#6750a4", "#0061a4", "#006a60", "#386a20", "#7d5800", "#9c4146", "#b4005f", "#5b5d72"];

/// Colour roles that a theme can set (the Slint `Theme` property names).
pub(super) const ROLES: &[&str] = &[
    "surface",
    "surface-dim",
    "surface-container-lowest",
    "surface-container-low",
    "surface-container",
    "surface-container-high",
    "surface-container-highest",
    "on-surface",
    "on-surface-variant",
    "outline",
    "outline-variant",
    "primary",
    "on-primary",
    "primary-container",
    "on-primary-container",
    "secondary-container",
    "on-secondary-container",
    "tertiary",
    "tertiary-container",
    "on-tertiary-container",
    "error",
    "inverse-surface",
    "inverse-on-surface",
];

/// A shareable theme file.
#[derive(Serialize, Deserialize, Clone, Default)]
pub(super) struct ThemeDef {
    pub name: String,
    /// "system", "light" or "dark".
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    /// 0 square, 1 subtle, 2 default, 3 round.
    #[serde(default)]
    pub corners: Option<i32>,
    #[serde(default)]
    pub font: Option<String>,
    /// Colour overrides per mode: {"dark": {"primary": "#ffb4ab", ...}, "light": {...}}.
    #[serde(default)]
    pub colors: BTreeMap<String, BTreeMap<String, String>>,
}

/// The user's own accent, corners, font and mode, restored when switching back to "Default".
#[derive(Serialize, Deserialize)]
struct OwnLook {
    accent: String,
    corner_style: String,
    font_family: String,
    theme: String,
}

/// The settings a theme changes. Values left at their neutral default (no font, "system"
/// mode, default corners) keep the user's own choice instead of resetting it.
fn theme_settings(def: &ThemeDef) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(a) = def.accent.as_deref().and_then(parse_hex) {
        out.push(("accent", hex(a)));
    }
    if let Some(c) = def.corners.filter(|c| *c != 2) {
        out.push(("corner_style", c.clamp(0, 3).to_string()));
    }
    if let Some(f) = def.font.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        out.push(("font_family", f.to_string()));
    }
    match def.mode.as_deref() {
        Some("light") => out.push(("theme", "1".into())),
        Some("dark") => out.push(("theme", "2".into())),
        _ => {}
    }
    out
}

pub(super) fn parse_hex(text: &str) -> Option<Color> {
    let t = text.trim().trim_start_matches('#');
    let v = u32::from_str_radix(t, 16).ok()?;
    match t.len() {
        6 => Some(Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8)),
        8 => Some(Color::from_argb_u8(v as u8, (v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8)),
        _ => None,
    }
}

fn hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

fn to_hsl(c: Color) -> (f32, f32, f32) {
    let (r, g, b) = (c.red() as f32 / 255.0, c.green() as f32 / 255.0, c.blue() as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Color {
    let h = h.rem_euclid(360.0) / 360.0;
    let f = |t: f32| {
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        let t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let u = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::from_rgb_u8(u(f(h + 1.0 / 3.0)), u(f(h)), u(f(h - 1.0 / 3.0)))
}

/// Material-style palette from an accent colour: primary / secondary / tertiary tones of the
/// accent hue and neutral surfaces tinted towards it.
pub(super) fn palette(dark: bool, accent: Color) -> BTreeMap<&'static str, Color> {
    let (h, s, _) = to_hsl(accent);
    let s = s.max(0.25);
    let tone = |sat: f32, l: f32| from_hsl(h, sat, l);
    let tert = |l: f32| from_hsl(h + 60.0, (s * 0.6).max(0.2), l);
    let n = (s * 0.12).min(0.08); // neutral tint
    let mut m = BTreeMap::new();
    if dark {
        m.insert("surface", tone(n, 0.075));
        m.insert("surface-dim", tone(n, 0.075));
        m.insert("surface-container-lowest", tone(n, 0.055));
        m.insert("surface-container-low", tone(n, 0.11));
        m.insert("surface-container", tone(n, 0.135));
        m.insert("surface-container-high", tone(n, 0.175));
        m.insert("surface-container-highest", tone(n, 0.22));
        m.insert("on-surface", tone(n, 0.9));
        m.insert("on-surface-variant", tone(n * 1.5, 0.79));
        m.insert("outline", tone(n * 1.5, 0.58));
        m.insert("outline-variant", tone(n * 1.5, 0.3));
        m.insert("primary", tone(s, 0.8));
        m.insert("on-primary", tone(s, 0.2));
        m.insert("primary-container", tone(s * 0.85, 0.36));
        m.insert("on-primary-container", tone(s, 0.9));
        m.insert("secondary-container", tone(s * 0.25, 0.3));
        m.insert("on-secondary-container", tone(s * 0.3, 0.9));
        m.insert("tertiary", tert(0.82));
        m.insert("tertiary-container", tert(0.32));
        m.insert("on-tertiary-container", tert(0.9));
        m.insert("error", Color::from_rgb_u8(0xf2, 0xb8, 0xb5));
        m.insert("inverse-surface", tone(n, 0.9));
        m.insert("inverse-on-surface", tone(n, 0.2));
    } else {
        m.insert("surface", tone(n * 2.0, 0.985));
        m.insert("surface-dim", tone(n, 0.86));
        m.insert("surface-container-lowest", Color::from_rgb_u8(255, 255, 255));
        m.insert("surface-container-low", tone(n * 2.0, 0.96));
        m.insert("surface-container", tone(n * 2.0, 0.945));
        m.insert("surface-container-high", tone(n * 2.0, 0.925));
        m.insert("surface-container-highest", tone(n * 2.0, 0.9));
        m.insert("on-surface", tone(n, 0.11));
        m.insert("on-surface-variant", tone(n * 1.5, 0.3));
        m.insert("outline", tone(n * 1.5, 0.47));
        m.insert("outline-variant", tone(n * 1.5, 0.79));
        m.insert("primary", tone(s, 0.4));
        m.insert("on-primary", Color::from_rgb_u8(255, 255, 255));
        m.insert("primary-container", tone(s, 0.9));
        m.insert("on-primary-container", tone(s, 0.15));
        m.insert("secondary-container", tone(s * 0.45, 0.91));
        m.insert("on-secondary-container", tone(s * 0.3, 0.13));
        m.insert("tertiary", tert(0.4));
        m.insert("tertiary-container", tert(0.91));
        m.insert("on-tertiary-container", tert(0.12));
        m.insert("error", Color::from_rgb_u8(0xb3, 0x26, 0x1e));
        m.insert("inverse-surface", tone(n, 0.2));
        m.insert("inverse-on-surface", tone(n * 2.0, 0.95));
    }
    m
}

/// Material 3 baseline (the default purple scheme), so "Default" looks exactly as designed.
fn baseline(dark: bool) -> BTreeMap<&'static str, Color> {
    let pairs: &[(&str, &str, &str)] = &[
        ("surface", "#141218", "#fef7ff"),
        ("surface-dim", "#141218", "#ded8e1"),
        ("surface-container-lowest", "#0f0d13", "#ffffff"),
        ("surface-container-low", "#1d1b20", "#f7f2fa"),
        ("surface-container", "#211f26", "#f3edf7"),
        ("surface-container-high", "#2b2930", "#ece6f0"),
        ("surface-container-highest", "#36343b", "#e6e0e9"),
        ("on-surface", "#e6e0e9", "#1d1b20"),
        ("on-surface-variant", "#cac4d0", "#49454f"),
        ("outline", "#938f99", "#79747e"),
        ("outline-variant", "#49454f", "#cac4d0"),
        ("primary", "#d0bcff", "#6750a4"),
        ("on-primary", "#381e72", "#ffffff"),
        ("primary-container", "#4f378b", "#eaddff"),
        ("on-primary-container", "#eaddff", "#21005d"),
        ("secondary-container", "#4a4458", "#e8def8"),
        ("on-secondary-container", "#e8def8", "#1d192b"),
        ("tertiary", "#efb8c8", "#7d5260"),
        ("tertiary-container", "#633b48", "#ffd8e4"),
        ("on-tertiary-container", "#ffd8e4", "#31111d"),
        ("error", "#f2b8b5", "#b3261e"),
        ("inverse-surface", "#e6e0e9", "#322f35"),
        ("inverse-on-surface", "#322f35", "#f5eff7"),
    ];
    pairs.iter().map(|(k, d, l)| (*k, parse_hex(if dark { d } else { l }).unwrap())).collect()
}

fn set_role(t: &Theme, role: &str, c: Color) {
    match role {
        "surface" => t.set_surface(c),
        "surface-dim" => t.set_surface_dim(c),
        "surface-container-lowest" => t.set_surface_container_lowest(c),
        "surface-container-low" => t.set_surface_container_low(c),
        "surface-container" => t.set_surface_container(c),
        "surface-container-high" => t.set_surface_container_high(c),
        "surface-container-highest" => t.set_surface_container_highest(c),
        "on-surface" => t.set_on_surface(c),
        "on-surface-variant" => t.set_on_surface_variant(c),
        "outline" => t.set_outline(c),
        "outline-variant" => t.set_outline_variant(c),
        "primary" => t.set_primary(c),
        "on-primary" => t.set_on_primary(c),
        "primary-container" => t.set_primary_container(c),
        "on-primary-container" => t.set_on_primary_container(c),
        "secondary-container" => t.set_secondary_container(c),
        "on-secondary-container" => t.set_on_secondary_container(c),
        "tertiary" => t.set_tertiary(c),
        "tertiary-container" => t.set_tertiary_container(c),
        "on-tertiary-container" => t.set_on_tertiary_container(c),
        "error" => t.set_error(c),
        "inverse-surface" => t.set_inverse_surface(c),
        "inverse-on-surface" => t.set_inverse_on_surface(c),
        _ => {}
    }
}

fn get_role(t: &Theme, role: &str) -> Color {
    match role {
        "surface" => t.get_surface(),
        "surface-dim" => t.get_surface_dim(),
        "surface-container-lowest" => t.get_surface_container_lowest(),
        "surface-container-low" => t.get_surface_container_low(),
        "surface-container" => t.get_surface_container(),
        "surface-container-high" => t.get_surface_container_high(),
        "surface-container-highest" => t.get_surface_container_highest(),
        "on-surface" => t.get_on_surface(),
        "on-surface-variant" => t.get_on_surface_variant(),
        "outline" => t.get_outline(),
        "outline-variant" => t.get_outline_variant(),
        "primary" => t.get_primary(),
        "on-primary" => t.get_on_primary(),
        "primary-container" => t.get_primary_container(),
        "on-primary-container" => t.get_on_primary_container(),
        "secondary-container" => t.get_secondary_container(),
        "on-secondary-container" => t.get_on_secondary_container(),
        "tertiary" => t.get_tertiary(),
        "tertiary-container" => t.get_tertiary_container(),
        "on-tertiary-container" => t.get_on_tertiary_container(),
        "error" => t.get_error(),
        "inverse-surface" => t.get_inverse_surface(),
        "inverse-on-surface" => t.get_inverse_on_surface(),
        _ => Color::default(),
    }
}

pub(super) fn corner_scale(style: i32) -> f32 {
    match style {
        0 => 0.0,
        1 => 0.5,
        3 => 1.6,
        _ => 1.0,
    }
}

impl App {
    fn saved_themes(&self) -> Vec<ThemeDef> {
        serde_json::from_str(&self.db.lock().unwrap().setting_or("themes", "[]")).unwrap_or_default()
    }

    fn save_themes(&self, themes: &[ThemeDef]) {
        let json = serde_json::to_string(themes).unwrap_or_else(|_| "[]".into());
        self.db.lock().unwrap().set_setting("themes", &json);
    }

    fn active_theme_def(&self) -> Option<ThemeDef> {
        let name = self.db.lock().unwrap().setting_or("active_theme", "Default");
        self.saved_themes().into_iter().find(|t| t.name == name)
    }

    /// Load all appearance settings into the UI (called at start and after an import).
    pub(super) fn load_appearance(&self) {
        let db = self.db.lock().unwrap();
        let b = self.backend();
        b.set_accent(s(db.setting_or("accent", DEFAULT_ACCENT)));
        b.set_corner_style(db.setting_or("corner_style", "2").parse().unwrap_or(2));
        b.set_density(db.setting_or("density", "1").parse().unwrap_or(1));
        b.set_ui_scale(db.setting_or("ui_scale", "100").parse().unwrap_or(100));
        b.set_font_family(s(db.setting_or("font_family", "")));
        b.set_sidebar_always_compact(db.setting_bool("sidebar_compact", false));
        b.set_active_theme(s(db.setting_or("active_theme", "Default")));
        let t = self.window.global::<Theme>();
        t.set_show_album(db.setting_bool("show_album", true));
        t.set_show_source(db.setting_bool("show_source", true));
        t.set_show_covers(db.setting_bool("show_covers", true));
        drop(db);
        let presets: Vec<AccentItem> =
            ACCENTS.iter().map(|h| AccentItem { hex: s(h), color: parse_hex(h).unwrap() }).collect();
        b.set_accent_presets(ModelRc::new(VecModel::from(presets)));
        self.refresh_theme_list();
        self.apply_layout();
    }

    fn refresh_theme_list(&self) {
        let mut names = vec![s("Default")];
        names.extend(self.saved_themes().into_iter().map(|t| s(t.name)));
        self.backend().set_themes(ModelRc::new(VecModel::from(names)));
    }

    /// Corners, density and font (colours are in `apply_theme`).
    pub(super) fn apply_layout(&self) {
        let b = self.backend();
        let t = self.window.global::<Theme>();
        t.set_corner_scale(corner_scale(b.get_corner_style()));
        t.set_row_height(match b.get_density() {
            0 => 44.0,
            2 => 64.0,
            _ => 56.0,
        });
        t.set_font_family(b.get_font_family());
        self.popup.global::<Theme>().set_corner_scale(corner_scale(b.get_corner_style()));
        self.popup.global::<Theme>().set_font_family(b.get_font_family());
        set_window_corners(self.window.window(), b.get_corner_style());
        set_window_corners(self.popup.window(), b.get_corner_style());
    }

    /// Set every colour role for the current mode, accent and active custom theme.
    pub(super) fn apply_theme(&self) {
        let b = self.backend();
        let dark = match b.get_theme_mode() {
            1 => false,
            2 => true,
            _ => !matches!(dark_light::detect(), Ok(dark_light::Mode::Light)),
        };
        let accent_hex = b.get_accent().to_string();
        let mut colors = match parse_hex(&accent_hex) {
            Some(c) if !accent_hex.eq_ignore_ascii_case(DEFAULT_ACCENT) => palette(dark, c),
            _ => baseline(dark),
        };
        if let Some(def) = self.active_theme_def() {
            if let Some(over) = def.colors.get(if dark { "dark" } else { "light" }) {
                for role in ROLES {
                    if let Some(c) = over.get(*role).and_then(|h| parse_hex(h)) {
                        colors.insert(role, c);
                    }
                }
            }
        }
        for theme in [self.window.global::<Theme>(), self.popup.global::<Theme>()] {
            theme.set_dark(dark);
            for (role, c) in &colors {
                set_role(&theme, role, *c);
            }
        }
    }

    pub(super) fn set_accent(&mut self, text: &str) {
        let Some(c) = parse_hex(text) else {
            self.toast("Enter a colour like #6750a4");
            return;
        };
        let h = hex(c);
        self.backend().set_accent(s(&h));
        self.db.lock().unwrap().set_setting("accent", &h);
        self.apply_theme();
    }

    pub(super) fn set_corner_style(&self, style: i32) {
        self.backend().set_corner_style(style);
        self.db.lock().unwrap().set_setting("corner_style", &style.to_string());
        self.apply_layout();
    }

    pub(super) fn set_density(&self, density: i32) {
        self.backend().set_density(density);
        self.db.lock().unwrap().set_setting("density", &density.to_string());
        self.apply_layout();
    }

    pub(super) fn set_font_family(&self) {
        let font = self.backend().get_font_family().trim().to_string();
        self.db.lock().unwrap().set_setting("font_family", &font);
        self.apply_layout();
    }

    /// Interface scale applies on the next start (Slint reads it when creating the window).
    pub(super) fn set_ui_scale(&self, percent: i32) {
        let b = self.backend();
        let current = b.get_ui_scale().max(50) as f32 / 100.0;
        let system = self.window.window().scale_factor() / current;
        let db = self.db.lock().unwrap();
        db.set_setting("ui_scale", &percent.to_string());
        if percent == 100 {
            db.set_setting("scale_override", "");
        } else {
            db.set_setting("scale_override", &format!("{:.3}", system * percent as f32 / 100.0));
        }
        let started = db.setting_or("ui_scale_started", "100");
        drop(db);
        b.set_ui_scale(percent);
        b.set_ui_scale_pending(started != percent.to_string());
    }

    pub(super) fn set_ui_flag(&mut self, name: &str, on: bool) {
        self.db.lock().unwrap().set_setting_bool(name, on);
        let t = self.window.global::<Theme>();
        match name {
            "show_album" => t.set_show_album(on),
            "show_source" => t.set_show_source(on),
            "show_covers" => t.set_show_covers(on),
            "sidebar_compact" => self.backend().set_sidebar_always_compact(on),
            _ => {}
        }
    }

    // ------------------------------------------------------------------ themes

    pub(super) fn theme_select(&mut self, name: &str) {
        let db = self.db.lock().unwrap();
        // Leaving "Default": remember the user's own look so it can come back later.
        if db.setting_or("active_theme", "Default") == "Default" && name != "Default" {
            let own = OwnLook {
                accent: db.setting_or("accent", DEFAULT_ACCENT),
                corner_style: db.setting_or("corner_style", "2"),
                font_family: db.setting_or("font_family", ""),
                theme: db.setting_or("theme", "0"),
            };
            db.set_setting("own_look", &serde_json::to_string(&own).unwrap_or_default());
        }
        db.set_setting("active_theme", name);
        drop(db);
        if let Some(def) = self.saved_themes().into_iter().find(|t| t.name == name) {
            let db = self.db.lock().unwrap();
            for (key, value) in theme_settings(&def) {
                db.set_setting(key, &value);
            }
            drop(db);
        } else {
            // Default: back to the user's own look (from before a theme was chosen).
            let db = self.db.lock().unwrap();
            if let Ok(own) = serde_json::from_str::<OwnLook>(&db.setting_or("own_look", "")) {
                db.set_setting("accent", &own.accent);
                db.set_setting("corner_style", &own.corner_style);
                db.set_setting("font_family", &own.font_family);
                db.set_setting("theme", &own.theme);
            }
        }
        let mode = self.db.lock().unwrap().setting_or("theme", "0").parse().unwrap_or(0);
        self.backend().set_theme_mode(mode);
        self.load_appearance();
        self.apply_theme();
    }

    pub(super) fn theme_export(&mut self) {
        let b = self.backend();
        let name = if b.get_active_theme() == "Default" { "My trax theme".to_string() } else { b.get_active_theme().to_string() };
        let Some(dest) = rfd::FileDialog::new()
            .set_title("Export theme")
            .set_file_name(format!("{}.json", crate::paths::sanitize_file_name(&name)))
            .add_filter("trax theme", &["json"])
            .save_file()
        else {
            return;
        };
        let t = self.window.global::<Theme>();
        let mode_key = if t.get_dark() { "dark" } else { "light" };
        let current: BTreeMap<String, String> = ROLES.iter().map(|r| (r.to_string(), hex(get_role(&t, r)))).collect();
        let mut colors = self.active_theme_def().map(|d| d.colors).unwrap_or_default();
        colors.insert(mode_key.to_string(), current);
        // Include the other mode as well, so the theme works in light and dark.
        let other = if t.get_dark() { "light" } else { "dark" };
        colors.entry(other.to_string()).or_insert_with(|| {
            let accent = parse_hex(&b.get_accent()).unwrap_or(parse_hex(DEFAULT_ACCENT).unwrap());
            let p = if b.get_accent().eq_ignore_ascii_case(DEFAULT_ACCENT) { baseline(!t.get_dark()) } else { palette(!t.get_dark(), accent) };
            p.into_iter().map(|(k, v)| (k.to_string(), hex(v))).collect()
        });
        let def = ThemeDef {
            name,
            mode: Some(match b.get_theme_mode() {
                1 => "light".into(),
                2 => "dark".into(),
                _ => "system".into(),
            }),
            accent: Some(b.get_accent().to_string()),
            corners: Some(b.get_corner_style()),
            font: Some(b.get_font_family().to_string()),
            colors,
        };
        match serde_json::to_string_pretty(&def).map_err(anyhow::Error::from).and_then(|j| Ok(std::fs::write(&dest, j)?)) {
            Ok(()) => self.toast(format!("Theme exported to {}", dest.display())),
            Err(e) => self.toast(format!("Export failed: {e}")),
        }
    }

    pub(super) fn theme_import(&mut self) {
        let Some(src) = rfd::FileDialog::new().set_title("Import theme").add_filter("trax theme", &["json"]).pick_file()
        else {
            return;
        };
        let def: ThemeDef = match std::fs::read_to_string(&src).map_err(anyhow::Error::from).and_then(|t| Ok(serde_json::from_str(&t)?)) {
            Ok(d) => d,
            Err(e) => {
                self.toast(format!("Not a valid theme file: {e}"));
                return;
            }
        };
        let mut def = def;
        if def.name.trim().is_empty() || def.name == "Default" {
            def.name = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Imported".into());
        }
        let mut themes = self.saved_themes();
        themes.retain(|t| t.name != def.name);
        let name = def.name.clone();
        themes.push(def);
        self.save_themes(&themes);
        self.theme_select(&name);
        self.toast(format!("Theme “{name}” imported"));
    }

    /// Every appearance setting back to its default. Saved themes are kept.
    pub(super) fn reset_design(&mut self) {
        let db = self.db.lock().unwrap();
        for (key, value) in [
            ("accent", DEFAULT_ACCENT),
            ("corner_style", "2"),
            ("density", "1"),
            ("font_family", ""),
            ("theme", "0"),
            ("active_theme", "Default"),
            ("own_look", ""),
        ] {
            db.set_setting(key, value);
        }
        for key in ["show_album", "show_source", "show_covers"] {
            db.set_setting_bool(key, true);
        }
        db.set_setting_bool("sidebar_compact", false);
        drop(db);
        self.set_ui_scale(100);
        self.backend().set_theme_mode(0);
        self.load_appearance();
        self.apply_theme();
        self.toast("Design reset to default");
    }

    pub(super) fn theme_delete(&mut self) {
        let name = self.backend().get_active_theme().to_string();
        if name == "Default" {
            return;
        }
        let mut themes = self.saved_themes();
        themes.retain(|t| t.name != name);
        self.save_themes(&themes);
        self.theme_select("Default");
        self.toast(format!("Theme “{name}” deleted"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let c = parse_hex("#6750a4").unwrap();
        assert_eq!(hex(c), "#6750a4");
        assert!(parse_hex("nope").is_none());
    }

    #[test]
    fn palette_has_every_role() {
        let accent = parse_hex("#0061a4").unwrap();
        for dark in [true, false] {
            let p = palette(dark, accent);
            for r in ROLES {
                assert!(p.contains_key(r), "missing {r}");
            }
            assert_eq!(baseline(dark).len(), ROLES.len());
        }
    }

    #[test]
    fn theme_json() {
        let json = r##"{"name":"Ocean","mode":"dark","accent":"#0061a4","corners":3,
            "colors":{"dark":{"primary":"#9ecaff"}}}"##;
        let t: ThemeDef = serde_json::from_str(json).unwrap();
        assert_eq!(t.corners, Some(3));
        assert_eq!(t.colors["dark"]["primary"], "#9ecaff");
    }

    #[test]
    fn neutral_theme_values_keep_user_settings() {
        let json = r##"{"name":"Plain","mode":"system","accent":"#0061a4","corners":2,"font":""}"##;
        let t: ThemeDef = serde_json::from_str(json).unwrap();
        assert_eq!(theme_settings(&t), vec![("accent", "#0061a4".to_string())]);
        let json = r##"{"name":"Bold","mode":"dark","corners":0,"font":"Segoe UI"}"##;
        let keys: Vec<_> = theme_settings(&serde_json::from_str(json).unwrap()).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["corner_style", "font_family", "theme"]);
    }
}

/// Windows 11 window corners for the chosen corner style. Borderless windows such as the
/// tray mini player are otherwise always square.
pub(super) fn set_window_corners(window: &slint::Window, style: i32) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND, DWMWCP_ROUNDSMALL, DwmSetWindowAttribute,
        };
        let handle = window.window_handle();
        let Ok(h) = handle.window_handle() else { return };
        let RawWindowHandle::Win32(h) = h.as_raw() else { return };
        let pref: i32 = match style {
            0 => DWMWCP_DONOTROUND,
            1 => DWMWCP_ROUNDSMALL,
            _ => DWMWCP_ROUND,
        };
        // SAFETY: valid HWND of our own window, attribute value is a 4-byte enum.
        unsafe {
            DwmSetWindowAttribute(
                h.hwnd.get() as _,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const i32 as *const core::ffi::c_void,
                4,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = (window, style);
}
