//! Qobuz Connect renderer engine for the pibuz daemon.
//!
//! Implements [`qconnect_app::QconnectRendererEngine`] over the daemon
//! `AppRuntime`'s `QbzCore` + `Player`, so pibuz becomes a QConnect renderer
//! that inherits the shared echo/cursor/materialize/shuffle orchestration in
//! `qconnect_app::renderer` instead of re-deriving it.
//!
//! The protected bit-perfect seams (`play_streaming_dynamic` / `play_data`) and
//! the HTTP feeder live here, impl-side, exactly as the Tauri `CoreBridge` impl
//! does; the probe-derived sample_rate/channels/bit_depth flow STRAIGHT into
//! `play_streaming_dynamic` (never defaulted, or hi-res remote playback silently
//! resamples). The feeder body is a near-verbatim port of the Tauri
//! `track_loading.rs` feeder, with `bridge.player()` -> `self.core().player()`;
//! the only deviation is the TLS backend — the crates workspace `reqwest` ships
//! `rustls-tls` (not `native-tls`), so the `.use_native_tls()` calls are dropped.
//! TLS is transport encryption only; the decoded audio bytes are identical, so
//! bit-perfect is unaffected. (If the Qobuz streaming CDN ever presents a cert
//! rustls rejects, add `native-tls` to pibuz's reqwest features.)
#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use qbz_app::shell::AppRuntime;
use qbz_core::QbzCore;
use qbz_models::{Quality, QueueTrack, RepeatMode, Track};
use qbz_player::PlaybackState;
use qconnect_app::QconnectRendererEngine;

use crate::adapter::DaemonAdapter;

// T10 (OD4, §7.4): daemon-only volume policy. The desktop has no equivalent —
// it always applies remote volume. The mode is read from the daemon-root
// `qconnect_settings.db` `volume_mode` KV key (transport::load_volume_mode_at)
// at connect time and injected into the engine + session host.
/// How the daemon treats a controller's remote volume command (01 §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VolumeMode {
    /// OD4 DEFAULT. Remote `SetVolume` is applied to the player via the core,
    /// and the player's real volume is reported back to the controller.
    #[default]
    Software,
    /// Bit-perfect purist. The player stays at 100 % (no software attenuation);
    /// remote `SetVolume` is acknowledged-but-ignored (logged at info) and 100
    /// is reported. For DACs feeding power amps where software gain is unwanted.
    Locked,
}

impl VolumeMode {
    /// Parse the `volume_mode` KV value. Anything but the literal `"locked"`
    /// (unset, empty, unknown) falls back to `Software` — the OD4 default.
    pub fn from_kv(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("locked") => VolumeMode::Locked,
            _ => VolumeMode::Software,
        }
    }

    /// Whether a controller's remote `SetVolume` should reach the player. True
    /// only in `Software`; `Locked` acknowledges-but-ignores.
    pub fn applies_remote_volume(self) -> bool {
        matches!(self, VolumeMode::Software)
    }

    /// The volume (0-100 percent) to REPORT to the controller given the player's
    /// real 0.0-1.0 fraction. `Software` reports the real (rounded) percent;
    /// `Locked` always reports 100 regardless of the player's actual level.
    pub fn reported_volume_pct(self, real_fraction: f32) -> i32 {
        match self {
            VolumeMode::Software => (real_fraction.clamp(0.0, 1.0) * 100.0).round() as i32,
            VolumeMode::Locked => 100,
        }
    }
}

/// QConnect renderer engine backed by the daemon `AppRuntime`. Holds the shared
/// runtime and forwards every trait method through `runtime.core()`; the async
/// feeder spawns on the ambient tokio runtime (`start_track_stream` is always
/// awaited from a runtime task).
pub struct DaemonRendererEngine {
    runtime: Arc<AppRuntime<DaemonAdapter>>,
    /// T10 (OD4): resolved volume policy for this session (from the KV at connect).
    volume_mode: VolumeMode,
    /// the current track's progressive-download feeder. A track
    /// change MUST abort the previous feeder — left alone it downloads the
    /// full file at line speed to the very end, and a few quick skips stack
    /// concurrent hi-res downloads that starve the new track's startup buffer
    /// (5-8 s starts observed on a Pi).
    current_feeder: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// the track whose buffer is still filling, so renderer
    /// reports can say BUFFERING (see `BufferingLatch`).
    buffering: Arc<BufferingLatch>,
    /// Daemon status + event bus, so a starting stream can announce itself to
    /// the host BEFORE it takes the audio device (see `start_track_stream`).
    shared: Arc<std::sync::Mutex<crate::state::DaemonShared>>,
    /// Pulsed when buffering starts so the report scheduler tells the
    /// controller within milliseconds instead of at its next 2 s tick.
    report_notify: Arc<tokio::sync::Notify>,
}

