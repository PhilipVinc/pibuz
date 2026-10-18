//! Daemon-side local-queue -> Connect-cloud publish.
//!
//! Without it a daemon-originated queue (CLI or API) never reaches the cloud,
//! and controllers render a different queue than the one actually playing.
//!
//! The gates, in order: live runtime -> `is_local_renderer_active` (a peer
//! owns playback -> the peer publishes) -> non-empty -> echo-suppress vs the
//! cloud's last-applied queue -> per-session `last_pushed_queue_ids` latch
//! -> admission (every track must be a Qobuz track).
//!
//! Trigger: a debounced `CoreEvent::QueueUpdated` subscriber, which ALSO
//! covers queue edits while paused/stopped — a hook on track transitions
//! would miss those.

use std::sync::Arc;

use qbz_app::shell::AppRuntime;
use qbz_models::{CoreEvent, RepeatMode};
use qconnect_app::renderer::{loop_mode_from_repeat_mode, qconnect_repeat_mode_from_loop_mode};
use qconnect_app::{
    is_local_renderer_active, QueueCommandType, RendererReport, RendererReportType,
};
use serde_json::json;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::DaemonQconnectInner;
use crate::adapter::DaemonAdapter;

/// Push the local core queue to the Connect session when it differs from the
/// cloud's. No-op under any of the gates listed in the module docs. Echo-safe
/// by construction: the inbound materialize path sets `last_applied_queue_state`
/// to the very queue it materialized locally, so a controller-pushed queue
/// compares equal and is never bounced back.
pub(crate) async fn publish_local_queue_if_changed(
    inner: &Arc<Mutex<DaemonQconnectInner>>,
    runtime: &Arc<AppRuntime<DaemonAdapter>>,
) {
    let (app, sync_state) = {
        let guard = inner.lock().await;
        match guard.runtime.as_ref() {
            Some(rt) => (Arc::clone(&rt.app), Arc::clone(&rt.sync_state)),
            None => return,
        }
    };

    // Only push while WE are the active renderer (the user is driving the
    // daemon). When a peer owns playback, the peer publishes its own queue.
    {
        let state = sync_state.lock().await;
        if !is_local_renderer_active(&state.session) {
            return;
        }
    }

    let (tracks, current_index) = runtime.core().get_all_queue_tracks().await;
    if tracks.is_empty() {
        return;
    }
    let ordered_ids: Vec<u64> = tracks.iter().map(|track| track.id).collect();

    // Echo-suppress: skip when this is the cloud's current queue (materialized
    // inbound) so our own adoption / a remote queue change never bounces back.
    {
        let state = sync_state.lock().await;
        if let Some(applied) = &state.last_applied_queue_state {
            let applied_ids: Vec<u64> = applied
                .queue_items
                .iter()
                .map(|item| item.track_id)
                .collect();
            if applied_ids == ordered_ids {
                return;
            }
        }
    }
    // ...and skip when we already pushed this exact queue (cloud echo pending).
    {
        let guard = inner.lock().await;
        if guard.last_pushed_queue_ids.as_deref() == Some(ordered_ids.as_slice()) {
            return;
        }
    }

    // Admission: refuse the whole push if any track isn't Qobuz-castable.
    let all_eligible = tracks.iter().all(|track| {
        let source = track
            .source
            .as_deref()
            .unwrap_or("qobuz")
            .to_ascii_lowercase();
        source != "local" && source != "plex" && track.id > 0
    });
    if !all_eligible {
        log::info!("[QConnect] Local queue has non-Qobuz tracks; not casting to Connect");
        // Remember it so we don't re-log on every queue event within this queue.
        let mut guard = inner.lock().await;
        guard.last_pushed_queue_ids = Some(ordered_ids);
        return;
    }

    let count = ordered_ids.len();
    let track_ids: Vec<i64> = ordered_ids.iter().map(|id| *id as i64).collect();
    let start_index = current_index.unwrap_or(0);
    let payload = json!({
        "track_ids": track_ids,
        "queue_position": start_index,
        "shuffle_mode": false,
        "shuffle_pivot_index": start_index,
        "context_uuid": Uuid::new_v4().to_string(),
        "autoplay_reset": true,
        "autoplay_loading": false,
    });
    let command = app
        .build_queue_command(QueueCommandType::CtrlSrvrQueueLoadTracks, payload)
        .await;
    match app.send_queue_command(command).await {
        Ok(_) => {
            log::info!(
                "[QConnect] Pushed local queue to Connect ({count} tracks, start={start_index})"
            );
            let mut guard = inner.lock().await;
            guard.last_pushed_queue_ids = Some(ordered_ids);
        }
        Err(err) => log::warn!("[QConnect] Failed to push local queue: {err}"),
    }
}

