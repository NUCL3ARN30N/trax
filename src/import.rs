//! Importing playlists by their link: YouTube / YouTube Music playlists through yt-dlp, and
//! Spotify playlists and albums, whose songs are then looked up on YouTube.

use crate::connectors::{save_cover, spotify, youtube};
use crate::model::Track;
use anyhow::{Result, bail};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_TRACKS: usize = 5000;
/// yt-dlp searches running at the same time while matching Spotify songs.
const SEARCHES: usize = 4;

#[derive(Debug, PartialEq)]
pub enum Link {
    /// A playlist id (`list=` in the link).
    YouTube(String),
    Spotify { kind: &'static str, id: String },
}

/// Ok(None) when the text isn't a link to one of these services (it may be a share code).
pub fn parse_link(text: &str) -> Result<Option<Link>> {
    let text = text.trim().trim_matches(|c| c == '"' || c == '\'' || c == '<' || c == '>');
    if text.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("spotify:")) {
        let mut parts = text[8..].split(':');
        let (kind, id) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        return spotify_link(&kind.to_ascii_lowercase(), id).map(Some);
    }
    let rest = text.split_once("://").map(|(_, r)| r).unwrap_or(text);
    let (host, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let host = host.to_ascii_lowercase();
    match host.strip_prefix("www.").unwrap_or(&host) {
        "open.spotify.com" | "play.spotify.com" => {
            let path = path.split(['?', '#']).next().unwrap_or("");
            let mut segments = path.split('/').filter(|s| !s.is_empty() && !s.starts_with("intl-"));
            let kind = segments.next().unwrap_or("").to_ascii_lowercase();
            spotify_link(&kind, segments.next().unwrap_or("")).map(Some)
        }
        "spotify.link" | "spotify.app.link" => {
            bail!("Open this short link in your browser and paste the open.spotify.com link it leads to")
        }
        "youtube.com" | "music.youtube.com" | "m.youtube.com" | "youtu.be" => {
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
            let list = query.split(['&', '#']).find_map(|p| p.strip_prefix("list="));
            // YouTube Music opens playlists as /browse/VL<id>.
            let browse = path.split(['?', '#']).next().unwrap_or("").strip_prefix("/browse/VL");
            match list.or(browse) {
                Some(id) if valid_youtube_list(id) => Ok(Some(Link::YouTube(id.to_string()))),
                _ => bail!("This YouTube link isn't a playlist (it has no “list=” part)"),
            }
        }
        _ => Ok(None),
    }
}

fn spotify_link(kind: &str, id: &str) -> Result<Link> {
    let kind = match kind {
        "playlist" => "playlist",
        "album" => "album",
        _ => bail!("Only Spotify playlists and albums can be imported"),
    };
    // Spotify ids are 22 characters of base62.
    if id.len() != 22 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("This Spotify link is incomplete");
    }
    Ok(Link::Spotify { kind, id: id.to_string() })
}

/// The id is passed to yt-dlp, so only accept what playlist ids look like.
fn valid_youtube_list(id: &str) -> bool {
    (10..=80).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub struct Imported {
    pub name: String,
    pub description: String,
    pub cover: Option<String>,
    pub tracks: Vec<Track>,
    /// Songs (Spotify) for which no YouTube video was found, as "Artist – Title".
    pub missing: Vec<String>,
}

/// Read the playlist and, for Spotify, find each song on YouTube. `progress(done, total)` is
/// called while matching. Runs for a while, so call it off the UI thread.
pub fn fetch(ytdlp: &str, link: &Link, progress: impl Fn(usize, usize) + Sync) -> Result<Imported> {
    if youtube::version(ytdlp).is_none() {
        bail!("{}", youtube::not_found_hint(ytdlp));
    }
    match link {
        Link::YouTube(list) => {
            let p = youtube::playlist(ytdlp, list, MAX_TRACKS)?;
            if p.tracks.is_empty() {
                bail!("The playlist is empty or private");
            }
            Ok(Imported {
                name: if p.name.is_empty() { "YouTube playlist".into() } else { p.name },
                description: p.description,
                cover: p.cover_urls.iter().find_map(|url| download_cover(&format!("ytpl:{list}"), url)),
                tracks: p.tracks,
                missing: vec![],
            })
        }
        Link::Spotify { kind, id } => {
            let list = spotify::list(kind, id)?;
            if list.tracks.is_empty() {
                bail!("The {kind} is empty or private");
            }
            let found = match_on_youtube(ytdlp, &list.tracks, &progress)?;
            let mut tracks = vec![];
            let mut missing = vec![];
            for (song, t) in list.tracks.iter().zip(found) {
                match t {
                    Some(t) => tracks.push(t),
                    None => missing.push(format!("{} – {}", song.artists, song.title)),
                }
            }
            if tracks.is_empty() {
                bail!("None of the songs were found on YouTube");
            }
            Ok(Imported {
                name: if list.name.is_empty() { "Spotify playlist".into() } else { list.name },
                description: if list.owner.is_empty() { String::new() } else { format!("From Spotify, by {}", list.owner) },
                cover: list.cover_url.and_then(|url| download_cover(&format!("sp:{id}"), &url)),
                tracks,
                missing,
            })
        }
    }
}

/// One YouTube video per song (None if nothing fits), in the songs' order.
fn match_on_youtube(ytdlp: &str, songs: &[spotify::ListTrack], progress: &(impl Fn(usize, usize) + Sync)) -> Result<Vec<Option<Track>>> {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<Track>>> = songs.iter().map(|_| Mutex::new(None)).collect();
    let error = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..SEARCHES.min(songs.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(song) = songs.get(i) else { break };
                    match youtube::find_song(ytdlp, &song.artists, &song.title, song.duration_ms) {
                        Ok(t) => *results[i].lock().unwrap() = t,
                        Err(e) => {
                            log::warn!("no YouTube match for {} – {}: {e:#}", song.artists, song.title);
                            error.lock().unwrap().get_or_insert(e);
                        }
                    }
                    progress(done.fetch_add(1, Ordering::Relaxed) + 1, songs.len());
                }
            });
        }
    });
    let results: Vec<Option<Track>> = results.into_iter().map(|m| m.into_inner().unwrap()).collect();
    // Every search failing means yt-dlp itself is broken, not that the songs are missing.
    if results.iter().all(Option::is_none) {
        if let Some(e) = error.into_inner().unwrap() {
            return Err(e);
        }
    }
    Ok(results)
}