/// Which track is filling its buffer, shared between the renderer engine (the
/// writer) and the report scheduler (the reader). A stream is "buffering" from
/// the moment its feeder opens until the player actually starts producing
/// audio — on a deep resume that is several seconds of downloading plus a
/// sample pre-skip, during which the controller deserves a loading state.
#[derive(Default)]
pub struct BufferingLatch(std::sync::Mutex<Option<Buffering>>);

struct Buffering {
    track_id: u64,
    /// Where the stream was opened. The playback clock sits here until audio
    /// flows, so a position past it is the audible edge.
    start_position_secs: u64,
    /// The loading track's duration, so a report sent BEFORE the player has
    /// switched to it can still describe it.
    duration_secs: u64,
    since: std::time::Instant,
}

/// A load that never becomes audible must not report BUFFERING forever.
const BUFFERING_MAX: std::time::Duration = std::time::Duration::from_secs(90);

impl BufferingLatch {
    /// Mark `track_id` as buffering from `start_position_secs` (replacing any
    /// previous track).
    pub fn begin(&self, track_id: u64, start_position_secs: u64, duration_secs: u64) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = Some(Buffering {
                track_id,
                start_position_secs,
                duration_secs,
                since: std::time::Instant::now(),
            });
        }
    }

    /// Clear the latch for `track_id`. Ignores a stale clear for a track that
    /// has already been superseded.
    pub fn finish(&self, track_id: u64) {
        if let Ok(mut guard) = self.0.lock() {
            if guard.as_ref().map(|b| b.track_id) == Some(track_id) {
                *guard = None;
            }
        }
    }

    /// The load in flight, as `(track_id, start_position_secs, duration_secs)`,
    /// or `None` once audio is flowing. Self-clearing on the audible edge.
    ///
    /// Deliberately NOT keyed on the player's track id: the player keeps
    /// reporting the OUTGOING track until the new stream produces audio, so a
    /// check keyed on it saw "not buffering" for the whole load and only turned
    /// true once audio had already started — the controller got no loading
    /// state during the wait and a stray spinner just after playback began.
    ///
    /// Nor on `is_playing`: during a next-track load it is simply still `true`
    /// from the OUTGOING track, so it says nothing about the new stream.
    ///
    /// The audible edge is the player arriving on the loading track AND its
    /// clock moving past where the stream opened — the clock only advances once
    /// audio actually flows.
    ///
    /// `position_ms` is MILLISECONDS on purpose. With whole seconds, a track
    /// loading at 0 needed the clock to reach a full 1 s before it counted as
    /// audible, so the controller kept a spinner up for a second of music it
    /// was already playing.
    pub fn in_flight(&self, player_track_id: u64, position_ms: u64) -> Option<(u64, u64, u64)> {
        self.in_flight_with_state(player_track_id, position_ms, true)
    }

    /// As [`Self::in_flight`], but told whether the player is actually running.
    ///
    /// `is_playing == false` on the LOADING track is the second way a load
    /// ends: it arrived, and the controller had asked for a pause. See
    /// `a_track_that_loads_straight_into_a_pause_is_not_buffering`.
    pub fn in_flight_with_state(
        &self,
        player_track_id: u64,
        position_ms: u64,
        is_playing: bool,
    ) -> Option<(u64, u64, u64)> {
        let Ok(mut guard) = self.0.lock() else {
            return None;
        };
        let b = guard.as_ref()?;
        let on_track = player_track_id == b.track_id;
        let start_ms = b.start_position_secs.saturating_mul(1000);
        // Audio flowed: the clock climbed past where the stream opened.
        let audible = on_track && position_ms > start_ms;
        // Or it arrived into a pause. A stopped clock can never climb past its
        // own offset, so without this a paused load reports BUFFERING until the
        // 90 s backstop — a spinner, and a suppressed position, on a session
        // that is doing exactly what it was told. `on_track` is what keeps this
        // off a cold load: until the new stream produces audio the player still
        // reports the OUTGOING track, so it never looks like arrival.
        let arrived_paused = on_track && !is_playing && position_ms >= start_ms;
        if audible || arrived_paused || b.since.elapsed() > BUFFERING_MAX {
            *guard = None;
            return None;
        }
        // TRIPWIRE. The latch releases on the player's clock passing the offset
        // the stream opened at, so a clock that is not measured from the start
        // of the TRACK never releases it: the controller spins, its position
        // frozen (the report suppresses position while buffering), and a track
        // change goes unreported — all three at once, on a track that is
        // audibly playing. That is not a hypothetical; it shipped, from a
        // writer that reported frames-since-the-current-SOURCE-started after a
        // seek rebuilt the engine around a source opened 166 s into the song.
        //
        // The signature is unmistakable and cheap to test for: we are on the
        // right track, well past the grace, and the clock is sitting far BELOW
        // the offset instead of climbing through it. Say so by name, once per
        // load, because the symptom on its own points at the cloud or the
        // controller and costs an evening.
        if player_track_id == b.track_id
            && b.since.elapsed() > STALLED_CLOCK_GRACE
            && position_ms + STALLED_CLOCK_SLACK_MS < b.start_position_secs.saturating_mul(1000)
        {
            log::error!(
                "[QConnect] track {} has been 'buffering' for {:?} with the player's clock at \
                 {} ms, BELOW the {} ms this stream opened at. The clock is almost certainly \
                 being reported relative to the current source rather than to the track, which \
                 never satisfies the audible edge — the controller will spin with a frozen \
                 position until the {:?} backstop fires.",
                b.track_id,
                b.since.elapsed(),
                position_ms,
                b.start_position_secs.saturating_mul(1000),
                BUFFERING_MAX,
            );
        }
        Some((b.track_id, b.start_position_secs, b.duration_secs))
    }
}