/// Tell the session about a repeat mode changed HERE — a manual skip turning
/// repeat-one off, or `POST /api/playback/repeat` — with `CtrlSrvrSetLoopMode`,
/// as the web player does. Nothing did before, so the phones went on showing
/// the old mode while the box played another.
///
/// Echo-safe like the others: the session's own loop mode is compared first,
/// and a mode that came FROM the session is already equal to it. Never pushed
/// while the session's mode is still unknown, so connecting does not overwrite
/// the session with whatever this box last played in.
pub(crate) async fn publish_local_loop_mode_if_changed(
    inner: &Arc<Mutex<DaemonQconnectInner>>,
    runtime: &Arc<AppRuntime<DaemonAdapter>>,
) {
    let (app, sync_state) = {
        let guard = inner.lock().await;
        match guard.runtime.as_ref() {
            Some(rt) => (Arc::clone(&rt.app), Arc::clone(&rt.sync_state)),
            None => return,
        }
    };
    let session_mode = {
        let state = sync_state.lock().await;
        if !is_local_renderer_active(&state.session) {
            return;
        }
        state.session_loop_mode
    };
    let local = runtime.core().get_queue_state().await.repeat;
    let Some(loop_mode) = loop_mode_to_publish(local, session_mode) else {
        return;
    };
    let command = app
        .build_queue_command(
            QueueCommandType::CtrlSrvrSetLoopMode,
            json!({ "loop_mode": loop_mode }),
        )
        .await;
    match app.send_queue_command(command).await {
        Ok(_) => log::info!("[QConnect] Told the session the loop mode is now {loop_mode}"),
        Err(err) => log::warn!("[QConnect] Failed to publish the loop mode: {err}"),
    }
}

/// The wire loop mode to send for a local repeat mode, or `None` when the
/// session already has it or its mode is not known yet.
fn loop_mode_to_publish(local: RepeatMode, session: Option<i32>) -> Option<i32> {
    let session = session.and_then(qconnect_repeat_mode_from_loop_mode)?;
    (session != local).then(|| loop_mode_from_repeat_mode(local))
}

/// The queue-publish subscriber: debounces `CoreEvent::QueueUpdated` bursts by
/// 2 s, then runs
/// [`publish_local_queue_if_changed`]. Non-queue events are drained WITHOUT
/// extending the debounce window, so they can never starve the publish. Holds
/// `Arc` clones of the qconnect inner + the runtime, so the handle is
/// aborted+joined in `QconnectHandle::shutdown` ahead of `drop(booted)` (the
/// #521 ordering), exactly like the report scheduler.
pub(crate) fn spawn_queue_cloud_publish(
    inner: Arc<Mutex<DaemonQconnectInner>>,
    runtime: Arc<AppRuntime<DaemonAdapter>>,
    mut rx: broadcast::Receiver<CoreEvent>,
) -> JoinHandle<()> {
    use tokio::sync::broadcast::error::RecvError;
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);
    tokio::spawn(async move {
        loop {
            // Block until the FIRST queue mutation of a burst. A repeat-mode
            // change goes out at once: it is one event, not a burst, and the
            // phones should not show the old mode for two seconds.
            match rx.recv().await {
                Ok(CoreEvent::QueueUpdated { .. }) => {}
                Ok(CoreEvent::RepeatModeChanged { .. }) => {
                    publish_local_loop_mode_if_changed(&inner, &runtime).await;
                    continue;
                }
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
            // Debounce: a fixed deadline that only a further QueueUpdated extends.
            let mut deadline = tokio::time::Instant::now() + DEBOUNCE;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    r = rx.recv() => match r {
                        Ok(CoreEvent::QueueUpdated { .. }) => {
                            deadline = tokio::time::Instant::now() + DEBOUNCE;
                        }
                        Ok(CoreEvent::RepeatModeChanged { .. }) => {
                            publish_local_loop_mode_if_changed(&inner, &runtime).await;
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return,
                    }
                }
            }
            publish_local_queue_if_changed(&inner, &runtime).await;
        }
    })
}

