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
//! The daemon's own report loop (`pibuz::qconnect::report`) is where
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
//! h.let_the_load_windows_expire();
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
    Seek {
        position_secs: u64,
    },
    SetVolume {
        pct: i32,
    },
    StartStream {
        track_id: u64,
        start_secs: u64,
        /// The quality the stream was opened at. Recorded, not discarded: the
        /// controller announces a ceiling and the renderer has to honour it,
        /// and a fake that drops the argument cannot tell honouring it from
        /// defaulting to the top — which on the bit-perfect path is the
        /// difference between passthrough and a silent resample.
        quality: Quality,
    },
    PlayIndex {
        index: usize,
    },
    SetQueue {
        len: usize,
        start: Option<usize>,
    },
    SetQueueWithOrder {
        len: usize,
        shuffle: bool,
    },
    ClearQueue {
        keep_current: bool,
    },
    SetRepeatMode {
        mode: RepeatMode,
    },
    /// The flag only. QConnect is WS-authoritative for order.
    SetShuffleFlag {
        enabled: bool,
    },
    /// The ORDER-GENERATING call. Distinct from `SetShuffleFlag` on purpose:
    /// folding the two together makes the rule that the renderer must never
    /// invent a local random order impossible to state, let alone assert.
    SetShuffleWithLocalOrder {
        enabled: bool,
    },
}