/// How long a load may sit below its own start offset before the tripwire
/// fires. Comfortably longer than a real load, which reaches the offset as soon
/// as the first samples are audible.
const STALLED_CLOCK_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How far below the offset counts as "not climbing towards it". A real load
/// sits AT the offset (the player has not moved yet), so only a clock that has
/// restarted from somewhere else lands here.
const STALLED_CLOCK_SLACK_MS: u64 = 2_000;

impl DaemonRendererEngine {
    pub fn new(
        runtime: Arc<AppRuntime<DaemonAdapter>>,
        volume_mode: VolumeMode,
        buffering: Arc<BufferingLatch>,
        report_notify: Arc<tokio::sync::Notify>,
        shared: Arc<std::sync::Mutex<crate::state::DaemonShared>>,
    ) -> Self {
        Self {
            runtime,
            volume_mode,
            current_feeder: std::sync::Mutex::new(None),
            buffering,
            shared,
            report_notify,
        }
    }

    /// The 0-100 percent this renderer should be TELLING the controller, given
    /// the player's real level and this session's volume policy.
    ///
    /// One place, because the join report and the join-time re-assertion must
    /// not be able to disagree about it.
    pub fn reported_volume_pct(&self) -> i32 {
        self.volume_mode
            .reported_volume_pct(self.core().get_playback_state().volume)
    }

    /// Abort the previous track's feeder (no-op when none). The dropped
    /// FailGuard marks the OLD writer errored, which is correct — that buffer
    /// belongs to the abandoned source.
    fn abort_current_feeder(&self) {
        if let Ok(mut guard) = self.current_feeder.lock() {
            if let Some(prev) = guard.take() {
                prev.abort();
            }
        }
    }

    fn core(&self) -> &Arc<QbzCore<DaemonAdapter>> {
        self.runtime.core()
    }

    /// Last-resort load for tracks the raw-URL path cannot fetch (the CDN
    /// header flood defeats every reqwest attempt — see
    /// `remote_stream::is_header_flood_error`): the CMAF path is unaffected by
    /// the h1 header cap. `play_track_resolved` does NOT move the queue cursor
    /// (nothing on the QConnect path does — the shared driver's cursor sync
    /// only fires on a playing->playing track edge), so sync it explicitly or
    /// `pibuz status` / the local now-playing truth keep showing the PREVIOUS
    /// track while the recovered one plays.
    async fn play_via_cmaf(
        &self,
        track_id: u64,
        quality: Quality,
        start_position_secs: u64,
    ) -> Result<(), String> {
        self.core()
            .play_track_resolved(track_id, quality, start_position_secs)
            .await
            .map_err(|err| format!("CMAF fallback for remote track {track_id}: {err}"))?;
        self.core().sync_current_to_id(track_id).await;
        Ok(())
    }
}

