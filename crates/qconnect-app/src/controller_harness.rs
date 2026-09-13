//! Controller-sync harness: a scripted phone in a test.
//!
//! # What this is for
//!
//! Every QConnect bug that reached hardware had the same shape. Someone tapped
//! play, pause or next on the Qobuz app, and what the app DREW afterwards was
//! wrong — a spinner that never stopped, a progress bar that blanked to 0:00, a
//! title one track behind the audio. None of it was visible from inside this
//! crate, because nothing here models the thing that got it wrong: the
//! controller's screen.
//!
//! So this harness models it. Three pieces stand between a scripted gesture and
//! an assertion:
//!
//! - [`VirtualController`] — the phone. Gestures (`tap_pause`, `drag_seek`,
//!   `push_queue`) expand into the exact inbound frames the cloud sends for
//!   them. It never speaks to the renderer directly.
//! - [`FakeCloud`] — the qws frontend. It relays the controller's frames, reads
//!   every renderer report that comes back, folds them into the view it would
//!   push to the phone, and — this is the part that catches feedback loops —
//!   ECHOES each state report back at the renderer as a `SET_STATE`, exactly as
//!   the real cloud does.
//! - [`ControllerView`] — the screen. `Buffering` is a spinner, `total_ms: None`
//!   is a blanked progress bar. Assertions are written against this, not against
//!   protocol fields, because this is what the user complained about.
//!
//! Under it all runs the REAL [`QconnectApp`], the REAL renderer orchestration
//! in [`crate::renderer`], and a [`FakeEngine`] that models the player well
//! enough to answer the questions the orchestration asks it (what track, what
//! position, is anything loaded).
//!
//! # What it does not reach
//!
//! The daemon's own report loop (`qbzd::qconnect::report`) is where
//! `buffer_state` is decided and where the periodic position reports come from.
//! It is monomorphic on `NativeWsTransport` + `AppRuntime`, so it cannot be
//! mounted here. Everything below sees only the ECHO reports the app itself
//! emits in response to a command. Spinner lifetimes are therefore out of reach
//! until the daemon glue takes its transport and engine as type parameters.
//!
//! # Writing a test
//!
//! ```ignore
//! let h = ControllerHarness::new().await;
//! h.become_active_renderer().await;
//! h.controller().push_queue_and_play(&[101, 102, 103], 0).await;
//! h.advance_playback(30_000);
//! h.leave_handoff_echo_window();
//! h.controller().tap_pause().await;
//! h.assert_invariants();
//! ```

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qbz_models::{Quality, QueueTrack, RepeatMode, Track};
use qbz_player::PlaybackState;
use qconnect_core::{QConnectQueueState, QueueItem, QueueVersion};
use qconnect_protocol::{
    InboundEnvelope, OutboundEnvelope, RendererCommandType, RendererServerCommand,
};
use qconnect_transport_ws::{InMemoryWsTransport, TransportEvent, WsTransportConfig};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::renderer_engine::QconnectRendererEngine;
use crate::{
    cache_renderer_snapshot, QConnectRendererState, QconnectApp, QconnectAppEvent,
    QconnectEventSink, QconnectRemoteSyncState, QconnectRendererInfo,
};

// ===================================================================== engine

/// One call the orchestration made on the player. Recorded in order so a test
/// can assert what the renderer actually DID, not only what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCall {
    Resume,
    Pause,
    Stop,
    Seek { position_secs: u64 },
    SetVolume { pct: i32 },
    StartStream { track_id: u64, start_secs: u64 },
    PlayIndex { index: usize },
    SetQueue { len: usize, start: Option<usize> },
    SetQueueWithOrder { len: usize, shuffle: bool },
    ClearQueue { keep_current: bool },
    SetRepeatMode,
    SetShuffleFlag { enabled: bool },
}

#[derive(Debug, Default)]
struct FakePlayer {
    playing: bool,
    track_id: u64,
    position_ms: u64,
    duration_ms: u64,
    loaded_audio: bool,
    volume: f32,
    queue_len: usize,
    queue_index: Option<usize>,
}

/// A player that models just enough to answer the orchestration's questions.
///
/// It is not a recorder with canned answers: `start_track_stream` really adopts
/// the track and marks audio loaded, `pause`/`resume` really move the flag.
/// That matters — half the renderer logic branches on `has_loaded_audio()` and
/// `get_playback_state().track_id`, so a mock that always answers the same
/// thing walks a single path through code whose bugs live in the others.
pub struct FakeEngine {
    player: StdMutex<FakePlayer>,
    calls: StdMutex<Vec<EngineCall>>,
    /// Seconds reported for every synthetic catalog track.
    track_duration_secs: u64,
}

impl FakeEngine {
    pub fn new() -> Self {
        Self {
            player: StdMutex::new(FakePlayer {
                volume: 0.5,
                ..FakePlayer::default()
            }),
            calls: StdMutex::new(Vec::new()),
            track_duration_secs: 240,
        }
    }

