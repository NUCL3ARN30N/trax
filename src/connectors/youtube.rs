//! YouTube via yt-dlp (search + audio download). No API key needed.

use super::{hidden_command, http_client, save_cover};
use crate::model::{SourceKind, Track};
use crate::paths;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// What trax looks for in its yt-dlp folder: the standalone build from the yt-dlp releases
/// for this system, or plain `yt-dlp` (renamed, or the Python zipapp).
#[cfg(windows)]
const BIN_NAMES: &[&str] = &["yt-dlp.exe"];
#[cfg(target_os = "macos")]
const BIN_NAMES: &[&str] = &["yt-dlp_macos", "yt-dlp"];
#[cfg(all(unix, not(target_os = "macos")))]
const BIN_NAMES: &[&str] = &["yt-dlp_linux", "yt-dlp"];

/// The yt-dlp binary to use: configured path, a copy the user put in trax's bin folder,
/// or whatever is on PATH. trax never downloads yt-dlp itself.
pub fn binary(configured: &str) -> PathBuf {
    if !configured.trim().is_empty() {
        let path = PathBuf::from(configured.trim());
        make_executable(&path);
        return path;
    }
    let dir = paths::bin_dir();
    if let Some(own) = BIN_NAMES.iter().map(|n| dir.join(n)).find(|p| p.is_file()) {
        make_executable(&own);
        return own;
    }
    // pipx and `pip install --user` put it in ~/.local/bin, which isn't always on PATH when
    // trax is started from the app launcher.
    #[cfg(unix)]
    if let Some(user) = directories::BaseDirs::new().and_then(|d| d.executable_dir().map(|e| e.join("yt-dlp"))) {
        if user.is_file() {
            return user;
        }
    }
    PathBuf::from("yt-dlp")
}

/// A file saved from the browser isn't executable yet; the user picked it to run as yt-dlp.
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return };
    let mode = meta.permissions().mode();
    if meta.is_file() && mode & 0o111 == 0 {
        match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o111)) {
            Ok(()) => log::info!("made {} executable", path.display()),
            Err(e) => log::warn!("can't make {} executable: {e}", path.display()),
        }
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Status line when yt-dlp can't be run.
pub fn not_found_hint(configured: &str) -> &'static str {
    #[cfg(not(windows))]
    {
        let windows_build = configured.trim().to_ascii_lowercase().ends_with(".exe")
            || (configured.trim().is_empty() && paths::bin_dir().join("yt-dlp.exe").exists());
        if windows_build {
            return "yt-dlp.exe is the Windows version — get yt-dlp_linux instead (see the steps above)";
        }
    }
    let _ = configured;
    "yt-dlp not found — see the steps above"
}

/// Setup steps shown in Settings → Sources; `folder` is trax's yt-dlp folder.
#[cfg(windows)]
pub fn setup_help(folder: &str) -> String {
    format!(
        "trax doesn't include or download yt-dlp. To use these sources, get yt-dlp yourself from the yt-dlp project's GitHub releases (yt-dlp.exe) and either:\n  \
         1. put yt-dlp.exe in trax's yt-dlp folder: {folder} (\"Open yt-dlp folder\" below), or\n  \
         2. put it anywhere and enter its full path below, or\n  \
         3. install it so it is on your PATH (e.g. winget install yt-dlp).\n\
         Then click Save to check it. Keep it up to date yourself (yt-dlp -U)."
    )
}

#[cfg(not(windows))]
pub fn setup_help(folder: &str) -> String {
    let file = BIN_NAMES[0];
    format!(
        "trax doesn't include or download yt-dlp. To use these sources, get yt-dlp yourself and either:\n  \
         1. download {file} (not yt-dlp.exe) from the yt-dlp project's GitHub releases and put it in trax's yt-dlp folder: {folder} (\"Open yt-dlp folder\" below; trax makes it executable), or\n  \
         2. put it anywhere and enter its full path below, or\n  \
         3. install it so it is on your PATH, e.g. pipx install yt-dlp. Distribution packages (apt install yt-dlp) are often too old for YouTube.\n\
         Then click Save to check it. Keep it up to date yourself ({file} -U, or pipx upgrade yt-dlp)."
    )
}

