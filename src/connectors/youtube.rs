//! YouTube via yt-dlp (search + audio download). No API key needed.

use super::{hidden_command, http_client, save_cover};
use crate::model::{SourceKind, Track};
use crate::paths;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

#[cfg(windows)]
const BIN_NAME: &str = "yt-dlp.exe";
#[cfg(target_os = "macos")]
const BIN_NAME: &str = "yt-dlp_macos";
#[cfg(all(unix, not(target_os = "macos")))]
const BIN_NAME: &str = "yt-dlp_linux";

/// The yt-dlp binary to use: configured path, a copy the user put in trax's bin folder,
/// or whatever is on PATH. trax never downloads yt-dlp itself.
pub fn binary(configured: &str) -> PathBuf {
    if !configured.trim().is_empty() {
        return PathBuf::from(configured.trim());
    }
    let own = paths::bin_dir().join(BIN_NAME);
    if own.exists() {
        return own;
    }
    PathBuf::from("yt-dlp")
}

pub fn version(configured: &str) -> Option<String> {
    let out = hidden_command(binary(configured)).arg("--version").output().ok()?;
    if out.status.success() { Some(String::from_utf8_lossy(&out.stdout).trim().to_string()) } else { None }
}

pub fn thumb_url(id: &str) -> String {
    format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")
}

pub fn search(configured: &str, query: &str, limit: usize) -> Result<Vec<Track>> {
    let out = hidden_command(binary(configured))
        .args(["--flat-playlist", "-J", "--no-warnings", "--ignore-config"])
        .arg(format!("ytsearch{limit}:{query}"))
        .output()
        .context("yt-dlp not found — set it up in Settings → Sources")?;
    if !out.status.success() {
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("search failed"));
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let mut tracks = vec![];
    for e in json["entries"].as_array().into_iter().flatten() {
        let Some(id) = e["id"].as_str() else { continue };
        let mut t = Track::new(SourceKind::YouTube, id);
        let (artist, title) = split_video_title(
            e["title"].as_str().unwrap_or(id),
            e["channel"].as_str().or(e["uploader"].as_str()).unwrap_or(""),
        );
        t.title = title;
        t.artist = artist;
        t.album = "YouTube".into();
        t.duration_ms = (e["duration"].as_f64().unwrap_or(0.0) * 1000.0) as i64;
        let cached = thumb_cover_path(id);
        if cached.exists() {
            t.cover = Some(cached.to_string_lossy().into_owned());
        }
        tracks.push(t);
    }
    Ok(tracks)
}

/// Where the video thumbnail used as a stand-in cover is stored.
pub fn thumb_cover_path(id: &str) -> PathBuf {
    paths::covers_dir().join(format!("{}.jpg", paths::hash(&format!("yt:{id}"))))
}

/// Download the video thumbnail as cover. Returns the cover path.
pub fn fetch_thumbnail(id: &str) -> Option<String> {
    let cached = thumb_cover_path(id);
    if cached.exists() {
        return Some(cached.to_string_lossy().into_owned());
    }
    let bytes = http_client().get(thumb_url(id)).send().ok()?.bytes().ok()?;
    save_cover(&format!("yt:{id}"), &bytes)
}

pub fn cache_path(id: &str) -> PathBuf {
    paths::cache_dir().join(format!("yt_{}.m4a", paths::sanitize_file_name(id)))
}

/// Download the audio track (AAC/m4a, decodable without ffmpeg) into the cache.
pub fn fetch(configured: &str, id: &str) -> Result<PathBuf> {
    let dest = cache_path(id);
    if dest.exists() {
        return Ok(dest);
    }
    // Download under a unique name, then move into place.
    let stem = format!("yt_{}.{}", paths::sanitize_file_name(id), paths::unique_suffix());
    let tmp = paths::cache_dir().join(format!("{stem}.m4a"));
    let template = paths::cache_dir().join(format!("{stem}.%(ext)s"));
    let out = hidden_command(binary(configured))
        .args(["-f", "bestaudio[ext=m4a]/140/bestaudio[acodec^=mp4a]", "--no-playlist", "--no-warnings", "--ignore-config", "--no-part", "-o"])
        .arg(&template)
        .arg(format!("https://www.youtube.com/watch?v={id}"))
        .output()
        .context("yt-dlp not found — set it up in Settings → Sources")?;
    if !out.status.success() || !tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("download failed"));
    }
    if !dest.exists() {
        std::fs::rename(&tmp, &dest)?;
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(dest)
}

/// Music videos are usually titled "Artist - Song (Official Video)" and uploaded by
/// "ArtistVEVO" or "Artist - Topic". Returns (artist, title).
pub fn split_video_title(title: &str, channel: &str) -> (String, String) {
    let channel = channel.trim_end_matches(" - Topic").trim();
    let channel = channel.strip_suffix("VEVO").unwrap_or(channel).trim().to_string();
    for sep in [" - ", " – ", " — "] {
        if let Some((a, t)) = title.split_once(sep) {
            let t = crate::model::clean_title(t);
            if !a.trim().is_empty() && !t.is_empty() {
                return (a.trim().to_string(), t);
            }
        }
    }
    let clean = crate::model::clean_title(title);
    (channel, if clean.is_empty() { title.to_string() } else { clean })
}

#[cfg(test)]
mod tests {
    use super::split_video_title;

    #[test]
    fn video_titles() {
        assert_eq!(
            split_video_title("Megadeth - Symphony Of Destruction (Official Video)", "Megadeth"),
            ("Megadeth".to_string(), "Symphony Of Destruction".to_string())
        );
        assert_eq!(split_video_title("Holy Wars", "MegadethVEVO"), ("Megadeth".to_string(), "Holy Wars".to_string()));
        assert_eq!(split_video_title("Holy Wars", "Megadeth - Topic"), ("Megadeth".to_string(), "Holy Wars".to_string()));
    }
}
