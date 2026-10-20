//! Locations of everything trax keeps on disk.

use std::path::PathBuf;
use std::sync::OnceLock;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Root data directory (e.g. `%APPDATA%\trax` on Windows, `~/.local/share/trax` on Linux).
/// `TRAX_DATA_DIR` overrides it (portable installs, tests).
pub fn data_dir() -> &'static PathBuf {
    DATA_DIR.get_or_init(|| {
        let dir = std::env::var_os("TRAX_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| directories::ProjectDirs::from("", "", "trax").map(|d| d.data_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from(".trax"));
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
}

/// Tests share one throw-away data folder. Every test that touches the data folder calls this
/// first, because whichever call comes first fixes `data_dir` for the whole test run.
#[cfg(test)]
pub fn use_test_dir() -> &'static PathBuf {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = std::env::temp_dir().join(format!("trax-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: tests only read the environment through this function.
        unsafe { std::env::set_var("TRAX_DATA_DIR", &dir) };
    });
    data_dir()
}

fn sub(name: &str) -> PathBuf {
    let p = data_dir().join(name);
    let _ = std::fs::create_dir_all(&p);
    p
}

pub fn db_file() -> PathBuf {
    data_dir().join("trax.db")
}

/// Album covers extracted from tags or downloaded thumbnails.
pub fn covers_dir() -> PathBuf {
    sub("covers")
}

/// Artist artwork used in the fullscreen view (manual or Spotify).
pub fn artwork_dir() -> PathBuf {
    sub("artwork")
}

/// Downloaded audio from remote connectors. Safe to delete at any time.
pub fn cache_dir() -> PathBuf {
    sub("cache")
}

/// Helper binaries such as yt-dlp.
pub fn bin_dir() -> PathBuf {
    sub("bin")
}

/// Stable 64-bit FNV-1a hash, used for cache file names.
pub fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn sanitize_file_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .to_lowercase()
}

/// Unique temp-file suffix, so parallel downloads of the same file (e.g. a preload and a
/// play request) never write into each other.
pub fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}
