//! Music sources. Each connector can be enabled/disabled in settings.

pub mod cache;
pub mod local;
pub mod nextcloud;
pub mod soundcloud;
pub mod spotify;
pub mod wikipedia;
pub mod youtube;

use crate::model::Track;
use crate::paths;
use std::path::{Path, PathBuf};

pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "mp4", "aac", "ogg", "oga", "wav", "alac"];

pub fn is_audio(path: &str) -> bool {
    path.rsplit('.')
        .next()
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Thumbnail path that belongs to a stored cover.
pub fn thumb_path(cover: &str) -> PathBuf {
    let p = Path::new(cover);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("cover");
    p.with_file_name(format!("{stem}_t.jpg"))
}

/// Save image bytes as cover (max 800px) plus a small thumbnail. Returns the cover path.
pub fn save_cover(key: &str, bytes: &[u8]) -> Option<String> {
    let name = paths::hash(key);
    let path = paths::covers_dir().join(format!("{name}.jpg"));
    if path.exists() && thumb_path(&path.to_string_lossy()).exists() {
        return Some(path.to_string_lossy().into_owned());
    }
    let img = image::load_from_memory(bytes).ok()?;
    let full = if img.width() > 800 || img.height() > 800 { img.thumbnail(800, 800) } else { img.clone() };
    full.to_rgb8().save_with_format(&path, image::ImageFormat::Jpeg).ok()?;
    let thumb = img.thumbnail(128, 128);
    let _ = thumb.to_rgb8().save_with_format(thumb_path(&path.to_string_lossy()), image::ImageFormat::Jpeg);
    Some(path.to_string_lossy().into_owned())
}

/// Read tags/duration/cover from an audio file into `t`. Missing fields keep their values.
pub fn read_tags(path: &Path, t: &mut Track) {
    use lofty::prelude::*;
    let Ok(tagged) = lofty::read_from_path(path) else {
        return;
    };
    let dur = tagged.properties().duration().as_millis() as i64;
    if dur > 0 {
        t.duration_ms = dur;
    }
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    if let Some(tag) = tag {
        if let Some(v) = tag.title() {
            if !v.trim().is_empty() {
                t.title = v.trim().to_string();
            }
        }
        if let Some(v) = tag.artist() {
            if !v.trim().is_empty() {
                t.artist = v.trim().to_string();
            }
        }
        if let Some(v) = tag.album() {
            if !v.trim().is_empty() {
                t.album = v.trim().to_string();
            }
        }
        if let Some(n) = tag.track() {
            t.track_no = n as i64;
        }
        if let Some(pic) = tag.pictures().first() {
            // Share one cover file per album where possible.
            let key = if t.album.is_empty() { t.uri.clone() } else { format!("{}|{}", t.artist, t.album) };
            if let Some(c) = save_cover(&key, pic.data()) {
                t.cover = Some(c);
            }
        }
    }
    if t.cover.is_none() {
        // Folder art next to the file (cover.jpg, folder.jpg, ...).
        if let Some(dir) = path.parent() {
            for name in ["cover.jpg", "cover.png", "folder.jpg", "folder.png", "front.jpg", "album.jpg"] {
                let candidate = dir.join(name);
                if let Ok(bytes) = std::fs::read(&candidate) {
                    if let Some(c) = save_cover(&candidate.to_string_lossy(), &bytes) {
                        t.cover = Some(c);
                        break;
                    }
                }
            }
        }
    }
}

/// Guess artist/album/title from a path like `Artist/Album/01 - Title.mp3`.
pub fn guess_from_path(t: &mut Track, path: &str) {
    let norm = path.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    let file = parts.last().copied().unwrap_or("");
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    let mut title = stem.to_string();
    // Strip a leading track number: "01 - ", "01. ", "1 "
    let trimmed = title.trim_start_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() < title.len() {
        if let Ok(n) = title[..title.len() - trimmed.len()].parse::<i64>() {
            t.track_no = n;
        }
        title = trimmed.trim_start_matches([' ', '-', '.', '_']).to_string();
    }
    if let Some((a, b)) = title.split_once(" - ") {
        if t.artist.is_empty() {
            t.artist = a.trim().to_string();
        }
        title = b.trim().to_string();
    }
    t.title = if title.is_empty() { stem.to_string() } else { title };
    if parts.len() >= 3 {
        if t.album.is_empty() {
            t.album = parts[parts.len() - 2].to_string();
        }
        if t.artist.is_empty() {
            t.artist = parts[parts.len() - 3].to_string();
        }
    }
}

pub fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("trax/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("http client")
}

#[cfg(windows)]
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(program);
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

#[cfg(not(windows))]
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    std::process::Command::new(program)
}