    pub fn calls(&self) -> Vec<EngineCall> {
        self.calls.lock().expect("engine calls").clone()
    }

    fn record(&self, call: EngineCall) {
        self.calls.lock().expect("engine calls").push(call);
    }

    pub fn snapshot(&self) -> PlaybackState {
        let player = self.player.lock().expect("fake player");
        PlaybackState {
            is_playing: player.playing,
            position: player.position_ms / 1000,
            duration: player.duration_ms / 1000,
            track_id: player.track_id,
            volume: player.volume,
        }
    }

    /// Adopt the next track from inside the player, the way a gapless hand-off
    /// does: no command, no load, no report — the audio simply moves on and the
    /// cloud does not know yet.
    pub fn advance_gaplessly_to(&self, track_id: u64) {
        let mut player = self.player.lock().expect("fake player");
        player.track_id = track_id;
        player.position_ms = 0;
        player.duration_ms = self.track_duration_secs * 1000;
        player.loaded_audio = true;
        player.playing = true;
    }

    /// Give up local playback on a hand-off to a peer.
    ///
    /// Models what `stop()` really leaves behind: the audio buffer is dropped
    /// and `has_loaded_audio` goes false, but `current_track_id` is UNTOUCHED.
    /// That asymmetry is load-bearing — on the takeback the track id still
    /// matches the target while no audio exists, so anything deciding on the id
    /// alone skips the reload and the following resume dies with "no audio data
    /// available".
    pub fn tear_down_for_handoff(&self) {
        let mut player = self.player.lock().expect("fake player");
        player.playing = false;
        player.loaded_audio = false;
    }

    /// Move the playback clock forward, as the audio thread would.
    pub fn advance(&self, ms: u64) {
        let mut player = self.player.lock().expect("fake player");
        if player.playing {
            player.position_ms = player.position_ms.saturating_add(ms);
        }
    }

    fn mock_track(&self, id: u64) -> Track {
        serde_json::from_value(json!({
            "id": id,
            "title": format!("track {id}"),
            "duration": self.track_duration_secs,
        }))
        .expect("synthetic track")
    }
}

impl Default for FakeEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl QconnectRendererEngine for FakeEngine {
    fn resume(&self) -> Result<(), String> {
        self.record(EngineCall::Resume);
        self.player.lock().expect("fake player").playing = true;
        Ok(())
    }

    fn pause(&self) -> Result<(), String> {
        self.record(EngineCall::Pause);
        self.player.lock().expect("fake player").playing = false;
        Ok(())
    }

    fn stop(&self) -> Result<(), String> {
        self.record(EngineCall::Stop);
        let mut player = self.player.lock().expect("fake player");
        player.playing = false;
        player.loaded_audio = false;
        player.position_ms = 0;
        Ok(())
    }

    fn seek(&self, position_secs: u64) -> Result<(), String> {
        self.record(EngineCall::Seek { position_secs });
        self.player.lock().expect("fake player").position_ms = position_secs * 1000;
        Ok(())
    }

    fn set_volume(&self, fraction: f32) -> Result<(), String> {
        self.record(EngineCall::SetVolume {
            pct: (fraction * 100.0).round() as i32,
        });
        self.player.lock().expect("fake player").volume = fraction;
        Ok(())
    }

    fn get_playback_state(&self) -> PlaybackState {
        self.snapshot()
    }

    fn has_loaded_audio(&self) -> bool {
        self.player.lock().expect("fake player").loaded_audio
    }

    async fn set_repeat_mode(&self, _mode: RepeatMode) {
        self.record(EngineCall::SetRepeatMode);
    }

    async fn set_shuffle(&self, enabled: bool) {
        self.record(EngineCall::SetShuffleFlag { enabled });
    }

    async fn set_shuffle_flag(&self, enabled: bool) {
        self.record(EngineCall::SetShuffleFlag { enabled });
    }

    async fn get_all_queue_tracks(&self) -> (Vec<QueueTrack>, Option<usize>) {
        let (len, index) = {
            let player = self.player.lock().expect("fake player");
            (player.queue_len, player.queue_index)
        };
        let tracks = (0..len)
            .map(|i| crate::renderer::model_track_to_core_queue_track(&self.mock_track(i as u64)))
            .collect();
        (tracks, index)
    }

    async fn set_queue(&self, tracks: Vec<QueueTrack>, start_index: Option<usize>) {
        self.record(EngineCall::SetQueue {
            len: tracks.len(),
            start: start_index,
        });
        let mut player = self.player.lock().expect("fake player");
        player.queue_len = tracks.len();
        player.queue_index = start_index;
    }

    async fn set_queue_with_order(
        &self,
        tracks: Vec<QueueTrack>,
        start_index: Option<usize>,
        shuffle_enabled: bool,
        _shuffle_order: Option<Vec<usize>>,
    ) {
        self.record(EngineCall::SetQueueWithOrder {
            len: tracks.len(),
            shuffle: shuffle_enabled,
        });
        let mut player = self.player.lock().expect("fake player");
        player.queue_len = tracks.len();
        player.queue_index = start_index;
    }

