//! Size limit for the audio cache (YouTube / Nextcloud downloads).
//!
//! A cached file's modification time is bumped every time it is played, so evicting the
//! oldest files first removes the songs that haven't been played for the longest time.

use crate::paths;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Files touched this recently are never evicted (the song playing now, preloaded ones).
const PROTECT: Duration = Duration::from_secs(15 * 60);

/// Mark a cached file as just played.
pub fn touch(path: &Path) {
    if path.starts_with(paths::cache_dir()) {
        if let Ok(f) = std::fs::File::options().write(true).open(path) {
            let _ = f.set_modified(SystemTime::now());
        }
    }
}

/// (total bytes, number of files) currently in the cache.
pub fn usage() -> (u64, usize) {
    let mut total = 0;
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(paths::cache_dir()) {
        for e in entries.filter_map(|e| e.ok()) {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    total += m.len();
                    count += 1;
                }
            }
        }
    }
    (total, count)
}

/// Remove least recently played files until the cache fits in `limit_bytes` (0 = no limit).
/// Returns the number of files removed.
pub fn enforce(limit_bytes: u64) -> usize {
    if limit_bytes == 0 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(paths::cache_dir()) else { return 0 };
    let mut files: Vec<(SystemTime, u64, std::path::PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            // Unfinished downloads ("name.<id>.part", "yt_<id>.<id>.m4a") belong to their downloader;
            // finished cache files have exactly one dot.
            let name = e.file_name().to_string_lossy().into_owned();
            if name.matches('.').count() != 1 {
                return None;
            }
            Some((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= limit_bytes {
        return 0;
    }
    files.sort_by_key(|f| f.0);
    let now = SystemTime::now();
    let mut removed = 0;
    for (modified, size, path) in files {
        if total <= limit_bytes {
            break;
        }
        if now.duration_since(modified).map(|d| d < PROTECT).unwrap_or(true) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            removed += 1;
        }
    }
    removed
}

pub fn format_size(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}
