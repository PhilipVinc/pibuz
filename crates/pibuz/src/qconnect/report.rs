//! Renderer playback-state report.
//!
//! `report_playback_state` is a free function the report tick calls on a tokio
//! interval. It self-gates on `is_local_renderer_active`, resolves
//! current/next queue_item_id from the playing track, sends a
//! `RndrSrvrStateUpdated`, keeps the app's renderer position in sync, and reports
//! the live output format for the controller's quality badge. `position_ms` /
//! `duration_ms` are MILLISECONDS (the QConnect protocol unit).

use std::sync::Arc;

use qbz_app::shell::AppRuntime;
use qconnect_app::{
    is_local_renderer_active, QconnectFileAudioQualitySnapshot, QconnectRemoteSyncState,
    RendererReport, RendererReportType,
};
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::sink::DaemonQconnectApp;
use super::transport::{BUFFER_STATE_BUFFERING, BUFFER_STATE_OK};
use crate::adapter::DaemonAdapter;

pub(crate) const QCONNECT_RENDERER_CHANNELS: i32 = 2;
/// The player's clock has reached the end of the track it is playing.
///
/// Not a proxy for anything subtle: if we are playing and the position has
/// caught up with the duration, the next track has not started yet.
fn clock_is_at_the_end(is_playing: bool, position_secs: u64, duration_secs: u64) -> bool {
    is_playing && duration_secs > 0 && position_secs >= duration_secs
}

/// Debounce [`clock_is_at_the_end`] into "a load is in progress".
///
/// `at_end_since` is the caller's memory of when the clock first arrived there,
/// cleared as soon as it moves again. The grace exists so an ordinary gapless
/// hand-off, which passes through this state for a fraction of a second, never
/// flashes a spinner at the controller.
fn awaiting_next_track(
    at_end: bool,
    at_end_since: &mut Option<std::time::Instant>,
    grace: std::time::Duration,
) -> bool {
    if !at_end {
        *at_end_since = None;
        return false;
    }
    let since = *at_end_since.get_or_insert_with(std::time::Instant::now);
    since.elapsed() >= grace
}

const AUDIO_QUALITY_UNKNOWN: i32 = 0;
const AUDIO_QUALITY_MP3: i32 = 1;
const AUDIO_QUALITY_CD: i32 = 2;
const AUDIO_QUALITY_HIRES_L1: i32 = 3;
const AUDIO_QUALITY_HIRES_L2: i32 = 4;

/// Tell the cloud the load of `track_id` failed (`PlaybackError`, message type
/// 2), as the official renderer does. Same gate as the state report: only while
/// we are the active renderer, and only for a track the cloud's queue holds —
/// the error names a queue item, and without one there is nothing to name.
async fn report_playback_error(
    app: &Arc<DaemonQconnectApp>,
    sync_state: &Arc<Mutex<QconnectRemoteSyncState>>,
    track_id: u64,
    error_type: i32,
) {
    {
        let state = sync_state.lock().await;
        if !is_local_renderer_active(&state.session) {
            return;
        }
    }
    // The pure resolver, not `resolve_queue_item_ids_by_track_id`: that one
    // also moves the reported cursor, which a failure report has no business
    // doing — the state report that follows decides what the cursor says.
    let queue = app.queue_state_snapshot().await;
    let (queue_item_id, _, _) =
        qconnect_app::queue_resolution::resolve_queue_item_ids_from_queue_state(&queue, track_id);
    let Some(queue_item_id) = queue_item_id else {
        log::info!("[QConnect] Track {track_id} failed but is not in the queue; not reported");
        return;
    };
    let queue_version = queue.version;
    let report = RendererReport::new(
        RendererReportType::PlaybackError,
        Uuid::new_v4().to_string(),
        queue_version,
        json!({ "queue_item_id": queue_item_id, "error_type": error_type }),
    );
    match app.send_renderer_report_command(report).await {
        Ok(_) => log::info!(
            "[QConnect] Reported playback error {error_type} on queue item {queue_item_id} \
             (track {track_id})"
        ),
        Err(err) => log::warn!("[QConnect] Failed to report the playback error: {err}"),
    }
}