    async fn clear_queue(&self, keep_current: bool) {
        self.record(EngineCall::ClearQueue { keep_current });
    }

    async fn play_index(&self, index: usize) -> Option<QueueTrack> {
        self.record(EngineCall::PlayIndex { index });
        self.player.lock().expect("fake player").queue_index = Some(index);
        None
    }

    async fn get_track(&self, track_id: u64) -> Result<Track, String> {
        Ok(self.mock_track(track_id))
    }

    async fn get_tracks_batch(&self, track_ids: &[u64]) -> Result<Vec<Track>, String> {
        Ok(track_ids.iter().map(|&id| self.mock_track(id)).collect())
    }

    async fn start_track_stream(
        &self,
        track_id: u64,
        _quality: Quality,
        duration_secs: u64,
        start_position_secs: u64,
    ) -> Result<(), String> {
        self.record(EngineCall::StartStream {
            track_id,
            start_secs: start_position_secs,
        });
        let mut player = self.player.lock().expect("fake player");
        player.track_id = track_id;
        player.position_ms = start_position_secs * 1000;
        player.duration_ms = if duration_secs > 0 {
            duration_secs * 1000
        } else {
            self.track_duration_secs * 1000
        };
        player.loaded_audio = true;
        player.playing = true;
        Ok(())
    }

    fn current_output_format(&self) -> Option<(u32, u32)> {
        Some((44_100, 16))
    }
}

// ======================================================================= sink

type HarnessApp = QconnectApp<InMemoryWsTransport, HarnessSink>;

/// The renderer-critical arms of `qbzd::qconnect::sink::DaemonEventSink`.
///
/// Only four of the daemon sink's arms touch the renderer, and all four are thin
/// forwards into [`crate::renderer`] — which is why they can be mirrored here
/// without re-deriving anything. The daemon's own extras (the `/api/status`
/// latch, the join-volume assertion, the peer-active edge detector) are status
/// and policy, not renderer behaviour, and are deliberately absent.
///
/// The divergence risk is real and is the argument for the next step: making
/// `DaemonEventSink` generic over its engine and transport so this harness can
/// mount the daemon's own sink instead of a copy of its shape.
pub struct HarnessSink {
    engine: Arc<FakeEngine>,
    sync_state: Arc<Mutex<QconnectRemoteSyncState>>,
    events: Arc<StdMutex<Vec<QconnectAppEvent>>>,
}

#[async_trait]
impl QconnectEventSink for HarnessSink {
    async fn on_event(&self, event: QconnectAppEvent) {
        self.events.lock().expect("sink events").push(event.clone());
        match &event {
            QconnectAppEvent::RendererUpdated(renderer_state) => {
                let mut sync_state = self.sync_state.lock().await;
                cache_renderer_snapshot(&mut sync_state, renderer_state);
            }
            QconnectAppEvent::QueueUpdated(queue_state) => {
                {
                    let mut sync_state = self.sync_state.lock().await;
                    sync_state.last_remote_queue_state = Some(queue_state.clone());
                }
                let _ = crate::renderer::materialize_remote_queue(
                    self.engine.as_ref(),
                    &self.sync_state,
                    queue_state,
                )
                .await;
            }
            QconnectAppEvent::RendererCommandApplied { command, state } => {
                let _ = crate::renderer::apply_renderer_command(
                    self.engine.as_ref(),
                    &self.sync_state,
                    command,
                    state,
                )
                .await;
            }
            _ => {}
        }
    }
}

// ================================================================== the screen

/// What the controller's transport control is drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Nothing has been reported yet.
    Unknown,
    Stopped,
    Playing,
    Paused,
    /// The spinner.
    Buffering,
}

/// The controller's screen.
///
/// Fields are `Option` because the protocol can genuinely blank them, and
/// blanking is the bug: `total_ms: None` is a progress bar reading `0:00`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerView {
    pub transport: Transport,
    pub track_id: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub volume: Option<i32>,
    pub muted: Option<bool>,
}

impl Default for ControllerView {
    fn default() -> Self {
        Self {
            transport: Transport::Unknown,
            track_id: None,
            elapsed_ms: None,
            total_ms: None,
            volume: None,
            muted: None,
        }
    }
}

