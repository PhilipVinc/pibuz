// crates/pibuz/src/hooks.rs — run a user script on daemon events (CONSOLE ext).
//
// Headless integrators (moOde, Volumio, home-grown setups) need PUSH
// notifications to coordinate the daemon with the rest of an audio box — stop
// the local player when a Qobuz Connect session starts playing, restore the
// volume when it ends — without keeping a poller or an SSE client alive.
// `hooks.script` names an executable the daemon forks once per bus event with
// the event described in `QBZ_*` environment variables, the same integration
// contract pleezer (`--hook`) and shairport-sync (`run_this_...`) established.
//
// Enablement: the `QBZD_HOOK` env var wins when set (empty disables);
// otherwise the persisted `daemon_prefs.hook_script` path, written by `pibuz
// settings set hooks.script`. Changing it needs a daemon restart (the
// dispatcher spawns at boot, like MPRIS).
//
// The dispatcher never waits for the script inside the recv loop (a slow
// script would lag the broadcast bus) — each event forks detached and a small
// reaper task collects the exit status. Scripts therefore may overlap; an
// integrator needing strict ordering can flock inside the script.
use std::path::PathBuf;

use qbz_models::{CoreEvent, PlaybackState};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::paths::ProfileRoots;

/// Resolve the configured hook script: `QBZD_HOOK` when set (empty string
/// disables), else the persisted `hooks.script` setting; None = hooks off.
pub fn script(roots: &ProfileRoots) -> Option<PathBuf> {
    let raw = match std::env::var("QBZD_HOOK") {
        Ok(v) => v,
        Err(_) => qbz_app::settings::daemon_prefs::load_at(&roots.data).hook_script,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Spawn the hook dispatcher. Holds no `Arc<AppRuntime>` (only the script path
/// and the bus receiver), so it sits outside the §8.2 audio-release ordering —
/// the caller aborts it for a clean shutdown, like the scrobbler.
pub fn spawn(script: PathBuf, mut rx: broadcast::Receiver<CoreEvent>) -> JoinHandle<()> {
    use broadcast::error::RecvError;
    tokio::spawn(async move {
        let mut gate = StopSettle::default();
        loop {
            let due = gate.due();
            let ev = tokio::select! {
                received = rx.recv() => match received {
                    Ok(ev) => ev,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                },
                _ = async {
                    match due {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(stop) = gate.settle(std::time::Instant::now()) {
                        run(&script, &stop);
                    }
                    continue;
                }
            };
            for ev in gate.offer(ev, std::time::Instant::now()) {
                run(&script, &ev);
            }
        }
    })
}

fn run(script: &std::path::Path, ev: &CoreEvent) {
    let Some(vars) = hook_env(ev) else { return };
    let mut cmd = tokio::process::Command::new(script);
    cmd.envs(vars).stdin(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            // Detached reaper: never block the recv loop on the script.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(e) => log::warn!("[hooks] could not run {}: {e}", script.display()),
    }
}

/// How long a `stopped` must last before the hook script hears it.
const STOP_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// Holds a `stopped` back until it has lasted [`STOP_SETTLE`].
///
/// The player is briefly "stopped" in the middle of things that are not the
/// end of anything: a new cast replacing the queue tears the old one down
/// before the new track loads, and an account taking the renderer over from
/// another does the same. Integrators read `stopped` as "the session is over"
/// — moOde's hook marks the renderer inactive on it, and moOde's worker then
/// RESTARTS the renderer — so a two-second gap in a takeover became a restart
/// in the middle of the new listener's session. On the Pi, 2026-09-27, 18:14:
/// a takeover, `stopped` for two seconds, a full stop/start of the daemon, and
/// the renderer vanished from the new controller's app until it rejoined.
///
/// Only the HOOK is held back. SSE and MPRIS still see every state as it
/// happens; a script that forks per event is the consumer that acts on a
/// flicker. A genuine stop reaches it [`STOP_SETTLE`] late, which is noise
/// next to how long a stopped session stays stopped.
#[derive(Default)]
struct StopSettle {
    /// The held `stopped`, and when it may go out.
    pending: Option<(std::time::Instant, CoreEvent)>,
}

impl StopSettle {
    /// Take one bus event; return what should reach the script now.
    fn offer(&mut self, ev: CoreEvent, now: std::time::Instant) -> Vec<CoreEvent> {
        match ev {
            CoreEvent::PlaybackStateChanged {
                state: PlaybackState::Stopped,
            } => {
                // A second stop while one is held keeps the ORIGINAL deadline:
                // it is the same stop, and restarting the clock would let a
                // chatty sequence hold it back for ever.
                if self.pending.is_none() {
                    self.pending = Some((now + STOP_SETTLE, ev));
                }
                Vec::new()
            }
            CoreEvent::PlaybackStateChanged { .. } => {
                // Anything else within the window means it was a gap, not an
                // end: the held stop is dropped, never delivered.
                self.pending = None;
                vec![ev]
            }
            other => vec![other],
        }
    }

    /// When the held stop falls due, if one is held.
    fn due(&self) -> Option<std::time::Instant> {
        self.pending.as_ref().map(|(at, _)| *at)
    }

    /// The held stop, once it has lasted long enough to be real.
    fn settle(&mut self, now: std::time::Instant) -> Option<CoreEvent> {
        match &self.pending {
            Some((at, _)) if *at <= now => self.pending.take().map(|(_, ev)| ev),
            _ => None,
        }
    }
}

fn state_label(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
        PlaybackState::Stopped => "stopped",
        PlaybackState::Loading => "loading",
    }
}

/// Map one bus event onto the script's `QBZ_*` environment, or None for
/// events the hook does not forward. Deliberately excluded: `PositionUpdated`
/// (a fork every ~2 s), `QueueUpdated` (bulky, and queue state is one
/// `GET /api/status` away), and everything on the SSE suppress list. The
/// `LoggedIn` session payload (auth token) is NEVER forwarded — the event
/// name alone is.
fn hook_env(ev: &CoreEvent) -> Option<Vec<(String, String)>> {
    use CoreEvent::*;
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| vars.push((format!("QBZ_{k}"), v));
    match ev {
        TrackStarted {
            track,
            position_secs,
        } => {
            push("EVENT", "TrackStarted".into());
            push("TRACK_ID", track.id.to_string());
            push("TITLE", track.title.clone());
            push("ARTIST", track.artist.clone());
            push("ALBUM", track.album.clone());
            push("DURATION", track.duration_secs.to_string());
            push("POSITION", position_secs.to_string());
            if let Some(url) = &track.artwork_url {
                push("COVER_URL", url.clone());
            }
            if let Some(bits) = track.bit_depth {
                push("BIT_DEPTH", bits.to_string());
            }
            if let Some(rate) = track.sample_rate {
                push("SAMPLE_RATE", rate.to_string());
            }
        }
        PlaybackStateChanged { state } => {
            push("EVENT", "PlaybackStateChanged".into());
            push("STATE", state_label(*state).into());
        }
        TrackEnded { track_id } => {
            push("EVENT", "TrackEnded".into());
            push("TRACK_ID", track_id.to_string());
        }
        VolumeChanged { volume } => {
            push("EVENT", "VolumeChanged".into());
            // 0-100, the same scale the `pibuz volume` verb speaks.
            push(
                "VOLUME",
                (((*volume).clamp(0.0, 1.0) * 100.0).round() as u32).to_string(),
            );
        }
        QconnectSessionChanged {
            state,
            device_name,
            session_active,
        } => {
            push("EVENT", "QconnectSessionChanged".into());
            push("STATE", state.clone());
            push("SESSION_ACTIVE", session_active.to_string());
            if let Some(name) = device_name {
                push("DEVICE_NAME", name.clone());
            }
        }
        LoggedIn { .. } => push("EVENT", "LoggedIn".into()),
        LoggedOut => push("EVENT", "LoggedOut".into()),
        SessionExpired => push("EVENT", "SessionExpired".into()),
        Error {
            code,
            message,
            recoverable,
        } => {
            push("EVENT", "Error".into());
            push("CODE", code.clone());
            push("MESSAGE", message.clone());
            push("RECOVERABLE", recoverable.to_string());
        }
        PlaybackError { track_id, message } => {
            push("EVENT", "PlaybackError".into());
            push("TRACK_ID", track_id.to_string());
            push("MESSAGE", message.clone());
        }
        AudioDeviceChanged { device_name } => {
            push("EVENT", "AudioDeviceChanged".into());
            push("DEVICE_NAME", device_name.clone());
        }
        _ => return None,
    }
    // The full tagged JSON for scripts that prefer jq over positional vars —
    // except LoggedIn, whose session payload stays out of child environments.
    if !matches!(ev, LoggedIn { .. }) {
        if let Ok(json) = serde_json::to_string(ev) {
            vars.push(("QBZ_EVENT_JSON".to_string(), json));
        }
    }
    Some(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// A minimal QueueTrack via its serde defaults (the struct has no Default
    /// impl, and adding one to the shared models is not this feature's call).
    fn test_track() -> qbz_models::QueueTrack {
        serde_json::from_value(serde_json::json!({
            "id": 42,
            "title": "Red Beans",
            "artist": "Jon Batiste",
            "album": "Monk Movements",
            "duration_secs": 163,
            "artwork_url": "https://example.com/cover.jpg",
            "bit_depth": 24,
            "sample_rate": 48.0,
            "album_id": null,
            "artist_id": null,
        }))
        .expect("QueueTrack from JSON")
    }

    #[test]
    fn track_started_carries_metadata_and_json() {
        let track = test_track();
        let vars = hook_env(&CoreEvent::TrackStarted {
            track,
            position_secs: 7,
        })
        .expect("TrackStarted is forwarded");
        assert_eq!(get(&vars, "QBZ_EVENT"), Some("TrackStarted"));
        assert_eq!(get(&vars, "QBZ_TITLE"), Some("Red Beans"));
        assert_eq!(get(&vars, "QBZ_ARTIST"), Some("Jon Batiste"));
        assert_eq!(get(&vars, "QBZ_DURATION"), Some("163"));
        assert_eq!(get(&vars, "QBZ_POSITION"), Some("7"));
        assert_eq!(get(&vars, "QBZ_BIT_DEPTH"), Some("24"));
        let json = get(&vars, "QBZ_EVENT_JSON").expect("JSON payload present");
        assert!(json.contains("\"type\":\"TrackStarted\""));
    }

    #[test]
    fn playback_state_uses_lowercase_labels() {
        for (state, label) in [
            (PlaybackState::Playing, "playing"),
            (PlaybackState::Paused, "paused"),
            (PlaybackState::Stopped, "stopped"),
            (PlaybackState::Loading, "loading"),
        ] {
            let vars = hook_env(&CoreEvent::PlaybackStateChanged { state }).unwrap();
            assert_eq!(get(&vars, "QBZ_STATE"), Some(label));
        }
    }

    #[test]
    fn qconnect_session_changed_is_forwarded() {
        let vars = hook_env(&CoreEvent::QconnectSessionChanged {
            state: "connected".into(),
            device_name: Some("Moode Qobuz".into()),
            session_active: true,
        })
        .unwrap();
        assert_eq!(get(&vars, "QBZ_EVENT"), Some("QconnectSessionChanged"));
        assert_eq!(get(&vars, "QBZ_STATE"), Some("connected"));
        assert_eq!(get(&vars, "QBZ_SESSION_ACTIVE"), Some("true"));
        assert_eq!(get(&vars, "QBZ_DEVICE_NAME"), Some("Moode Qobuz"));
    }

    #[test]
    fn logged_in_forwards_the_event_name_but_never_the_session() {
        let session: qbz_models::UserSession = serde_json::from_value(serde_json::json!({
            "user_auth_token": "SECRET-TOKEN",
            "user_id": 1,
            "email": "user@example.com",
            "display_name": "User",
            "subscription_label": "Studio",
        }))
        .expect("UserSession from JSON");
        let vars = hook_env(&CoreEvent::LoggedIn { session }).unwrap();
        assert_eq!(get(&vars, "QBZ_EVENT"), Some("LoggedIn"));
        // Redaction: no JSON payload, no token, exactly one variable.
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn chatty_and_bulky_events_are_not_forwarded() {
        assert!(hook_env(&CoreEvent::PositionUpdated {
            position_secs: 1,
            duration_secs: 2
        })
        .is_none());
        assert!(hook_env(&CoreEvent::ShuffleChanged { enabled: true }).is_none());
        assert!(hook_env(&CoreEvent::LoadingStarted {
            operation: "x".into()
        })
        .is_none());
    }

    fn state(state: PlaybackState) -> CoreEvent {
        CoreEvent::PlaybackStateChanged { state }
    }

    fn is_stop(ev: &CoreEvent) -> bool {
        matches!(
            ev,
            CoreEvent::PlaybackStateChanged {
                state: PlaybackState::Stopped
            }
        )
    }

    /// The Pi, 2026-09-27, 18:14:38-40: a takeover stopped the old queue and
    /// loaded the new one two seconds later. The script must never hear that
    /// stop — moOde restarted the whole renderer on it, mid-session.
    #[test]
    fn a_stop_that_turns_into_loading_never_reaches_the_script() {
        let mut gate = StopSettle::default();
        let t0 = std::time::Instant::now();
        assert!(gate.offer(state(PlaybackState::Stopped), t0).is_empty());
        let out = gate.offer(
            state(PlaybackState::Loading),
            t0 + std::time::Duration::from_secs(2),
        );
        assert_eq!(out.len(), 1, "loading goes out as usual");
        assert!(!is_stop(&out[0]));
        assert!(
            gate.due().is_none(),
            "and the stop is gone, not merely late"
        );
        assert!(gate.settle(t0 + STOP_SETTLE * 2).is_none());
    }

    /// A stop that LASTS is a real one, and goes out once it has.
    #[test]
    fn a_stop_that_lasts_is_delivered_after_the_settle() {
        let mut gate = StopSettle::default();
        let t0 = std::time::Instant::now();
        assert!(gate.offer(state(PlaybackState::Stopped), t0).is_empty());
        assert_eq!(gate.due(), Some(t0 + STOP_SETTLE));
        assert!(gate.settle(t0 + STOP_SETTLE / 2).is_none(), "not yet");
        let out = gate.settle(t0 + STOP_SETTLE).expect("delivered");
        assert!(is_stop(&out));
        assert!(gate.due().is_none());
    }

    /// Other events are not held while a stop is: the volume restore and the
    /// session-changed events moOde reacts to still arrive in time, and they
    /// do not cancel the stop either.
    #[test]
    fn other_events_pass_straight_through_a_held_stop() {
        let mut gate = StopSettle::default();
        let t0 = std::time::Instant::now();
        gate.offer(state(PlaybackState::Stopped), t0);
        let out = gate.offer(CoreEvent::VolumeChanged { volume: 0.4 }, t0);
        assert_eq!(out.len(), 1);
        assert!(gate.due().is_some(), "the stop is still held");
    }

    /// A repeated stop is the same stop: the deadline does not move, or a
    /// chatty sequence could hold a real stop back indefinitely.
    #[test]
    fn a_repeated_stop_keeps_its_original_deadline() {
        let mut gate = StopSettle::default();
        let t0 = std::time::Instant::now();
        gate.offer(state(PlaybackState::Stopped), t0);
        gate.offer(
            state(PlaybackState::Stopped),
            t0 + std::time::Duration::from_secs(3),
        );
        assert_eq!(gate.due(), Some(t0 + STOP_SETTLE));
    }

    #[test]
    fn volume_is_forwarded_on_the_cli_percent_scale() {
        let vars = hook_env(&CoreEvent::VolumeChanged { volume: 0.45 }).unwrap();
        assert_eq!(get(&vars, "QBZ_VOLUME"), Some("45"));
    }
}
