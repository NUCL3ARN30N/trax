//! Local folders on disk.

use super::{guess_from_path, is_audio, read_tags};
use crate::db::Db;
use crate::model::{SourceKind, Track};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Scan all folders, add new/changed files, and hide files that disappeared.
/// Unchanged files (same modification time) are skipped, so rescans are fast.
/// `progress` is called with the number of files processed so far.
pub fn scan(db: &Arc<Mutex<Db>>, folders: &[String], progress: impl Fn(usize)) -> anyhow::Result<usize> {
    let known = db.lock().unwrap().local_mtimes();
    let mut seen = HashSet::new();
    let mut count = 0;
    let mut batch: Vec<(Track, i64)> = Vec::new();
    for folder in folders {
        for entry in walkdir::WalkDir::new(folder).follow_links(true).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let path_str = path.to_string_lossy().into_owned();
            if !is_audio(&path_str) {
                continue;
            }
            count += 1;
            let mtime = entry.metadata().map(|m| mtime_of(&m)).unwrap_or(0);
            seen.insert(path_str.clone());
            if known.get(&path_str) == Some(&mtime) {
                continue;
            }
            let mut t = Track::new(SourceKind::Local, path_str.clone());
            guess_from_path(&mut t, &path_str);
            read_tags(path, &mut t);
            batch.push((t, mtime));
            if batch.len() >= 100 {
                flush(db, &mut batch);
                progress(count);
            }
        }
    }
    flush(db, &mut batch);
    progress(count);
    // Hides files that were deleted and files from folders removed from the settings.
    db.lock().unwrap().prune_source(SourceKind::Local, &seen, None)?;
    db.lock().unwrap().hide_files_in_folders(&seen)?;
    Ok(count)
}

fn flush(db: &Arc<Mutex<Db>>, batch: &mut Vec<(Track, i64)>) {
    let db = db.lock().unwrap();
    for (t, mtime) in batch.drain(..) {
        match db.upsert_track(&t, true) {
            Ok(id) => {
                let _ = db.set_mtime(id, mtime);
            }
            Err(e) => log::warn!("cannot store {}: {e}", t.uri),
        }
    }
}