#[async_trait]
impl QconnectRendererEngine for DaemonRendererEngine {
    // ---- transport (sync) ----
    fn resume(&self) -> Result<(), String> {
        self.core().resume().map_err(|err| err.to_string())
    }
    fn pause(&self) -> Result<(), String> {
        self.core().pause().map_err(|err| err.to_string())
    }
    fn stop(&self) -> Result<(), String> {
        self.core().stop().map_err(|err| err.to_string())
    }
    fn seek(&self, position_secs: u64) -> Result<(), String> {
        // tell the controller we are buffering, exactly as a load
        // does. A seek is not instant — Qobuz FLACs carry no SEEKTABLE, so
        // Symphonia bisects, and a cold ranged open off the CDN costs seconds.
        // Measured on hardware: `Resume: landed on 60s in 10498ms`, ten and a
        // half seconds during which the app was told "playing" at a position
        // nothing was coming out of. It reads as a frozen player rather than a
        // busy one.
        //
        // This does not make the seek faster. It makes the app show its
        // spinner, and the report scheduler clears the latch as soon as audio
        // is actually being produced. The protocol has no buffering-PROGRESS
        // field — `buffer_state` is BUFFERING or OK and nothing else — so this
        // is the whole of what can be said on the wire.
        let state = self.get_playback_state();
        if state.track_id != 0 {
            self.buffering
                .begin(state.track_id, position_secs, state.duration);
            self.report_notify.notify_one();
        }
        self.core()
            .seek(position_secs)
            .map_err(|err| err.to_string())
    }
    fn set_volume(&self, fraction: f32) -> Result<(), String> {
        // T10 (OD4, §7.4): volume-mode gate. In `Locked` mode the player stays
        // at 100 % and a controller's remote SetVolume is acknowledged-but-
        // ignored (logged at info), so the DAC keeps receiving full-scale,
        // bit-perfect samples. `Software` (default) applies it via the core.
        if !self.volume_mode.applies_remote_volume() {
            log::info!(
                "[QConnect] volume_mode=locked: ignoring remote SetVolume({:.3}); player stays at 100%",
                fraction
            );
            return Ok(());
        }
        self.core()
            .set_volume(fraction)
            .map_err(|err| err.to_string())
    }
    fn get_playback_state(&self) -> PlaybackState {
        self.core().get_playback_state()
    }
    fn has_loaded_audio(&self) -> bool {
        self.core().player().has_loaded_audio()
    }

    // ---- queue / mode (async) ----
    async fn set_repeat_mode(&self, mode: RepeatMode) {
        self.core().set_repeat_mode(mode).await
    }
    async fn set_shuffle(&self, enabled: bool) {
        self.core().set_shuffle(enabled).await
    }
    async fn set_shuffle_flag(&self, enabled: bool) {
        self.core().set_shuffle_with_order(enabled, None).await
    }
    async fn get_all_queue_tracks(&self) -> (Vec<QueueTrack>, Option<usize>) {
        self.core().get_all_queue_tracks().await
    }
    async fn set_queue(&self, tracks: Vec<QueueTrack>, start_index: Option<usize>) {
        self.core().set_queue(tracks, start_index).await
    }
    async fn set_queue_with_order(
        &self,
        tracks: Vec<QueueTrack>,
        start_index: Option<usize>,
        shuffle_enabled: bool,
        shuffle_order: Option<Vec<usize>>,
    ) {
        self.core()
            .set_queue_with_order(tracks, start_index, shuffle_enabled, shuffle_order)
            .await
    }
    async fn clear_queue(&self, keep_current: bool) {
        self.core().clear_queue(keep_current).await
    }
    async fn play_index(&self, index: usize) -> Option<QueueTrack> {
        self.core().play_index(index).await
    }

    // ---- catalog (async) ----
    async fn get_track(&self, track_id: u64) -> Result<Track, String> {
        self.core()
            .get_track(track_id)
            .await
            .map_err(|err| err.to_string())
    }
    async fn get_tracks_batch(&self, track_ids: &[u64]) -> Result<Vec<Track>, String> {
        self.core()
            .get_tracks_batch(track_ids)
            .await
            .map_err(|err| err.to_string())
    }

