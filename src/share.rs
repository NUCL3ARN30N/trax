//! Playlist share codes: the whole playlist, compressed into one line of text.
//!
//! `trax1:` + base64url(deflate(text)). The text is the name, the description, then one line
//! per song: `y` + YouTube id or `s` + SoundCloud path, with title, artist and duration.
//! Only YouTube and SoundCloud songs are shared; local and Nextcloud files can't be played
//! by anyone else.

use crate::model::{SourceKind, Track};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use std::io::{Read, Write};

const PREFIX: &str = "trax1:";
const SOUNDCLOUD: &str = "https://soundcloud.com/";
/// Imported text is limited, so a crafted code can't make trax unpack gigabytes.
const MAX_TEXT: u64 = 2 * 1024 * 1024;
const MAX_TRACKS: usize = 10_000;

pub struct Shared {
    pub name: String,
    pub description: String,
    pub tracks: Vec<Track>,
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ").trim().to_string()
}

/// The share code and how many songs were left out (local / Nextcloud).
pub fn encode(name: &str, description: &str, tracks: &[Track]) -> (String, usize, usize) {
    let mut text = format!("{}\n{}\n", clean(name), clean(description));
    let mut included = 0;
    for t in tracks {
        let key = match t.source {
            SourceKind::YouTube => format!("y{}", t.uri),
            SourceKind::SoundCloud => match t.uri.strip_prefix(SOUNDCLOUD) {
                Some(path) => format!("s{path}"),
                None => continue,
            },
            _ => continue,
        };
        included += 1;
        text.push_str(&format!("{}\t{}\t{}\t{}\n", clean(&key), clean(&t.title), clean(&t.artist), t.duration_ms / 1000));
    }
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
    let _ = enc.write_all(text.as_bytes());
    let bytes = enc.finish().unwrap_or_default();
    (format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)), included, tracks.len() - included)
}

pub fn decode(code: &str) -> Result<Shared> {
    // Pasted codes may be wrapped or quoted by chat apps.
    let code: String = code.chars().filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'').collect();
    let Some(data) = code.strip_prefix(PREFIX) else {
        bail!("This isn't a trax playlist code (it should start with “{PREFIX}”)");
    };
    let bytes = URL_SAFE_NO_PAD.decode(data).context("The code is incomplete or damaged")?;
    let mut text = String::new();
    DeflateDecoder::new(&bytes[..])
        .take(MAX_TEXT)
        .read_to_string(&mut text)
        .context("The code is incomplete or damaged")?;
    let mut lines = text.lines();
    let name = lines.next().map(clean).filter(|n| !n.is_empty()).unwrap_or_else(|| "Shared playlist".into());
    let description = lines.next().map(clean).unwrap_or_default();
    let mut tracks = vec![];
    for line in lines.take(MAX_TRACKS) {
        let mut f = line.split('\t');
        let key = f.next().unwrap_or("");
        let (source, uri) = match key.split_at_checked(1) {
            Some(("y", id)) if valid_youtube_id(id) => (SourceKind::YouTube, id.to_string()),
            Some(("s", path)) if valid_soundcloud_path(path) => (SourceKind::SoundCloud, format!("{SOUNDCLOUD}{path}")),
            _ => continue,
        };
        let mut t = Track::new(source, uri);
        t.title = f.next().map(clean).unwrap_or_default();
        t.artist = f.next().map(clean).unwrap_or_default();
        t.duration_ms = f.next().and_then(|d| d.parse::<i64>().ok()).unwrap_or(0).clamp(0, 24 * 3600) * 1000;
        t.album = source.label().into();
        if t.title.is_empty() {
            t.title = t.uri.clone();
        }
        tracks.push(t);
    }
    if tracks.is_empty() {
        bail!("The code contains no songs");
    }
    Ok(Shared { name, description, tracks })
}

/// The id is passed to yt-dlp, so only accept what YouTube ids look like.
fn valid_youtube_id(id: &str) -> bool {
    (6..=20).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_soundcloud_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 300
        && !path.starts_with('-')
        && !path.contains("..")
        && path.chars().all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(source: SourceKind, uri: &str, title: &str) -> Track {
        let mut t = Track::new(source, uri);
        t.title = title.into();
        t.artist = "Artist\twith tab".into();
        t.duration_ms = 215_000;
        t
    }

    #[test]
    fn roundtrip_keeps_only_online_songs() {
        let tracks = vec![
            track(SourceKind::YouTube, "dQw4w9WgXcQ", "Song one"),
            track(SourceKind::Local, r"D:\Music\a.mp3", "Local song"),
            track(SourceKind::SoundCloud, "https://soundcloud.com/artist/track-name", "Song two"),
            track(SourceKind::Nextcloud, "Music/b.flac", "Cloud song"),
        ];
        let (code, included, skipped) = encode("Road trip", "Songs\nfor the car", &tracks);
        assert!(code.starts_with("trax1:"));
        assert_eq!((included, skipped), (2, 2));
        // Chat apps may wrap long lines.
        let wrapped = format!("  {}\n{}  ", &code[..20], &code[20..]);
        let s = decode(&wrapped).unwrap();
        assert_eq!(s.name, "Road trip");
        assert_eq!(s.description, "Songs for the car");
        assert_eq!(s.tracks.len(), 2);
        assert_eq!(s.tracks[0].source, SourceKind::YouTube);
        assert_eq!(s.tracks[0].uri, "dQw4w9WgXcQ");
        assert_eq!(s.tracks[0].artist, "Artist with tab");
        assert_eq!(s.tracks[0].duration_ms, 215_000);
        assert_eq!(s.tracks[1].uri, "https://soundcloud.com/artist/track-name");
    }

    #[test]
    fn rejects_bad_codes() {
        assert!(decode("hello").is_err());
        assert!(decode("trax1:!!!").is_err());
        // Ids that could be read as yt-dlp options or other URLs are dropped.
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
        enc.write_all(b"x\n\ny--exec calc\tA\tB\t1\nshttps://evil\tA\tB\t1\ns../x\tA\tB\t1\n").unwrap();
        let code = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(enc.finish().unwrap()));
        assert!(decode(&code).is_err());
    }
}
