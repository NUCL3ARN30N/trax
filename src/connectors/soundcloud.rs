//! SoundCloud via yt-dlp (search + audio download), sharing the YouTube connector's yt-dlp.
//! A track's uri is its soundcloud.com permalink.

use super::youtube::{binary, split_video_title};
use super::{hidden_command, http_client, save_cover};
use crate::model::{SourceKind, Track};
use crate::paths;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// Formats that play without ffmpeg: progressive MP3 where offered, otherwise AAC (HLS).
const FORMAT: &str = "http_mp3_1_0/http_mp3_128/bestaudio[ext=mp3]/bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]";

/// Search results with the artwork URL to download in the background.
pub fn search(configured: &str, query: &str, limit: usize) -> Result<Vec<(Track, Option<String>)>> {
    let out = hidden_command(binary(configured))
        .args(["--flat-playlist", "-J", "--no-warnings", "--ignore-config"])
        .arg(format!("scsearch{limit}:{query}"))
        .output()
        .context("yt-dlp not found — set it up in Settings → Sources")?;
    if !out.status.success() {
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("search failed"));
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(json["entries"].as_array().into_iter().flatten().filter_map(parse_entry).collect())
}

fn parse_entry(e: &serde_json::Value) -> Option<(Track, Option<String>)> {
    let uri = e["webpage_url"].as_str()?;
    let mut t = Track::new(SourceKind::SoundCloud, uri);
    let title = e["title"].as_str().unwrap_or(uri);
    let uploader = e["uploader"].as_str().unwrap_or("");
    match e["artists"].as_array().and_then(|a| a.first()).and_then(|a| a.as_str()) {
        // "Song - Artist" uploads by someone else: drop the artist part from the title.
        Some(artist) => {
            let bare = title
                .strip_suffix(&format!(" - {artist}"))
                .or_else(|| title.strip_prefix(&format!("{artist} - ")))
                .unwrap_or(title);
            let clean = crate::model::clean_title(bare);
            t.artist = artist.to_string();
            t.title = if clean.is_empty() { title.to_string() } else { clean };
        }
        None => (t.artist, t.title) = split_video_title(title, uploader),
    }
    t.album = "SoundCloud".into();
    t.duration_ms = (e["duration"].as_f64().unwrap_or(0.0) * 1000.0) as i64;
    let cached = thumb_cover_path(uri);
    if cached.exists() {
        t.cover = Some(cached.to_string_lossy().into_owned());
    }
    Some((t, artwork_url(e)))
}

/// The 500×500 artwork, or the largest one listed.
fn artwork_url(e: &serde_json::Value) -> Option<String> {
    let thumbs = e["thumbnails"].as_array()?;
    let pick = thumbs.iter().find(|t| t["id"] == "t500x500").or_else(|| thumbs.iter().rev().find(|t| t["url"].is_string()))?;
    pick["url"].as_str().map(str::to_string)
}

/// Where the track artwork used as a stand-in cover is stored.
pub fn thumb_cover_path(uri: &str) -> PathBuf {
    paths::covers_dir().join(format!("{}.jpg", paths::hash(&format!("sc:{uri}"))))
}

/// Download the track artwork as cover. Without a known URL, yt-dlp looks it up.
pub fn fetch_thumbnail(configured: &str, uri: &str, url: Option<&str>) -> Option<String> {
    let cached = thumb_cover_path(uri);
    if cached.exists() {
        return Some(cached.to_string_lossy().into_owned());
    }
    let url = match url {
        Some(u) => u.to_string(),
        None => {
            let out = hidden_command(binary(configured))
                .args(["--no-warnings", "--ignore-config", "--skip-download", "--print", "thumbnail"])
                .arg(uri)
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    };
    if !url.starts_with("http") {
        return None;
    }
    let bytes = http_client().get(url).send().ok()?.bytes().ok()?;
    save_cover(&format!("sc:{uri}"), &bytes)
}

fn cache_stem(uri: &str) -> String {
    format!("sc_{}", paths::hash(uri))
}

/// The cached audio file, if downloaded (MP3 or AAC depending on what SoundCloud offered).
pub fn cached(uri: &str) -> Option<PathBuf> {
    let stem = cache_stem(uri);
    ["mp3", "m4a"].iter().map(|ext| paths::cache_dir().join(format!("{stem}.{ext}"))).find(|p| p.exists())
}

/// Where the audio is or will be cached (for cache bookkeeping).
pub fn cache_path(uri: &str) -> PathBuf {
    cached(uri).unwrap_or_else(|| paths::cache_dir().join(format!("{}.mp3", cache_stem(uri))))
}

/// Download the audio into the cache.
pub fn fetch(configured: &str, uri: &str) -> Result<PathBuf> {
    if let Some(p) = cached(uri) {
        return Ok(p);
    }
    // Download under a unique name, then move into place.
    let stem = format!("{}.{}", cache_stem(uri), paths::unique_suffix());
    let template = paths::cache_dir().join(format!("{stem}.%(ext)s"));
    let out = hidden_command(binary(configured))
        .args(["-f", FORMAT, "--no-playlist", "--no-warnings", "--ignore-config", "--no-part", "--no-progress", "-o"])
        .arg(&template)
        .arg(uri)
        .output()
        .context("yt-dlp not found — set it up in Settings → Sources")?;
    let tmp = ["mp3", "m4a"].iter().map(|ext| paths::cache_dir().join(format!("{stem}.{ext}"))).find(|p| p.exists());
    let Some(tmp) = tmp.filter(|_| out.status.success()) else {
        for ext in ["mp3", "m4a"] {
            let _ = std::fs::remove_file(paths::cache_dir().join(format!("{stem}.{ext}")));
        }
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("download failed"));
    };
    let ext = tmp.extension().and_then(|e| e.to_str()).unwrap_or("mp3").to_string();
    let dest = paths::cache_dir().join(format!("{}.{ext}", cache_stem(uri)));
    if !dest.exists() {
        std::fs::rename(&tmp, &dest)?;
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::parse_entry;

    #[test]
    fn search_entry() {
        crate::paths::use_test_dir();
        let e: serde_json::Value = serde_json::from_str(
            r#"{"uploader":"DJ KB","title":"Get Lucky [Original Version] - Daft Punk","duration":246.381,
            "webpage_url":"https://soundcloud.com/eldjkb/get-lucky","artists":["Daft Punk"],
            "thumbnails":[{"id":"large","url":"https://i1.sndcdn.com/a-large.jpg"},
                          {"id":"t500x500","url":"https://i1.sndcdn.com/a-t500x500.jpg"}]}"#,
        )
        .unwrap();
        let (t, art) = parse_entry(&e).unwrap();
        assert_eq!(t.artist, "Daft Punk");
        assert_eq!(t.title, "Get Lucky");
        assert_eq!(t.uri, "https://soundcloud.com/eldjkb/get-lucky");
        assert_eq!(t.album, "SoundCloud");
        assert_eq!(t.duration_ms, 246381);
        assert_eq!(art.as_deref(), Some("https://i1.sndcdn.com/a-t500x500.jpg"));

        let e: serde_json::Value =
            serde_json::from_str(r#"{"uploader":"Someone","title":"Artist X - Song Y","webpage_url":"https://soundcloud.com/a/b"}"#).unwrap();
        let (t, art) = parse_entry(&e).unwrap();
        assert_eq!((t.artist.as_str(), t.title.as_str()), ("Artist X", "Song Y"));
        assert!(art.is_none());
    }
}