fn download_cover(key: &str, url: &str) -> Option<String> {
    let bytes = spotify::download(url).ok()?;
    save_cover(key, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(s: &str) -> Option<Link> {
        parse_link(s).unwrap()
    }

    #[test]
    fn links() {
        let sp = |kind, id: &str| Some(Link::Spotify { kind, id: id.to_string() });
        assert_eq!(link("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc"), sp("playlist", "37i9dQZF1DXcBWIGoYBM5M"));
        assert_eq!(link("open.spotify.com/intl-de/album/4aawyAB9vmqN3uQ7FjRGTy"), sp("album", "4aawyAB9vmqN3uQ7FjRGTy"));
        assert_eq!(link("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"), sp("playlist", "37i9dQZF1DXcBWIGoYBM5M"));
        assert_eq!(
            link(" https://music.youtube.com/playlist?list=RDCLAK5uy_kmPRjHDECIcuVwnKsx2Ng7fyNgFKWNJFs&si=x "),
            Some(Link::YouTube("RDCLAK5uy_kmPRjHDECIcuVwnKsx2Ng7fyNgFKWNJFs".into()))
        );
        assert_eq!(
            link("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLx0sYbCqOb8TBPRdmBHs5Iftvv9TPboYG"),
            Some(Link::YouTube("PLx0sYbCqOb8TBPRdmBHs5Iftvv9TPboYG".into()))
        );
        assert_eq!(
            link("https://music.youtube.com/browse/VLPLx0sYbCqOb8TBPRdmBHs5Iftvv9TPboYG"),
            Some(Link::YouTube("PLx0sYbCqOb8TBPRdmBHs5Iftvv9TPboYG".into()))
        );
        // Share codes and other text are left to the share code import.
        assert_eq!(link("trax1:abc"), None);
        assert_eq!(link("https://example.com/playlist?list=PLx0sYbCqOb8TBPRdmBHs5Iftvv9TPboYG"), None);
        // Links to these services that can't be imported say why.
        assert!(parse_link("https://open.spotify.com/track/2sFepQNyl1WjpROruP8bJ0").is_err());
        assert!(parse_link("https://open.spotify.com/playlist/short").is_err());
        assert!(parse_link("https://spotify.link/abcdef").is_err());
        assert!(parse_link("https://www.youtube.com/watch?v=dQw4w9WgXcQ").is_err());
        assert!(parse_link("https://www.youtube.com/playlist?list=--exec%20calc").is_err());
    }

    /// Set YTDLP to the yt-dlp to use, if it isn't on PATH.
    #[test]
    #[ignore = "needs the internet and yt-dlp"]
    fn imports_real_playlists() {
        crate::paths::use_test_dir();
        let bin = std::env::var("YTDLP").unwrap_or_default();
        for url in [
            "https://open.spotify.com/album/4aawyAB9vmqN3uQ7FjRGTy",
            "https://music.youtube.com/playlist?list=RDCLAK5uy_kmPRjHDECIcuVwnKsx2Ng7fyNgFKWNJFs",
        ] {
            let p = fetch(&bin, &link(url).unwrap(), |done, total| println!("{done}/{total}")).unwrap();
            println!("{} ({:?}): {} songs, missing {:?}", p.name, p.cover, p.tracks.len(), p.missing);
            for t in p.tracks.iter().take(5) {
                println!("  {} – {} [{}] {} s", t.artist, t.title, t.uri, t.duration_ms / 1000);
            }
            assert!(!p.tracks.is_empty());
        }
    }
}