#[derive(Debug, Default)]
struct FakePlayer {
    playing: bool,
    track_id: u64,
    position_ms: u64,
    duration_ms: u64,
    loaded_audio: bool,
    volume: f32,
    /// The queue as the player really holds it, ids and all.
    ///
    /// Storing the real tracks rather than regenerating placeholders is not a
    /// detail: `align_queue_cursor` looks the target track UP in this list, and
    /// a fake that answers with the wrong ids sends it down the "not in queue"
    /// fallback on every call — which replaces the whole queue with a
    /// single-track one and hides every cursor bug behind harness noise.
    queue: Vec<QueueTrack>,
    queue_index: Option<usize>,
    /// While set, the audio thread has not caught up with the last
    /// `start_track_stream`: the stream is open but nothing is audible yet, so
    /// the player still reports the OUTGOING track. Applied by `catch_up`.
    pending_track: Option<(u64, u64, u64)>,
    /// Whether `start_track_stream` lands instantly or has to be caught up.
    audio_thread_lags: bool,
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
        // The player pulls the next track from its OWN queue, so the cursor
        // moves with the audio. Only the cloud is left behind — which is the
        // whole point of the manoeuvre, and would be lost if the fake let the
        // local cursor go stale too.
        if let Some(index) = player.queue.iter().position(|track| track.id == track_id) {
            player.queue_index = Some(index);
        }
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
        // A hand-off goes through `stop()`, which drops the buffer and with it
        // the clock. Keeping the old position here would be comfortable and
        // wrong: the takeback then finds the engine already sitting at the
        // position the cloud is about to ask for, and every guard that exists
        // for the gap between "stream opened at 77 s" and "player is at 0"
        // becomes unreachable.
        player.position_ms = 0;
    }

    /// Move the playback clock forward, as the audio thread would.
    pub fn advance(&self, ms: u64) {
        let mut player = self.player.lock().expect("fake player");
        if player.playing {
            player.position_ms = player.position_ms.saturating_add(ms);
        }
    }

    /// From here on, opening a stream does not make it audible.
    ///
    /// This is how the real player behaves and the fake does not: the audio
    /// thread adopts a track only once it has samples, so between
    /// `start_track_stream` and the first sample the player STILL REPORTS THE
    /// OUTGOING TRACK at the outgoing clock. Several guards exist only for that
    /// window, and with an instantly-adopting fake they are unreachable.
    pub fn make_the_audio_thread_lag(&self) {
        self.player.lock().expect("fake player").audio_thread_lags = true;
    }

    /// Whether a stream has been opened that the audio thread has not adopted.
    pub fn is_catching_up(&self) -> bool {
        self.player
            .lock()
            .expect("fake player")
            .pending_track
            .is_some()
    }

    /// The first samples of the pending stream arrive.
    pub fn catch_up(&self) {
        let mut player = self.player.lock().expect("fake player");
        if let Some((track_id, position_ms, duration_ms)) = player.pending_track.take() {
            player.track_id = track_id;
            player.position_ms = position_ms;
            player.duration_ms = duration_ms;
            player.loaded_audio = true;
            player.playing = true;
        }
    }

    /// What the player holds: how many tracks, and which one the cursor names.
    pub fn queue(&self) -> (Vec<u64>, Option<u64>) {
        let player = self.player.lock().expect("fake player");
        let ids: Vec<u64> = player.queue.iter().map(|track| track.id).collect();
        let current = player.queue_index.and_then(|index| ids.get(index).copied());
        (ids, current)
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

    async fn set_repeat_mode(&self, mode: RepeatMode) {
        self.record(EngineCall::SetRepeatMode { mode });
    }

    async fn set_shuffle(&self, enabled: bool) {
        self.record(EngineCall::SetShuffleWithLocalOrder { enabled });
    }

    async fn set_shuffle_flag(&self, enabled: bool) {
        self.record(EngineCall::SetShuffleFlag { enabled });
    }

    async fn get_all_queue_tracks(&self) -> (Vec<QueueTrack>, Option<usize>) {
        let player = self.player.lock().expect("fake player");
        (player.queue.clone(), player.queue_index)
    }

    async fn set_queue(&self, tracks: Vec<QueueTrack>, start_index: Option<usize>) {
        self.record(EngineCall::SetQueue {
            len: tracks.len(),
            start: start_index,
        });
        let mut player = self.player.lock().expect("fake player");
        player.queue = tracks;
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
        player.queue = tracks;
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
        quality: Quality,
        duration_secs: u64,
        start_position_secs: u64,
    ) -> Result<(), String> {
        self.record(EngineCall::StartStream {
            track_id,
            start_secs: start_position_secs,
            quality,
        });
        let mut player = self.player.lock().expect("fake player");
        let duration_ms = if duration_secs > 0 {
            duration_secs * 1000
        } else {
            self.track_duration_secs * 1000
        };
        if player.audio_thread_lags {
            // The stream is open; nothing is audible yet. The player goes on
            // reporting the outgoing track until `catch_up`.
            player.pending_track = Some((track_id, start_position_secs * 1000, duration_ms));
            return Ok(());
        }
        player.track_id = track_id;
        player.position_ms = start_position_secs * 1000;
        player.duration_ms = duration_ms;
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

/// The renderer-critical arms of `pibuz::qconnect::sink::DaemonEventSink`.
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
                view.total_ms = as_stated_ms(duration);
            }
            if let Some(position) = payload.get("current_position") {
                view.elapsed_ms = as_stated_ms(position);
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

/// A stated millisecond field, or `None` when the renderer stated nothing.
///
/// The distinction that matters is `null` versus a number, NOT zero versus
/// non-zero: `current_position: 0` is a real answer — the top of a track — while
/// `null` is a refusal to answer, and only the refusal blanks the display. An
/// earlier version of this folded them together and reported every track change
/// as a blanked progress bar.
fn as_stated_ms(value: &Value) -> Option<u64> {
    match value.as_i64() {
        Some(ms) if ms >= 0 => Some(ms as u64),
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
    /// Track ids in the player's queue after this step.
    pub player_queue: Vec<u64>,
    /// The track the player's CURSOR names — what `pibuz status` and the moOde
    /// overlay would call the now-playing title.
    pub cursor_track: Option<u64>,
    /// The track that is actually AUDIBLE. When this and `cursor_track` disagree
    /// outside a load, the title on screen belongs to a different song than the
    /// one coming out of the speakers.
    pub audible_track: u64,
    /// Track ids in the queue the CLOUD holds.
    pub cloud_queue: Vec<u64>,
    /// A stream is open that the audio thread has not adopted yet.
    pub audio_catching_up: bool,
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
    /// The queue push on its own: `QUEUE_TRACKS_LOADED` naming the selection,
    /// with no SetState behind it. Returns the queue items the cloud minted, so
    /// a caller can name them in a later frame.
    ///
    /// Queue item ids are minted fresh on every call, exactly as the cloud does
    /// — so pushing the same track ids twice is a re-announcement of the same
    /// QUEUE, not of the same items.
    pub async fn push_queue(&self, track_ids: &[u64], selected: usize) -> Vec<QueueItem> {
        let items = self.queue_items(track_ids);
        self.announce_queue(&items, selected).await;
        items
    }

    /// Announce a queue the cloud has ALREADY minted, item ids and all.
    ///
    /// This is how a re-announcement really looks — a reconnect, an
    /// AskForQueueState, an edit elsewhere in the queue. Handing the same items
    /// back is the whole point: minting fresh ids would make it a different
    /// queue, and the renderer would be right to rebuild.
    pub async fn announce_queue(&self, items: &[QueueItem], selected: usize) {
        let payload = json!({
            "tracks": items.iter().map(queue_item_json).collect::<Vec<_>>(),
            "queue_position": selected,
        });
        let ids: Vec<u64> = items.iter().map(|item| item.track_id).collect();
        self.harness
            .step(
                &format!("push queue {ids:?} select {selected}"),
                move |_| {
                    vec![queue_event(
                        "MESSAGE_TYPE_SRVR_CTRL_QUEUE_TRACKS_LOADED",
                        payload.clone(),
                    )]
                },
            )
            .await;
    }

    /// Tap play on a queue the controller just picked: the cloud pushes
    /// `QUEUE_TRACKS_LOADED` naming the selection, then a `SET_STATE` naming the
    /// track. This is the ordinary "user tapped a track in an album" flow.
    pub async fn push_queue_and_play(&self, track_ids: &[u64], selected: usize) {
        let items = self.push_queue(track_ids, selected).await;
        self.tap_track(&items, selected).await;
    }

    /// The `SET_STATE` that follows a push: play THIS item of THAT queue.
    pub async fn tap_track(&self, items: &[QueueItem], selected: usize) {
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

    /// Tap next, in the shape that carries NO position.
    ///
    /// The controller does send this — the track is stated, the position is
    /// not. A new track starts at zero; filling the blank in from the cloud's
    /// cached position starts it wherever the PREVIOUS one had got to.
    pub async fn tap_next_track_without_a_position(
        &self,
        track_id: u64,
        next_track_id: Option<u64>,
    ) {
        let current = self.mint_item(track_id);
        let next = next_track_id.map(|id| self.mint_item(id));
        self.harness
            .step(&format!("tap next (no position) -> {track_id}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({
                        "playing_state": 2,
                        "current_track": queue_item_json(&current),
                        "next_track": next.as_ref().map(queue_item_json),
                    }),
                )]
            })
            .await;
    }

    /// The cloud states where the session is: this track, at this position,
    /// playing. The authoritative frame that follows a takeback.
    pub async fn resume_at(&self, track_id: u64, position_ms: u64) {
        let item = self.mint_item(track_id);
        self.harness
            .step(&format!("resume {track_id} at {position_ms}ms"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({
                        "playing_state": 2,
                        "current_position": position_ms,
                        "current_track": queue_item_json(&item),
                    }),
                )]
            })
            .await;
    }

    /// The cloud re-emits a SetState for the track already playing.
    ///
    /// It does this routinely when only secondary fields change — a next_track
    /// correction, a queue_item_id refresh — and it re-states `current_position`
    /// as 0 while doing so. Treating each one as a fresh load is the "first
    /// track hiccups on album change" and "needs several taps" report.
    pub async fn reemits_the_current_state(&self, track_id: u64, queue_item_id: u64) {
        let item = QueueItem {
            track_context_uuid: "ctx-harness".to_string(),
            track_id,
            queue_item_id,
        };
        self.harness
            .step(&format!("cloud re-emits state for {track_id}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetState,
                    json!({
                        "playing_state": 2,
                        "current_position": 0,
                        "current_track": queue_item_json(&item),
                    }),
                )]
            })
            .await;
    }

    /// Press the volume-up key: a RELATIVE step, with no absolute level.
    pub async fn nudge_volume(&self, delta: i32) {
        self.harness
            .step(&format!("volume {delta:+}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetVolume,
                    json!({ "volume_delta": delta }),
                )]
            })
            .await;
    }

    /// Announce the ceiling for stream quality. The controller sends this on
    /// join and whenever the user changes the streaming-quality preference.
    pub async fn set_max_audio_quality(&self, level: i32) {
        self.harness
            .step(&format!("max audio quality {level}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetMaxAudioQuality,
                    json!({ "max_audio_quality": level }),
                )]
            })
            .await;
    }

    /// Set the repeat mode from the controller. 1 = off, 2 = one, 3 = all.
    pub async fn set_loop_mode(&self, loop_mode: i32) {
        self.harness
            .step(&format!("loop mode {loop_mode}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetLoopMode,
                    json!({ "loop_mode": loop_mode }),
                )]
            })
            .await;
    }

    /// Tap shuffle. The cloud owns the resulting ORDER and sends it separately;
    /// this frame carries the flag alone.
    pub async fn set_shuffle(&self, shuffle_mode: bool) {
        self.harness
            .step(&format!("shuffle {shuffle_mode}"), |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrSetShuffleMode,
                    json!({ "shuffle_mode": shuffle_mode }),
                )]
            })
            .await;
    }

    /// Tap the mute button.
    pub async fn set_muted(&self, muted: bool) {
        self.harness
            .step(if muted { "mute" } else { "unmute" }, |_| {
                vec![renderer_command(
                    RendererCommandType::SrvrRndrMuteVolume,
                    json!({ "value": muted }),
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
                    // The cloud does not wait to be told what it just ordered.
                    // A SetState naming a track moves its own view of the
                    // session there immediately, and the phone redraws on that
                    // — the renderer's report only confirms it. Without this the
                    // harness would credit the renderer for knowledge the cloud
                    // already had, and read a perfectly ordinary track change as
                    // the screen losing track of the song.
                    if let Some(item) = command.payload.get("current_track") {
                        if let Some(queue_item_id) =
                            item.get("queue_item_id").and_then(Value::as_u64)
                        {
                            self.cloud.view.lock().expect("view").track_id = Some(queue_item_id);
                        }
                    }
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

        let (player_queue, cursor_track) = self.engine.queue();
        let audible_track = self.engine.snapshot().track_id;
        let cloud_queue = self
            .app
            .queue_state_snapshot()
            .await
            .queue_items
            .iter()
            .map(|item| item.track_id)
            .collect();

        self.timeline.lock().expect("timeline").push(Step {
            label: label.to_string(),
            view: self.cloud.view(),
            reports,
            engine_calls,
            echo_engine_calls,
            player_queue,
            cursor_track,
            audible_track,
            cloud_queue,
            audio_catching_up: self.engine.is_catching_up(),
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

    /// Let time pass for the two windows that hang off the last load.
    ///
    /// # Read this before writing a test
    ///
    /// `last_load_attempt` is one `std::time::Instant`, and TWO rules read it:
    /// the 5 s load-dedup window and the 1.5 s handoff-echo window. A test runs
    /// in microseconds, so unless it says otherwise it is permanently inside
    /// BOTH — every pause is swallowed as a peer echo and every load is deduped
    /// away. That is not a neutral default: it silently walks the happy path and
    /// makes whole guards untestable, which is how a mutation that deletes one
    /// of them can pass a suite that looks thorough.
    ///
    /// So: unless a case is specifically ABOUT one of those windows, call this
    /// first. The alternative is sleeping through 1.5 s of wall clock per case,
    /// or teaching the renderer to take its clock as a parameter — which is the
    /// real fix, and the reason this helper carries a warning instead of being
    /// quietly convenient.
    pub fn let_the_load_windows_expire(&self) {
        let sync = Arc::clone(&self.inner.sync_state);
        // The lock is never contended between steps, so try_lock is enough and
        // keeps this helper synchronous at the call site.
        let mut state = sync.try_lock().expect("sync state is idle between steps");
        if let Some((track_id, _)) = state.last_load_attempt {
            state.last_load_attempt = Some((track_id, Instant::now() - Duration::from_secs(10)));
        }
    }

    /// From here on, a stream that opens is not instantly audible. See
    /// [`FakeEngine::make_the_audio_thread_lag`].
    pub fn make_the_audio_thread_lag(&self) {
        self.inner.engine.make_the_audio_thread_lag();
    }

    /// The first samples of the pending stream arrive.
    pub fn audio_thread_catches_up(&self) {
        self.inner.engine.catch_up();
    }

    /// The queue the PLAYER holds, and the track its cursor names.
    pub fn player_queue(&self) -> (Vec<u64>, Option<u64>) {
        self.inner.engine.queue()
    }

    /// The player is holding exactly the queue the cloud is.
    ///
    /// Not a blanket invariant, because one legitimate divergence exists:
    /// `align_queue_cursor` falls back to a single-track queue when the cloud
    /// names a track the player does not hold. So this is asserted where a test
    /// means it, rather than everywhere.
    pub fn assert_player_queue_matches_the_cloud(&self) {
        let timeline = self.timeline();
        let Some(step) = timeline.last() else {
            panic!("no steps recorded");
        };
        assert_eq!(
            step.player_queue,
            step.cloud_queue,
            "the player holds {:?} while the cloud holds {:?}; timeline:\n{}",
            step.player_queue,
            step.cloud_queue,
            render_timeline(&timeline),
        );
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
            // 5. The queue cursor names the track that is actually audible.
            //    When it does not, `pibuz status` and the moOde overlay show one
            //    song's title over another song's audio — the title said
            //    "Golden Seams" while the 213 s duration belonged to "Pulse".
            //
            //    Exempt while a load is in flight: the cursor is legitimately
            //    AHEAD there, because the stream for the new track has opened
            //    and the audio thread has not adopted it yet. That exemption has
            //    to outlast the step that opened the stream — the gap is the
            //    whole point of it, and a load can stay unadopted across several
            //    frames from the cloud.
            let load_in_flight = step.audio_catching_up
                || step
                    .engine_calls
                    .iter()
                    .any(|call| matches!(call, EngineCall::StartStream { .. }));
            if !load_in_flight
                && step.audible_track != 0
                && step.cursor_track.is_some()
                && step.cursor_track != Some(step.audible_track)
            {
                violations.push(format!(
                    "'{}': the queue cursor names {:?} while {} is audible",
                    step.label, step.cursor_track, step.audible_track
                ));
            }
            // 6. No report storm. One gesture is one or two reports; anything
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
                "  {:<34} {:?} track={:?} {:?}/{:?} vol={:?}  reports={:?} cursor={:?} audible={} engine={:?}{}",
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
                step.cursor_track,
                step.audible_track,
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
// cannot be mounted here (`pibuz::qconnect::report` is monomorphic on
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

/// `buffer_state` wire values (`pibuz::qconnect::transport`).
pub const BUFFER_STATE_OK: i32 = 0;
pub const BUFFER_STATE_BUFFERING: i32 = 1;

impl ControllerHarness {
    /// Run one tick of the renderer's own report loop.
    ///
    /// Mirrors `pibuz::qconnect::report::report_playback_state`: publish the
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
