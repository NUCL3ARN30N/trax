//! Artist profiles and online metadata (Spotify artwork/details, Wikipedia bios).

use super::{App, post, s};
use crate::connectors::{self, spotify, wikipedia, youtube};
use crate::db::{ArtistRow, Db};
use crate::model::{SourceKind, Track, clean_title, format_hours, primary_artist};
use crate::ui_models::ImageCache;
use crate::{AlbumItem, ArtistItem, MatchItem};
use slint::{ModelRc, VecModel};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Artist info older than this is refreshed automatically (when enabled).
const STALE_SECS: i64 = 30 * 86400;

pub(super) enum MatchCandidate {
    Artist(spotify::ArtistInfo),
    Track(spotify::TrackMatch),
}

type Thumb = Option<(u32, u32, Vec<u8>)>;

fn rgba_image(t: &Thumb) -> Option<slint::Image> {
    let (w, h, px) = t.as_ref()?;
    Some(slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(px, *w, *h)))
}

fn thumb_from_url(url: Option<&str>) -> Thumb {
    let bytes = spotify::download(url?).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.thumbnail(96, 96).to_rgba8();
    Some((img.width(), img.height(), img.into_raw()))
}

fn format_followers(n: i64) -> String {
    match n {
        0 => String::new(),
        n if n >= 1_000_000 => format!("{:.1}M followers", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}K followers", n as f64 / 1_000.0),
        n => format!("{n} followers"),
    }
}

/// Store an artist image (with a list thumbnail). Returns the file path.
fn store_artist_image(name: &str, bytes: &[u8]) -> anyhow::Result<String> {
    let path = spotify::save_artist_image(name, bytes)?;
    if let Ok(img) = image::open(&path) {
        let _ = img.thumbnail(256, 256).to_rgb8().save_with_format(connectors::thumb_path(&path), image::ImageFormat::Jpeg);
    }
    Ok(path)
}

/// Fetch Spotify details (by `id` or best name match), the image and a Wikipedia bio for an
/// artist and store them. Fields the user set by hand are left alone.
fn update_artist_blocking(db: &Mutex<Db>, creds: &spotify::Credentials, name: &str, id: Option<String>) -> anyhow::Result<()> {
    let mut row = db.lock().unwrap().artist(name).unwrap_or_else(|| ArtistRow { name: name.to_string(), ..Default::default() });
    let manual_pick = id.is_some();
    let id = id.or_else(|| if row.match_manual { row.spotify_id.clone() } else { None });
    let info = match id {
        Some(id) => Some(spotify::artist_by_id(creds, &id)?),
        None => spotify::find_artist(creds, name)?,
    };
    let mut lookup_name = name.to_string();
    if let Some(info) = &info {
        row.spotify_id = Some(info.id.clone());
        row.genres = info.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        row.followers = info.followers;
        row.url = info.url.clone();
        if manual_pick {
            row.match_manual = true;
        }
        lookup_name = info.name.clone();
        if !row.image_manual {
            if let Some(url) = &info.image_url {
                let bytes = spotify::download(url)?;
                let path = store_artist_image(name, &bytes)?;
                db.lock().unwrap().set_artist_art(name, &path);
            }
        }
    }
    if !row.bio_manual {
        if let Some(sum) = wikipedia::artist_summary(&lookup_name) {
            row.bio = sum.text;
            row.bio_url = sum.url;
        }
    }
    row.updated_at = crate::db::now();
    db.lock().unwrap().save_artist(&row)?;
    Ok(())
}

/// Cover for a track from Spotify. Returns (cover path, album name).
fn find_cover_blocking(creds: &spotify::Credentials, t: &Track) -> anyhow::Result<Option<(String, String)>> {
    let artist = primary_artist(&t.artist);
    let title = clean_title(&t.title);
    if title.is_empty() {
        return Ok(None);
    }
    let Some(m) = spotify::find_track(creds, &artist, &title)? else { return Ok(None) };
    let Some(url) = m.image_url else { return Ok(None) };
    let bytes = spotify::download(&url)?;
    Ok(connectors::save_cover(&format!("spotify:{url}"), &bytes).map(|p| (p, m.album)))
}

impl App {
    /// The artist a profile is shown for. A combined name such as "Earth, Wind & Fire" is kept
    /// when it is a known artist; otherwise "A, B" / "A feat. B" map to "A".
    pub(super) fn main_artist(&self, raw: &str) -> String {
        let primary = primary_artist(raw);
        if primary != raw.trim() && self.db.lock().unwrap().artist(raw.trim()).is_some() {
            return raw.trim().to_string();
        }
        primary
    }