/// Report this device's playback state to the cloud while the daemon is the
/// ACTIVE LOCAL renderer. Self-gates on `is_local_renderer_active` (no-op when a
/// PEER owns playback), resolves the current/next queue_item_id from the playing
/// track, sends a `RndrSrvrStateUpdated`, and keeps the app's renderer position
/// in sync.
pub(crate) async fn report_playback_state(
    app: &Arc<DaemonQconnectApp>,
    sync_state: &Arc<Mutex<QconnectRemoteSyncState>>,
    runtime: &Arc<AppRuntime<DaemonAdapter>>,
    playing_state: i32,
    position_ms: i64,
    duration_ms: i64,
    track_id: u64,
    buffer_state: i32,
) {
    // Only report when WE are the active renderer. When a peer renderer owns
    // playback (the daemon is acting as a controller) the renderer reports come
    // from the peer, not us.
    {
        let state = sync_state.lock().await;
        if !is_local_renderer_active(&state.session) {
            return;
        }
    }

    let (current_qid, next_qid) =
        resolve_queue_item_ids_by_track_id(app, sync_state, track_id).await;
    let queue_version = app.queue_state_snapshot().await.version;

    let report = RendererReport::new(
        RendererReportType::RndrSrvrStateUpdated,
        Uuid::new_v4().to_string(),
        queue_version,
        json!({
            "playing_state": playing_state,
            "buffer_state": buffer_state,
            "current_position": position_ms,
            "duration": duration_ms,
            "current_queue_item_id": current_qid,
            "next_queue_item_id": next_qid,
            "queue_version": {
                "major": queue_version.major,
                "minor": queue_version.minor
            }
        }),
    );
    if let Err(err) = app.send_renderer_report_command(report).await {
        log::warn!("[QConnect] Failed to report playback state: {err}");
    }

    // Not while buffering: `renderer.current_position_ms` is the HIGHER-priority
    // input to the renderer's load-offset and seek decisions, so publishing the
    // offset a stream is still filling toward makes a later state-only SetState
    // (a bare pause/resume, carrying no position of its own) compare the player's
    // real clock against it and fire a seek to a track that is not playing yet.
    // The in-flight offset is for the CONTROLLER's benefit only.
    if position_ms >= 0 && buffer_state != BUFFER_STATE_BUFFERING {
        app.update_renderer_position(position_ms as u64).await;
    }
    // The duration IS published while buffering, unlike the position: it belongs
    // to the track being loaded and is what the SetState echo needs in order to
    // avoid blanking the controller's display. Nothing seeks on a duration.
    if duration_ms > 0 {
        app.update_renderer_duration(duration_ms as u64).await;
    }

    // Report the live output format so the controller shows the correct quality
    // badge (CD / Hi-Res). Reads the player's current output (sample_rate/
    // bit_depth); channels default to stereo. Both reports dedup internally in
    // qconnect-app, so calling them every report tick is cheap.
    let player = runtime.core().player();
    let sample_rate = player.state.get_sample_rate();
    let bit_depth = player.state.get_bit_depth();
    if let Some(snapshot) =
        build_file_audio_quality_snapshot(sample_rate, bit_depth, QCONNECT_RENDERER_CHANNELS)
    {
        if let Err(err) = app
            .report_file_audio_quality_if_changed(queue_version, snapshot)
            .await
        {
            log::warn!("[QConnect] Failed to report file audio quality: {err}");
        }
        // FIX (daemon-copy only): the DEVICE report must describe what the DAC
        // is actually receiving, not the source file. Both reports used to
        // carry the stream format, so a device resampling 24/96 down to 24/48
        // still told the controller it was running 24/96 — the protocol has
        // separate File and Device messages precisely to distinguish them.
        // /proc/asound carries the negotiated hardware rate; fall back to the
        // stream format when nothing is open (nothing better to say).
        let device = qbz_audio::dac_probe::negotiated_active_rate();
        let (device_rate, device_channels) = match &device {
            Some(negotiated) => (negotiated.sample_rate as i32, negotiated.channels as i32),
            None => (snapshot.sampling_rate, snapshot.nb_channels),
        };
        // ALSA reports a container format (24-bit audio commonly rides in
        // S32_LE), so the container width would overstate the real depth —
        // keep the stream's bit depth, which is the honest number.
        if let Err(err) = app
            .report_device_audio_quality_if_changed(
                queue_version,
                device_rate,
                snapshot.bit_depth,
                device_channels,
            )
            .await
        {
            log::warn!("[QConnect] Failed to report device audio quality: {err}");
        }
    }
}

/// Classify a (sample_rate, bit_depth) output into the QConnect AudioQuality
/// level, the way the web player does. It classifies by Qobuz format id
/// (5 MP3, 6 CD, 7 Hi-Res up to 96 kHz, 27 Hi-Res above), so anything above
/// 96 kHz is HIRES_L2 — 176.4 kHz included — and HIRES_L3 is never reported:
/// the apps offer exactly four tiers.
fn classify_audio_quality(sample_rate: u32, bit_depth: u32) -> i32 {
    if sample_rate == 0 || bit_depth == 0 {
        AUDIO_QUALITY_UNKNOWN
    } else if sample_rate > 96_000 {
        AUDIO_QUALITY_HIRES_L2
    } else if bit_depth > 16 || sample_rate > 48_000 {
        AUDIO_QUALITY_HIRES_L1
    } else if sample_rate >= 44_100 {
        AUDIO_QUALITY_CD
    } else {
        AUDIO_QUALITY_MP3
    }
}

/// Build a file-audio-quality snapshot from the live output format, or None when
/// the format isn't known yet.
fn build_file_audio_quality_snapshot(
    sample_rate: u32,
    bit_depth: u32,
    nb_channels: i32,
) -> Option<QconnectFileAudioQualitySnapshot> {
    if sample_rate == 0 || bit_depth == 0 {
        return None;
    }
    Some(QconnectFileAudioQualitySnapshot {
        sampling_rate: sample_rate as i32,
        bit_depth: bit_depth as i32,
        nb_channels,
        audio_quality: classify_audio_quality(sample_rate, bit_depth),
    })
}

