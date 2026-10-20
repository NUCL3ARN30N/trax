//! OS media controls (media keys, Windows media overlay, MPRIS on Linux, Now Playing on macOS).

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use std::time::Duration;

pub enum MediaAction {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    Seek(Duration),
    Raise,
}

pub struct MediaKeys {
    controls: MediaControls,
}

impl MediaKeys {
    pub fn new(window: &slint::Window, handler: fn(MediaAction)) -> Option<MediaKeys> {
        #[cfg(windows)]
        let hwnd = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let handle = window.window_handle();
            match handle.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
                _ => None,
            }
        };
        #[cfg(not(windows))]
        let hwnd = {
            let _ = window;
            None
        };
        let config = PlatformConfig { display_name: "trax", dbus_name: "trax", hwnd };
        let mut controls = MediaControls::new(config).ok()?;
        controls
            .attach(move |e| {
                let action = match e {
                    MediaControlEvent::Toggle => MediaAction::PlayPause,
                    MediaControlEvent::Play => MediaAction::Play,
                    MediaControlEvent::Pause | MediaControlEvent::Stop => MediaAction::Pause,
                    MediaControlEvent::Next => MediaAction::Next,
                    MediaControlEvent::Previous => MediaAction::Previous,
                    MediaControlEvent::SetPosition(MediaPosition(p)) => MediaAction::Seek(p),
                    MediaControlEvent::Raise => MediaAction::Raise,
                    _ => return,
                };
                let _ = slint::invoke_from_event_loop(move || handler(action));
            })
            .ok()?;
        Some(MediaKeys { controls })
    }

    pub fn set_metadata(&mut self, title: &str, artist: &str, album: &str, cover: Option<&str>, duration: Duration) {
        let cover_url = cover.map(|c| format!("file://{}", c.replace('\\', "/")));
        let _ = self.controls.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: Some(album),
            cover_url: cover_url.as_deref(),
            duration: if duration.is_zero() { None } else { Some(duration) },
        });
    }

    pub fn set_playing(&mut self, playing: bool, position: Duration) {
        let progress = Some(MediaPosition(position));
        let _ = self.controls.set_playback(if playing {
            MediaPlayback::Playing { progress }
        } else {
            MediaPlayback::Paused { progress }
        });
    }

    pub fn stop(&mut self) {
        let _ = self.controls.set_playback(MediaPlayback::Stopped);
    }
}
