//! Size limit for the audio cache (YouTube / Nextcloud downloads).
//!
//! A cached file's modification time is bumped every time it is played, so evicting the
//! oldest files first removes the songs that haven't been played for the longest time.

use crate::paths;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
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

/// (evictable bytes, number of files, bytes kept offline) currently in the cache.
/// Songs from playlists kept offline (`keep`) are reported on their own because they do not
/// count against the cache limit.
pub fn usage(keep: &HashSet<PathBuf>) -> (u64, usize, u64) {
    let mut total = 0;
    let mut count = 0;
    let mut pinned = 0;
    if let Ok(entries) = std::fs::read_dir(paths::cache_dir()) {
        for e in entries.filter_map(|e| e.ok()) {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    if keep.contains(&e.path()) {
                        pinned += m.len();
                    } else {
                        total += m.len();
                    }
                    count += 1;
                }
            }
        }
    }
    (total, count, pinned)
}

/// Remove least recently played files until the cache fits in `limit_bytes` (0 = no limit).
/// Files in `keep` belong to playlists kept offline: they are neither removed nor counted
/// towards the limit. Returns the number of files removed.
pub fn enforce(limit_bytes: u64, keep: &HashSet<PathBuf>) -> usize {
    if limit_bytes == 0 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(paths::cache_dir()) else { return 0 };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() || keep.contains(&e.path()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Songs of a playlist kept offline survive eviction and don't fill up the limit.
    #[test]
    fn offline_songs_are_never_evicted() {
        paths::use_test_dir();
        let dir = paths::cache_dir();
        let old = SystemTime::now() - Duration::from_secs(3600);
        let mut files = vec![];
        // "pinned" is the oldest, so plain LRU would remove it first.
        for (i, name) in ["pinned.m4a", "stale.m4a", "fresh.m4a"].iter().enumerate() {
            let path = dir.join(name);
            std::fs::write(&path, vec![0u8; 1000]).unwrap();
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(old + Duration::from_secs(i as u64)).unwrap();
            files.push(path);
        }
        let keep: HashSet<PathBuf> = [files[0].clone()].into();

        // 1500 bytes for the 2000 bytes that are not kept offline: the oldest one goes.
        assert_eq!(enforce(1500, &keep), 1);
        assert!(files[0].exists(), "a song kept offline must not be evicted");
        assert!(!files[1].exists());
        assert!(files[2].exists());

        let (bytes, count, pinned) = usage(&keep);
        assert_eq!((bytes, count, pinned), (1000, 2, 1000));

        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }
}