/// Fold one renderer report into the screen, the way the Qobuz app does.
///
/// The two asymmetries below are not guesses — they are the observed behaviour
/// the production comments in `app.rs` and `report.rs` were written against, and
/// encoding them here is the whole point of the model:
///
/// - an explicit `duration: null` BLANKS the progress display (this is the
///   `0:00` flicker on every play/pause tap),
/// - an explicit `buffer_state: null` leaves the spinner alone (the renderer
///   omits it on purpose so the report loop stays authoritative).
fn fold_report_into_view(view: &mut ControllerView, message_type: &str, payload: &Value) {
    match message_type {
        "MESSAGE_TYPE_RNDR_SRVR_STATE_UPDATED" => {
            if let Some(playing_state) = payload.get("playing_state").and_then(Value::as_i64) {
                view.transport = match playing_state {
                    1 => Transport::Stopped,
                    2 => Transport::Playing,
                    3 => Transport::Paused,
                    _ => Transport::Unknown,
                };
            }
            // Buffering overrides the transport glyph with a spinner.
            if let Some(buffer_state) = payload.get("buffer_state").and_then(Value::as_i64) {
                if buffer_state == 1 {
                    view.transport = Transport::Buffering;
                }
            }
            if let Some(duration) = payload.get("duration") {
                view.total_ms = as_positive_ms(duration);
            }
            if let Some(position) = payload.get("current_position") {
                view.elapsed_ms = as_positive_ms(position);
            }
            if let Some(track_id) = payload.get("current_queue_item_id").and_then(Value::as_u64) {
                view.track_id = Some(track_id);
            }
        }
        "MESSAGE_TYPE_RNDR_SRVR_VOLUME_CHANGED" => {
            if let Some(volume) = payload.get("volume").and_then(Value::as_i64) {
                view.volume = Some(volume as i32);
            }
        }
        "MESSAGE_TYPE_RNDR_SRVR_VOLUME_MUTED" => {
            if let Some(value) = payload.get("value").and_then(Value::as_bool) {
                view.muted = Some(value);
            }
        }
        _ => {}
    }
}

fn as_positive_ms(value: &Value) -> Option<u64> {
    match value.as_i64() {
        Some(ms) if ms > 0 => Some(ms as u64),
        _ => None,
    }
}

/// One recorded step: a gesture, and everything that followed from it.
#[derive(Debug, Clone)]
pub struct Step {
    pub label: String,
    pub view: ControllerView,
    /// Reports the renderer sent during this step, as `(message_type, payload)`.
    pub reports: Vec<(String, Value)>,
    /// Engine calls the renderer made during this step.
    pub engine_calls: Vec<EngineCall>,
    /// The subset of those calls the renderer made while handling the CLOUD's
    /// ECHO of its own report — which must always be empty. Acting on an echo is
    /// how a pause becomes two pauses and a resume fights the user.
    pub echo_engine_calls: Vec<EngineCall>,
}

// =================================================================== the cloud

/// The qws frontend: relays the controller's frames, folds the renderer's
/// reports into the screen, and echoes state reports back at the renderer.
pub struct FakeCloud {
    /// How many outbound envelopes have already been folded in.
    drained: StdMutex<usize>,
    view: StdMutex<ControllerView>,
    /// Reproduce the cloud's SET_STATE echo of every state report. On by
    /// default: without it the harness cannot see a feedback loop at all.
    echo_state_reports: StdMutex<bool>,
}

impl FakeCloud {
    fn take_new_outbound(&self, all: &[OutboundEnvelope]) -> Vec<OutboundEnvelope> {
        let mut drained = self.drained.lock().expect("drain cursor");
        let fresh = all[(*drained).min(all.len())..].to_vec();
        *drained = all.len();
        fresh
    }

    pub fn view(&self) -> ControllerView {
        self.view.lock().expect("view").clone()
    }
}

// ============================================================== the controller

/// The phone. Gestures expand into the frames the cloud sends for them.
pub struct VirtualController {
    harness: Arc<HarnessInner>,
    next_queue_item_id: StdMutex<u64>,
}