pub fn version(configured: &str) -> Option<String> {
    let out = hidden_command(binary(configured)).arg("--version").output().ok()?;
    if out.status.success() { Some(String::from_utf8_lossy(&out.stdout).trim().to_string()) } else { None }
}

pub fn thumb_url(id: &str) -> String {
    format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")
}

/// Run yt-dlp on a search or playlist without resolving the videos (`--flat-playlist -J`).
fn flat_json(configured: &str, target: &str) -> Result<serde_json::Value> {
    let out = hidden_command(binary(configured))
        .args(["--flat-playlist", "-J", "--no-warnings", "--ignore-config"])
        .arg(target)
        .output()
        .context("yt-dlp not found — set it up in Settings → Sources")?;
    if !out.status.success() {
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("request failed"));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// A video from a flat search or playlist as a track.
fn entry_track(e: &serde_json::Value) -> Option<Track> {
    let id = e["id"].as_str()?;
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
    Some(t)
}

pub fn search(configured: &str, query: &str, limit: usize) -> Result<Vec<Track>> {
    let json = flat_json(configured, &format!("ytsearch{limit}:{query}"))?;
    Ok(json["entries"].as_array().into_iter().flatten().filter_map(entry_track).collect())
}

pub struct Playlist {
    pub name: String,
    pub description: String,
    /// Playlist thumbnails, largest first (the largest isn't always there).
    pub cover_urls: Vec<String>,
    pub tracks: Vec<Track>,
}

/// A public or unlisted YouTube / YouTube Music playlist (albums are playlists there too).
/// `list` is the playlist id, already checked by the caller.
pub fn playlist(configured: &str, list: &str, max: usize) -> Result<Playlist> {
    let json = flat_json(configured, &format!("https://www.youtube.com/playlist?list={list}"))?;
    let tracks = json["entries"]
        .as_array()
        .into_iter()
        .flatten()
        // Deleted and private videos stay in the list as placeholders.
        .filter(|e| !matches!(e["title"].as_str(), Some("[Deleted video]" | "[Private video]")))
        .filter_map(entry_track)
        .take(max)
        .collect();
    Ok(Playlist {
        name: json["title"].as_str().unwrap_or("").trim().trim_start_matches("Album - ").to_string(),
        description: json["description"].as_str().unwrap_or("").trim().to_string(),
        cover_urls: json["thumbnails"].as_array().into_iter().flatten().rev().filter_map(|t| t["url"].as_str()).map(String::from).collect(),
        tracks,
    })
}

/// The YouTube video that best stands in for a song from another service (title, artist and
/// length in ms, 0 if unknown). None when no result looks like the same song.
pub fn find_song(configured: &str, artist: &str, title: &str, duration_ms: i64) -> Result<Option<Track>> {
    let title = song_title(title);
    let json = flat_json(configured, &format!("ytsearch5:{artist} - {title}"))?;
    let entries = json["entries"].as_array().cloned().unwrap_or_default();
    let best = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let score = match_score(
                e["title"].as_str().unwrap_or(""),
                e["channel"].as_str().or(e["uploader"].as_str()).unwrap_or(""),
                e["duration"].as_f64().map(|d| (d * 1000.0) as i64).unwrap_or(0),
                artist,
                &title,
                duration_ms,
            )?;
            // Search order breaks ties.
            Some((score - i as i32 * 2, e))
        })
        .max_by_key(|(score, _)| *score);
    Ok(best.and_then(|(_, e)| {
        let mut t = entry_track(e)?;
        // The other service's tags are cleaner than a video title.
        t.title = title.clone();
        t.artist = artist.to_string();
        Some(t)
    }))
}

