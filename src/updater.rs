//! Updates from GitHub releases.
//!
//! Each release carries `trax-setup-<version>.exe` (installer), `trax-<version>-portable.zip`
//! (just `trax.exe`) and `SHA256SUMS.txt`. Downloads are only used when their checksum matches.
//! An installed copy runs the new installer; a portable copy swaps its own exe.

use crate::connectors::http_client;
use crate::paths;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub const REPO: &str = "NUCL3ARN30N/trax";

/// Started once the app has saved everything and is about to exit.
static AFTER_EXIT: Mutex<Option<Command>> = Mutex::new(None);

#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub page: String,
    assets: Vec<Asset>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallKind {
    Installed,
    Portable,
}

impl InstallKind {
    pub fn label(self) -> &'static str {
        match self {
            InstallKind::Installed => "Installed",
            InstallKind::Portable => "Portable",
        }
    }
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("can't find trax.exe")
}

/// The installer puts its uninstaller next to trax.exe; a portable copy has none.
pub fn install_kind() -> InstallKind {
    let installed = current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("unins000.exe").exists()))
        .unwrap_or(false);
    if installed { InstallKind::Installed } else { InstallKind::Portable }
}

/// `TRAX_UPDATE_URL` points the check at another server (testing).
fn latest_url() -> String {
    std::env::var("TRAX_UPDATE_URL").unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"))
}

/// "v1.2.3", "1.2.3-beta" → (1, 2, 3, is-prerelease).
fn parse_version(v: &str) -> Option<(u64, u64, u64, bool)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((c, rest)) => (c, !rest.is_empty() && v.contains('-')),
        None => (v, false),
    };
    let mut it = core.split('.').map(|p| p.parse::<u64>());
    let major = it.next()?.ok()?;
    let minor = it.next().unwrap_or(Ok(0)).ok()?;
    let patch = it.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch, pre))
}

/// Whether `remote` is a newer version than `current`.
pub fn is_newer(remote: &str, current: &str) -> bool {
    match (parse_version(remote), parse_version(current)) {
        (Some((a, b, c, ap)), Some((x, y, z, cp))) => {
            (a, b, c) > (x, y, z) || ((a, b, c) == (x, y, z) && cp && !ap)
        }
        _ => false,
    }
}

/// The latest release, if it is newer than this build.
pub fn check() -> Result<Option<Release>> {
    let resp = http_client()
        .get(latest_url())
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .context("can't reach GitHub")?;
    if resp.status().as_u16() == 404 {
        return Ok(None); // no release published yet
    }
    if !resp.status().is_success() {
        bail!("GitHub answered {}", resp.status());
    }
    let json: serde_json::Value = resp.json()?;
    let release = parse_release(&json).context("unexpected release data")?;
    Ok(is_newer(&release.version, env!("CARGO_PKG_VERSION")).then_some(release))
}

fn parse_release(json: &serde_json::Value) -> Option<Release> {
    if json["draft"].as_bool() == Some(true) || json["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let tag = json["tag_name"].as_str()?;
    parse_version(tag)?;
    let assets = json["assets"]
        .as_array()?
        .iter()
        .filter_map(|a| {
            Some(Asset {
                name: a["name"].as_str()?.to_string(),
                url: a["browser_download_url"].as_str()?.to_string(),
                size: a["size"].as_u64().unwrap_or(0),
            })
        })
        .collect();
    Some(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        notes: json["body"].as_str().unwrap_or("").trim().to_string(),
        page: json["html_url"].as_str().unwrap_or("").to_string(),
        assets,
    })
}

impl Release {
    fn asset(&self, pred: impl Fn(&str) -> bool) -> Option<&Asset> {
        self.assets.iter().find(|a| pred(&a.name.to_ascii_lowercase()))
    }

    /// The download that fits this copy of trax.
    pub fn package(&self, kind: InstallKind) -> Option<&Asset> {
        match kind {
            InstallKind::Installed => self.asset(|n| n.starts_with("trax-setup") && n.ends_with(".exe")),
            InstallKind::Portable => self.asset(|n| n.contains("portable") && n.ends_with(".zip")),
        }
    }

    fn checksums(&self) -> Option<&Asset> {
        self.asset(|n| n.starts_with("sha256sums"))
    }
}

/// Expected checksum for `name` from a `SHA256SUMS` file ("<hex>  <name>" per line).
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