impl VirtualController {
    /// Tap play on a queue the controller just picked: the cloud pushes
    /// `QUEUE_TRACKS_LOADED` naming the selection, then a `SET_STATE` naming the
    /// track. This is the ordinary "user tapped a track in an album" flow.
    pub async fn push_queue_and_play(&self, track_ids: &[u64], selected: usize) {
        let items = self.queue_items(track_ids);
        self.harness
            .step(
                &format!("push queue {track_ids:?} select {selected}"),
                |_| {
                    vec![queue_event(
                        "MESSAGE_TYPE_SRVR_CTRL_QUEUE_TRACKS_LOADED",
                        json!({
                            "tracks": items.iter().map(queue_item_json).collect::<Vec<_>>(),
                            "queue_position": selected,
                        }),
                    )]
                },
            )
            .await;

        let current = items[selected].clone();
        let next = items.get(selected + 1).cloned();
        self.harness
            .step(&format!("tap track {}", current.track_id), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({
                        "playing_state": 2,
                        "current_position": 0,
                        "current_track": queue_item_json(&current),
                        "next_track": next.as_ref().map(queue_item_json),
                    }),
                )]
            })
            .await;
    }

    /// Tap pause. The real controller sends state ONLY — no position, no track.
    /// That shape is why pause has been the source of so many of these bugs.
    pub async fn tap_pause(&self) {
        self.harness
            .step("tap pause", |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({ "playing_state": 3 }),
                )]
            })
            .await;
    }

    /// Tap play on an already-loaded track. State only, same as pause.
    pub async fn tap_play(&self) {
        self.harness
            .step("tap play", |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({ "playing_state": 2 }),
                )]
            })
            .await;
    }

    /// Drag the progress bar. The controller sends a position and NOTHING else
    /// (issue #387) — no playing_state, no track.
    pub async fn drag_seek(&self, position_ms: u64) {
        self.harness
            .step(&format!("seek to {position_ms}ms"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({ "current_position": position_ms }),
                )]
            })
            .await;
    }

    /// Move the volume slider.
    pub async fn set_volume(&self, pct: i32) {
        self.harness
            .step(&format!("volume {pct}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetVolume,
                    json!({ "volume": pct }),
                )]
            })
            .await;
    }

    /// Tap next: the cloud names the new track and its successor, at position 0.
    pub async fn tap_next_track(&self, track_id: u64, next_track_id: Option<u64>) {
        let current = self.mint_item(track_id);
        let next = next_track_id.map(|id| self.mint_item(id));
        self.harness
            .step(&format!("tap next -> {track_id}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({
                        "playing_state": 2,
                        "current_position": 0,
                        "current_track": queue_item_json(&current),
                        "next_track": next.as_ref().map(queue_item_json),
                    }),
                )]
            })
            .await;
    }

    /// Hand the render role to this device.
    pub async fn take_renderer(&self) {
        self.harness
            .step("set active", |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetActive,
                    json!({ "active": true }),
                )]
            })
            .await;
    }

    /// Mint one queue item with the id the cloud would have assigned it.
    fn mint_item(&self, track_id: u64) -> QueueItem {
        self.queue_items(&[track_id]).remove(0)
    }

    fn queue_items(&self, track_ids: &[u64]) -> Vec<QueueItem> {
        let mut next_id = self.next_queue_item_id.lock().expect("queue item ids");
        track_ids
            .iter()
            .map(|&track_id| {
                let queue_item_id = *next_id;
                *next_id += 1;
                QueueItem {
                    track_context_uuid: "ctx-harness".to_string(),
                    track_id,
                    queue_item_id,
                }
            })
            .collect()
    }
}

/// One frame the cloud pushes at the renderer.
enum CloudFrame {
    Renderer(RendererServerCommand),
    Queue(InboundEnvelope),
}

fn renderer_command(command_type: RendererCommandType, payload: Value) -> CloudFrame {
    CloudFrame::Renderer(RendererServerCommand {
        command_type,
        payload,
    })
}

fn queue_event(message_type: &str, payload: Value) -> CloudFrame {
    CloudFrame::Queue(InboundEnvelope {
        service: "QConnect".to_string(),
        source: "Backend".to_string(),
        message_type: message_type.to_string(),
        action_uuid: None,
        queue_version: Some(QueueVersion::new(1, 1)),
        payload,
    })
}

fn queue_item_json(item: &QueueItem) -> Value {
    json!({
        "track_context_uuid": item.track_context_uuid,
        "track_id": item.track_id,
        "queue_item_id": item.queue_item_id,
    })
}

// ===================================================================== harness

struct HarnessInner {
    app: Arc<HarnessApp>,
    /// Held so the in-memory transport always has a subscriber; never read.
    _transport_rx: tokio::sync::broadcast::Receiver<TransportEvent>,
    transport: Arc<InMemoryWsTransport>,
    engine: Arc<FakeEngine>,
    sync_state: Arc<Mutex<QconnectRemoteSyncState>>,
    cloud: Arc<FakeCloud>,
    timeline: StdMutex<Vec<Step>>,
    engine_calls_drained: StdMutex<usize>,
}

impl HarnessInner {
    /// Run one gesture to quiescence and record what the controller would see.
    ///
    /// Everything the renderer does in response happens inside this await: the
    /// sink dispatches into the engine synchronously, and the app's own report
    /// goes out before `handle_transport_event` returns. So a step is settled
    /// when it returns — no sleeps, no polling.
    async fn step<F>(&self, label: &str, frames: F)
    where
        F: FnOnce(&FakeCloud) -> Vec<CloudFrame>,
    {
        for frame in frames(&self.cloud) {
            let event = match frame {
                CloudFrame::Renderer(command) => {
                    TransportEvent::InboundRendererServerCommand(command)
                }
                CloudFrame::Queue(envelope) => TransportEvent::InboundReceived(envelope),
            };
            let _ = self.app.handle_transport_event(event).await;
        }
        self.settle(label).await;
    }

