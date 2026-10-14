//! Engine seam for QConnect renderer-side playback.
//!
//! The hard-won renderer orchestration (echo-seek rejection, cursor align,
//! queue materialize, shuffle deferral, load dedup window) lives ABOVE this
//! trait in `qconnect-app` and is written ONLY against these methods plus
//! `QconnectRemoteSyncState`. It must never be re-derived per host.
//!
//! Implemented by `pibuz`'s `DaemonRendererEngine` (one-line forwards to
//! `QbzCore` + `Player`), and by a mock in this crate's tests.
//!
//! Errors are `String`. The two protected bit-perfect audio seams
//! (`play_data` / `play_streaming_dynamic`) are reached ONLY through
//! [`QconnectRendererEngine::start_track_stream`]; they are never modified, only
//! called, and the probe-derived sample_rate/channels/bit_depth must pass
//! straight through (defaulting them silently resamples hi-res).

use async_trait::async_trait;
use qbz_models::{Quality, QueueTrack, RepeatMode, Track};
use qbz_player::PlaybackState;

/// The renderer-side engine surface. An implementor forwards each method to its
/// `QbzCore` / `Player` in one line; the async/sync split follows `QbzCore`
/// (queue/catalog are async; raw transport is sync). Keep this MINIMAL — exactly
/// these methods.
// clippy 1.99 `double_must_use` fires on `#[async_trait]`'s output, not on
// this code; see `qbz_models::FrontendAdapter`.
#[allow(
    unknown_lints,
    clippy::double_must_use,
    reason = "fires on #[async_trait]'s generated code, not on this trait"
)]
#[async_trait]
pub trait QconnectRendererEngine: Send + Sync {
    // ---- transport (sync on the engine) ----
    fn resume(&self) -> Result<(), String>;
    fn pause(&self) -> Result<(), String>;
    fn stop(&self) -> Result<(), String>;
    /// Seek in WHOLE SECONDS (matches `QbzCore::seek`).
    fn seek(&self, position_secs: u64) -> Result<(), String>;
    /// Volume as a 0.0–1.0 fraction (caller normalizes via `normalize_volume_to_fraction`).
    fn set_volume(&self, fraction: f32) -> Result<(), String>;
    /// Fresh, synchronous snapshot of the audio thread (track_id/position/...).
    /// MUST NOT be cached/stale — the echo filter and the #387 seek-diff gate
    /// depend on its freshness.
    fn get_playback_state(&self) -> PlaybackState;
    /// Whether the audio thread currently holds decodable audio for the loaded
    /// track. Distinct from `get_playback_state().track_id`: `stop()` clears the
    /// audio buffer + flips this false but LEAVES `current_track_id` untouched,
    /// so after handing the render to a peer (which stopped local playback) the
    /// track id can still match the target while NO audio is buffered. The
    /// takeback force-stream reads this to know a reload is required even when the
    /// track id matches (`should_reload_remote_track` alone would skip it and the
    /// following resume would fail with "no audio data available").
    fn has_loaded_audio(&self) -> bool;

    // ---- queue / mode (async) ----
    async fn set_repeat_mode(&self, mode: RepeatMode);
    async fn set_shuffle(&self, enabled: bool);
    /// Set the shuffle FLAG only, WITHOUT generating any local order. QConnect is
    /// WS-authoritative for shuffle: the renderer must never invent a random
    /// order; the cloud's authoritative order is applied separately via
    /// `set_queue_with_order` (`sync_remote_shuffle_projection` /
    /// `materialize_remote_queue`). Forwards to
    /// `QbzCore::set_shuffle_with_order(enabled, None)` -> identity order on
    /// enable (current order preserved, NO RNG), clear on disable.
    async fn set_shuffle_flag(&self, enabled: bool);
    async fn get_all_queue_tracks(&self) -> (Vec<QueueTrack>, Option<usize>);
    async fn set_queue(&self, tracks: Vec<QueueTrack>, start_index: Option<usize>);
    /// The deferred-shuffle core: a real shuffle order, never an invented
    /// identity order (an implementor must not fake it via `set_queue`).
    async fn set_queue_with_order(
        &self,
        tracks: Vec<QueueTrack>,
        start_index: Option<usize>,
        shuffle_enabled: bool,
        shuffle_order: Option<Vec<usize>>,
    );
    async fn clear_queue(&self, keep_current: bool);
    async fn play_index(&self, index: usize) -> Option<QueueTrack>;

    // ---- catalog (async) ----
    async fn get_track(&self, track_id: u64) -> Result<Track, String>;
    async fn get_tracks_batch(&self, track_ids: &[u64]) -> Result<Vec<Track>, String>;

    // ---- protected audio seam, behind one high-level method ----
    /// Resolve the stream URL at `quality`, probe the FLAC format, start a
    /// progressive stream into the player and SPAWN the HTTP feeder; on a
    /// streaming error, fall back to a full download + `play_data`. The impl owns
    /// `reqwest`, the `BufferWriter`, and the detached spawn — none of which
    /// crosses this crate boundary. `start_position_secs` is where the stream
    /// starts: 0 for a fresh track, the handed-off position on a takeback.
    ///
    /// This is the ONLY method that touches the protected bit-perfect seams
    /// (`play_data` / `play_streaming_dynamic`). It MUST pass the probed
    /// sample_rate/channels/bit_depth straight through — never default them, or
    /// hi-res remote playback silently resamples.
    async fn start_track_stream(
        &self,
        track_id: u64,
        quality: Quality,
        duration_secs: u64,
        start_position_secs: u64,
    ) -> Result<(), String>;

    /// A `start_track_stream` for `track_id` failed with `error`. Called by the
    /// orchestration on every failed load, so the host can tell the session
    /// with a `PlaybackError` — what the official renderer does, and the only
    /// way the controller learns WHICH track failed and why. Default: nothing.
    fn on_load_failed(&self, _track_id: u64, _error: &str) {}

    /// This renderer's OWN quality ceiling, or `None` when it is uncapped.
    ///
    /// The controller's `max_audio_quality` is a request; this is the local
    /// policy that bounds it (see [`crate::session::cap_quality`]). The default
    /// is uncapped; pibuz returns the configured `playback.quality`, read fresh
    /// so a `settings set` applies to the next load without a reconnect.
    fn local_max_quality(&self) -> Option<Quality> {
        None
    }

    // ---- report-back source (the single "engine read") ----
    /// The ACTUAL DAC output format `(sample_rate, bit_depth)` under bit-perfect
    /// passthrough — read from `player().state`. Drives the file/device
    /// audio-quality reports. Read-only; never device init.
    fn current_output_format(&self) -> Option<(u32, u32)>;
}