/// Tell the controller about a volume or mute change that did not come from it.
///
/// A controller's own `SetVolume` is reported back by the shared renderer
/// (`QconnectApp`'s SetVolume echo); nothing reported a change made HERE — an
/// integrator's `POST /api/playback/volume` — so the app's slider sat
/// where it was while the level moved under it. In `external` mode that is
/// the whole point of the POST (issue #3): the level lives with whoever owns
/// the DAC, and this is how the phone learns it.
///
/// A mute goes out as a mute (`VOLUME_MUTED`), the message the controller's
/// own mute produces, and the level reported is the NOMINAL one — the level
/// the unmute brings back. Reporting a box-side mute as a level of 0 dragged
/// the phone's slider to the bottom, and the unmute was then lost (issue #4).
///
/// Echo-safe the same way the queue publish is: the cloud's current view of
/// OUR volume and mute (`session_renderer_states`, updated from every
/// `CTRL_VOLUME_CHANGED` / `CTRL_VOLUME_MUTED` it sends) is compared first, so
/// a change that came from the controller — or that we already reported —
/// goes nowhere.
pub(crate) async fn publish_local_volume_if_changed(
    inner: &Arc<Mutex<DaemonQconnectInner>>,
    runtime: &Arc<AppRuntime<DaemonAdapter>>,
    shared: &super::SharedState,
) {
    let (app, sync_state, volume_mode) = {
        let guard = inner.lock().await;
        match guard.runtime.as_ref() {
            Some(rt) => (
                Arc::clone(&rt.app),
                Arc::clone(&rt.sync_state),
                rt.volume_mode,
            ),
            None => return,
        }
    };
    let mute = crate::mute::current(shared);
    let live = runtime.core().get_playback_state().volume;
    let ours = volume_mode.reported_volume_pct(mute.nominal(live));
    let catch_up = {
        let state = sync_state.lock().await;
        if !is_local_renderer_active(&state.session) {
            return;
        }
        let renderer = state
            .session
            .local_renderer_id
            .and_then(|id| state.session_renderer_states.get(&id));
        volume_catch_up(
            ours,
            mute.is_muted(),
            renderer.and_then(|renderer| renderer.volume),
            renderer.and_then(|renderer| renderer.muted),
        )
    };
    let version = app.queue_state_snapshot().await.version;
    if let Some(muted) = catch_up.muted {
        log::info!(
            "[QConnect] Reporting {} (changed locally)",
            if muted { "mute" } else { "unmute" }
        );
        let report = RendererReport::new(
            RendererReportType::RndrSrvrVolumeMuted,
            Uuid::new_v4().to_string(),
            version,
            json!({ "value": muted }),
        );
        if let Err(err) = app.send_renderer_report_command(report).await {
            log::warn!("[QConnect] Failed to report a local mute change: {err}");
        }
    }
    if let Some(level) = catch_up.level {
        log::info!("[QConnect] Reporting volume {level}% (changed locally)");
        let report = RendererReport::new(
            RendererReportType::RndrSrvrVolumeChanged,
            Uuid::new_v4().to_string(),
            version,
            json!({ "volume": level }),
        );
        if let Err(err) = app.send_renderer_report_command(report).await {
            log::warn!("[QConnect] Failed to report a local volume change: {err}");
        }
    }
}

/// What the controller has to be told to show what we are doing: our mute
/// flag and our NOMINAL level, each only where its view (`theirs_*`, the
/// cloud's echoes) differs. An unknown level (`None`) is reported — the
/// controller has told us nothing, so tell it. An unknown mute reads as
/// unmuted, the cloud's own default, so an unmuted renderer says nothing.
#[derive(Debug, Default, PartialEq, Eq)]
struct VolumeCatchUp {
    muted: Option<bool>,
    level: Option<i32>,
}