/// Resolve the current + next `queue_item_id` for a playing `track_id` from the
/// cloud queue snapshot, caching the result into the sync accumulator.
async fn resolve_queue_item_ids_by_track_id(
    app: &Arc<DaemonQconnectApp>,
    sync_state: &Arc<Mutex<QconnectRemoteSyncState>>,
    track_id: u64,
) -> (Option<u64>, Option<u64>) {
    let queue = app.queue_state_snapshot().await;
    let (current_qid, next_qid, next_track_id) =
        qconnect_app::queue_resolution::resolve_queue_item_ids_from_queue_state(&queue, track_id);

    if let Some(current_qid) = current_qid {
        let mut state = sync_state.lock().await;
        state.last_renderer_queue_item_id = Some(current_qid);
        state.last_renderer_next_queue_item_id = next_qid;
        state.last_renderer_track_id = Some(track_id);
        state.last_renderer_next_track_id = next_track_id;
        (Some(current_qid), next_qid)
    } else {
        (None, None)
    }
}

// The report-tick scheduler. The daemon has no UI poll loop to report from, so
// a dedicated tokio task owns the cadence. It calls `report_playback_state` on
// the LIVE session (a no-op when not connected or when a peer owns playback,
// since the body self-gates on `is_local_renderer_active`).
//
// Two triggers (~2 s while playing + edge-triggered on track/play-state
// transitions):
//   * `notify` — the driver's `DriverAction::ReportEdge` signal (daemon.rs wires
//     `on_edge -> Notify::notify_one`). The driver folds the ~2 s periodic
//     cadence AND the transition edges into this one signal
//     (`playback_driver`, `transition || periodic`).
//   * a ~2 s `interval` — the periodic FLOOR. Because the driver
//     already supplies the periodic edge, the interval is RESET on every wake so
//     it only elapses when the edge stream goes quiet (no double-reporting during
//     active playback); interval-driven reports are additionally gated on
//     `is_playing`, so a paused/stopped renderer stays silent.
pub(crate) async fn run_report_scheduler(
    notify: Arc<tokio::sync::Notify>,
    inner: Arc<Mutex<super::DaemonQconnectInner>>,
    runtime: Arc<AppRuntime<DaemonAdapter>>,
    buffering: Arc<super::engine::BufferingLatch>,
    shared: super::SharedState,
) {
    // The periodic floor. It tightens to LOADING_FLOOR while a stream is
    // filling, because the two things that end a load — audio becoming audible,
    // and the real position/duration replacing the blank ones in the shared
    // SetState echo — have no event of their own and can only go out on a tick.
    // At the 2 s floor the spinner ran on for up to two seconds past the first
    // sample, and the controller sat on a 0:00 duration just as long.
    const IDLE_FLOOR: std::time::Duration = std::time::Duration::from_millis(2_000);
    const LOADING_FLOOR: std::time::Duration = std::time::Duration::from_millis(300);

    // A fresh interval fires its first tick immediately; start one period out.
    let period_from = |floor: std::time::Duration| {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + floor, floor);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    };

    // How long the clock must sit at the end of a track before we call it a
    // load. Long enough that an ordinary gapless hand-off — where this is true
    // for a fraction of a second — never flashes a spinner at the controller.
    const AWAITING_NEXT_GRACE: std::time::Duration = std::time::Duration::from_millis(1_000);

    let mut floor = IDLE_FLOOR;
    let mut interval = period_from(floor);
    let mut was_buffering = false;
    // When the clock first reached the end of the current track.
    let mut at_end_since: Option<std::time::Instant> = None;
    // (playing_state, buffer_state, track) of the last report we sent, so a
    // transition can be told from a routine position update.
    let mut last_signature: Option<(i32, i32, u64)> = None;

    loop {
        let via_interval = tokio::select! {
            _ = notify.notified() => false,
            _ = interval.tick() => true,
        };

        // Read the live player state.
        let ev = runtime.core().player().get_playback_event();
        // The load in flight, if any. Asked of the latch rather than of the
        // player: until the new stream produces audio the player still reports
        // the OUTGOING track, so anything keyed on ev.track_id missed the whole
        // load window and only noticed once audio had started. The MILLISECOND
        // clock is what makes the audible edge prompt — see `in_flight`.
        let player = runtime.core().player();
        let in_flight = buffering.in_flight_with_state(
            ev.track_id,
            player.state.current_position_ms(),
            ev.is_playing,
        );
        // Between tracks. The clock has reached the end of the track and the
        // next one has not started, which is a load in progress that the latch
        // never hears about: a gapless prefetch runs inside the player, with no
        // `begin`/`finish` around it. Without this the controller sat at
        // "4:18 / 4:18", PLAYING and buffer OK, for as long as the prefetch
        // took — the spinner only appeared later, when the cloud happened to
        // send a SetState for the next track and armed the latch.
        let awaiting_next = awaiting_next_track(
            clock_is_at_the_end(ev.is_playing, ev.position, ev.duration),
            &mut at_end_since,
            AWAITING_NEXT_GRACE,
        );
        let is_buffering = in_flight.is_some() || awaiting_next;
        // Re-arm the floor for whichever phase we are now in, and reset it either
        // way so the floor only elapses after a full period of edge silence.
        let wanted = if is_buffering {
            LOADING_FLOOR
        } else {
            IDLE_FLOOR
        };
        if wanted == floor {
            interval.reset();
        } else {
            floor = wanted;
            interval = period_from(floor);
        }
        // Whether the last report we sent claimed BUFFERING. A load that FAILS
        // clears the latch without the player ever adopting the track, so
        // without this the falling edge fell into the `continue` below and the
        // controller was left spinning on a load that had already given up.
        let falling_edge = was_buffering && !is_buffering;
        was_buffering = is_buffering;
        // Nothing loaded, nothing loading, and nothing to retract.
        if ev.track_id == 0 && !is_buffering && !falling_edge {
            continue;
        }

        // The periodic floor only fires while actually playing (or buffering);
        // edge notifications (transitions + the driver's periodic) always report.
        if via_interval && !ev.is_playing && !is_buffering && !falling_edge {
            continue;
        }

        // Reconcile the queue cursor with the audible track. A gapless hand-off
        // advances inside the player, and the driver only syncs the cursor on
        // the exact tick the track id changes while playing on BOTH sides of
        // the tick — a playback-state blip during the hand-off ("PlayNext
        // landed after track finished") loses that edge for good, leaving the
        // cursor one track behind: `pibuz status` and the moOde overlay named
        // the previous track while the next one played (title said "Golden
        // Seams" while the reported duration, 213s, was "Pulse"). Skipped while
        // buffering, where the cursor is legitimately AHEAD of the player: the
        // stream for the new track has not started yet, and syncing there would
        // drag the cursor back to the outgoing track. `sync_current_to_id` only
        // moves the pointer (and emits) when it actually differs.
        if ev.is_playing && ev.track_id != 0 && !is_buffering {
            runtime.core().sync_current_to_id(ev.track_id).await;
        }

        // Resolve the LIVE session (app + the shared sync accumulator). No runtime
        // means QConnect is not connected -> a no-op this tick.
        let (app, sync_state) = {
            let guard = inner.lock().await;
            match guard.runtime.as_ref() {
                Some(rt) => (Arc::clone(&rt.app), Arc::clone(&rt.sync_state)),
                None => continue,
            }
        };

        // A load that failed since the last tick: name it to the cloud BEFORE
        // the state report that retracts the spinner, so the controller learns
        // which track failed and why rather than only seeing it stop.
        if let Some((failed_track, error_type)) = buffering.take_failure() {
            report_playback_error(&app, &sync_state, failed_track, error_type).await;
        }

        // A played-out queue at rest on its first track (`queue_at_rest`, set
        // by the driver's rewind): the player still names the track that
        // ended, but the controller must show the first one, paused at 0:00,
        // as the Qobuz apps do — so that play starts the queue again. Ends the
        // moment anything plays or loads.
        //
        // Not keyed on `has_loaded_audio`: the driver's stop reaches the audio
        // thread some 50 ms after the rewind, so the report it prompts still
        // sees the ended track loaded.
        let at_rest = queue_rest_state(&shared, ev.is_playing || in_flight.is_some());
        let rested_on = if at_rest {
            runtime
                .core()
                .get_queue_state()
                .await
                .current_track
                .map(|track| (track.id, track.duration_secs))
        } else {
            None
        };

        let decision = decide_report(
            PlayerSnapshot {
                track_id: ev.track_id,
                // The MILLISECOND clock, not `ev.position`'s whole seconds: the
                // controller extrapolates from the position we send, so a
                // truncated one put its scrubber up to a second behind the audio
                // after every report — the web player sends milliseconds.
                position_ms: player.state.current_position_ms(),
                duration_secs: ev.duration,
                is_playing: ev.is_playing,
            },
            in_flight,
            awaiting_next,
        );
        let decision = match rested_on {
            Some((track_id, duration_secs)) => decide_report(
                PlayerSnapshot {
                    track_id,
                    position_ms: 0,
                    duration_secs,
                    is_playing: false,
                },
                None,
                false,
            ),
            None => decision,
        };
        // `report_playback_state` wants MILLISECONDS; the duration is seconds.
        report_playback_state(
            &app,
            &sync_state,
            &runtime,
            decision.playing_state,
            decision.position_ms as i64,
            (decision.duration_secs as i64) * 1000,
            decision.track_id,
            decision.buffer_state,
        )
        .await;
        // The cloud now shows the rested-on track; the renderer's cached view
        // must too, or a state-only play cold-loads the track that ended.
        if let Some((track_id, _)) = rested_on {
            let item = app
                .queue_state_snapshot()
                .await
                .queue_items
                .into_iter()
                .find(|item| item.track_id == track_id);
            if let Some(item) = item {
                app.update_renderer_current_track(item).await;
            }
        }

        // A state TRANSITION just went out (play/pause, a new track, buffering
        // starting or ending). Schedule a prompt follow-up rather than waiting
        // out the 2 s floor.
        //
        // Every controller command is echoed by the shared SetState handler,
        // which cannot know a duration — there is none in the renderer state to
        // read — so it reports `duration: null`. The controller blanks its
        // progress display on that, which is why hitting pause or play made the
        // time and total length flick to 0:00 and stay there until our next
        // report. That echo lands a few milliseconds AFTER ours, so being fast
        // is not enough; the fix is to speak again right behind it.
        let signature = (
            decision.playing_state,
            decision.buffer_state,
            decision.track_id,
        );
        if last_signature != Some(signature) {
            last_signature = Some(signature);
            floor = LOADING_FLOOR;
            interval = period_from(floor);
        }
    }
}

