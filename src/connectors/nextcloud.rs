//! Nextcloud via WebDAV. Use an app password (Settings → Security → Devices & sessions).

use super::{guess_from_path, http_client, is_audio};
use crate::db::Db;
use crate::model::{SourceKind, Track};
use crate::paths;
use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

#[derive(Clone, Default)]
pub struct Config {
    pub url: String,
    pub user: String,
    pub password: String,
    pub folder: String,
}

impl Config {
    pub fn is_complete(&self) -> bool {
        !self.url.trim().is_empty() && !self.user.trim().is_empty() && !self.password.is_empty()
    }

    fn files_root(&self) -> String {
        format!(
            "{}/remote.php/dav/files/{}",
            self.url.trim().trim_end_matches('/'),
            utf8_percent_encode(self.user.trim(), SEGMENT)
        )
    }

    /// Full URL for a path relative to the user's files root ("/Music/a b.mp3").
    fn url_for(&self, path: &str) -> String {
        let encoded: Vec<String> =
            path.split('/').filter(|s| !s.is_empty()).map(|s| utf8_percent_encode(s, SEGMENT).to_string()).collect();
        format!("{}/{}", self.files_root(), encoded.join("/"))
    }

    fn music_folder(&self) -> String {
        let f = self.folder.trim().trim_matches('/');
        if f.is_empty() { "/".into() } else { format!("/{f}") }
    }
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>"#;

struct Entry {
    path: String,
    is_dir: bool,
}

fn propfind(client: &reqwest::blocking::Client, cfg: &Config, path: &str) -> Result<Vec<Entry>> {
    let resp = client
        .request(reqwest::Method::from_bytes(b"PROPFIND")?, cfg.url_for(path))
        .basic_auth(cfg.user.trim(), Some(&cfg.password))
        .header("Depth", "1")
        .header("Content-Type", "application/xml")
        .body(PROPFIND_BODY)
        .send()
        .context("cannot reach Nextcloud")?;
    match resp.status().as_u16() {
        207 => {}
        401 => bail!("Nextcloud rejected the credentials (use an app password)"),
        404 => bail!("folder not found on Nextcloud: {path}"),
        s => bail!("unexpected WebDAV status {s}"),
    }
    let text = resp.text()?;
    let doc = roxmltree::Document::parse(&text).context("invalid WebDAV response")?;
    let marker = format!("/remote.php/dav/files/{}", cfg.user.trim());
    let mut out = vec![];
    for node in doc.descendants().filter(|n| n.tag_name().name() == "response") {
        let Some(href) = node.descendants().find(|n| n.tag_name().name() == "href").and_then(|n| n.text()) else {
            continue;
        };
        let decoded = percent_decode_str(href).decode_utf8_lossy().into_owned();
        let Some(idx) = decoded.find(&marker) else { continue };
        let rel = decoded[idx + marker.len()..].trim_end_matches('/').to_string();
        let is_dir = node.descendants().any(|n| n.tag_name().name() == "collection");
        out.push(Entry { path: if rel.is_empty() { "/".into() } else { rel }, is_dir });
    }
    Ok(out)
}

pub fn test(cfg: &Config) -> Result<usize> {
    let client = http_client();
    let entries = propfind(&client, cfg, &cfg.music_folder())?;
    Ok(entries.len().saturating_sub(1))
}

/// Recursively list audio files below the configured music folder and store them.
pub fn scan(db: &Arc<Mutex<Db>>, cfg: &Config, progress: impl Fn(usize)) -> Result<usize> {
    let client = http_client();
    let root = cfg.music_folder();
    let mut stack = vec![root.clone()];
    let mut visited = HashSet::new();
    let mut seen = HashSet::new();
    let mut batch = vec![];
    while let Some(dir) = stack.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        for e in propfind(&client, cfg, &dir)? {
            if e.path.trim_end_matches('/') == dir.trim_end_matches('/') {
                continue;
            }
            if e.is_dir {
                stack.push(e.path);
            } else if is_audio(&e.path) {
                let mut t = Track::new(SourceKind::Nextcloud, e.path.clone());
                let rel = e.path.strip_prefix(&root).unwrap_or(&e.path).to_string();
                guess_from_path(&mut t, &rel);
                seen.insert(e.path);
                batch.push(t);
            }
        }
        if batch.len() >= 100 {
            store(db, &mut batch);
            progress(seen.len());
        }
    }
    store(db, &mut batch);
    progress(seen.len());
    db.lock().unwrap().prune_source(SourceKind::Nextcloud, &seen, None)?;
    Ok(seen.len())
}

fn store(db: &Arc<Mutex<Db>>, batch: &mut Vec<Track>) {
    let db = db.lock().unwrap();
    for t in batch.drain(..) {
        // Don't overwrite tags that were read after a download.
        let existing = db.ensure_track(&t).ok().and_then(|id| db.track(id));
        match existing {
            Some(e) => {
                let _ = db.set_in_library(e.id, true);
            }
            None => {
                let _ = db.upsert_track(&t, true);
            }
        }
    }
}

pub fn cache_path(uri: &str) -> PathBuf {
    let ext = uri.rsplit('.').next().unwrap_or("bin").to_ascii_lowercase();
    paths::cache_dir().join(format!("nc_{}.{ext}", paths::hash(uri)))
}

/// Download a file into the cache (if not cached yet) and return its local path.
pub fn fetch(cfg: &Config, uri: &str) -> Result<PathBuf> {
    let dest = cache_path(uri);
    if dest.exists() {
        return Ok(dest);
    }
    let mut resp = http_client()
        .get(cfg.url_for(uri))
        .basic_auth(cfg.user.trim(), Some(&cfg.password))
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .context("cannot reach Nextcloud")?;
    if !resp.status().is_success() {
        bail!("Nextcloud download failed ({})", resp.status());
    }
    let tmp = dest.with_extension(format!("{}.part", paths::unique_suffix()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        resp.copy_to(&mut f)?;
        f.flush()?;
    }
    if dest.exists() {
        // Another download (preload) finished first.
        let _ = std::fs::remove_file(&tmp);
    } else {
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(dest)
}