    // ---- protected audio seam (the only protected touch) ----
    async fn start_track_stream(
        &self,
        track_id: u64,
        quality: Quality,
        duration_secs: u64,
        start_position_secs: u64,
    ) -> Result<(), String> {
        // Announce the load BEFORE anything touches the audio device.
        //
        // A host integration has to free the card for us: on moOde the event
        // hook stops MPD, and MPD keeps its ALSA device for seconds after that.
        // Until now the first thing the host heard about a cast was
        // PlaybackError — the daemon went straight from `paused` to opening an
        // exclusive device, failed because MPD still held it, and only then
        // said so. Casting to a player that was already playing something
        // simply did not work, which is the "Qobuz will not start while moOde
        // is playing" report.
        //
        // Emitting `loading` here gives the host the whole stream-URL resolve
        // (a network round-trip) plus the buffer fill to get out of the way,
        // and it is also the honest state to show a controller that is
        // otherwise told the renderer is paused while a track loads.
        if let Ok(shared) = self.shared.lock() {
            shared.emit(qbz_models::CoreEvent::PlaybackStateChanged {
                state: qbz_models::PlaybackState::Loading,
            });
        }

        // Ask the cache before the network. This path did not, which made the
        // daemon fill a cache its primary route never read: the driver writes
        // L1/L2 on a natural advance, and then a `previous` tap from the phone
        // — which arrives here, not there — re-downloaded a track already on
        // the card. The L2 directory on the test Pi was empty for exactly this
        // reason.
        //
        // `Loading` has already been emitted above, which is what frees the card
        // on moOde, and that must still happen before either path.
        {
            let player = self.core().player();
            if player.play_cached_if_present(track_id, quality, start_position_secs) {
                self.abort_current_feeder();

                // Arm the latch here TOO, not just on the streaming path.
                //
                // An earlier version of this skipped it, reasoning that a cache
                // hit is audible almost immediately and has no feeder to wait
                // for. Both are true and neither is what the latch is for: it
                // stops the report scheduler publishing the state of the track
                // we are leaving while the audio thread is still changing over.
                // Without it, a correct report naming the new track was followed
                // 27 ms later by a stale one naming the OLD track as stopped —
                //
                //   21:37:37.223  Track 74936706 served from the cache
                //   21:37:37.223  report: track=(74936706, 7)
                //   21:37:37.250  report: track=(208204223, 0) playing=Some(0)
                //
                // — so the controller drew the previous track's cover over the
                // new one and corrected itself a moment later. Reported as
                // artwork flicking to the wrong song when jumping back a few
                // tracks in the queue, which is exactly when the cache hits.
                //
                // It clears on the audible edge like any other, which for a
                // cache hit is the next few milliseconds.
                self.buffering
                    .begin(track_id, start_position_secs, duration_secs);
                self.report_notify.notify_one();

                log::info!("[QConnect] Track {track_id} served from the cache — no stream needed");
                return Ok(());
            }
        }

        let stream_url = self
            .core()
            .get_stream_url(track_id, quality)
            .await
            .map_err(|err| format!("resolve stream url for remote track {track_id}: {err}"))?;

        // stop the previous track's download before starting the
        // next one (see `current_feeder`).
        self.abort_current_feeder();

        // tell the controller we are loading. The stream is not
        // audible until the feeder reaches `start_position_secs` and the
        // pre-skip completes; the report scheduler clears this once the player
        // starts producing audio.
        self.buffering
            .begin(track_id, start_position_secs, duration_secs);
        self.report_notify.notify_one();

        let player = self.core().player();
        let stream_result = super::remote_stream::stream_remote_track_into_player(
            &player,
            track_id,
            duration_secs,
            start_position_secs,
            &stream_url.url,
            "QConnect",
        )
        .await;

        let stream_err = match stream_result {
            Ok(feeder) => {
                if let Ok(mut guard) = self.current_feeder.lock() {
                    *guard = Some(feeder);
                }
                return Ok(());
            }
            Err(err) => err,
        };

        // past this point the raw stream is gone and the latch,
        // armed above, no longer necessarily describes what is happening. Its
        // only other exits are the audible edge and a 90 s safety expiry, so a
        // stale entry means a minute and a half of spinner for audio that will
        // never arrive. Clear it whenever a fallback ends in Err.
        //
        // The CMAF fallback keeps the latch on success: it streams the same
        // track from the same offset, so the audible edge still fits.
        let clear_on_failure = |result: Result<(), String>| {
            if result.is_err() {
                self.buffering.finish(track_id);
                self.report_notify.notify_one();
            }
            result
        };

        // Akamai small-object header flood: SMALL raw-url objects come back
        // with ~106 headers, over hyper's hard-coded 100-header h1 cap, so
        // EVERY reqwest fetch of this URL fails — the full download would die
        // the same death. Skip it and go straight to the CMAF last resort.
        if super::remote_stream::is_header_flood_error(&stream_err) {
            log::warn!(
                "[QConnect] Raw-URL streaming hit the CDN header flood for track {track_id}: {stream_err}. Skipping full download; last resort: CMAF."
            );
            return clear_on_failure(
                self.play_via_cmaf(track_id, quality, start_position_secs)
                    .await,
            );
        }

        log::warn!(
            "[QConnect] Streaming handoff unavailable for track {}: {}. Falling back to full download.",
            track_id,
            stream_err
        );
        match download_remote_audio(&stream_url.url).await {
            Ok(audio_data) => {
                let played = self
                    .core()
                    .player()
                    .play_data(audio_data, track_id)
                    .map(|_| ())
                    .map_err(|err| format!("play remote track {track_id}: {err}"));
                // Clear either way: on success the complete file is in hand, so
                // nothing is filling — and `play_data` restarts from 0, so the
                // latch's start offset (82 s on a resume) would never be passed
                // and it would sit on BUFFERING until the safety expiry.
                self.buffering.finish(track_id);
                self.report_notify.notify_one();
                played
            }
            Err(download_err) if super::remote_stream::is_header_flood_error(&download_err) => {
                log::warn!(
                    "[QConnect] Full download hit the CDN header flood for track {track_id}: {download_err}. Last resort: CMAF."
                );
                clear_on_failure(
                    self.play_via_cmaf(track_id, quality, start_position_secs)
                        .await,
                )
            }
            Err(download_err) => clear_on_failure(Err(download_err)),
        }
    }