    /// Fold everything the renderer emitted into the cloud's view, echo it back
    /// the way the real cloud does, and record the step.
    async fn settle(&self, label: &str) {
        let mut reports = Vec::new();
        let mut echo_engine_calls = Vec::new();
        // The echo can provoke another report, which the cloud would echo in
        // turn. Keep going until it stops — a flow that never stops IS the
        // feedback-loop bug, so cap it and let the invariant name it.
        const MAX_ECHO_ROUNDS: usize = 8;
        for _ in 0..MAX_ECHO_ROUNDS {
            let all = self.transport.sent_messages().await;
            let fresh = self.cloud.take_new_outbound(&all);
            if fresh.is_empty() {
                break;
            }
            let mut echoes = 0usize;
            {
                let mut view = self.cloud.view.lock().expect("view");
                for envelope in &fresh {
                    fold_report_into_view(&mut view, &envelope.message_type, &envelope.payload);
                    reports.push((envelope.message_type.clone(), envelope.payload.clone()));
                    if envelope.message_type == "MESSAGE_TYPE_RNDR_SRVR_STATE_UPDATED"
                        && *self.cloud.echo_state_reports.lock().expect("echo flag")
                    {
                        echoes += 1;
                    }
                }
            }
            for _ in 0..echoes {
                // The cloud's echo of a state report: a SET_STATE carrying only
                // next_track. Acting on this is the feedback loop the `is_echo`
                // guard exists to break — so everything the engine does across
                // this call is attributed to the echo and checked separately.
                let before = self.engine.calls().len();
                let _ = self
                    .app
                    .handle_transport_event(TransportEvent::InboundRendererServerCommand(
                        RendererServerCommand {
                            command_type: RendererCommandType::SrvrRndrSetState,
                            payload: json!({ "next_track": null }),
                        },
                    ))
                    .await;
                echo_engine_calls.extend(self.engine.calls().into_iter().skip(before));
            }
        }

        let engine_calls = {
            let all = self.engine.calls();
            let mut drained = self.engine_calls_drained.lock().expect("engine cursor");
            let fresh = all[(*drained).min(all.len())..].to_vec();
            *drained = all.len();
            fresh
        };

        self.timeline.lock().expect("timeline").push(Step {
            label: label.to_string(),
            view: self.cloud.view(),
            reports,
            engine_calls,
            echo_engine_calls,
        });
    }
}

/// The harness. One per test.
pub struct ControllerHarness {
    inner: Arc<HarnessInner>,
    controller: VirtualController,
}

impl ControllerHarness {
    pub async fn new() -> Self {
        let transport = Arc::new(InMemoryWsTransport::new());
        let engine = Arc::new(FakeEngine::new());
        let sync_state = Arc::new(Mutex::new(QconnectRemoteSyncState::default()));
        let sink = Arc::new(HarnessSink {
            engine: Arc::clone(&engine),
            sync_state: Arc::clone(&sync_state),
            events: Arc::new(StdMutex::new(Vec::new())),
        });
        let app = Arc::new(QconnectApp::new(
            Arc::clone(&transport),
            sink,
            Arc::clone(&sync_state),
        ));
        // The in-memory transport broadcasts on connect and fails when nobody is
        // listening, so subscribe BEFORE connecting and keep the receiver for the
        // life of the harness. Nothing reads it: the harness drives the app by
        // calling `handle_transport_event` directly, which is what the daemon's
        // session loop does with the events arriving on this same channel.
        let transport_rx = app.subscribe_transport_events();
        app.connect(WsTransportConfig {
            endpoint_url: "wss://harness.invalid/ws".to_string(),
            subscribe_channels: vec![vec![1]],
            ..Default::default()
        })
        .await
        .expect("harness connect");

        let cloud = Arc::new(FakeCloud {
            drained: StdMutex::new(0),
            view: StdMutex::new(ControllerView::default()),
            echo_state_reports: StdMutex::new(true),
        });

        let inner = Arc::new(HarnessInner {
            app,
            _transport_rx: transport_rx,
            transport,
            engine,
            sync_state,
            cloud,
            timeline: StdMutex::new(Vec::new()),
            engine_calls_drained: StdMutex::new(0),
        });

        // The connect itself puts nothing on the wire worth folding, but drain
        // the cursor so step 1 reports only its own traffic.
        inner.settle("connect").await;
        inner.timeline.lock().expect("timeline").clear();

        let controller = VirtualController {
            harness: Arc::clone(&inner),
            next_queue_item_id: StdMutex::new(1000),
        };
        Self { inner, controller }
    }

    pub fn controller(&self) -> &VirtualController {
        &self.controller
    }

    pub fn engine(&self) -> &FakeEngine {
        &self.inner.engine
    }

    pub fn view(&self) -> ControllerView {
        self.inner.cloud.view()
    }

    pub fn timeline(&self) -> Vec<Step> {
        self.inner.timeline.lock().expect("timeline").clone()
    }

    /// The timeline as a human reads it. Pass it to every assertion message:
    /// the question is always "what did the screen do", never "what did the
    /// assert say".
    pub fn rendered_timeline(&self) -> String {
        render_timeline(&self.timeline())
    }

    /// Every engine call so far, across all steps.
    pub fn engine_calls(&self) -> Vec<EngineCall> {
        self.inner.engine.calls()
    }