fn updates_dir() -> PathBuf {
    let p = paths::data_dir().join("updates");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Download the package for `kind` and check it against the release checksums.
/// `progress` gets values from 0.0 to 1.0.
pub fn download(release: &Release, kind: InstallKind, progress: impl Fn(f32)) -> Result<PathBuf> {
    let asset = release.package(kind).with_context(|| format!("this release has no {} download", kind.label().to_lowercase()))?;
    let sums = release.checksums().context("this release has no checksums, so it can't be verified")?;
    let sums = http_client().get(&sums.url).send()?.error_for_status()?.text()?;
    let expected = expected_sum(&sums, &asset.name).context("the download isn't listed in the checksums")?;

    let dest = updates_dir().join(paths::sanitize_file_name(&asset.name));
    let tmp = dest.with_extension("part");
    let mut resp = http_client()
        .get(&asset.url)
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .context("download failed")?
        .error_for_status()?;
    let total = resp.content_length().unwrap_or(asset.size).max(1);
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::create(&tmp)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done += n as u64;
        progress((done as f32 / total as f32).min(1.0));
    }
    file.flush()?;
    drop(file);
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        let _ = std::fs::remove_file(&tmp);
        bail!("the download is damaged (checksum mismatch)");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Prepare the update so it is applied when trax exits: run the installer, or put the new
/// portable exe in place and start it.
pub fn stage(package: &Path, kind: InstallKind) -> Result<()> {
    let cmd = stage_at(&current_exe()?, package, kind)?;
    *AFTER_EXIT.lock().unwrap() = Some(cmd);
    Ok(())
}

fn stage_at(exe: &Path, package: &Path, kind: InstallKind) -> Result<Command> {
    let exe = exe.to_path_buf();
    let cmd = match kind {
        InstallKind::Installed => {
            let mut c = Command::new(package);
            // /update=1 makes the installer start trax again when it is done.
            c.args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS", "/update=1"]);
            c
        }
        InstallKind::Portable => {
            let new_exe = exe.with_extension("exe.new");
            extract_exe(package, &new_exe)?;
            // A running exe can be renamed, so the old one moves aside and is deleted on the
            // next start.
            let old = exe.with_extension("exe.old");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&exe, &old).context("can't replace trax.exe (is the folder read-only?)")?;
            if let Err(e) = std::fs::rename(&new_exe, &exe) {
                let _ = std::fs::rename(&old, &exe);
                return Err(e).context("can't put the new trax.exe in place");
            }
            Command::new(&exe)
        }
    };
    Ok(cmd)
}

fn extract_exe(zip_path: &Path, dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path)?)?;
    let index = (0..archive.len())
        .find(|&i| {
            archive
                .by_index(i)
                .ok()
                .and_then(|f| f.name().ok().map(|n| n.rsplit(['/', '\\']).next().unwrap_or("").eq_ignore_ascii_case("trax.exe")))
                .unwrap_or(false)
        })
        .context("trax.exe not found in the download")?;
    let mut entry = archive.by_index(index)?;
    let mut out = std::fs::File::create(dest)?;
    std::io::copy(&mut entry, &mut out)?;
    out.flush()?;
    Ok(())
}

/// Called after the event loop ended and everything is saved.
pub fn run_after_exit() {
    if let Some(mut cmd) = AFTER_EXIT.lock().unwrap().take() {
        if let Err(e) = cmd.spawn() {
            log::error!("couldn't start the update: {e}");
        }
    }
}

/// At start: remove what a previous update left behind. The old exe can only be deleted once
/// the previous process has exited, so this waits for that briefly.
pub fn cleanup() {
    if let Ok(exe) = current_exe() {
        let old = exe.with_extension("exe.old");
        for _ in 0..50 {
            if !old.exists() || std::fs::remove_file(&old).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    let _ = std::fs::remove_dir_all(updates_dir());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(is_newer("0.2.0", "0.2.0-beta"));
        assert!(!is_newer("0.2.0-beta", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }

    /// Against a test release server: `TRAX_UPDATE_URL=http://…/release.json cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn download_from_test_server() {
        crate::paths::use_test_dir();
        let release = check().unwrap().expect("test server should offer a newer version");
        let zip = download(&release, InstallKind::Portable, |_| {}).unwrap();
        let exe = zip.with_extension("exe");
        extract_exe(&zip, &exe).unwrap();
        let bytes = std::fs::read(&exe).unwrap();
        assert!(bytes.starts_with(b"MZ"));
        if let Ok(marker) = std::env::var("TRAX_UPDATE_MARKER") {
            assert!(bytes.ends_with(marker.as_bytes()));
        }

        // Portable swap on a stand-in exe: the new one takes its place, the old one moves aside.
        let dir = zip.parent().unwrap().join("swap");
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("trax.exe");
        std::fs::write(&fake, b"old").unwrap();
        let cmd = stage_at(&fake, &zip, InstallKind::Portable).unwrap();
        assert_eq!(std::fs::read(&fake).unwrap(), bytes);
        assert_eq!(std::fs::read(dir.join("trax.exe.old")).unwrap(), b"old");
        assert!(!dir.join("trax.exe.new").exists());
        assert_eq!(Path::new(cmd.get_program()), fake);
    }

    #[test]
    fn release_and_checksums() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"tag_name":"v0.2.0","body":"Notes","html_url":"https://github.com/x/y","draft":false,"prerelease":false,
            "assets":[{"name":"trax-setup-0.2.0.exe","browser_download_url":"https://e/setup","size":10},
                      {"name":"trax-0.2.0-portable.zip","browser_download_url":"https://e/zip","size":5},
                      {"name":"SHA256SUMS.txt","browser_download_url":"https://e/sums","size":1}]}"#,
        )
        .unwrap();
        let r = parse_release(&json).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.package(InstallKind::Installed).unwrap().url, "https://e/setup");
        assert_eq!(r.package(InstallKind::Portable).unwrap().url, "https://e/zip");
        let sums = "ABCDEF  trax-setup-0.2.0.exe\n123456 *trax-0.2.0-portable.zip\n";
        assert_eq!(expected_sum(sums, "trax-setup-0.2.0.exe").as_deref(), Some("abcdef"));
        assert_eq!(expected_sum(sums, "trax-0.2.0-portable.zip").as_deref(), Some("123456"));
        assert!(expected_sum(sums, "other.zip").is_none());
    }
}