/// Read the driver's "queue at rest" latch, ending it once something plays
/// or starts loading: the listener has moved on.
fn queue_rest_state(shared: &super::SharedState, something_is_happening: bool) -> bool {
    let Ok(mut s) = shared.lock() else {
        return false;
    };
    if something_is_happening {
        s.queue_at_rest = false;
    }
    s.queue_at_rest
}

use qconnect_app::renderer::{PLAYING_STATE_PAUSED, PLAYING_STATE_PLAYING, PLAYING_STATE_STOPPED};

/// What the controller will be told this tick.
///
/// # Why this is a function and not four `let`s inside the loop
///
/// This decision used to live inline in the report loop, which needs a live
/// `AppRuntime`, a connected QConnect session and a real player. Nothing could
/// assert it, so the only way to learn what a controller would see was to cast
/// to a Pi and watch. Two spinner bugs shipped through that gap — a seek whose
/// clock was reported source-relative, and a track that loaded straight into a
/// pause — and neither was a fault in any single component. Both were this
/// triple being wrong.
///
/// Plain values in, plain values out: every combination of player state, load
/// state and hand-off timing runs in microseconds. See `report_decision_tests`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReportDecision {
    pub playing_state: i32,
    pub(crate) buffer_state: i32,
    pub track_id: u64,
    pub(crate) position_ms: u64,
    pub duration_secs: u64,
}