    fn current_output_format(&self) -> Option<(u32, u32)> {
        let player = self.core().player();
        Some((player.state.get_sample_rate(), player.state.get_bit_depth()))
    }
}

async fn download_remote_audio(url: &str) -> Result<Vec<u8>, String> {
    let response = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|err| {
            format!(
                "download remote audio request failed: {}",
                super::remote_stream::describe_reqwest_error(&err)
            )
        })?;

    if !response.status().is_success() {
        return Err(format!(
            "download remote audio failed with status {}",
            response.status()
        ));
    }

    let bytes = response.bytes().await.map_err(|err| {
        format!(
            "read remote audio bytes failed: {}",
            super::remote_stream::describe_reqwest_error(&err)
        )
    })?;
    Ok(bytes.to_vec())
}

// T10 (OD4, §7.4): volume-mode policy tests. These pin the decision the engine's
// `set_volume` gate and the session host's join-time volume report consult — the
// two enforcement points of the software|locked contract.
#[cfg(test)]
mod tests {

    /// THE ARTWORK FLICKER. While a track change is in progress the audio
    /// thread still reports the track being LEFT, and the latch is what stops
    /// that reaching the controller as the current state.
    ///
    /// Observed after a cache hit, where the latch had not been armed: a
    /// correct report naming the new track, then 27 ms later a stale one
    /// naming the old track as stopped, so the controller drew the previous
    /// cover over the new one.
    ///
    ///   21:37:37.223  Track 74936706 served from the cache
    ///   21:37:37.223  report: track=(74936706, 7)
    ///   21:37:37.250  report: track=(208204223, 0) playing=Some(0)
    #[test]
    fn the_outgoing_tracks_state_does_not_clear_the_latch() {
        const LEAVING: u64 = 208204223;
        const ARRIVING: u64 = 74936706;

        let latch = BufferingLatch::default();
        latch.begin(ARRIVING, 0, 300);

        // What the audio thread reports mid-changeover: still the old track,
        // stopped, at zero.
        let held = latch.in_flight_with_state(LEAVING, 0, false);
        assert_eq!(
            held.map(|(id, _, _)| id),
            Some(ARRIVING),
            "a report for the track being left must not be taken for arrival"
        );
    }

    /// And it must let go the moment the new track is actually audible, or the
    /// latch becomes the 90-second spinner it was once blamed for.
    #[test]
    fn the_latch_clears_when_the_new_track_is_audible() {
        const ARRIVING: u64 = 74936706;
        let latch = BufferingLatch::default();
        latch.begin(ARRIVING, 0, 300);
        assert!(
            latch.in_flight_with_state(ARRIVING, 1_000, true).is_none(),
            "audio on the new track is arrival"
        );
    }

