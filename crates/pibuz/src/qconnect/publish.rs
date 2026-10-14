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

/// Tell the controller about a volume change that did not come from it.
///
/// A controller's own `SetVolume` is reported back by the shared renderer
/// (`QconnectApp`'s SetVolume echo); nothing reported a change made HERE — an
/// integrator's `POST /api/playback/volume` — so the app's slider sat
/// where it was while the level moved under it. In `external` mode that is
/// the whole point of the POST (issue #3): the level lives with whoever owns
/// the DAC, and this is how the phone learns it.
///
/// Echo-safe the same way the queue publish is: the cloud's current view of
/// OUR volume (`session_renderer_states`, updated from every
/// `CTRL_VOLUME_CHANGED` it sends) is compared first, so a level that came from
/// the controller — or that we already reported — goes nowhere.
pub(crate) async fn publish_local_volume_if_changed(
    inner: &Arc<Mutex<DaemonQconnectInner>>,
    runtime: &Arc<AppRuntime<DaemonAdapter>>,
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
    let ours = volume_mode.reported_volume_pct(runtime.core().get_playback_state().volume);
    {
        let state = sync_state.lock().await;
        if !is_local_renderer_active(&state.session) {
            return;
        }
        let renderer = state
            .session
            .local_renderer_id
            .and_then(|id| state.session_renderer_states.get(&id));
        let theirs = renderer.and_then(|renderer| renderer.volume);
        let muted = renderer.and_then(|renderer| renderer.muted) == Some(true);
        if volume_report_is_redundant(ours, theirs, muted) {
            return;
        }
    }
    log::info!("[QConnect] Reporting volume {ours}% (changed locally)");
    let report = RendererReport::new(
        RendererReportType::RndrSrvrVolumeChanged,
        Uuid::new_v4().to_string(),
        app.queue_state_snapshot().await.version,
        json!({ "volume": ours }),
    );
    if let Err(err) = app.send_renderer_report_command(report).await {
        log::warn!("[QConnect] Failed to report a local volume change: {err}");
    }
}

/// Whether the controller already shows `ours`. An unknown view (`None`) is
/// NOT redundant: the controller has told us nothing, so tell it.
///
/// A mute is the controller's too: it arrives as `MuteVolume`, which the
/// player carries out as a level of 0 while the cloud keeps the pre-mute level
/// and a `muted` flag. Reporting that 0 as a volume would drag the phone's
/// slider to the bottom, and unmuting would then report the old level back.
fn volume_report_is_redundant(ours: i32, theirs: Option<i32>, muted: bool) -> bool {
    theirs == Some(ours) || (muted && ours == 0)
}

/// The volume-publish subscriber: coalesces `CoreEvent::VolumeChanged` bursts
/// (a slider drag) over a short window, then runs
/// [`publish_local_volume_if_changed`] once. Same #521 contract as
/// [`spawn_queue_cloud_publish`]: aborted+joined in `QconnectHandle::shutdown`.
pub(crate) fn spawn_volume_cloud_publish(
    inner: Arc<Mutex<DaemonQconnectInner>>,
    runtime: Arc<AppRuntime<DaemonAdapter>>,
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
            publish_local_volume_if_changed(&inner, &runtime).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::volume_report_is_redundant;

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

    #[test]
    fn a_level_the_controller_already_shows_is_not_reported_again() {
        // The controller's own SetVolume lands here too (the player emits
        // VolumeChanged for it); the cloud's echo has already told us it
        // shows that level, so a report would be a duplicate.
        assert!(volume_report_is_redundant(40, Some(40), false));
    }

    #[test]
    fn a_local_change_is_reported() {
        assert!(!volume_report_is_redundant(25, Some(40), false));
    }

    #[test]
    fn the_controllers_own_mute_is_not_reported_as_a_level_of_zero() {
        assert!(volume_report_is_redundant(0, Some(40), true));
        // But a level changed here while muted is still news.
        assert!(!volume_report_is_redundant(25, Some(40), true));
    }

    #[test]
    fn an_unknown_controller_view_is_reported() {
        assert!(!volume_report_is_redundant(25, None, false));
    }
}