/// The slice of player state the decision reads — a trimmed `PlaybackEvent`, so
/// a test need not build one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlayerSnapshot {
    pub track_id: u64,
    pub(crate) position_ms: u64,
    pub duration_secs: u64,
    pub is_playing: bool,
}

/// `in_flight` is the latch's answer — `(track_id, start_secs, duration_secs)`
/// while a load is in flight. `awaiting_next` is the clock having run to the end
/// with the next track not yet started, a gapless prefetch the latch never hears
/// about.
pub(crate) fn decide_report(
    player: PlayerSnapshot,
    in_flight: Option<(u64, u64, u64)>,
    awaiting_next: bool,
) -> ReportDecision {
    let is_buffering = in_flight.is_some() || awaiting_next;

    // While buffering, report PLAYING + BUFFERING — the pair the official
    // client itself sends. Observed from the desktop app's own renderer reports
    // while IT buffers: playing_state 2, buffer_state 1, position and duration
    // populated, repeated for the whole load.
    //
    // UNKNOWN was tried (StreamCore32 uses it) and is worse here: for a fresh
    // track the position is 0, which the wire omits, so an UNKNOWN report
    // carries neither a state nor a position and the controller drew nothing at
    // all during a next-track load. The loading state comes from buffer_state;
    // the playing state's job is to say we intend to play.
    let playing_state = if player.is_playing || is_buffering {
        PLAYING_STATE_PLAYING
    } else if player.track_id == 0 {
        // Nothing loaded at all — only reachable on the falling edge of a load
        // that failed, where PAUSED would invite the controller to offer a
        // resume for audio that was never there.
        PLAYING_STATE_STOPPED
    } else {
        PLAYING_STATE_PAUSED
    };
    let buffer_state = if is_buffering {
        BUFFER_STATE_BUFFERING
    } else {
        BUFFER_STATE_OK
    };
    // While a load is in flight the report must describe the track being
    // loaded, taken from the latch — the player is still on the OUTGOING track
    // (or on nothing at all, freshly after a hand-off, where it has neither a
    // duration nor a track id). Reporting the player's own state there named
    // the previous song during a next-track load, and showed "0:00 of 0:00" for
    // the whole wait when switching output from another device.
    //
    // The offset the stream opened at is the honest position: on a resume at
    // 2:19 the controller should draw the scrubber there while it fills, not at
    // zero.
    let (track_id, position_ms, duration_secs) = match in_flight {
        Some((track_id, start_secs, duration_secs)) => {
            (track_id, start_secs.saturating_mul(1000), duration_secs)
        }
        None => (player.track_id, player.position_ms, player.duration_secs),
    };
    ReportDecision {
        playing_state,
        buffer_state,
        track_id,
        position_ms,
        duration_secs,
    }
}