    pub(super) fn spotify_creds(&self) -> spotify::Credentials {
        let b = self.backend();
        spotify::Credentials { id: b.get_sp_client_id().to_string(), secret: b.get_sp_client_secret().to_string() }
    }

    pub(super) fn spotify_ready(&self) -> bool {
        self.backend().get_spotify_enabled() && self.spotify_creds().is_set()
    }

    fn spotify_auto(&self) -> bool {
        self.spotify_ready() && self.backend().get_spotify_auto()
    }

    // ------------------------------------------------------------------ fullscreen art

    /// Fullscreen background: the artist's image, else the album cover.
    pub(super) fn update_fullscreen_art(&mut self) {
        let Some(t) = self.queue.current().cloned() else { return };
        let artist = self.main_artist(&t.artist);
        let art = self.db.lock().unwrap().artist_art(&artist);
        let img = art.as_deref().and_then(ImageCache::full).or_else(|| t.cover.as_deref().and_then(ImageCache::full));
        let b = self.backend();
        b.set_has_fullscreen_art(img.is_some());
        b.set_fullscreen_art(img.unwrap_or_default());
    }

    /// Fill in missing artwork and stale artist info for the track that just started.
    pub(super) fn auto_metadata_for(&mut self, t: &Track) {
        if !self.spotify_auto() {
            return;
        }
        let artist = self.main_artist(&t.artist);
        if !artist.is_empty() {
            self.ensure_artist_info(&artist, false, false);
        }
        // For YouTube the video thumbnail is only a stand-in and may be replaced.
        let placeholder =
            (t.source == SourceKind::YouTube).then(|| youtube::thumb_cover_path(&t.uri).to_string_lossy().into_owned());
        let missing = t.cover.is_none() || (placeholder.is_some() && t.cover == placeholder);
        if missing && self.cover_fetching.lock().unwrap().insert(t.uri.clone()) {
            let creds = self.spotify_creds();
            let db = self.db.clone();
            let busy = self.cover_fetching.clone();
            let track = t.clone();
            let token = self.play_token;
            std::thread::spawn(move || {
                let found = find_cover_blocking(&creds, &track);
                busy.lock().unwrap().remove(&track.uri);
                let Ok(Some((cover, album))) = found else { return };
                let updated = {
                    let db = db.lock().unwrap();
                    let _ = db.set_found_cover(track.id, &cover, Some(&album), placeholder.as_deref());
                    db.track(track.id)
                };
                if let Some(t) = updated {
                    post(move |a| a.on_track_updated(token, t));
                }
            });
        }
    }

    // ------------------------------------------------------------------ artist info

    /// Refresh an artist's info in the background when missing or stale (or always with `force`).
    /// Background refresh when automatic updates are on and the info is missing or stale.
    pub(super) fn auto_artist_info(&self, name: &str) {
        if !self.spotify_auto() {
            return;
        }
        if let Some(row) = self.db.lock().unwrap().artist(name) {
            if crate::db::now() - row.updated_at < STALE_SECS {
                return;
            }
        }
        self.fetch_artist(name, None, false);
    }

    pub(super) fn ensure_artist_info(&mut self, name: &str, force: bool, user: bool) {
        if !self.spotify_ready() {
            if user {
                self.toast("Turn on Spotify and enter a client id/secret in Settings first");
            }
            return;
        }
        if !force {
            if let Some(row) = self.db.lock().unwrap().artist(name) {
                if crate::db::now() - row.updated_at < STALE_SECS {
                    return;
                }
            }
        }
        self.fetch_artist(name, None, user);
    }

