//! Slint models backed by `Track` vectors. Covers are loaded lazily, only for visible rows.

use crate::TrackItem;
use crate::connectors::thumb_path;
use crate::model::{Track, format_duration};
use slint::{Image, Model, ModelNotify, ModelTracker, SharedString};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Default)]
pub struct ImageCache {
    thumbs: RefCell<HashMap<String, Option<Image>>>,
}

impl ImageCache {
    /// Small thumbnail for list rows (falls back to the full cover).
    pub fn thumb(&self, cover: &str) -> Option<Image> {
        if let Some(img) = self.thumbs.borrow().get(cover) {
            return img.clone();
        }
        let thumb = thumb_path(cover);
        let img = Image::load_from_path(&thumb).or_else(|_| Image::load_from_path(std::path::Path::new(cover))).ok();
        self.thumbs.borrow_mut().insert(cover.to_string(), img.clone());
        img
    }

    pub fn forget(&self, path: &str) {
        self.thumbs.borrow_mut().remove(path);
    }

    pub fn full(cover: &str) -> Option<Image> {
        Image::load_from_path(std::path::Path::new(cover)).ok()
    }
}

/// Ids of starred tracks, shared by all track lists.
pub type Favorites = Rc<RefCell<std::collections::HashSet<i64>>>;

pub struct TrackModel {
    favorites: Favorites,
    tracks: RefCell<Vec<Track>>,
    /// (source, uri) of the playing track.
    playing: RefCell<Option<(crate::model::SourceKind, String)>>,
    /// Highlight by exact row instead of by identity (used for the queue).
    playing_row: RefCell<Option<usize>>,
    by_row: bool,
    images: Rc<ImageCache>,
    notify: ModelNotify,
}

impl TrackModel {
    pub fn new(images: Rc<ImageCache>, by_row: bool, favorites: Favorites) -> Rc<Self> {
        Rc::new(TrackModel {
            favorites,
            tracks: RefCell::new(vec![]),
            playing: RefCell::new(None),
            playing_row: RefCell::new(None),
            by_row,
            images,
            notify: ModelNotify::default(),
        })
    }

    pub fn set(&self, tracks: Vec<Track>) {
        *self.tracks.borrow_mut() = tracks;
        self.notify.reset();
    }

    pub fn tracks(&self) -> std::cell::Ref<'_, Vec<Track>> {
        self.tracks.borrow()
    }

    pub fn get(&self, row: usize) -> Option<Track> {
        self.tracks.borrow().get(row).cloned()
    }

    pub fn len(&self) -> usize {
        self.tracks.borrow().len()
    }

    /// Update one track everywhere it appears (e.g. after tags/cover were read).
    pub fn update_track(&self, t: &Track) {
        let mut changed = vec![];
        {
            let mut tracks = self.tracks.borrow_mut();
            for (i, row) in tracks.iter_mut().enumerate() {
                if row.source == t.source && row.uri == t.uri {
                    let play_count = row.play_count;
                    *row = t.clone();
                    row.play_count = play_count;
                    changed.push(i);
                }
            }
        }
        for i in changed {
            self.notify.row_changed(i);
        }
    }

    /// Refresh the star of every row showing `id`.
    pub fn favorite_changed(&self, id: i64) {
        let rows: Vec<usize> =
            self.tracks.borrow().iter().enumerate().filter(|(_, t)| t.id == id).map(|(i, _)| i).collect();
        for r in rows {
            self.notify.row_changed(r);
        }
    }

    pub fn set_playing(&self, key: Option<(crate::model::SourceKind, String)>, row: Option<usize>) {
        let old_key = self.playing.replace(key.clone());
        let old_row = self.playing_row.replace(row);
        if self.by_row {
            for r in [old_row, row].into_iter().flatten() {
                if r < self.len() {
                    self.notify.row_changed(r);
                }
            }
            return;
        }
        if old_key == key {
            return;
        }
        let rows: Vec<usize> = self
            .tracks
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let k = (t.source, t.uri.clone());
                Some(&k) == old_key.as_ref() || Some(&k) == key.as_ref()
            })
            .map(|(i, _)| i)
            .collect();
        for r in rows {
            self.notify.row_changed(r);
        }
    }
}

impl Model for TrackModel {
    type Data = TrackItem;

    fn row_count(&self) -> usize {
        self.tracks.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<TrackItem> {
        let tracks = self.tracks.borrow();
        let t = tracks.get(row)?;
        let cover = t.cover.as_deref().and_then(|c| self.images.thumb(c));
        let playing = if self.by_row {
            *self.playing_row.borrow() == Some(row)
        } else {
            self.playing.borrow().as_ref().map(|(s, u)| *s == t.source && *u == t.uri).unwrap_or(false)
        };
        Some(TrackItem {
            id: t.id as i32,
            title: SharedString::from(if t.title.is_empty() { t.uri.as_str() } else { t.title.as_str() }),
            artist: SharedString::from(t.display_artist()),
            album: SharedString::from(t.album.as_str()),
            duration: format_duration(t.duration_ms).into(),
            source: t.source.label().into(),
            has_cover: cover.is_some(),
            cover: cover.unwrap_or_default(),
            favorite: self.favorites.borrow().contains(&t.id),
            playing,
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