/// What a CONTROLLER sees, driven through the real latch.
///
/// # Why these are not more unit tests of `BufferingLatch`
///
/// Both spinner bugs this renderer has shipped passed every unit test that
/// existed. Neither was a fault in the latch or in the report assembly; each
/// was the PAIR disagreeing — the latch waiting for a clock edge the player
/// was never going to deliver, and the report faithfully turning that into
/// BUFFERING forever. So these drive `BufferingLatch` and `decide_report`
/// together and assert the triple that reaches the Qobuz app.
///
/// # The rule for adding here
///
/// Prefer an invariant over an anecdote. `a_settled_player_always_reaches_a_
/// settled_report` is the one that matters: it sweeps the state space and
/// would have caught both shipped bugs before either reached hardware. A named
/// scenario is for pinning a specific wire shape a controller depends on.
#[cfg(test)]
mod report_decision_tests {
    use super::super::transport::{BUFFER_STATE_BUFFERING, BUFFER_STATE_OK};
    use super::*;
    use crate::qconnect::engine::BufferingLatch;

    /// One tick of the real pipeline: ask the latch, then decide the report.
    fn tick(latch: &BufferingLatch, player: PlayerSnapshot, awaiting_next: bool) -> ReportDecision {
        let in_flight =
            latch.in_flight_with_state(player.track_id, player.position_ms, player.is_playing);
        decide_report(player, in_flight, awaiting_next)
    }

    fn on(track_id: u64, position_secs: u64, is_playing: bool) -> PlayerSnapshot {
        PlayerSnapshot {
            track_id,
            position_ms: position_secs * 1000,
            duration_secs: 341,
            is_playing,
        }
    }

    /// The controller extrapolates from the position we report, so it must be
    /// the millisecond clock as read, not whole seconds: 12.345 s truncated to
    /// 12 s left its scrubber 345 ms behind the audio until the next report.
    #[test]
    fn a_playing_position_is_reported_to_the_millisecond() {
        let latch = BufferingLatch::default();
        let player = PlayerSnapshot {
            track_id: 7,
            position_ms: 12_345,
            duration_secs: 341,
            is_playing: true,
        };
        assert_eq!(tick(&latch, player, false).position_ms, 12_345);
    }

    /// THE FLICKER, the other way round. While a load is in flight the report
    /// must name the track being LOADED, never the one being left — the player
    /// is still on the outgoing track (or on nothing) until audio starts.
    ///
    /// Reported as the now-playing cover flicking to the previous song when
    /// jumping back a few tracks in the queue. Backwards jumps land on cached
    /// tracks, and the cache-hit path was the one that armed no latch, so the
    /// scheduler fell through to the player's own state and published the
    /// outgoing track as current.
    #[test]
    fn a_load_in_flight_reports_the_arriving_track_not_the_departing_one() {
        const LEAVING: u64 = 208204223;
        const ARRIVING: u64 = 74936706;

        let latch = BufferingLatch::default();
        latch.begin(ARRIVING, 0, 300);

        // Mid-changeover: the audio thread is still reporting the old track.
        let d = tick(&latch, on(LEAVING, 0, false), false);
        assert_eq!(
            d.track_id, ARRIVING,
            "the controller must be told what is arriving, not what is leaving"
        );
        assert_eq!(d.buffer_state, BUFFER_STATE_BUFFERING);
    }

    /// And with no load in flight the player is authoritative — otherwise a
    /// stale latch would pin the report to a track that already finished.
    #[test]
    fn with_nothing_in_flight_the_player_is_authoritative() {
        let latch = BufferingLatch::default();
        let d = tick(&latch, on(74936706, 12, true), false);
        assert_eq!(d.track_id, 74936706);
        assert_eq!(d.buffer_state, BUFFER_STATE_OK);
    }

    /// THE INVARIANT. A player that has arrived on the loading track and is
    /// not going anywhere must stop being reported as BUFFERING — promptly,
    /// and without help from the 90 s backstop.
    ///
    /// Both shipped spinner bugs are instances of this being false:
    ///
    /// - the seek bug: arrived, clock reported source-relative, sat below the
    ///   offset forever;
    /// - the pause bug: arrived at exactly the offset with the clock stopped,
    ///   so `position > start` could never become true.
    ///
    /// "Arrived and settled" means the player reports the loading track AND
    /// either its clock has moved past where the stream opened, or it is not
    /// playing. The one cell that is deliberately NOT here — playing, clock
    /// still exactly on the offset — is the honest "started, no audio yet";
    /// `the_instant_before_the_first_sample_is_the_only_arrived_spinner` pins
    /// that it is transient rather than a state you can sit in.
    #[test]
    fn a_settled_player_always_reaches_a_settled_report() {
        const TRACK: u64 = 62_589_633;
        for start_secs in [0_u64, 7, 166, 340] {
            for extra in [0_u64, 1, 30] {
                for is_playing in [true, false] {
                    if is_playing && extra == 0 {
                        continue; // the transient cell; see the test below
                    }
                    let latch = BufferingLatch::default();
                    latch.begin(TRACK, start_secs, 341);
                    let player = on(TRACK, start_secs + extra, is_playing);

                    // Hold that state. Nothing changes, so nothing may keep
                    // claiming a load is in flight.
                    let mut last = tick(&latch, player, false);
                    for _ in 0..50 {
                        last = tick(&latch, player, false);
                    }
                    assert_eq!(
                        last.buffer_state,
                        BUFFER_STATE_OK,
                        "a player sitting on track {TRACK} at {}s (opened at {start_secs}s, \
                         playing={is_playing}) still reports BUFFERING after 50 ticks. The \
                         controller shows a spinner on a session that has arrived and is not \
                         moving; the only thing that ends it is the 90 s backstop.",
                        start_secs + extra
                    );
                    assert_eq!(
                        last.track_id, TRACK,
                        "a settled report must name the track the player is on"
                    );
                    // A settled PAUSED report must say so — a controller shown
                    // PLAYING on a paused renderer draws a pause button that
                    // does nothing.
                    if !is_playing {
                        assert_eq!(last.playing_state, PLAYING_STATE_PAUSED);
                    }
                }
            }
        }
    }