    /// A load into a pause arrives too: a stopped clock can never climb past
    /// its own offset, so without this the latch would hold until the backstop.
    #[test]
    fn a_load_that_arrives_into_a_pause_clears_the_latch() {
        const ARRIVING: u64 = 74936706;
        let latch = BufferingLatch::default();
        latch.begin(ARRIVING, 51, 300);
        assert!(
            latch
                .in_flight_with_state(ARRIVING, 51_000, false)
                .is_none(),
            "arriving into a pause at the requested offset is arrival"
        );
    }

    use super::*;

    #[test]
    fn buffering_latch_tracks_one_track_at_a_time() {
        let latch = BufferingLatch::default();
        assert!(
            latch.in_flight(7, 0).is_none(),
            "nothing is buffering initially"
        );

        latch.begin(7, 0, 200);
        assert_eq!(latch.in_flight(7, 0), Some((7, 0, 200)));

        // A track change supersedes: the old track's late clear must not
        // release the new track's buffering state.
        latch.begin(8, 0, 300);
        latch.finish(7);
        assert_eq!(
            latch.in_flight(8, 0),
            Some((8, 0, 300)),
            "stale clear must be ignored"
        );

        latch.finish(8);
        assert!(
            latch.in_flight(8, 0).is_none(),
            "explicit clear releases it"
        );
    }

    #[test]
    fn buffering_latch_reports_the_loading_track_not_the_players() {
        // The whole point of the latch: during a load the PLAYER still names the
        // outgoing track (11) — or nothing at all, right after a hand-off — while
        // the load in flight is track 12 at 139s. The report must describe 12, or
        // the controller names the previous song and draws 0:00 of 0:00.
        let latch = BufferingLatch::default();
        latch.begin(12, 139, 254);
        assert_eq!(
            latch.in_flight(11, 42_000),
            Some((12, 139, 254)),
            "outgoing track playing: still the new track's load"
        );
        assert_eq!(
            latch.in_flight(0, 0),
            Some((12, 139, 254)),
            "player empty after a hand-off: still the new track's load"
        );
    }

    #[test]
    fn buffering_latch_clears_only_once_the_clock_moves() {
        // A resume at 80s: the player parks the clock at 80 while it downloads
        // to that offset and pre-skips, and reports itself "playing" long
        // before the first sample — so only a position PAST 80 means audible.
        let latch = BufferingLatch::default();
        latch.begin(9, 80, 200);
        assert!(
            latch.in_flight(9, 80_000).is_some(),
            "still filling at the start offset"
        );
        assert!(
            latch.in_flight(9, 80_000).is_some(),
            "repeated ticks stay buffering"
        );
        // Milliseconds, so the very first sample past the offset counts —
        // whole seconds kept the spinner up for a second of audible music.
        assert!(
            latch.in_flight(9, 80_050).is_none(),
            "clock moved: audio is flowing"
        );
        assert!(latch.in_flight(9, 80_050).is_none(), "and it stays cleared");
    }

    /// The exact shape of the shipped bug, encoded so it cannot come back
    /// silently: a seek to 2:46, and a clock that restarts from zero instead of
    /// counting from the start of the track.
    ///
    /// The latch cannot FIX that — it is downstream of the clock, and its job
    /// is to believe what the player tells it — so this asserts the two things
    /// it can do: keep reporting the load (which is the honest reading of that
    /// input), and hit the 90 s backstop rather than spinning forever.
    #[test]
    fn a_clock_that_restarts_after_a_seek_never_satisfies_the_audible_edge() {
        let latch = BufferingLatch::default();
        // The stream opened 166 seconds into the track.
        latch.begin(9, 166, 372);

        // A clock counting from the start of the SOURCE reports seconds since
        // the seek — 0, 1, 2, 3 — and never passes 166 000 ms.
        for since_seek_ms in [0_u64, 1_000, 2_000, 3_000, 60_000] {
            assert!(
                latch.in_flight(9, since_seek_ms).is_some(),
                "a source-relative clock of {since_seek_ms} ms cannot release a latch armed at                  166 000 ms — this is the bug, and the fix belongs in the writer"
            );
        }

        // A clock counting from the start of the TRACK passes it immediately,
        // which is what the fix makes happen.
        let latch = BufferingLatch::default();
        latch.begin(9, 166, 372);
        assert!(
            latch.in_flight(9, 166_500).is_none(),
            "a track-relative clock one half-second past the seek must release the latch"
        );
    }

