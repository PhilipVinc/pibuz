// crates/pibuz/src/events_bridge.rs — bridge playback-driver edges onto the
// CoreEvent bus.
//
// The 450 ms playback driver detects track/play-state transitions
// (`DriverAction::ReportEdge` -> `deps.on_edge`), but does not emit the
// playback CoreEvent variants the bus consumers are written for; without this
// bridge `/api/events` (SSE) / `pibuz watch` carry no playback traffic at all.
// This task wakes on the same edge Notify pulse the
// QConnect report scheduler uses (with a 2 s poll fallback, because
// `plan_tick` only reports edges while a track id is present — a stop that
// clears the track id would otherwise go unseen), reads the live player state,
// dedups against what it last published, and sends the delta to the bus.
//
// Holds only a Weak<AppRuntime> (upgraded per wake, dropped before the next
// wait), so it sits outside the #521 audio-release ordering — the caller
// aborts it for a clean shutdown.
use std::sync::{Arc, Weak};
use std::time::Duration;

use qbz_app::shell::AppRuntime;
use qbz_models::{CoreEvent, PlaybackState};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

use crate::adapter::DaemonAdapter;
use crate::state::DaemonShared;

type Runtime = Arc<AppRuntime<DaemonAdapter>>;

/// The poll fallback cadence: only exercised when no edge pulse arrives (e.g.
/// a stop cleared the track id, which suppresses `ReportEdge`).
const FALLBACK_POLL: Duration = Duration::from_secs(2);

/// Spawn the bridge task. Emits, deduped against the last published values:
/// * `TrackStarted` on a track-id change while playing (with the queue's
///   current-track metadata),
/// * `PlaybackStateChanged` on a playing/paused/stopped change,
/// * `PositionUpdated` on every wake while playing (~2 s cadence, the
///   progress feed for SSE clients),
/// * `VolumeChanged` on a volume change.
pub(crate) fn spawn(
    runtime: &Runtime,
    shared: Arc<std::sync::Mutex<DaemonShared>>,
    bus: broadcast::Sender<CoreEvent>,
    edge: Arc<Notify>,
) -> JoinHandle<()> {
    let weak: Weak<AppRuntime<DaemonAdapter>> = Arc::downgrade(runtime);
    tokio::spawn(async move {
        let mut last_track_id: u64 = 0;
        let mut last_state: Option<PlaybackState> = None;
        let mut last_volume: Option<f32> = None;
        let mut was_at_rest = false;
        loop {
            // Wake on a driver edge, or fall back to a slow poll so a
            // track-id-clearing stop still surfaces as a transition.
            let _ = tokio::time::timeout(FALLBACK_POLL, edge.notified()).await;

            let Some(rt) = weak.upgrade() else { return };
            let core = rt.core();
            let player = core.player();
            let ev = player.get_playback_event();
            let has_audio = player.has_loaded_audio();

            if ev.track_id != 0 && ev.track_id != last_track_id && ev.is_playing {
                // The queue's current track carries the metadata (title/artist/
                // artwork). Skip when the cursor hasn't caught up yet — the next
                // wake retries because last_track_id only advances on a send.
                let queue = core.get_queue_state().await;
                if let Some(track) = queue.current_track.as_ref().filter(|t| t.id == ev.track_id) {
                    let _ = bus.send(CoreEvent::TrackStarted {
                        track: track.clone(),
                        position_secs: ev.position,
                    });
                    last_track_id = ev.track_id;
                }
            }

            let rests_here = shared
                .lock()
                .is_ok_and(|s| s.queue_at_rest && s.qconnect.is_active);
            let state = playback_state(ev.is_playing, has_audio, rests_here);
            if last_state != Some(state) {
                let _ = bus.send(CoreEvent::PlaybackStateChanged { state });
                last_state = Some(state);
                // A stop ends the "current track": forget it so replaying the
                // same track emits a fresh TrackStarted (hooks, SSE clients).
                if state == PlaybackState::Stopped {
                    last_track_id = 0;
                }
            }
            // So does a queue coming to rest, which reads as Paused rather than
            // Stopped: play from there starts the queue again, and a one-track
            // queue starts the very track that just ended.
            if rests_here && !was_at_rest {
                last_track_id = 0;
            }
            was_at_rest = rests_here;

            if ev.is_playing {
                let _ = bus.send(CoreEvent::PositionUpdated {
                    position_secs: ev.position,
                    duration_secs: ev.duration,
                });
            }

            if last_volume.is_none_or(|v| (v - ev.volume).abs() > 0.001) {
                let _ = bus.send(CoreEvent::VolumeChanged { volume: ev.volume });
                last_volume = Some(ev.volume);
            }
        }
    })
}

/// What the bus calls the player's state.
///
/// Loaded-but-not-playing audio is Paused, nothing loaded is Stopped — except a
/// queue that played out and came to rest on its first track while this device
/// holds the render (`rests_here`). Nothing is loaded then, but the controller
/// shows that track paused at 0:00 and play starts the queue again
/// (`qconnect::report`), and the session is very much still ours. Integrators
/// read `stopped` as "the session is over" — moOde's hook marks the renderer
/// inactive on it and its worker restarts the daemon, which dropped the device
/// from the app seven seconds after every album ended (#6). Losing the render
/// while at rest still reads as Stopped, so a real disconnect is not hidden.
///
/// Callers dedup on the MAPPED state, not `is_playing` — paused and stopped
/// are both not-playing, and a pause -> stop transition must still emit.
fn playback_state(is_playing: bool, has_audio: bool, rests_here: bool) -> PlaybackState {
    if is_playing {
        PlaybackState::Playing
    } else if has_audio || rests_here {
        PlaybackState::Paused
    } else {
        PlaybackState::Stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_audio_is_paused_and_nothing_loaded_is_stopped() {
        assert_eq!(playback_state(true, true, false), PlaybackState::Playing);
        assert_eq!(playback_state(false, true, false), PlaybackState::Paused);
        assert_eq!(playback_state(false, false, false), PlaybackState::Stopped);
    }

    /// #6, case A: the last track of an album ends, the driver stops and
    /// rewinds to the first track with nothing loaded. The hook heard `paused`,
    /// then `stopped`, and moOde restarted the daemon on the second one.
    #[test]
    fn a_queue_at_rest_on_this_device_stays_paused_once_the_audio_unloads() {
        assert_eq!(playback_state(false, true, true), PlaybackState::Paused);
        assert_eq!(playback_state(false, false, true), PlaybackState::Paused);
    }

    /// Playing wins over the latch: the report loop clears it once something
    /// plays, but the bridge may read it a wake before that.
    #[test]
    fn playing_wins_over_a_stale_rest() {
        assert_eq!(playback_state(true, true, true), PlaybackState::Playing);
    }
}