    /// The one arrived-but-still-buffering cell, and proof it is a moment
    /// rather than a state: a playing track whose clock has not yet left the
    /// offset has not produced audio, so the spinner is honest — and the very
    /// next millisecond of clock ends it.
    #[test]
    fn the_instant_before_the_first_sample_is_the_only_arrived_spinner() {
        const TRACK: u64 = 62_589_633;
        let latch = BufferingLatch::default();
        latch.begin(TRACK, 7, 341);
        assert_eq!(
            tick(&latch, on(TRACK, 7, true), false).buffer_state,
            BUFFER_STATE_BUFFERING,
            "playing, but the clock has not moved off the offset: no audio has flowed yet"
        );
        assert_eq!(
            tick(&latch, on(TRACK, 8, true), false).buffer_state,
            BUFFER_STATE_OK,
            "and one second of clock is all it takes to clear it"
        );
    }

    /// The exclusion that keeps the invariant honest: until the new stream
    /// produces audio the player still reports the OUTGOING track, and THAT is
    /// a real load in flight. The spinner belongs there.
    #[test]
    fn a_load_that_has_not_arrived_keeps_its_spinner() {
        let latch = BufferingLatch::default();
        latch.begin(62_589_633, 0, 341);
        for player in [on(0, 0, false), on(62_589_600, 240, true)] {
            let d = tick(&latch, player, false);
            assert_eq!(
                d.buffer_state, BUFFER_STATE_BUFFERING,
                "the player is on {} — not the loading track — so the load really is in flight",
                player.track_id
            );
            assert_eq!(
                d.track_id, 62_589_633,
                "and the report must name the track being LOADED, not the outgoing one"
            );
        }
    }

    /// FROM HARDWARE (2026-09-13). Cast, pause, restart the daemon. The
    /// controller re-attaches with `SetState { playing_state: PAUSED,
    /// current_position_ms: 7000 }`; the renderer opens at 7 s, arrives, and
    /// pauses as asked — then reported PLAYING + BUFFERING for 90 seconds,
    /// position frozen, until the backstop fired.
    #[test]
    fn a_session_reattached_into_a_pause_reports_paused_not_buffering() {
        const TRACK: u64 = 62_589_633;
        let latch = BufferingLatch::default();
        latch.begin(TRACK, 7, 341);

        // Opening the stream: not there yet, spinner is correct.
        assert_eq!(
            tick(&latch, on(0, 0, false), false).buffer_state,
            BUFFER_STATE_BUFFERING
        );

        // Arrived at 7 s and paused, exactly as instructed.
        let d = tick(&latch, on(TRACK, 7, false), false);
        assert_eq!(
            d.buffer_state, BUFFER_STATE_OK,
            "a paused track sitting on its own start offset has ARRIVED"
        );
        assert_eq!(
            d.playing_state, PLAYING_STATE_PAUSED,
            "and the controller must be told PAUSED, so its play button is a play button"
        );
        assert_eq!(d.position_ms, 7_000, "with the position it is actually at");
    }

    /// FROM HARDWARE (the seek spinner). The latch cannot fix a clock reported
    /// relative to the current source — it is downstream of the clock — so this
    /// pins the honest reading: keep reporting the load. The fix lives in the
    /// writer, and `a_seeked_source_reports_its_position_within_the_track` in
    /// qbz-player is what holds it there.
    #[test]
    fn a_source_relative_clock_after_a_seek_still_reads_as_a_load() {
        const TRACK: u64 = 9;
        let latch = BufferingLatch::default();
        latch.begin(TRACK, 166, 372);
        for since_seek in [0_u64, 1, 2, 60] {
            assert_eq!(
                tick(&latch, on(TRACK, since_seek, true), false).buffer_state,
                BUFFER_STATE_BUFFERING,
                "a clock at {since_seek}s is BELOW the 166 s this stream opened at; the latch \
                 can only believe the player"
            );
        }
    }

    /// A gapless hand-off has no latch around it — the prefetch runs inside the
    /// player — so `awaiting_next` is the only thing that puts a spinner up
    /// between tracks. The report must still name the outgoing track.
    #[test]
    fn awaiting_the_next_track_buffers_without_a_latch() {
        let latch = BufferingLatch::default();
        let d = tick(&latch, on(77, 258, true), true);
        assert_eq!(d.buffer_state, BUFFER_STATE_BUFFERING);
        assert_eq!(d.playing_state, PLAYING_STATE_PLAYING);
        assert_eq!(d.track_id, 77, "the player is still on the outgoing track");
    }