    /// FROM HARDWARE. Cast to a Pi, pause, restart the daemon, let the
    /// controller re-attach: it sends `SetState { playing_state: PAUSED,
    /// current_position_ms: 7000 }`. The renderer opens the stream at 7 s,
    /// arrives, and pauses exactly as asked — and then reported BUFFERING for
    /// a minute and a half, because the audible edge is `position > start` and
    /// a paused clock sits AT 7000, never past it. The controller showed a
    /// spinner on a track that was doing precisely what it had been told.
    ///
    /// The 90 s backstop did eventually fire, which is how the log ends. A
    /// backstop is not an answer: for those 90 seconds the report also
    /// suppresses the position, so the session looks hung.
    ///
    /// Arrival is the edge when the player is not playing BY REQUEST. Landing
    /// on the track at or past the offset with the clock stopped is not a load
    /// still in flight; it is a load that finished into a pause.
    #[test]
    fn a_track_that_loads_straight_into_a_pause_is_not_buffering() {
        let latch = BufferingLatch::default();
        latch.begin(9, 7, 341);

        // Still loading: the player has not adopted the track yet.
        assert!(
            latch.in_flight_with_state(0, 0, true).is_some(),
            "before the player arrives, the load really is in flight"
        );
        // Arrived, paused, clock sitting exactly on the offset. Not buffering.
        assert!(
            latch.in_flight_with_state(9, 7_000, false).is_none(),
            "a paused track sitting on the offset it opened at has ARRIVED — reporting it as \
             buffering is a spinner on a session that is behaving correctly"
        );
    }

    /// The pause release must not swallow a genuine cold load. A player that
    /// is not yet playing because the stream has not produced audio still
    /// reports the OUTGOING track (or none), so it never looks like arrival.
    #[test]
    fn a_cold_load_still_reports_buffering_while_it_is_not_playing() {
        let latch = BufferingLatch::default();
        latch.begin(9, 0, 341);
        for (track, pos) in [(0u64, 0u64), (8, 240_000)] {
            assert!(
                latch.in_flight_with_state(track, pos, false).is_some(),
                "track {track} at {pos} ms is not the loading track — the load is still in flight"
            );
        }
    }

    #[test]
    fn buffering_latch_gives_up_on_a_load_that_never_starts() {
        let latch = BufferingLatch::default();
        latch.begin(9, 0, 200);
        // Backdate past the safety window: a load that never becomes audible
        // must not report BUFFERING forever.
        if let Ok(mut guard) = latch.0.lock() {
            if let Some(b) = guard.as_mut() {
                b.since =
                    std::time::Instant::now() - (BUFFERING_MAX + std::time::Duration::from_secs(1));
            }
        }
        assert!(latch.in_flight(9, 0).is_none());
    }

    #[test]
    fn software_mode_applies_and_reports_real() {
        // remote SetVolume 0.4 -> engine.set_volume(0.4); report reads real volume.
        let mode = VolumeMode::from_kv(Some("software"));
        assert_eq!(mode, VolumeMode::Software);
        assert!(mode.applies_remote_volume());
        assert_eq!(mode.reported_volume_pct(0.4), 40);
        assert_eq!(mode.reported_volume_pct(1.0), 100);
    }

    #[test]
    fn locked_mode_ignores_and_reports_100() {
        // remote SetVolume -> acknowledged-but-ignored; player stays 1.0; 100 reported.
        let mode = VolumeMode::from_kv(Some("locked"));
        assert_eq!(mode, VolumeMode::Locked);
        assert!(!mode.applies_remote_volume());
        // 100 reported regardless of the player's actual level.
        assert_eq!(mode.reported_volume_pct(0.4), 100);
        assert_eq!(mode.reported_volume_pct(1.0), 100);
    }

    #[test]
    fn default_mode_is_software_od4() {
        // Unset / empty / unknown all resolve to the OD4 default (software).
        assert_eq!(VolumeMode::default(), VolumeMode::Software);
        assert_eq!(VolumeMode::from_kv(None), VolumeMode::Software);
        assert_eq!(VolumeMode::from_kv(Some("")), VolumeMode::Software);
        assert_eq!(VolumeMode::from_kv(Some("  ")), VolumeMode::Software);
        assert_eq!(VolumeMode::from_kv(Some("garbage")), VolumeMode::Software);
        // Whitespace around the real value is tolerated.
        assert_eq!(VolumeMode::from_kv(Some(" locked ")), VolumeMode::Locked);
    }
}