fn volume_catch_up(
    ours_level: i32,
    ours_muted: bool,
    theirs_level: Option<i32>,
    theirs_muted: Option<bool>,
) -> VolumeCatchUp {
    VolumeCatchUp {
        muted: (theirs_muted.unwrap_or(false) != ours_muted).then_some(ours_muted),
        level: (theirs_level != Some(ours_level)).then_some(ours_level),
    }
}

/// The volume-publish subscriber: coalesces `CoreEvent::VolumeChanged` bursts
/// (a slider drag) over a short window, then runs
/// [`publish_local_volume_if_changed`] once. Same #521 contract as
/// [`spawn_queue_cloud_publish`]: aborted+joined in `QconnectHandle::shutdown`.
pub(crate) fn spawn_volume_cloud_publish(
    inner: Arc<Mutex<DaemonQconnectInner>>,
    runtime: Arc<AppRuntime<DaemonAdapter>>,
    shared: super::SharedState,
    mut rx: broadcast::Receiver<CoreEvent>,
) -> JoinHandle<()> {
    use tokio::sync::broadcast::error::RecvError;
    // Short: this is a slider the listener is watching, not a queue edit.
    const COALESCE: std::time::Duration = std::time::Duration::from_millis(150);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(CoreEvent::VolumeChanged { .. }) => {}
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
            let deadline = tokio::time::Instant::now() + COALESCE;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    r = rx.recv() => if let Err(RecvError::Closed) = r { return },
                }
            }
            publish_local_volume_if_changed(&inner, &runtime, &shared).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{volume_catch_up, VolumeCatchUp};

    #[test]
    fn a_loop_mode_goes_out_only_when_the_session_shows_another() {
        use super::loop_mode_to_publish as publish;
        use qbz_models::RepeatMode;
        // A manual skip turned repeat-one off while the session still says one.
        assert_eq!(publish(RepeatMode::Off, Some(2)), Some(1));
        assert_eq!(publish(RepeatMode::All, Some(1)), Some(3));
        // The session's own change, applied here: already equal, so no echo.
        assert_eq!(publish(RepeatMode::One, Some(2)), None);
        // 0 is "unknown" on the wire and reads as off, like everywhere else.
        assert_eq!(publish(RepeatMode::Off, Some(0)), None);
        // Before the session has said anything: never push ours over it.
        assert_eq!(publish(RepeatMode::One, None), None);
    }

    fn catch_up(muted: Option<bool>, level: Option<i32>) -> VolumeCatchUp {
        VolumeCatchUp { muted, level }
    }

    #[test]
    fn what_the_controller_already_shows_is_not_reported_again() {
        // The controller's own SetVolume or mute lands here too (the player
        // emits VolumeChanged for it); the cloud's echo has already told us
        // it shows that, so a report would be a duplicate.
        assert_eq!(
            volume_catch_up(40, false, Some(40), Some(false)),
            catch_up(None, None)
        );
        assert_eq!(
            volume_catch_up(40, true, Some(40), Some(true)),
            catch_up(None, None)
        );
    }

    #[test]
    fn a_local_level_change_is_reported() {
        assert_eq!(
            volume_catch_up(25, false, Some(40), Some(false)),
            catch_up(None, Some(25))
        );
    }

    /// Issue #4: mute from the box at 40, then unmute. Both go out as mutes,
    /// and the level — nominal, so 40 throughout — never moves the slider.
    #[test]
    fn a_mute_from_the_box_is_reported_as_a_mute_and_so_is_its_unmute() {
        assert_eq!(
            volume_catch_up(40, true, Some(40), Some(false)),
            catch_up(Some(true), None)
        );
        assert_eq!(
            volume_catch_up(40, false, Some(40), Some(true)),
            catch_up(Some(false), None)
        );
    }

    #[test]
    fn a_level_changed_while_muted_is_still_news() {
        assert_eq!(
            volume_catch_up(25, true, Some(40), Some(true)),
            catch_up(None, Some(25))
        );
    }

    #[test]
    fn an_unknown_controller_view() {
        // An unknown level is reported; an unknown mute reads as unmuted.
        assert_eq!(
            volume_catch_up(25, false, None, None),
            catch_up(None, Some(25))
        );
        assert_eq!(
            volume_catch_up(25, true, Some(25), None),
            catch_up(Some(true), None)
        );
    }
}