/// Spotify writes versions as "Song - Remastered 2011" or "Song - Live"; the search and the
/// comparison use the song name only.
fn song_title(title: &str) -> String {
    let cut = title.split(" - ").next().unwrap_or(title);
    let clean = crate::model::clean_title(cut);
    if clean.is_empty() { title.trim().to_string() } else { clean }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// Lower case words between spaces, so whole words can be found (" live " but not "alive").
fn words(s: &str) -> String {
    let w: String = s.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    format!(" {} ", w.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Versions nobody wants instead of the original, unless the song asks for them.
const OTHER_VERSIONS: &[&str] =
    &["live", "remix", "cover", "karaoke", "sped up", "slowed", "nightcore", "instrumental", "8d", "reverb", "acoustic", "1 hour"];

/// How well a search result fits the wanted song; None if it is something else.
fn match_score(video_title: &str, channel: &str, video_ms: i64, artist: &str, title: &str, wanted_ms: i64) -> Option<i32> {
    let title_ok = norm(video_title).contains(&norm(title));
    let artist_ok = artist.split(", ").next().map(norm).is_some_and(|a| !a.is_empty() && norm(&format!("{video_title} {channel}")).contains(&a));
    let diff = if video_ms > 0 && wanted_ms > 0 { Some((video_ms - wanted_ms).abs() / 1000) } else { None };
    if diff.is_some_and(|d| d > 25) {
        return None;
    }
    if !title_ok && !(artist_ok && diff.is_some_and(|d| d <= 3)) {
        return None;
    }
    let (video, wanted) = (words(video_title), words(&format!("{artist} {title}")));
    let other = OTHER_VERSIONS.iter().filter(|w| video.contains(&format!(" {w} ")) && !wanted.contains(&format!(" {w} "))).count() as i32;
    let mut score = 100 - diff.unwrap_or(10) as i32 * 3 - other * 50;
    if title_ok {
        score += 30;
    }
    if artist_ok {
        score += 30;
    }
    // "Artist - Topic" channels carry the album version.
    if channel.ends_with(" - Topic") {
        score += 10;
    }
    Some(score)
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
    use super::{match_score, song_title, split_video_title};

    /// A yt-dlp_linux saved from the browser (not executable) is found and made executable;
    /// the Windows build in the folder gets its own hint.
    #[test]
    #[cfg(target_os = "linux")]
    fn linux_binary_in_own_folder() {
        use std::os::unix::fs::PermissionsExt;
        crate::paths::use_test_dir();
        let dir = crate::paths::bin_dir();
        let exe = dir.join("yt-dlp_linux");
        std::fs::write(&exe, b"#!/bin/sh\necho 2099.01.01\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(super::binary(""), exe);
        assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(super::version("").as_deref(), Some("2099.01.01"));

        std::fs::write(dir.join("yt-dlp.exe"), b"MZ").unwrap();
        assert!(super::not_found_hint("").contains("Windows version"));
        assert!(super::not_found_hint("/opt/yt-dlp.exe").contains("Windows version"));
        std::fs::remove_file(dir.join("yt-dlp.exe")).unwrap();
        std::fs::remove_file(&exe).unwrap();
        assert!(super::setup_help("/x").contains("yt-dlp_linux"));
    }

    #[test]
    fn video_titles() {
        assert_eq!(
            split_video_title("Megadeth - Symphony Of Destruction (Official Video)", "Megadeth"),
            ("Megadeth".to_string(), "Symphony Of Destruction".to_string())
        );
        assert_eq!(split_video_title("Holy Wars", "MegadethVEVO"), ("Megadeth".to_string(), "Holy Wars".to_string()));
        assert_eq!(split_video_title("Holy Wars", "Megadeth - Topic"), ("Megadeth".to_string(), "Holy Wars".to_string()));
    }

    #[test]
    fn song_matching() {
        assert_eq!(song_title("Let It Be - Remastered 2009"), "Let It Be");
        assert_eq!(song_title("Song (feat. Other)"), "Song");
        // The original beats the live version and an unrelated video.
        let original = match_score("Tate McRae - Housewife (Lyric Video)", "Tate McRae", 210_000, "Tate McRae", "Housewife", 208_810);
        let live = match_score("Tate McRae - Housewife (Live 2026)", "Tate McRae", 220_000, "Tate McRae", "Housewife", 208_810);
        assert!(original.unwrap() > live.unwrap());
        assert!(match_score("Alive - Official Audio", "Band - Topic", 200_000, "Band", "Alive", 200_000).unwrap() > 150);
        assert_eq!(match_score("Something else", "Other", 209_000, "Tate McRae", "Housewife", 208_810), None);
        assert_eq!(match_score("Tate McRae - Housewife (1 hour loop)", "x", 3_600_000, "Tate McRae", "Housewife", 208_810), None);
        // A live song may be a live video.
        assert!(match_score("Song (Live)", "Band", 200_000, "Band", "Song live", 200_000).unwrap() > 150);
    }
}
