//! SQLite storage: library, plays, playlists, settings and artwork.

use crate::model::{SourceKind, Track};
use crate::social;
use crate::social::friends::{Friend, State as FriendState};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::Path;

const SCHEMA_VERSION: i64 = 7;

pub struct Db {
    conn: Connection,
}

pub struct PlaylistInfo {
    pub id: i64,
    pub name: String,
    pub count: i64,
    pub description: String,
    pub cover: Option<String>,
    pub duration_ms: i64,
    /// The built-in "Favourites" playlist (starred tracks).
    pub favorites: bool,
    /// Downloads of this playlist's songs are kept and never evicted from the cache.
    pub offline: bool,
}

pub struct AutoPlaylist {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub const AUTO_PLAYLISTS: &[AutoPlaylist] = &[
    AutoPlaylist { key: "week", name: "Top this week", description: "Your most played tracks of the last 7 days" },
    AutoPlaylist { key: "month", name: "Top this month", description: "Most played in the last 30 days" },
    AutoPlaylist { key: "year", name: "Top this year", description: "Your most played tracks since January" },
    AutoPlaylist { key: "alltime", name: "All-time favourites", description: "Most played ever" },
    AutoPlaylist { key: "recent", name: "Recently played", description: "Pick up where you left off" },
    AutoPlaylist { key: "added", name: "Recently added", description: "New in your library" },
    AutoPlaylist { key: "longago", name: "Not played in a long time", description: "Haven't heard these in over 3 months" },
    AutoPlaylist { key: "forgotten", name: "Forgotten favourites", description: "Tracks you loved but stopped playing" },
    AutoPlaylist { key: "never", name: "Never played", description: "Give these a first listen" },
];

#[derive(Clone, Default)]
pub struct ArtistRow {
    pub name: String,
    pub spotify_id: Option<String>,
    pub genres: String,
    pub followers: i64,
    pub url: String,
    pub bio: String,
    pub bio_url: String,
    pub match_manual: bool,
    pub bio_manual: bool,
    pub image_manual: bool,
    pub updated_at: i64,
}

pub struct Stats {
    pub ms_week: i64,
    pub ms_year: i64,
    pub ms_total: i64,
    pub plays_week: i64,
    pub track_count: i64,
    pub top_artists_year: Vec<(String, i64)>,
}

const TRACK_COLS: &str =
    "t.id, t.source, t.uri, t.title, t.artist, t.album, t.duration_ms, t.track_no, t.cover, t.play_count, t.start_ms, t.end_ms";

fn row_to_track(r: &Row) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        source: SourceKind::parse(&r.get::<_, String>(1)?),
        uri: r.get(2)?,
        title: r.get(3)?,
        artist: r.get(4)?,
        album: r.get(5)?,
        duration_ms: r.get(6)?,
        track_no: r.get(7)?,
        cover: r.get(8)?,
        play_count: r.get(9)?,
        start_ms: r.get(10)?,
        end_ms: r.get(11)?,
    })
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS tracks (
                    id INTEGER PRIMARY KEY,
                    source TEXT NOT NULL,
                    uri TEXT NOT NULL,
                    title TEXT NOT NULL DEFAULT '',
                    artist TEXT NOT NULL DEFAULT '',
                    album TEXT NOT NULL DEFAULT '',
                    duration_ms INTEGER NOT NULL DEFAULT 0,
                    track_no INTEGER NOT NULL DEFAULT 0,
                    cover TEXT,
                    added_at INTEGER NOT NULL,
                    play_count INTEGER NOT NULL DEFAULT 0,
                    last_played INTEGER,
                    in_library INTEGER NOT NULL DEFAULT 1,
                    mtime INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(source, uri)
                );
                CREATE TABLE IF NOT EXISTS plays (
                    id INTEGER PRIMARY KEY,
                    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                    played_at INTEGER NOT NULL,
                    ms_played INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS plays_time ON plays(played_at);
                CREATE INDEX IF NOT EXISTS plays_track ON plays(track_id);
                CREATE TABLE IF NOT EXISTS playlists (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS playlist_tracks (
                    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL,
                    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS pt_playlist ON playlist_tracks(playlist_id, position);
                CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS artist_art (artist TEXT PRIMARY KEY, path TEXT NOT NULL);",
            )?;
        }
        if version < 2 {
            // Playlist details and user-edited track metadata (kept across rescans).
            self.conn.execute_batch(
                "ALTER TABLE playlists ADD COLUMN description TEXT NOT NULL DEFAULT '';
                 ALTER TABLE playlists ADD COLUMN cover TEXT;
                 ALTER TABLE tracks ADD COLUMN user_edited INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if version < 3 {
            // Artist profiles. The *_manual flags stop automatic updates from overwriting
            // a match, bio or image the user chose.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS artists (
                    name TEXT PRIMARY KEY COLLATE NOCASE,
                    spotify_id TEXT,
                    genres TEXT NOT NULL DEFAULT '',
                    followers INTEGER NOT NULL DEFAULT 0,
                    url TEXT NOT NULL DEFAULT '',
                    bio TEXT NOT NULL DEFAULT '',
                    bio_url TEXT NOT NULL DEFAULT '',
                    match_manual INTEGER NOT NULL DEFAULT 0,
                    bio_manual INTEGER NOT NULL DEFAULT 0,
                    image_manual INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL DEFAULT 0
                );",
            )?;
        }
        if version < 4 {
            // Favourite section of a track (play from start_ms to end_ms) and the favourites playlist.
            self.conn.execute_batch(
                "ALTER TABLE tracks ADD COLUMN start_ms INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE tracks ADD COLUMN end_ms INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE playlists ADD COLUMN kind TEXT NOT NULL DEFAULT 'user';",
            )?;
        }
        if version < 5 {
            // "Keep offline": the playlist's remote songs are downloaded and pinned in the cache.
            self.conn
                .execute_batch("ALTER TABLE playlists ADD COLUMN offline INTEGER NOT NULL DEFAULT 0;")?;
        }
        if version < 6 {
            // Lyrics (from LRCLIB) and measured loudness, both cached per song so they are
            // fetched or computed once. An empty lyrics row means "looked, found nothing".
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS lyrics (
                    key TEXT PRIMARY KEY,
                    synced TEXT NOT NULL DEFAULT '',
                    plain TEXT NOT NULL DEFAULT '',
                    instrumental INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS loudness (
                    source TEXT NOT NULL,
                    uri TEXT NOT NULL,
                    lufs REAL NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (source, uri)
                );",
            )?;
        }
        if version < 7 {
            // Friends (see `social`), and the gift wraps already handled, so stored messages
            // aren't applied again after a restart.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS friends (
                    pubkey TEXT PRIMARY KEY,
                    name TEXT NOT NULL DEFAULT '',
                    state TEXT NOT NULL,
                    their_key TEXT,
                    presence TEXT,
                    added_at INTEGER NOT NULL,
                    changed_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS social_seen (id TEXT PRIMARY KEY, at INTEGER NOT NULL);",
            )?;
        }
        self.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    // ---------- settings ----------

    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn setting_or(&self, key: &str, default: &str) -> String {
        self.get_setting(key).unwrap_or_else(|| default.to_string())
    }

    pub fn setting_bool(&self, key: &str, default: bool) -> bool {
        self.get_setting(key).map(|v| v == "1").unwrap_or(default)
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let _ = self.conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        );
    }

    pub fn set_setting_bool(&self, key: &str, value: bool) {
        self.set_setting(key, if value { "1" } else { "0" });
    }

    // ---------- lyrics and loudness ----------

    /// Cached lyrics for a song: (synced LRC, plain text, instrumental, when it was fetched).
    pub fn lyrics(&self, key: &str) -> Option<(String, String, bool, i64)> {
        self.conn
            .query_row(
                "SELECT synced, plain, instrumental, updated_at FROM lyrics WHERE key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Store what a lookup returned; empty strings record that nothing was found.
    pub fn set_lyrics(&self, key: &str, synced: &str, plain: &str, instrumental: bool) {
        let _ = self.conn.execute(
            "INSERT INTO lyrics(key, synced, plain, instrumental, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET synced = ?2, plain = ?3, instrumental = ?4, updated_at = ?5",
            params![key, synced, plain, instrumental as i64, now()],
        );
    }

    pub fn forget_lyrics(&self, key: &str) {
        let _ = self.conn.execute("DELETE FROM lyrics WHERE key = ?1", [key]);
    }

    /// Measured loudness of a track in LUFS.
    pub fn loudness(&self, source: &str, uri: &str) -> Option<f32> {
        self.conn
            .query_row("SELECT lufs FROM loudness WHERE source = ?1 AND uri = ?2", params![source, uri], |r| {
                r.get::<_, f64>(0)
            })
            .optional()
            .ok()
            .flatten()
            .map(|v| v as f32)
    }

    pub fn set_loudness(&self, source: &str, uri: &str, lufs: f32) {
        let _ = self.conn.execute(
            "INSERT INTO loudness(source, uri, lufs, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(source, uri) DO UPDATE SET lufs = ?3, updated_at = ?4",
            params![source, uri, lufs as f64, now()],
        );
    }

    // ---------- tracks ----------

    /// Insert or update a track by (source, uri). Returns its id. Keeps play statistics.
    pub fn upsert_track(&self, t: &Track, in_library: bool) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO tracks(source, uri, title, artist, album, duration_ms, track_no, cover, added_at, in_library)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(source, uri) DO UPDATE SET
               title = CASE WHEN user_edited THEN title ELSE excluded.title END,
               artist = CASE WHEN user_edited THEN artist ELSE excluded.artist END,
               album = CASE WHEN user_edited THEN album ELSE excluded.album END,
               duration_ms = CASE WHEN excluded.duration_ms > 0 THEN excluded.duration_ms ELSE duration_ms END,
               track_no = excluded.track_no,
               cover = CASE WHEN user_edited THEN cover ELSE COALESCE(excluded.cover, cover) END,
               in_library = MAX(in_library, excluded.in_library)",
            params![
                t.source.as_str(),
                t.uri,
                t.title,
                t.artist,
                t.album,
                t.duration_ms,
                t.track_no,
                t.cover,
                now(),
                in_library as i64
            ],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM tracks WHERE source = ?1 AND uri = ?2",
            params![t.source.as_str(), t.uri],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Ensure a track exists (e.g. a YouTube result being played) without adding it to the library.
    pub fn ensure_track(&self, t: &Track) -> Result<i64> {
        if t.id > 0 {
            return Ok(t.id);
        }
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM tracks WHERE source = ?1 AND uri = ?2",
                params![t.source.as_str(), t.uri],
                |r| r.get(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        self.upsert_track(t, false)
    }

    pub fn update_track_meta(&self, t: &Track) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET
               title = CASE WHEN user_edited THEN title ELSE ?2 END,
               artist = CASE WHEN user_edited THEN artist ELSE ?3 END,
               album = CASE WHEN user_edited THEN album ELSE ?4 END,
               duration_ms = CASE WHEN ?5 > 0 THEN ?5 ELSE duration_ms END,
               track_no = ?6,
               cover = COALESCE(cover, ?7)
             WHERE id = ?1",
            params![t.id, t.title, t.artist, t.album, t.duration_ms, t.track_no, t.cover],
        )?;
        Ok(())
    }

    /// Modification times of local files in the library, keyed by path (for incremental scans).
    pub fn local_mtimes(&self) -> std::collections::HashMap<String, i64> {
        let Ok(mut stmt) = self.conn.prepare("SELECT uri, mtime FROM tracks WHERE source = 'local' AND in_library = 1")
        else {
            return Default::default();
        };
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn set_mtime(&self, id: i64, mtime: i64) -> Result<()> {
        self.conn.execute("UPDATE tracks SET mtime = ?2 WHERE id = ?1", params![id, mtime])?;
        Ok(())
    }

    pub fn set_in_library(&self, id: i64, in_library: bool) -> Result<()> {
        self.conn
            .execute("UPDATE tracks SET in_library = ?2 WHERE id = ?1", params![id, in_library as i64])?;
        Ok(())
    }

    pub fn track(&self, id: i64) -> Option<Track> {
        self.conn
            .query_row(&format!("SELECT {TRACK_COLS} FROM tracks t WHERE t.id = ?1"), [id], row_to_track)
            .optional()
            .ok()
            .flatten()
    }

    /// The track for a file on this PC, from a library folder or opened on its own before.
    pub fn disk_track(&self, path: &str) -> Option<Track> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {TRACK_COLS} FROM tracks t WHERE t.source IN ('local', 'file') AND t.uri = ?1 COLLATE NOCASE
                     ORDER BY t.source = 'local' DESC, t.in_library DESC LIMIT 1"
                ),
                [path],
                row_to_track,
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Single opened files that a library folder now covers are shown once, as the folder's.
    pub fn hide_files_in_folders(&self, scanned: &std::collections::HashSet<String>) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT id, uri FROM tracks WHERE source = 'file' AND in_library = 1")?;
        let rows: Vec<(i64, String)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
        let lower: std::collections::HashSet<String> = scanned.iter().map(|s| s.to_lowercase()).collect();
        let mut hidden = 0;
        for (id, uri) in rows {
            if lower.contains(&uri.to_lowercase()) {
                self.set_in_library(id, false)?;
                hidden += 1;
            }
        }
        Ok(hidden)
    }

    /// Remove library entries of a source whose uri is no longer present (after a rescan).
    /// Tracks still referenced by playlists or plays are kept but hidden from the library.
    pub fn prune_source(&self, source: SourceKind, keep: &std::collections::HashSet<String>, prefix: Option<&str>) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT id, uri FROM tracks WHERE source = ?1 AND in_library = 1")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([source.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        let mut removed = 0;
        for (id, uri) in rows {
            if let Some(p) = prefix {
                if !uri.starts_with(p) {
                    continue;
                }
            }
            if !keep.contains(&uri) {
                self.conn.execute("UPDATE tracks SET in_library = 0 WHERE id = ?1", [id])?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Library source filter; "Local" also covers single opened files.
    fn one_source(s: SourceKind) -> String {
        match s {
            SourceKind::Local => " AND t.source IN ('local', 'file')".into(),
            s => format!(" AND t.source = '{}'", s.as_str()),
        }
    }

    fn source_filter(enabled: &[SourceKind]) -> String {
        if enabled.is_empty() {
            return "0".into();
        }
        let list: Vec<String> = enabled.iter().map(|s| format!("'{}'", s.as_str())).collect();
        format!("t.source IN ({})", list.join(","))
    }

    fn query_tracks(&self, sql: &str, p: impl rusqlite::Params) -> Vec<Track> {
        let Ok(mut stmt) = self.conn.prepare(sql) else {
            log::error!("bad query: {sql}");
            return vec![];
        };
        stmt.query_map(p, row_to_track)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// `sort`: 0 title, 1 artist, 2 album, 3 recently added, 4 most played.
    pub fn library(&self, enabled: &[SourceKind], sort: i32, filter: &str, source: Option<SourceKind>) -> Vec<Track> {
        let order = match sort {
            1 => "t.artist COLLATE NOCASE, t.album COLLATE NOCASE, t.track_no, t.title COLLATE NOCASE",
            2 => "t.album COLLATE NOCASE, t.track_no, t.title COLLATE NOCASE",
            3 => "t.added_at DESC, t.id DESC",
            4 => "t.play_count DESC, t.title COLLATE NOCASE",
            _ => "t.title COLLATE NOCASE",
        };
        let src = match source {
            Some(s) => Self::one_source(s),
            None => String::new(),
        };
        let sql = format!(
            "SELECT {TRACK_COLS} FROM tracks t WHERE t.in_library = 1 AND {}{src}
             AND (?1 = '' OR t.title LIKE ?1 OR t.artist LIKE ?1 OR t.album LIKE ?1)
             ORDER BY {order}",
            Self::source_filter(enabled)
        );
        let pattern = if filter.trim().is_empty() { String::new() } else { format!("%{}%", filter.trim()) };
        self.query_tracks(&sql, [pattern])
    }

    pub fn search(&self, enabled: &[SourceKind], q: &str, limit: i64) -> Vec<Track> {
        let sql = format!(
            "SELECT {TRACK_COLS} FROM tracks t WHERE {} AND (t.in_library = 1 OR t.play_count > 0)
             AND (t.title LIKE ?1 OR t.artist LIKE ?1 OR t.album LIKE ?1)
             ORDER BY t.play_count DESC, t.title COLLATE NOCASE LIMIT ?2",
            Self::source_filter(enabled)
        );
        self.query_tracks(&sql, params![format!("%{}%", q.trim()), limit])
    }

    // ---------- plays & statistics ----------

    pub fn record_play(&self, track_id: i64, ms_played: i64) -> Result<()> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO plays(track_id, played_at, ms_played) VALUES (?1, ?2, ?3)",
            params![track_id, ts, ms_played],
        )?;
        self.conn.execute(
            "UPDATE tracks SET play_count = play_count + 1, last_played = ?2 WHERE id = ?1",
            params![track_id, ts],
        )?;
        Ok(())
    }

    fn start_of_year() -> i64 {
        use chrono::{Datelike, Local, TimeZone};
        let now = Local::now();
        Local
            .with_ymd_and_hms(now.year(), 1, 1, 0, 0, 0)
            .single()
            .map(|d| d.timestamp())
            .unwrap_or(0)
    }

    fn ms_since(&self, since: i64) -> i64 {
        self.conn
            .query_row("SELECT COALESCE(SUM(ms_played), 0) FROM plays WHERE played_at >= ?1", [since], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn stats(&self) -> Stats {
        let week = now() - 7 * 86400;
        let year = Self::start_of_year();
        let mut top = vec![];
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT t.artist, SUM(p.ms_played) AS ms FROM plays p JOIN tracks t ON t.id = p.track_id
             WHERE p.played_at >= ?1 AND t.artist != '' GROUP BY t.artist ORDER BY ms DESC LIMIT 50",
        ) {
            if let Ok(rows) = stmt.query_map([year], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                top = rows.filter_map(|r| r.ok()).collect();
            }
        }
        Stats {
            ms_week: self.ms_since(week),
            ms_year: self.ms_since(year),
            ms_total: self.ms_since(0),
            plays_week: self
                .conn
                .query_row("SELECT COUNT(*) FROM plays WHERE played_at >= ?1", [week], |r| r.get(0))
                .unwrap_or(0),
            track_count: self
                .conn
                .query_row("SELECT COUNT(*) FROM tracks WHERE in_library = 1", [], |r| r.get(0))
                .unwrap_or(0),
            top_artists_year: top,
        }
    }

    pub fn auto_playlist(&self, key: &str, enabled: &[SourceKind]) -> Vec<Track> {
        let src = Self::source_filter(enabled);
        let top_since = |since: i64| {
            let sql = format!(
                "SELECT {TRACK_COLS} FROM tracks t JOIN plays p ON p.track_id = t.id
                 WHERE p.played_at >= ?1 AND {src}
                 GROUP BY t.id ORDER BY COUNT(p.id) DESC, SUM(p.ms_played) DESC LIMIT 50"
            );
            self.query_tracks(&sql, [since])
        };
        match key {
            "week" => top_since(now() - 7 * 86400),
            "month" => top_since(now() - 30 * 86400),
            "year" => top_since(Self::start_of_year()),
            "alltime" => self.query_tracks(
                &format!("SELECT {TRACK_COLS} FROM tracks t WHERE t.play_count > 0 AND {src} ORDER BY t.play_count DESC LIMIT 100"),
                [],
            ),
            "recent" => self.query_tracks(
                &format!("SELECT {TRACK_COLS} FROM tracks t WHERE t.last_played IS NOT NULL AND {src} ORDER BY t.last_played DESC LIMIT 50"),
                [],
            ),
            "added" => self.query_tracks(
                &format!("SELECT {TRACK_COLS} FROM tracks t WHERE t.in_library = 1 AND {src} ORDER BY t.added_at DESC, t.id DESC LIMIT 100"),
                [],
            ),
            "longago" => self.query_tracks(
                &format!(
                    "SELECT {TRACK_COLS} FROM tracks t WHERE t.last_played < ?1 AND {src}
                     ORDER BY t.last_played ASC LIMIT 100"
                ),
                [now() - 90 * 86400],
            ),
            "forgotten" => self.query_tracks(
                &format!(
                    "SELECT {TRACK_COLS} FROM tracks t WHERE t.play_count >= 3 AND t.last_played < ?1 AND {src}
                     ORDER BY t.play_count DESC LIMIT 50"
                ),
                [now() - 60 * 86400],
            ),
            "never" => self.query_tracks(
                &format!("SELECT {TRACK_COLS} FROM tracks t WHERE t.play_count = 0 AND t.in_library = 1 AND {src} ORDER BY RANDOM() LIMIT 100"),
                [],
            ),
            _ => vec![],
        }
    }

    // ---------- playlists ----------

    pub fn playlists(&self) -> Vec<PlaylistInfo> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT p.id, p.name, COUNT(pt.track_id), p.description, p.cover, COALESCE(SUM(t.duration_ms), 0), p.kind, p.offline
             FROM playlists p
             LEFT JOIN playlist_tracks pt ON pt.playlist_id = p.id
             LEFT JOIN tracks t ON t.id = pt.track_id
             GROUP BY p.id ORDER BY p.kind = 'favorites' DESC, p.name COLLATE NOCASE",
        ) else {
            return vec![];
        };
        stmt.query_map([], |r| {
            Ok(PlaylistInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                count: r.get(2)?,
                description: r.get(3)?,
                cover: r.get(4)?,
                duration_ms: r.get(5)?,
                favorites: r.get::<_, String>(6)? == "favorites",
                offline: r.get::<_, i64>(7)? != 0,
            })
        })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn playlist_name(&self, id: i64) -> Option<String> {
        self.conn
            .query_row("SELECT name FROM playlists WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// New playlists start out kept offline while the "always keep offline" setting is on.
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        let ts = now();
        let offline = self.setting_bool("offline_all", false);
        self.conn.execute(
            "INSERT INTO playlists(name, created_at, updated_at, offline) VALUES (?1, ?2, ?2, ?3)",
            params![name, ts, offline as i64],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn playlist(&self, id: i64) -> Option<PlaylistInfo> {
        self.playlists().into_iter().find(|p| p.id == id)
    }

    pub fn set_playlist_details(&self, id: i64, name: &str, description: &str, cover: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE playlists SET name = ?2, description = ?3, cover = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, name, description, cover, now()],
        )?;
        Ok(())
    }

    /// Save user-edited metadata; it then survives rescans and tag reads.
    pub fn edit_track(&self, id: i64, title: &str, artist: &str, album: &str, cover: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET title = ?2, artist = ?3, album = ?4, cover = ?5, user_edited = 1 WHERE id = ?1",
            params![id, title, artist, album, cover],
        )?;
        Ok(())
    }

    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        // The favourites playlist always exists.
        self.conn.execute("DELETE FROM playlists WHERE id = ?1 AND kind != 'favorites'", [id])?;
        Ok(())
    }

    /// Id of the favourites playlist, created on first use.
    pub fn favorites_id(&self) -> Result<i64> {
        if let Some(id) = self
            .conn
            .query_row("SELECT id FROM playlists WHERE kind = 'favorites' LIMIT 1", [], |r| r.get(0))
            .optional()?
        {
            return Ok(id);
        }
        let ts = now();
        self.conn.execute(
            "INSERT INTO playlists(name, created_at, updated_at, kind) VALUES ('Favourites', ?1, ?1, 'favorites')",
            [ts],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn favorite_ids(&self) -> std::collections::HashSet<i64> {
        let Ok(fav) = self.favorites_id() else { return Default::default() };
        let Ok(mut stmt) = self.conn.prepare("SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1") else {
            return Default::default();
        };
        stmt.query_map([fav], |r| r.get(0)).map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    /// Star or unstar a track. Returns the new state.
    pub fn set_favorite(&self, track_id: i64, on: bool) -> Result<bool> {
        let fav = self.favorites_id()?;
        self.conn.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2", params![fav, track_id])?;
        if on {
            self.add_to_playlist(fav, &[track_id])?;
        }
        self.conn.execute("UPDATE playlists SET updated_at = ?2 WHERE id = ?1", params![fav, now()])?;
        Ok(on)
    }

    /// Favourite section of a track; 0 means "from the start" / "to the end".
    pub fn set_section(&self, track_id: i64, start_ms: i64, end_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET start_ms = ?2, end_ms = ?3 WHERE id = ?1",
            params![track_id, start_ms.max(0), end_ms.max(0)],
        )?;
        Ok(())
    }

    // ---------- keep offline ----------

    pub fn set_playlist_offline(&self, id: i64, on: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE playlists SET offline = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, on as i64, now()],
        )?;
        Ok(())
    }

    /// Every track of every playlist kept offline, each one once.
    pub fn offline_tracks(&self) -> Vec<Track> {
        self.query_tracks(
            &format!(
                "SELECT DISTINCT {TRACK_COLS} FROM playlist_tracks pt
                 JOIN playlists p ON p.id = pt.playlist_id
                 JOIN tracks t ON t.id = pt.track_id
                 WHERE p.offline = 1"
            ),
            [],
        )
    }

    /// Keep every playlist offline. Returns how many changed.
    pub fn set_all_playlists_offline(&self) -> Result<usize> {
        Ok(self.conn.execute("UPDATE playlists SET offline = 1 WHERE offline = 0", [])?)
    }

    /// Stop keeping every playlist offline. Returns how many were kept offline.
    pub fn clear_offline_playlists(&self) -> Result<usize> {
        Ok(self.conn.execute("UPDATE playlists SET offline = 0 WHERE offline = 1", [])?)
    }

    pub fn playlist_tracks(&self, id: i64) -> Vec<Track> {
        self.query_tracks(
            &format!(
                "SELECT {TRACK_COLS} FROM playlist_tracks pt JOIN tracks t ON t.id = pt.track_id
                 WHERE pt.playlist_id = ?1 ORDER BY pt.position"
            ),
            [id],
        )
    }

    pub fn add_to_playlist(&self, playlist: i64, track_ids: &[i64]) -> Result<()> {
        let mut pos: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_tracks WHERE playlist_id = ?1",
            [playlist],
            |r| r.get(0),
        )?;
        for id in track_ids {
            self.conn.execute(
                "INSERT INTO playlist_tracks(playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
                params![playlist, pos, id],
            )?;
            pos += 1;
        }
        self.conn.execute("UPDATE playlists SET updated_at = ?2 WHERE id = ?1", params![playlist, now()])?;
        Ok(())
    }

    /// Rewrite the full order of a playlist.
    pub fn set_playlist_order(&mut self, playlist: i64, track_ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", [playlist])?;
        for (i, id) in track_ids.iter().enumerate() {
            tx.execute(
                "INSERT INTO playlist_tracks(playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
                params![playlist, i as i64, id],
            )?;
        }
        tx.execute("UPDATE playlists SET updated_at = ?2 WHERE id = ?1", params![playlist, now()])?;
        tx.commit()?;
        Ok(())
    }

    // ---------- artwork ----------

    pub fn artist_art(&self, artist: &str) -> Option<String> {
        self.conn
            .query_row("SELECT path FROM artist_art WHERE artist = ?1 COLLATE NOCASE", [artist], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    // ---------- artists ----------

    pub fn artist(&self, name: &str) -> Option<ArtistRow> {
        self.conn
            .query_row(
                "SELECT name, spotify_id, genres, followers, url, bio, bio_url, match_manual, bio_manual, image_manual, updated_at
                 FROM artists WHERE name = ?1",
                [name],
                |r| {
                    Ok(ArtistRow {
                        name: r.get(0)?,
                        spotify_id: r.get(1)?,
                        genres: r.get(2)?,
                        followers: r.get(3)?,
                        url: r.get(4)?,
                        bio: r.get(5)?,
                        bio_url: r.get(6)?,
                        match_manual: r.get::<_, i64>(7)? != 0,
                        bio_manual: r.get::<_, i64>(8)? != 0,
                        image_manual: r.get::<_, i64>(9)? != 0,
                        updated_at: r.get(10)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn save_artist(&self, a: &ArtistRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO artists(name, spotify_id, genres, followers, url, bio, bio_url, match_manual, bio_manual, image_manual, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(name) DO UPDATE SET spotify_id = ?2, genres = ?3, followers = ?4, url = ?5, bio = ?6,
               bio_url = ?7, match_manual = ?8, bio_manual = ?9, image_manual = ?10, updated_at = ?11",
            params![
                a.name,
                a.spotify_id,
                a.genres,
                a.followers,
                a.url,
                a.bio,
                a.bio_url,
                a.match_manual as i64,
                a.bio_manual as i64,
                a.image_manual as i64,
                a.updated_at
            ],
        )?;
        Ok(())
    }

    /// Tracks by an artist, including features ("A feat. B", "A, B", "A & B").
    pub fn artist_tracks(&self, name: &str, enabled: &[SourceKind]) -> Vec<Track> {
        let sql = format!(
            "SELECT {TRACK_COLS} FROM tracks t WHERE (t.in_library = 1 OR t.play_count > 0) AND {}
             AND (t.artist = ?1 COLLATE NOCASE
               OR t.artist LIKE ?1 || ',%' OR t.artist LIKE ?1 || ' feat%' OR t.artist LIKE ?1 || ' ft%'
               OR t.artist LIKE ?1 || ';%' OR t.artist LIKE '% feat. ' || ?1 OR t.artist LIKE '% ft. ' || ?1
               OR t.artist LIKE '%, ' || ?1 OR t.artist LIKE '%, ' || ?1 || ',%')
             ORDER BY t.play_count DESC, t.album COLLATE NOCASE, t.track_no, t.title COLLATE NOCASE",
            Self::source_filter(enabled)
        );
        self.query_tracks(&sql, [name])
    }

    /// (plays, milliseconds listened) for the given tracks.
    pub fn plays_for(&self, ids: &[i64]) -> (i64, i64) {
        if ids.is_empty() {
            return (0, 0);
        }
        let list: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        self.conn
            .query_row(
                &format!("SELECT COUNT(*), COALESCE(SUM(ms_played), 0) FROM plays WHERE track_id IN ({})", list.join(",")),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0))
    }

    /// Distinct artist strings in the library with their track counts.
    pub fn library_artists(&self, enabled: &[SourceKind], source: Option<SourceKind>) -> Vec<(String, i64)> {
        let src = source.map(Self::one_source).unwrap_or_default();
        let sql = format!(
            "SELECT t.artist, COUNT(*) FROM tracks t WHERE t.in_library = 1 AND t.artist != '' AND {}{src}
             GROUP BY t.artist COLLATE NOCASE",
            Self::source_filter(enabled)
        );
        let Ok(mut stmt) = self.conn.prepare(&sql) else { return vec![] };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn tracks_missing_cover(&self, enabled: &[SourceKind], limit: i64) -> Vec<Track> {
        let sql = format!(
            "SELECT {TRACK_COLS} FROM tracks t WHERE t.cover IS NULL AND (t.in_library = 1 OR t.play_count > 0) AND {}
             ORDER BY t.play_count DESC LIMIT ?1",
            Self::source_filter(enabled)
        );
        self.query_tracks(&sql, [limit])
    }

    /// Fill in artwork found online. Never replaces an existing cover; the album name is
    /// only filled when it is empty or a placeholder.
    /// `placeholder` is a cover that may be replaced (a YouTube video thumbnail).
    pub fn set_found_cover(&self, id: i64, cover: &str, album: Option<&str>, placeholder: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET cover = ?2 WHERE id = ?1 AND (cover IS NULL OR cover = ?3)",
            params![id, cover, placeholder],
        )?;
        if let Some(album) = album {
            self.conn.execute(
                "UPDATE tracks SET album = ?2 WHERE id = ?1 AND user_edited = 0 AND (album = '' OR album IN ('YouTube', 'SoundCloud'))",
                params![id, album],
            )?;
        }
        Ok(())
    }

    pub fn delete_artist_art(&self, artist: &str) {
        let _ = self.conn.execute("DELETE FROM artist_art WHERE artist = ?1 COLLATE NOCASE", [artist]);
    }

    pub fn set_artist_art(&self, artist: &str, path: &str) {
        let _ = self.conn.execute(
            "INSERT INTO artist_art(artist, path) VALUES (?1, ?2) ON CONFLICT(artist) DO UPDATE SET path = ?2",
            params![artist, path],
        );
    }

    // ---------- friends ----------

    pub fn friends(&self) -> Vec<Friend> {
        let Ok(mut st) = self
            .conn
            .prepare("SELECT pubkey, name, state, their_key, presence, added_at, changed_at FROM friends")
        else {
            return vec![];
        };
        st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .map(|rows| {
            rows.filter_map(|r| r.ok())
                .filter_map(|(pubkey, name, state, key, presence, added, changed)| {
                    Some(Friend {
                        pubkey,
                        name,
                        state: FriendState::parse(&state)?,
                        their_key: key.as_deref().and_then(social::presence::key_from_string),
                        presence: presence.as_deref().and_then(|p| serde_json::from_str(p).ok()),
                        added,
                        changed,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn save_friend(&self, f: &Friend) {
        let _ = self.conn.execute(
            "INSERT INTO friends(pubkey, name, state, their_key, presence, added_at, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(pubkey) DO UPDATE SET name = ?2, state = ?3, their_key = ?4, presence = ?5,
                 added_at = ?6, changed_at = ?7",
            params![
                f.pubkey,
                f.name,
                f.state.as_str(),
                f.their_key.as_ref().map(social::presence::key_to_string),
                f.presence.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                f.added,
                f.changed,
            ],
        );
    }

    pub fn delete_friend(&self, pubkey: &str) {
        let _ = self.conn.execute("DELETE FROM friends WHERE pubkey = ?1", [pubkey]);
    }

    /// Remember a handled gift wrap. False if it was handled before.
    pub fn mark_seen(&self, id: &str, now: i64) -> bool {
        self.conn
            .execute("INSERT OR IGNORE INTO social_seen(id, at) VALUES (?1, ?2)", params![id, now])
            .map(|n| n == 1)
            .unwrap_or(true)
    }

    pub fn prune_seen(&self, before: i64) {
        let _ = self.conn.execute("DELETE FROM social_seen WHERE at < ?1", [before]);
    }

    // ---------- maintenance ----------

    /// Write a consistent copy of the database to `dest` (used by the export).
    pub fn snapshot_to(&self, dest: &Path) -> Result<()> {
        let _ = std::fs::remove_file(dest);
        self.conn.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
        Ok(())
    }

    /// Replace a path prefix for local tracks and stored file paths (after moving to another PC).
    pub fn remap_paths(&self, from: &str, to: &str) -> Result<usize> {
        if from.is_empty() {
            return Ok(0);
        }
        let n = self.conn.execute(
            "UPDATE tracks SET uri = ?2 || substr(uri, length(?1) + 1)
             WHERE source = 'local' AND substr(uri, 1, length(?1)) = ?1",
            params![from, to],
        )?;
        if let Some(folders) = self.get_setting("local_folders") {
            let updated: Vec<String> = folders
                .lines()
                .map(|f| if let Some(rest) = f.strip_prefix(from) { format!("{to}{rest}") } else { f.to_string() })
                .collect();
            self.set_setting("local_folders", &updated.join("\n"));
        }
        Ok(n)
    }

    /// Covers and artwork are stored as absolute paths; re-point them at the current data dir.
    pub fn rebase_data_paths(&self, data_dir: &str) -> Result<()> {
        for (table, col) in [("tracks", "cover"), ("artist_art", "path"), ("playlists", "cover")] {
            let mut stmt = self.conn.prepare(&format!("SELECT rowid, {col} FROM {table} WHERE {col} IS NOT NULL"))?;
            let rows: Vec<(i64, String)> =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
            for (rowid, path) in rows {
                let norm = path.replace('\\', "/");
                for marker in ["/covers/", "/artwork/"] {
                    if let Some(idx) = norm.rfind(marker) {
                        let rebased = Path::new(data_dir).join(&norm[idx + 1..]);
                        self.conn.execute(
                            &format!("UPDATE {table} SET {col} = ?2 WHERE rowid = ?1"),
                            params![rowid, rebased.to_string_lossy()],
                        )?;
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn track(db: &Db, source: SourceKind, uri: &str) -> i64 {
        db.upsert_track(&Track::new(source, uri), true).unwrap()
    }

    #[test]
    fn friends_and_seen() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut f = Friend {
            pubkey: "ab".into(),
            name: "Alex".into(),
            state: FriendState::PendingOut,
            their_key: None,
            presence: None,
            added: 1,
            changed: 1,
        };
        db.save_friend(&f);
        assert_eq!(db.friends(), vec![f.clone()]);
        f.state = FriendState::Friend;
        f.their_key = Some([7; 32]);
        f.presence = Some(social::presence::Presence { name: "Alex".into(), online: true, updated: 5, ..Default::default() });
        db.save_friend(&f);
        assert_eq!(db.friends(), vec![f]);
        db.delete_friend("ab");
        assert!(db.friends().is_empty());

        assert!(db.mark_seen("e1", 10));
        assert!(!db.mark_seen("e1", 11));
        db.prune_seen(20);
        assert!(db.mark_seen("e1", 21));
    }

    #[test]
    fn offline_playlists() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let yt = track(&db, SourceKind::YouTube, "video1");
        let nc = track(&db, SourceKind::Nextcloud, "/Music/a.mp3");
        let local = track(&db, SourceKind::Local, r"D:\Music\b.mp3");

        let kept = db.create_playlist("Road trip").unwrap();
        db.add_to_playlist(kept, &[yt, nc, local]).unwrap();
        let other = db.create_playlist("Streaming only").unwrap();
        db.add_to_playlist(other, &[yt]).unwrap();

        // Nothing is kept offline until it is switched on.
        assert!(db.offline_tracks().is_empty());
        assert!(!db.playlist(kept).unwrap().offline);

        db.set_playlist_offline(kept, true).unwrap();
        assert!(db.playlist(kept).unwrap().offline);
        let ids: HashSet<i64> = db.offline_tracks().iter().map(|t| t.id).collect();
        assert_eq!(ids, HashSet::from([yt, nc, local]), "every source of the playlist counts");

        // A song in two playlists is listed once.
        db.set_playlist_offline(other, true).unwrap();
        assert_eq!(db.offline_tracks().iter().filter(|t| t.id == yt).count(), 1);

        assert_eq!(db.clear_offline_playlists().unwrap(), 2);
        assert!(db.offline_tracks().is_empty());
    }

    /// With "always keep playlists offline" on, playlists made later are kept offline too.
    #[test]
    fn offline_all_applies_to_new_playlists() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let before = db.create_playlist("Made earlier").unwrap();
        assert!(!db.playlist(before).unwrap().offline);

        db.set_setting_bool("offline_all", true);
        db.set_all_playlists_offline().unwrap();
        let after = db.create_playlist("Made later").unwrap();
        assert!(db.playlist(before).unwrap().offline);
        assert!(db.playlist(after).unwrap().offline);

        db.set_setting_bool("offline_all", false);
        db.clear_offline_playlists().unwrap();
        let last = db.create_playlist("Made last").unwrap();
        assert!(!db.playlist(last).unwrap().offline);
    }
}