    fn fetch_artist(&self, name: &str, id: Option<String>, user: bool) {
        if !self.artist_fetching.lock().unwrap().insert(name.to_lowercase()) {
            return;
        }
        if self.artist_view.as_deref() == Some(name) {
            self.backend().set_artist_loading(true);
        }
        let creds = self.spotify_creds();
        let db = self.db.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            let res = update_artist_blocking(&db, &creds, &name, id).map_err(|e| format!("{e:#}"));
            post(move |a| a.on_artist_updated(&name, res, user));
        });
    }

    fn on_artist_updated(&mut self, name: &str, res: Result<(), String>, user: bool) {
        self.artist_fetching.lock().unwrap().remove(&name.to_lowercase());
        if let Some(art) = self.db.lock().unwrap().artist_art(name) {
            self.images.forget(&art);
        }
        if self.artist_view.as_deref() == Some(name) {
            self.load_artist_view();
        }
        if self.queue.current().map(|t| self.main_artist(&t.artist).eq_ignore_ascii_case(name)).unwrap_or(false) {
            self.update_fullscreen_art();
        }
        if self.backend().get_page() == 0 {
            self.refresh_home();
        }
        if self.backend().get_library_view() == 1 {
            self.refresh_library();
        }
        match res {
            Ok(()) if user => self.toast(format!("Updated {name}")),
            Err(e) if user => self.toast(e),
            Err(e) => log::info!("artist info for {name}: {e}"),
            _ => {}
        }
    }

    pub(super) fn open_artist(&mut self, raw: &str) {
        let name = self.main_artist(raw);
        if name.is_empty() || name == "Unknown artist" {
            return;
        }
        self.artist_view = Some(name.clone());
        self.load_artist_view();
        self.backend().set_page(5);
        self.nav_push(super::NavEntry::Artist(name.clone()));
        if self.spotify_auto() {
            self.ensure_artist_info(&name, false, false);
        }
    }

    pub(super) fn load_artist_view(&mut self) {
        let Some(name) = self.artist_view.clone() else { return };
        let (row, art, tracks, (plays, ms)) = {
            let db = self.db.lock().unwrap();
            let tracks = db.artist_tracks(&name, &self.enabled_sources());
            let ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
            (db.artist(&name), db.artist_art(&name), tracks, db.plays_for(&ids))
        };
        // Albums, in name order, each with the first cover found.
        let mut albums: Vec<(String, Vec<Track>)> = vec![];
        for t in &tracks {
            if t.album.is_empty() || t.album == "YouTube" || t.album == "SoundCloud" {
                continue;
            }
            match albums.iter_mut().find(|(a, _)| a.eq_ignore_ascii_case(&t.album)) {
                Some((_, list)) => list.push(t.clone()),
                None => albums.push((t.album.clone(), vec![t.clone()])),
            }
        }
        albums.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        for (_, list) in albums.iter_mut() {
            list.sort_by_key(|t| t.track_no);
        }
        let album_items: Vec<AlbumItem> = albums
            .iter()
            .map(|(name, list)| {
                let cover = list.iter().find_map(|t| t.cover.as_deref()).and_then(|c| self.images.thumb(c));
                AlbumItem {
                    name: s(name),
                    has_cover: cover.is_some(),
                    cover: cover.unwrap_or_default(),
                    subtitle: s(format!("{} track{}", list.len(), if list.len() == 1 { "" } else { "s" })),
                }
            })
            .collect();

        let image = art.as_deref().and_then(ImageCache::full);
        let row = row.unwrap_or_default();
        let b = self.backend();
        b.set_artist_name(s(&name));
        b.set_artist_has_image(image.is_some());
        b.set_artist_image(image.unwrap_or_default());
        b.set_artist_genres(s(&row.genres));
        b.set_artist_followers(s(format_followers(row.followers)));
        b.set_artist_bio(s(&row.bio));
        b.set_artist_bio_url(s(&row.bio_url));
        b.set_artist_url(s(&row.url));
        b.set_artist_matched(row.spotify_id.is_some());
        b.set_artist_loading(self.artist_fetching.lock().unwrap().contains(&name.to_lowercase()));
        let mut stats = format!("{} track{} in your library", tracks.len(), if tracks.len() == 1 { "" } else { "s" });
        if plays > 0 {
            stats += &format!(" · {plays} plays · {} listened", format_hours(ms));
        }
        b.set_artist_stats(s(stats));
        b.set_artist_albums(ModelRc::new(VecModel::from(album_items)));
        self.artist_albums = albums.into_iter().map(|(_, l)| l).collect();
        self.artist_model.set(tracks);
        self.mark_playing();
    }

    pub(super) fn play_album(&mut self, index: usize) {
        let Some(tracks) = self.artist_albums.get(index).cloned() else { return };
        self.queue.set(tracks, 0);
        self.queue_context = None;
        self.start_current();
    }

    pub(super) fn artist_edit_begin(&mut self) {
        let Some(name) = self.artist_view.clone() else { return };
        let bio = self.db.lock().unwrap().artist(&name).map(|r| r.bio).unwrap_or_default();
        self.backend().set_artist_bio_edit(s(bio));
    }

    pub(super) fn artist_edit_save(&mut self) {
        let Some(name) = self.artist_view.clone() else { return };
        let text = self.backend().get_artist_bio_edit().trim().to_string();
        {
            let db = self.db.lock().unwrap();
            let mut row = db.artist(&name).unwrap_or_else(|| ArtistRow { name: name.clone(), ..Default::default() });
            if text != row.bio {
                row.bio_manual = !text.is_empty();
                row.bio_url = String::new();
                row.bio = text;
                if !row.bio_manual {
                    row.updated_at = 0; // fetch again on the next update
                }
                let _ = db.save_artist(&row);
            }
        }
        self.load_artist_view();
    }

    /// Choose an artist image from disk. `name` defaults to the open artist page.
    pub(super) fn pick_artist_image(&mut self, name: Option<String>) {
        let Some(name) = name.or_else(|| self.artist_view.clone()) else { return };
        let Some(file) = rfd::FileDialog::new()
            .set_title("Choose artist image")
            .add_filter("Images", &["jpg", "jpeg", "png", "webp", "bmp"])
            .pick_file()
        else {
            return;
        };
        let res = std::fs::read(&file).map_err(anyhow::Error::from).and_then(|b| store_artist_image(&name, &b));
        match res {
            Ok(path) => {
                self.images.forget(&path);
                let db = self.db.lock().unwrap();
                db.set_artist_art(&name, &path);
                let mut row = db.artist(&name).unwrap_or_else(|| ArtistRow { name: name.clone(), ..Default::default() });
                row.image_manual = true;
                let _ = db.save_artist(&row);
            }
            Err(e) => self.toast(format!("Can't use that image: {e:#}")),
        }
        self.after_artist_image_change(&name);
    }

    pub(super) fn clear_artist_image(&mut self, name: Option<String>) {
        let Some(name) = name.or_else(|| self.artist_view.clone()) else { return };
        {
            let db = self.db.lock().unwrap();
            if let Some(path) = db.artist_art(&name) {
                self.images.forget(&path);
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(connectors::thumb_path(&path));
            }
            db.delete_artist_art(&name);
            if let Some(mut row) = db.artist(&name) {
                row.image_manual = false;
                row.updated_at = 0; // allow an automatic refetch
                let _ = db.save_artist(&row);
            }
        }
        self.after_artist_image_change(&name);
    }

    fn after_artist_image_change(&mut self, name: &str) {
        if self.artist_view.as_deref() == Some(name) {
            self.load_artist_view();
        }
        self.update_fullscreen_art();
        if self.backend().get_library_view() == 1 {
            self.refresh_library();
        }
    }

    pub(super) fn current_primary_artist(&self) -> Option<String> {
        self.queue.current().map(|t| self.main_artist(&t.artist)).filter(|a| !a.is_empty())
    }

    /// Library "Artists" view: one entry per main artist.
    pub(super) fn artist_items(&self, filter: &str) -> Vec<ArtistItem> {
        let source = match self.backend().get_library_source() {
            1 => Some(SourceKind::Local),
            2 => Some(SourceKind::Nextcloud),
            3 => Some(SourceKind::YouTube),
            4 => Some(SourceKind::SoundCloud),
            _ => None,
        };
        let raw = self.db.lock().unwrap().library_artists(&self.enabled_sources(), source);
        let mut merged: Vec<(String, i64)> = vec![];
        for (artist, count) in raw {
            let p = self.main_artist(&artist);
            match merged.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(&p)) {
                Some((_, c)) => *c += count,
                None => merged.push((p, count)),
            }
        }
        let f = filter.trim().to_lowercase();
        merged.retain(|(n, _)| f.is_empty() || n.to_lowercase().contains(&f));
        merged.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        let db = self.db.lock().unwrap();
        merged
            .into_iter()
            .map(|(name, count)| {
                let img = db.artist_art(&name).and_then(|p| self.images.thumb(&p));
                ArtistItem {
                    has_image: img.is_some(),
                    image: img.unwrap_or_default(),
                    subtitle: s(format!("{count} track{}", if count == 1 { "" } else { "s" })),
                    name: s(name),
                }
            })
            .collect()
    }

    // ------------------------------------------------------------------ manual matching

    pub(super) fn match_begin_artist(&mut self) {
        let Some(name) = self.artist_view.clone() else { return };
        let b = self.backend();
        b.set_match_kind(0);
        b.set_match_query(s(&name));
        self.match_search();
    }

    pub(super) fn match_begin_track(&mut self) {
        let b = self.backend();
        let q = format!("{} {}", primary_artist(&b.get_edit_artist()), clean_title(&b.get_edit_title()));
        b.set_match_kind(1);
        b.set_match_query(s(q.trim()));
        self.match_search();
    }

    pub(super) fn match_search(&mut self) {
        self.match_candidates.clear();
        self.match_token += 1;
        let token = self.match_token;
        let ready = self.spotify_ready();
        let b = self.backend();
        let q = b.get_match_query().trim().to_string();
        b.set_match_results(ModelRc::new(VecModel::from(Vec::<MatchItem>::new())));
        if q.is_empty() {
            return;
        }
        if !ready {
            b.set_match_error(s("Turn on Spotify and enter a client id/secret in Settings first."));
            return;
        }
        b.set_match_error(s(""));
        b.set_match_searching(true);
        let kind = b.get_match_kind();
        let creds = self.spotify_creds();
        std::thread::spawn(move || {
            let res: Result<Vec<(MatchCandidate, Thumb)>, String> = if kind == 0 {
                spotify::search_artists(&creds, &q, 10).map(|list| {
                    list.into_iter()
                        .map(|a| {
                            let t = thumb_from_url(a.thumb_url.as_deref());
                            (MatchCandidate::Artist(a), t)
                        })
                        .collect()
                })
            } else {
                spotify::search_tracks(&creds, &q, 10).map(|list| {
                    list.into_iter()
                        .map(|m| {
                            let t = thumb_from_url(m.thumb_url.as_deref());
                            (MatchCandidate::Track(m), t)
                        })
                        .collect()
                })
            }
            .map_err(|e| format!("{e:#}"));
            let _ = slint::invoke_from_event_loop(move || {
                super::with_app(|a| a.on_match_results(token, res));
            });
        });
    }

    fn on_match_results(&mut self, token: u64, res: Result<Vec<(MatchCandidate, Thumb)>, String>) {
        if token != self.match_token {
            return;
        }
        let b = self.backend();
        b.set_match_searching(false);
        match res {
            Ok(list) => {
                if list.is_empty() {
                    b.set_match_error(s("Nothing found. Try a different search."));
                }
                let items: Vec<MatchItem> = list
                    .iter()
                    .map(|(c, thumb)| {
                        let img = rgba_image(thumb);
                        let (title, subtitle) = match c {
                            MatchCandidate::Artist(a) => {
                                let mut sub = a.genres.iter().take(2).cloned().collect::<Vec<_>>().join(", ");
                                let f = format_followers(a.followers);
                                if !f.is_empty() {
                                    sub = if sub.is_empty() { f } else { format!("{sub} · {f}") };
                                }
                                (a.name.clone(), sub)
                            }
                            MatchCandidate::Track(t) => (t.title.clone(), format!("{} · {}", t.artists, t.album)),
                        };
                        MatchItem { title: s(title), subtitle: s(subtitle), has_image: img.is_some(), image: img.unwrap_or_default() }
                    })
                    .collect();
                b.set_match_results(ModelRc::new(VecModel::from(items)));
                self.match_candidates = list.into_iter().map(|(c, _)| c).collect();
            }
            Err(e) => b.set_match_error(s(e)),
        }
    }

    pub(super) fn match_pick(&mut self, index: usize) {
        let Some(c) = self.match_candidates.get(index) else { return };
        match c {
            MatchCandidate::Artist(info) => {
                let Some(name) = self.artist_view.clone() else { return };
                let id = info.id.clone();
                // A manual pick always refetches, even if a fetch is already running.
                self.artist_fetching.lock().unwrap().remove(&name.to_lowercase());
                self.fetch_artist(&name, Some(id), true);
            }
            MatchCandidate::Track(m) => {
                let Some(url) = m.image_url.clone() else {
                    self.toast("That match has no artwork");
                    return;
                };
                let album = m.album.clone();
                std::thread::spawn(move || {
                    let res = spotify::download(&url)
                        .ok()
                        .and_then(|bytes| connectors::save_cover(&format!("spotify:{url}"), &bytes));
                    post(move |a| match res {
                        Some(path) => {
                            a.set_edit_cover_preview(Some(&path));
                            a.edit_cover = Some(Some(path));
                            let b = a.backend();
                            let cur = b.get_edit_album();
                            if cur.trim().is_empty() || cur == "YouTube" || cur == "SoundCloud" {
                                b.set_edit_album(s(album));
                            }
                        }
                        None => a.toast("Couldn't download that artwork"),
                    });
                });
            }
        }
    }

    // ------------------------------------------------------------------ batch jobs

    pub(super) fn fill_missing_artwork(&mut self) {
        if !self.spotify_ready() {
            self.toast("Turn on Spotify and enter a client id/secret in Settings first");
            return;
        }
        if self.backend().get_scanning() {
            return;
        }
        let tracks: Vec<(Track, Option<String>)> = {
            let db = self.db.lock().unwrap();
            let mut list: Vec<(Track, Option<String>)> =
                db.tracks_missing_cover(&self.enabled_sources(), 5000).into_iter().map(|t| (t, None)).collect();
            // YouTube video thumbnails count as placeholders.
            if self.backend().get_yt_enabled() {
                for t in db.library(&[SourceKind::YouTube], 0, "", Some(SourceKind::YouTube)) {
                    let thumb = youtube::thumb_cover_path(&t.uri).to_string_lossy().into_owned();
                    if t.cover.as_deref() == Some(thumb.as_str()) {
                        list.push((t, Some(thumb)));
                    }
                }
            }
            list
        };
        if tracks.is_empty() {
            self.toast("Every track already has artwork");
            return;
        }
        let b = self.backend();
        b.set_scanning(true);
        b.set_scan_status(s(format!("Looking up artwork for {} tracks…", tracks.len())));
        let creds = self.spotify_creds();
        let db = self.db.clone();
        std::thread::spawn(move || {
            let total = tracks.len();
            let mut found = 0;
            let mut by_album: HashMap<String, Option<(String, String)>> = HashMap::new();
            for (i, (t, placeholder)) in tracks.iter().enumerate() {
                let album_key = (!t.album.is_empty() && t.album != "YouTube" && t.album != "SoundCloud")
                    .then(|| format!("{}|{}", primary_artist(&t.artist).to_lowercase(), t.album.to_lowercase()));
                let hit = match album_key.as_ref().and_then(|k| by_album.get(k)) {
                    Some(hit) => hit.clone(),
                    None => {
                        let hit = find_cover_blocking(&creds, t).ok().flatten();
                        if let Some(k) = album_key {
                            by_album.insert(k, hit.clone());
                        }
                        std::thread::sleep(Duration::from_millis(120));
                        hit
                    }
                };
                if let Some((cover, album)) = hit {
                    if db.lock().unwrap().set_found_cover(t.id, &cover, Some(&album), placeholder.as_deref()).is_ok() {
                        found += 1;
                    }
                }
                if i % 10 == 9 {
                    let msg = format!("Looking up artwork… {}/{total} ({found} found)", i + 1);
                    post(move |a| a.backend().set_scan_status(s(msg)));
                }
            }
            post(move |a| {
                let b = a.backend();
                b.set_scanning(false);
                b.set_scan_status(s(format!("Artwork: found {found} of {total}")));
                a.toast(format!("Found artwork for {found} of {total} tracks"));
                a.refresh_library();
                a.refresh_home();
                a.reload_playlist_view();
                a.load_artist_view();
            });
        });
    }

    pub(super) fn update_all_artists(&mut self) {
        if !self.spotify_ready() {
            self.toast("Turn on Spotify and enter a client id/secret in Settings first");
            return;
        }
        if self.backend().get_scanning() {
            return;
        }
        let names: Vec<String> = self.artist_items("").iter().map(|a| a.name.to_string()).collect();
        if names.is_empty() {
            return;
        }
        let b = self.backend();
        b.set_scanning(true);
        b.set_scan_status(s(format!("Updating {} artists…", names.len())));
        let creds = self.spotify_creds();
        let db = self.db.clone();
        std::thread::spawn(move || {
            let total = names.len();
            let mut ok = 0;
            for (i, name) in names.iter().enumerate() {
                match update_artist_blocking(&db, &creds, name, None) {
                    Ok(()) => ok += 1,
                    Err(e) => log::info!("artist {name}: {e:#}"),
                }
                let msg = format!("Updating artists… {}/{total}", i + 1);
                post(move |a| a.backend().set_scan_status(s(msg)));
                std::thread::sleep(Duration::from_millis(150));
            }
            post(move |a| {
                let b = a.backend();
                b.set_scanning(false);
                b.set_scan_status(s(format!("Artists: updated {ok} of {total}")));
                a.toast(format!("Updated {ok} of {total} artists"));
                a.refresh_library();
                a.load_artist_view();
                a.update_fullscreen_art();
            });
        });
    }

    pub(super) fn open_url(&self, url: &str) {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return;
        }
        #[cfg(windows)]
        let _ = connectors::hidden_command("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(url).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}
