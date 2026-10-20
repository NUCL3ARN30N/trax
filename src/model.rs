//! Core data types shared by the database, connectors and UI.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Local,
    Nextcloud,
    YouTube,
    SoundCloud,
    /// A single file opened from anywhere ("Open with trax"), not part of a library folder.
    File,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Local => "local",
            SourceKind::Nextcloud => "nextcloud",
            SourceKind::YouTube => "youtube",
            SourceKind::SoundCloud => "soundcloud",
            SourceKind::File => "file",
        }
    }

    pub fn parse(s: &str) -> SourceKind {
        match s {
            "nextcloud" => SourceKind::Nextcloud,
            "youtube" => SourceKind::YouTube,
            "soundcloud" => SourceKind::SoundCloud,
            "file" => SourceKind::File,
            _ => SourceKind::Local,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Local => "Local",
            SourceKind::Nextcloud => "Nextcloud",
            SourceKind::YouTube => "YouTube",
            SourceKind::SoundCloud => "SoundCloud",
            SourceKind::File => "Local",
        }
    }

    /// Streamed through yt-dlp: kept out of the library until played, starred or saved.
    pub fn is_online(self) -> bool {
        matches!(self, SourceKind::YouTube | SourceKind::SoundCloud)
    }

    /// A file on this PC (library folder or opened on its own).
    pub fn is_on_disk(self) -> bool {
        matches!(self, SourceKind::Local | SourceKind::File)
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    /// Database id, 0 if the track is not stored yet (e.g. a YouTube search result).
    pub id: i64,
    pub source: SourceKind,
    /// Local path, WebDAV path relative to the user's files root, or YouTube video id.
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: i64,
    pub track_no: i64,
    pub cover: Option<String>,
    pub play_count: i64,
    /// Favourite section: play from `start_ms` until `end_ms` (0 = whole track).
    pub start_ms: i64,
    pub end_ms: i64,
}

impl Track {
    pub fn new(source: SourceKind, uri: impl Into<String>) -> Self {
        Track {
            id: 0,
            source,
            uri: uri.into(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 0,
            track_no: 0,
            cover: None,
            play_count: 0,
            start_ms: 0,
            end_ms: 0,
        }
    }

    pub fn display_artist(&self) -> &str {
        if self.artist.is_empty() { "Unknown artist" } else { &self.artist }
    }
}

pub fn format_duration(ms: i64) -> String {
    if ms <= 0 {
        return "--:--".into();
    }
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn format_hours(ms: i64) -> String {
    let h = ms as f64 / 3_600_000.0;
    if h < 1.0 {
        format!("{} min", (ms / 60_000).max(0))
    } else {
        format!("{h:.1} h")
    }
}

/// The main artist of an artist string: "A feat. B" / "A ft. B" / "A; B" / "A, B" -> "A".
/// "&" and "x" are left alone: they are part of many band names (Simon & Garfunkel).
/// Names that contain a comma themselves are handled by the caller (see `App::main_artist`).
pub fn primary_artist(artist: &str) -> String {
    let lower = artist.to_lowercase();
    let mut cut = artist.len();
    for sep in [" feat. ", " feat ", " ft. ", " ft ", " featuring ", "; ", ", "] {
        if let Some(i) = lower.find(sep) {
            cut = cut.min(i);
        }
    }
    artist[..cut].trim().to_string()
}

/// Strip video noise from a title for online lookups: "(Official Video)", "[HD]", "ft. X", …
pub fn clean_title(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let lower = out.to_lowercase();
    let cut = [" feat.", " ft.", " feat ", " | "].iter().filter_map(|s| lower.find(s)).min().unwrap_or(out.len());
    out[..cut].trim().trim_matches('-').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artist_and_title_cleanup() {
        assert_eq!(primary_artist("Megadeth"), "Megadeth");
        assert_eq!(primary_artist("Calvin Harris feat. Rihanna"), "Calvin Harris");
        assert_eq!(primary_artist("Simon & Garfunkel"), "Simon & Garfunkel");
        assert_eq!(primary_artist("A; B"), "A");
        assert_eq!(primary_artist("Expulze, Narfos"), "Expulze");
        assert_eq!(clean_title("Symphony Of Destruction (Official Video) [HD]"), "Symphony Of Destruction");
        assert_eq!(clean_title("Song ft. Other"), "Song");
    }
}