    /// Issue #2: the device is busy and the load gives up. The player's
    /// `fail_play` leaves it on the track it failed to play, not playing, at
    /// the offset it was asked for — and from that alone the latch must let go
    /// and the controller must see THAT track PAUSED: not "playing" over
    /// silence, not a spinner until the 90 s backstop, and not the previous
    /// track (its cursor would jump back).
    #[test]
    fn a_load_the_player_gave_up_on_reports_that_track_paused() {
        const LEAVING: u64 = 111;
        const FAILED: u64 = 222;
        for start_secs in [0, 139] {
            let latch = BufferingLatch::default();
            latch.begin(FAILED, start_secs, 300);
            // While the device retries, the player still names what it left.
            let d = tick(&latch, on(LEAVING, 40, true), false);
            assert_eq!(d.buffer_state, BUFFER_STATE_BUFFERING);

            // `fail_play`'s end state.
            let d = tick(&latch, on(FAILED, start_secs, false), false);
            assert_eq!(d.playing_state, PLAYING_STATE_PAUSED, "from {start_secs}s");
            assert_eq!(d.buffer_state, BUFFER_STATE_OK, "from {start_secs}s");
            assert_eq!(d.track_id, FAILED, "from {start_secs}s");
            assert_eq!(d.position_ms, start_secs * 1000);
            assert!(
                latch.in_flight(FAILED, start_secs * 1000).is_none(),
                "the latch is still holding a load that has ended"
            );
        }
    }

    /// Nothing loaded is STOPPED, not PAUSED: PAUSED invites the controller to
    /// offer a resume for audio that was never there.
    #[test]
    fn nothing_loaded_reports_stopped() {
        let latch = BufferingLatch::default();
        let d = tick(&latch, on(0, 0, false), false);
        assert_eq!(d.playing_state, PLAYING_STATE_STOPPED);
        assert_eq!(d.buffer_state, BUFFER_STATE_OK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_audio_quality_matches_the_web_player_tiers() {
        assert_eq!(classify_audio_quality(0, 0), AUDIO_QUALITY_UNKNOWN);
        assert_eq!(classify_audio_quality(44_100, 16), AUDIO_QUALITY_CD);
        assert_eq!(classify_audio_quality(48_000, 16), AUDIO_QUALITY_CD);
        assert_eq!(classify_audio_quality(44_100, 24), AUDIO_QUALITY_HIRES_L1);
        assert_eq!(classify_audio_quality(96_000, 24), AUDIO_QUALITY_HIRES_L1);
        assert_eq!(classify_audio_quality(176_400, 24), AUDIO_QUALITY_HIRES_L2);
        assert_eq!(classify_audio_quality(192_000, 24), AUDIO_QUALITY_HIRES_L2);
        assert_eq!(classify_audio_quality(384_000, 24), AUDIO_QUALITY_HIRES_L2);
        assert_eq!(classify_audio_quality(22_050, 16), AUDIO_QUALITY_MP3);
    }

    #[test]
    fn snapshot_is_none_until_format_known() {
        assert!(build_file_audio_quality_snapshot(0, 0, 2).is_none());
        let snap = build_file_audio_quality_snapshot(96_000, 24, 2).expect("known format");
        assert_eq!(snap.sampling_rate, 96_000);
        assert_eq!(snap.bit_depth, 24);
        assert_eq!(snap.nb_channels, 2);
        assert_eq!(snap.audio_quality, AUDIO_QUALITY_HIRES_L1);
    }
}

#[cfg(test)]
mod awaiting_next_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_clock_is_at_the_end_only_while_playing_a_track_of_known_length() {
        assert!(clock_is_at_the_end(true, 258, 258));
        assert!(
            clock_is_at_the_end(true, 259, 258),
            "past the end counts too"
        );
        assert!(!clock_is_at_the_end(true, 257, 258));
        // Paused at the end is not a load, it is a paused track.
        assert!(!clock_is_at_the_end(false, 258, 258));
        // A stream with no duration yet would otherwise read as permanently
        // finished at position 0.
        assert!(!clock_is_at_the_end(true, 0, 0));
    }

    #[test]
    fn a_gapless_handoff_passes_through_without_arming() {
        let mut since = None;
        // A whole grace period has not elapsed on the first observation, so the
        // fraction of a second a hand-off spends here reports nothing.
        assert!(!awaiting_next_track(
            true,
            &mut since,
            Duration::from_secs(3600)
        ));
        assert!(since.is_some(), "the wait is now being timed");
        // The next track starts: forget it happened.
        assert!(!awaiting_next_track(
            false,
            &mut since,
            Duration::from_secs(3600)
        ));
        assert!(since.is_none());
    }

    #[test]
    fn a_wait_that_outlasts_the_grace_arms_and_stays_armed() {
        let mut since = None;
        assert!(awaiting_next_track(true, &mut since, Duration::ZERO));
        let first = since;
        assert!(awaiting_next_track(true, &mut since, Duration::ZERO));
        assert_eq!(
            since, first,
            "the timer keeps its original start across ticks"
        );
    }
}