    /// Seed the session topology so this device IS the session's active
    /// renderer — the state every gesture below assumes.
    ///
    /// Seeded directly rather than driven through `SESSION_STATE`, because the
    /// session-management arm lives in the daemon's sink, not here.
    pub async fn become_active_renderer(&self) {
        let mut state = self.inner.sync_state.lock().await;
        state.session.session_uuid = Some("sess-harness".to_string());
        state.session.local_renderer_id = Some(1);
        state.session.active_renderer_id = Some(1);
        state.session.renderers = vec![QconnectRendererInfo {
            renderer_id: 1,
            device_uuid: Some("local-harness".to_string()),
            friendly_name: None,
            brand: None,
            model: None,
            device_type: None,
            volume_remote_control: None,
        }];
        state.local_render_active = Some(true);
    }

    /// A peer (the phone, a desktop app) takes the render away from us.
    ///
    /// The session's active renderer moves to the peer and — as the daemon's own
    /// sink does on that transition — local playback is torn down. The audio
    /// buffer is gone but the player still remembers the track id, which is
    /// exactly the state the takeback path has to cope with.
    pub async fn peer_takes_the_render(&self) {
        self.inner.engine.tear_down_for_handoff();
        let mut state = self.inner.sync_state.lock().await;
        state.session.active_renderer_id = Some(2);
        state.session.renderers.push(QconnectRendererInfo {
            renderer_id: 2,
            device_uuid: Some("peer-phone".to_string()),
            friendly_name: None,
            brand: None,
            model: None,
            device_type: None,
            volume_remote_control: None,
        });
        state.local_render_active = Some(false);
    }

    /// Advance the playback clock, as the audio thread would.
    pub fn advance_playback(&self, ms: u64) {
        self.inner.engine.advance(ms);
    }

    /// The player finishes a track and starts the next one on its own.
    ///
    /// The cloud learns nothing from this — a gapless hand-off runs inside the
    /// player, with no command and no report around it — so afterwards the
    /// cloud's `current_track` is a track BEHIND what is audible. That gap is
    /// where the state-only commands get dangerous.
    pub fn advance_gaplessly_to(&self, track_id: u64) {
        self.inner.engine.advance_gaplessly_to(track_id);
    }

    /// Step outside the 1.5 s handoff-echo window by backdating the load stamp.
    ///
    /// The window is keyed on `std::time::Instant::elapsed()`, so the only
    /// alternatives are sleeping through it for real or leaving a whole class of
    /// gesture untestable. Any test where the user "taps pause a while after the
    /// track started" must call this first, or the renderer will read the pause
    /// as the previous renderer's handoff echo and ignore it.
    pub fn leave_handoff_echo_window(&self) {
        let sync = Arc::clone(&self.inner.sync_state);
        // The lock is never contended between steps, so try_lock is enough and
        // keeps this helper synchronous at the call site.
        let mut state = sync.try_lock().expect("sync state is idle between steps");
        if let Some((track_id, _)) = state.last_load_attempt {
            state.last_load_attempt = Some((track_id, Instant::now() - Duration::from_secs(10)));
        }
    }

    pub async fn queue_snapshot(&self) -> QConnectQueueState {
        self.inner.app.queue_state_snapshot().await
    }

    pub async fn renderer_state(&self) -> QConnectRendererState {
        self.inner.app.state_handle().lock().await.renderer.clone()
    }

    // ------------------------------------------------------------ invariants

    /// Assert the screen never did anything a user would file a bug about.
    ///
    /// Each rule below is a bug that actually shipped. Adding a rule here makes
    /// every existing test enforce it, which is the point of asserting against
    /// the screen rather than against a protocol field.
    pub fn assert_invariants(&self) {
        let timeline = self.timeline();
        let mut violations = Vec::new();
        let mut previous: Option<&Step> = None;

        for step in &timeline {
            if let Some(prev) = previous {
                // 1. The progress bar never blanks once a length is known. This
                //    is the `duration: null` flicker to 0:00 on every tap.
                if prev.view.total_ms.is_some() && step.view.total_ms.is_none() {
                    violations.push(format!(
                        "'{}': track length blanked ({:?} -> None) — the controller draws 0:00",
                        step.label, prev.view.total_ms
                    ));
                }
                // 2. The elapsed time never blanks either.
                if prev.view.elapsed_ms.is_some() && step.view.elapsed_ms.is_none() {
                    violations.push(format!(
                        "'{}': elapsed time blanked ({:?} -> None)",
                        step.label, prev.view.elapsed_ms
                    ));
                }
                // 3. A settled screen never falls back to Unknown: the phone has
                //    no glyph for it and keeps whatever it last drew.
                if prev.view.transport != Transport::Unknown
                    && step.view.transport == Transport::Unknown
                {
                    violations.push(format!(
                        "'{}': transport fell back to Unknown from {:?}",
                        step.label, prev.view.transport
                    ));
                }
            }
            // 4. The cloud's echo of our own report never reaches the player.
            //    An echo is not an instruction, and obeying it is how one tap
            //    became two — pause, pause; resume fighting the user's pause.
            if !step.echo_engine_calls.is_empty() {
                violations.push(format!(
                    "'{}': the cloud's echo of our own report reached the player — {:?}",
                    step.label, step.echo_engine_calls
                ));
            }
            // 5. No report storm. One gesture is one or two reports; anything
            //    more is the renderer answering the cloud's echo of its own
            //    report, which is the feedback loop that used to lock the app up.
            if step.reports.len() > 2 {
                violations.push(format!(
                    "'{}': {} reports for one gesture — {:?}",
                    step.label,
                    step.reports.len(),
                    step.reports
                        .iter()
                        .map(|(message_type, _)| message_type.as_str())
                        .collect::<Vec<_>>()
                ));
            }
            previous = Some(step);
        }

        if !violations.is_empty() {
            panic!(
                "controller-sync invariants violated:\n  {}\n\ntimeline:\n{}",
                violations.join("\n  "),
                render_timeline(&timeline),
            );
        }
    }
}

