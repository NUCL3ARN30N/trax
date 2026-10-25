//! Export/import of the whole trax state (database, covers, artwork) as one `.traxbak` zip.

use crate::db::Db;
use crate::paths;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MANIFEST: &str = "trax-backup.json";

pub fn export(db: &std::sync::Mutex<Db>, dest: &Path) -> Result<()> {
    let snapshot = paths::data_dir().join("export-snapshot.db");
    db.lock().unwrap().snapshot_to(&snapshot)?;
    let file = File::create(dest).with_context(|| format!("cannot write {}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let manifest = serde_json::json!({
        "app": "trax",
        "version": env!("CARGO_PKG_VERSION"),
        "created": chrono::Local::now().to_rfc3339(),
        "data_dir": paths::data_dir().to_string_lossy(),
    });
    zip.start_file(MANIFEST, opts)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;

    zip.start_file("trax.db", opts)?;
    zip.write_all(&std::fs::read(&snapshot)?)?;
    let _ = std::fs::remove_file(&snapshot);

    for (dir, prefix) in [(paths::covers_dir(), "covers"), (paths::artwork_dir(), "artwork")] {
        for entry in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                zip.start_file(format!("{prefix}/{}", entry.file_name().to_string_lossy()), opts)?;
                zip.write_all(&std::fs::read(entry.path())?)?;
            }
        }
    }
    zip.finish()?;
    Ok(())
}

/// Unpack a backup next to the current data. The caller must close the database first and
/// reopen it afterwards. Returns the old data directory recorded in the backup.
pub fn import(src: &Path) -> Result<String> {
    let file = File::open(src).with_context(|| format!("cannot open {}", src.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("not a trax backup")?;
    let manifest: serde_json::Value = {
        let mut f = zip.by_name(MANIFEST).context("not a trax backup (manifest missing)")?;
        let mut s = String::new();
        f.read_to_string(&mut s)?;
        serde_json::from_str(&s)?
    };
    if manifest["app"] != "trax" {
        bail!("not a trax backup");
    }
    // Validate the database before touching anything.
    let staged = paths::data_dir().join("import-staged.db");
    {
        let mut f = zip.by_name("trax.db").context("backup has no database")?;
        let mut out = File::create(&staged)?;
        std::io::copy(&mut f, &mut out)?;
    }
    Db::open(&staged).context("backup database is damaged")?;

    // Keep the previous database around, just in case.
    let current = paths::db_file();
    if current.exists() {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let _ = std::fs::copy(&current, paths::data_dir().join(format!("trax-before-import-{stamp}.db")));
    }
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{ext}", current.display())));
    }
    std::fs::rename(&staged, &current)?;
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{ext}", staged.display())));
    }

    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let Ok(name) = f.name().map(|n| n.replace('\\', "/")) else { continue };
        let target = if let Some(rest) = name.strip_prefix("covers/") {
            paths::covers_dir().join(rest)
        } else if let Some(rest) = name.strip_prefix("artwork/") {
            paths::artwork_dir().join(rest)
        } else {
            continue;
        };
        // Reject path traversal.
        if target.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            continue;
        }
        let mut out = File::create(&target)?;
        std::io::copy(&mut f, &mut out)?;
    }
    Ok(manifest["data_dir"].as_str().unwrap_or("").to_string())
}

