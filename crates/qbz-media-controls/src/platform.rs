//! macOS (MediaRemote / MPNowPlayingInfoCenter) + Windows (SMTC) backend via
//! souvlaki. No MPRIS / DesktopEntry here — macOS keys the Now Playing icon off
//! the app bundle; Windows SMTC off the package. On macOS `MediaControls` is a
//! zero-sized handle over global objc singletons (Send + Sync) and its command
//! callbacks fire on the app's run loop (Slint's winit loop), so it is driven
//! from any thread without main-thread marshaling — same as the Tauri build.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

use crate::types::{MediaEvent, MediaIntegration, PlaybackStatus, TrackMeta};

/// Default step for a magnitude-less MPRIS-style `Seek` (5 seconds, micros).
const SEEK_STEP_MICROS: i64 = 5_000_000;

type EventCb = Arc<dyn Fn(MediaEvent) + Send + Sync>;

pub struct PlatformHandle {
    controls: Arc<Mutex<Option<MediaControls>>>,
}

impl MediaIntegration for PlatformHandle {
    fn set_metadata(&self, meta: &TrackMeta) {
        if let Ok(mut guard) = self.controls.lock() {
            if let Some(c) = guard.as_mut() {
                let md = MediaMetadata {
                    title: Some(meta.title.as_str()),
                    artist: Some(meta.artist.as_str()),
                    album: Some(meta.album.as_str()),
                    duration: meta.duration,
                    cover_url: meta.art_url.as_deref(),
                };
                let _ = c.set_metadata(md);
            }
        }
    }

    fn set_playback(&self, status: PlaybackStatus, position: Option<Duration>) {
        if let Ok(mut guard) = self.controls.lock() {
            if let Some(c) = guard.as_mut() {
                let progress = position.map(MediaPosition);
                let pb = match status {
                    PlaybackStatus::Playing => MediaPlayback::Playing { progress },
                    PlaybackStatus::Paused => MediaPlayback::Paused { progress },
                    PlaybackStatus::Stopped => MediaPlayback::Stopped,
                };
                let _ = c.set_playback(pb);
            }
        }
    }

    fn set_volume(&self, _vol: f64) {
        // souvlaki exposes no outbound volume (SMTC/MediaRemote manage it);
        // inbound SetVolume still arrives as a MediaEvent. No-op here.
    }
}

fn map_event(e: MediaControlEvent) -> Option<MediaEvent> {
    Some(match e {
        MediaControlEvent::Play => MediaEvent::Play,
        MediaControlEvent::Pause => MediaEvent::Pause,
        MediaControlEvent::Toggle => MediaEvent::Toggle,
        MediaControlEvent::Next => MediaEvent::Next,
        MediaControlEvent::Previous => MediaEvent::Previous,
        MediaControlEvent::Stop => MediaEvent::Stop,
        MediaControlEvent::Raise => MediaEvent::Raise,
        MediaControlEvent::Quit => MediaEvent::Quit,
        MediaControlEvent::Seek(SeekDirection::Forward) => MediaEvent::SeekBy(SEEK_STEP_MICROS),
        MediaControlEvent::Seek(SeekDirection::Backward) => MediaEvent::SeekBy(-SEEK_STEP_MICROS),
        MediaControlEvent::SeekBy(dir, dur) => {
            let micros = dur.as_micros() as i64;
            MediaEvent::SeekBy(match dir {
                SeekDirection::Forward => micros,
                SeekDirection::Backward => -micros,
            })
        }
        MediaControlEvent::SetPosition(pos) => MediaEvent::SetPosition(pos.0.as_micros() as i64),
        MediaControlEvent::SetVolume(v) => MediaEvent::SetVolume(v),
        MediaControlEvent::OpenUri(_) => return None,
    })
}

pub fn spawn(on_event: EventCb) -> Option<PlatformHandle> {
    let config = PlatformConfig {
        dbus_name: "pibuz",
        display_name: "Pibuz",
        // macOS: unused. Windows SMTC needs the window HWND; not shipped/tested,
        // so left None (init may fail there → None handle, no media controls).
        hwnd: None,
    };

    let mut controls = match MediaControls::new(config) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[media-controls] souvlaki init failed: {e:?}");
            return None;
        }
    };

    if let Err(e) = controls.attach(move |event: MediaControlEvent| {
        if let Some(ev) = map_event(event) {
            on_event(ev);
        }
    }) {
        log::warn!("[media-controls] souvlaki attach failed: {e:?}");
        return None;
    }

    log::info!("[media-controls] souvlaki (SMTC/MediaRemote) initialized");
    Some(PlatformHandle {
        controls: Arc::new(Mutex::new(Some(controls))),
    })
}