/// The timeline as a human reads it. Printed on every failure, because the
/// question is always "what did the screen do", not "what did the assert say".
pub fn render_timeline(timeline: &[Step]) -> String {
    timeline
        .iter()
        .map(|step| {
            format!(
                "  {:<34} {:?} track={:?} {:?}/{:?} vol={:?}  reports={:?} engine={:?}{}",
                step.label,
                step.view.transport,
                step.view.track_id,
                step.view.elapsed_ms,
                step.view.total_ms,
                step.view.volume,
                step.reports
                    .iter()
                    .map(|(message_type, _)| message_type
                        .trim_start_matches("MESSAGE_TYPE_RNDR_SRVR_"))
                    .collect::<Vec<_>>(),
                step.engine_calls,
                if step.echo_engine_calls.is_empty() {
                    String::new()
                } else {
                    format!("  ECHO-DRIVEN={:?}", step.echo_engine_calls)
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================== the report tick
//
// The daemon's report loop is the OTHER source of renderer reports, and the one
// that decides `buffer_state` and carries the real position and duration. It
// cannot be mounted here (`qbzd::qconnect::report` is monomorphic on
// `NativeWsTransport` + `AppRuntime`), so this mirrors the payload it builds.
//
// A mirror is weaker than the real thing and this is the seam most worth
// closing: everything the daemon loop decides — the buffering latch, the
// end-of-track grace, the transition follow-up — is invisible from here. What
// the mirror DOES buy is the state the echo path reads back: `update_renderer_*`
// is how a duration ever reaches `send_renderer_reports`, and without a tick the
// duration-blanking invariant can never fire.

/// What the renderer is telling the cloud on this tick.
#[derive(Debug, Clone, Copy)]
pub struct ReportTick {
    pub playing_state: i32,
    pub buffer_state: i32,
    pub position_ms: u64,
    pub duration_ms: u64,
}

/// `buffer_state` wire values (`qbzd::qconnect::transport`).
pub const BUFFER_STATE_OK: i32 = 0;
pub const BUFFER_STATE_BUFFERING: i32 = 1;

impl ControllerHarness {
    /// Run one tick of the renderer's own report loop.
    ///
    /// Mirrors `qbzd::qconnect::report::report_playback_state`: publish the
    /// position and duration into the app's renderer state (so a later echo can
    /// state them), then send a `RndrSrvrStateUpdated` carrying the same triple
    /// the daemon sends.
    pub async fn report_tick(&self) {
        let playback = self.inner.engine.snapshot();
        let playing_state = if playback.track_id == 0 {
            1 // STOPPED
        } else if playback.is_playing {
            2 // PLAYING
        } else {
            3 // PAUSED
        };
        self.report_tick_as(ReportTick {
            playing_state,
            buffer_state: BUFFER_STATE_OK,
            position_ms: playback.position * 1000,
            duration_ms: playback.duration * 1000,
        })
        .await;
    }

    /// A report tick with the triple stated outright — for the states the fake
    /// player has no way to reach, above all BUFFERING.
    pub async fn report_tick_as(&self, tick: ReportTick) {
        let queue_version = self.inner.app.queue_state_snapshot().await.version;
        let queue_item_id = {
            let state = self.inner.sync_state.lock().await;
            state.last_renderer_queue_item_id
        };

        self.inner
            .app
            .update_renderer_position(tick.position_ms)
            .await;
        if tick.duration_ms > 0 {
            self.inner
                .app
                .update_renderer_duration(tick.duration_ms)
                .await;
        }

        let report = crate::RendererReport::new(
            crate::RendererReportType::RndrSrvrStateUpdated,
            format!("tick-{}", tick.position_ms),
            queue_version,
            json!({
                "playing_state": tick.playing_state,
                "buffer_state": tick.buffer_state,
                "current_position": tick.position_ms,
                "duration": tick.duration_ms,
                "current_queue_item_id": queue_item_id,
                "next_queue_item_id": Option::<u64>::None,
                "queue_version": {
                    "major": queue_version.major,
                    "minor": queue_version.minor,
                },
            }),
        );
        let _ = self.inner.app.send_renderer_report_command(report).await;
        self.inner
            .settle(&format!("report tick @{}ms", tick.position_ms))
            .await;
    }
}