/// Swap the live database for the backup at `src`: closes `db`, unpacks the backup, reopens
/// the database and re-points stored cover/artwork paths at this machine's data folder.
pub fn restore(db: &std::sync::Mutex<Db>, src: &Path) -> Result<String> {
    let mut guard = db.lock().unwrap();
    let old = std::mem::replace(&mut *guard, Db::open(Path::new(":memory:"))?);
    drop(old);
    let res = import(src);
    let reopened = Db::open(&paths::db_file())?;
    reopened.rebase_data_paths(&paths::data_dir().to_string_lossy())?;
    *guard = reopened;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SourceKind, Track};
    use std::sync::Mutex;

    /// Simulates moving to a new PC: export, wipe everything, import, and check that
    /// library, edits, playlists (with description/artwork), history, settings, artwork, the
    /// profile and friends all come back, with file paths re-pointed at the new data folder.
    #[test]
    fn export_import_roundtrip() {
        let dir = paths::use_test_dir().clone();
        assert!(dir.starts_with(std::env::temp_dir()));

        let png = {
            let mut buf = std::io::Cursor::new(Vec::new());
            image::RgbImage::from_pixel(4, 4, image::Rgb([200, 10, 10]))
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };
        let cover = crate::connectors::save_cover("test-cover", &png).unwrap();
        let art = paths::artwork_dir().join("artist.jpg");
        std::fs::write(&art, &png).unwrap();

        let db = Mutex::new(Db::open(&paths::db_file()).unwrap());
        {
            let db = db.lock().unwrap();
            let mut t = Track::new(SourceKind::Local, r"D:\Music\a.mp3");
            t.title = "Original".into();
            let id = db.upsert_track(&t, true).unwrap();
            // Pretend the cover was stored on another PC.
            let foreign = format!(r"C:\Users\old\AppData\Roaming\trax\data\covers\{}",
                Path::new(&cover).file_name().unwrap().to_string_lossy());
            db.edit_track(id, "Edited title", "Edited artist", "Album", Some(&foreign)).unwrap();
            db.record_play(id, 60_000).unwrap();
            let pl = db.create_playlist("Road trip").unwrap();
            db.add_to_playlist(pl, &[id]).unwrap();
            db.set_playlist_details(pl, "Road trip", "Songs for the car", Some(&foreign)).unwrap();
            db.set_artist_art("Edited artist", &art.to_string_lossy());
            db.save_artist(&crate::db::ArtistRow {
                name: "Edited artist".into(),
                spotify_id: Some("abc".into()),
                genres: "thrash metal".into(),
                bio: "My own words".into(),
                bio_manual: true,
                match_manual: true,
                ..Default::default()
            })
            .unwrap();
            db.set_setting("crossfade_secs", "3.5");
            db.set_setting("eq_custom_gains", "1,2,3,4,5,6,7,8,9,10");
            db.set_setting("profile_secret", "secret-hex");
            db.save_friend(&crate::social::friends::Friend {
                pubkey: "friendkey".into(),
                name: "Sam".into(),
                state: crate::social::friends::State::Friend,
                their_key: Some([3; 32]),
                presence: None,
                added: 1,
                changed: 1,
            });
        }

        let backup = std::env::temp_dir().join(format!("trax-test-{}.traxbak", std::process::id()));
        export(&db, &backup).unwrap();

        // "New PC": nothing left of the old data.
        *db.lock().unwrap() = Db::open(Path::new(":memory:")).unwrap();
        for sub in ["covers", "artwork"] {
            std::fs::remove_dir_all(dir.join(sub)).unwrap();
        }
        for f in ["trax.db", "trax.db-wal", "trax.db-shm"] {
            let _ = std::fs::remove_file(dir.join(f));
        }

        restore(&db, &backup).unwrap();
        let db = db.lock().unwrap();
        let tracks = db.library(&[SourceKind::Local], 0, "", None);
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert_eq!(t.title, "Edited title");
        assert_eq!(t.artist, "Edited artist");
        assert_eq!(t.play_count, 1);
        let c = t.cover.as_deref().unwrap();
        assert!(Path::new(c).starts_with(&dir), "cover not rebased: {c}");
        assert!(Path::new(c).exists());

        let pls = db.playlists();
        assert_eq!(pls.len(), 1);
        assert_eq!(pls[0].description, "Songs for the car");
        assert_eq!(pls[0].count, 1);
        assert!(Path::new(pls[0].cover.as_deref().unwrap()).exists());

        assert!(Path::new(&db.artist_art("Edited artist").unwrap()).exists());
        let artist = db.artist("edited ARTIST").unwrap();
        assert_eq!(artist.bio, "My own words");
        assert!(artist.bio_manual && artist.match_manual);
        assert_eq!(artist.spotify_id.as_deref(), Some("abc"));
        assert_eq!(db.artist_tracks("Edited artist", &[SourceKind::Local]).len(), 1);
        assert_eq!(db.get_setting("crossfade_secs").as_deref(), Some("3.5"));
        assert_eq!(db.get_setting("eq_custom_gains").as_deref(), Some("1,2,3,4,5,6,7,8,9,10"));
        assert_eq!(db.stats().ms_total, 60_000);
        // The profile and the friends move along.
        assert_eq!(db.get_setting("profile_secret").as_deref(), Some("secret-hex"));
        let friends = db.friends();
        assert_eq!(friends.len(), 1);
        assert_eq!(friends[0].name, "Sam");
        assert_eq!(friends[0].their_key, Some([3; 32]));

        // A rescan must not overwrite the user's edits.
        let mut rescanned = Track::new(SourceKind::Local, r"D:\Music\a.mp3");
        rescanned.title = "From tags".into();
        db.upsert_track(&rescanned, true).unwrap();
        assert_eq!(db.library(&[SourceKind::Local], 0, "", None)[0].title, "Edited title");

        drop(db);
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
