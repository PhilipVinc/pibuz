// crates/pibuz/src/api/status.rs — `GET /api/status` composite contract
// (02-cli-and-api.md §3.3.3; memo D14). Struct shape only in T2 — the
// `auth`/`qconnect`/`last_errors` sections already read from `DaemonShared`;
// `audio`/`playback`/`network` are placeholders wired by T3 (audio/playback
// driver), T6 (HTTP server + network reachability) and T10 (qconnect report
// tick, already flowing through `DaemonShared::qconnect`).
use std::io::Cursor;

use serde::Serialize;
use tiny_http::Response;

use crate::state::{LatchedErrors, QconnectStatus};

#[derive(Debug, Clone, Serialize)]
pub struct StatusDoc {
    pub version: String,
    pub api_version: u32,
    pub uptime_secs: u64,
    pub data_root: String,
    pub driver_tick_age_ms: Option<u64>,
    pub audio: AudioStatus,
    pub playback: PlaybackStatus,
    pub qconnect: QconnectStatus,
    pub network: NetworkStatus,
    /// The host's memory class and the sizing that follows from it. Resolved
    /// DAEMON-side on purpose: `memory_profile()` is a `OnceLock` off this
    /// box's `/proc/meminfo`, so a laptop running `pibuz status --host pi`
    /// would otherwise report its own class for the Pi's daemon.
    pub memory: MemoryStatus,
    pub cache: CacheStatus,
    pub buffers: BufferStatus,
    pub last_errors: LatchedErrors,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryStatus {
    /// "normal" | "low" — the `MemoryClass` that gates prefetch, gapless,
    /// ring depth and the compressed window.
    pub class: String,
    /// `null` where the daemon cannot read `/proc/meminfo` — macOS and
    /// Windows, i.e. a dev box. `detect_profile` falls back to the Normal
    /// class by passing `u64::MAX`, which is a sentinel and not a RAM figure:
    /// putting it on the wire would have the status block report 16 EB.
    pub total_kb: Option<u64>,
    /// Whether a gapless successor can be bounded on this host at all — RAM
    /// above the floor, OR a disk cache to stream an oversized one to. Not
    /// `MemoryProfile::allow_gapless_prefetch`, which is only the RAM half:
    /// on a 512 MB board with `audio.cache_to_disk` on the two disagree, and
    /// this is the one that answers "will my album play gapless".
    pub gapless_prefetch: bool,
    pub hires_prefetch: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheStatus {
    pub l1: L1CacheStatus,
    /// `null` when `audio.cache_to_disk` is off or the disk cache failed to
    /// open: nothing is being written to the card either way.
    pub l2: Option<L2CacheStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct L1CacheStatus {
    pub tracks: usize,
    pub bytes: usize,
    /// The budget in force — `audio.memory_cache_mb` when set, else the
    /// profile's share of RAM.
    pub budget_bytes: usize,
    pub fetching: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct L2CacheStatus {
    pub tracks: usize,
    pub bytes: u64,
    pub budget_bytes: u64,
    pub dir: String,
}

/// Every buffer between the CDN and the DAC, in the order the bytes cross them.
#[derive(Debug, Clone, Serialize)]
pub struct BufferStatus {
    /// Compressed read-ahead: `audio.stream_window_seconds` of music, clamped
    /// to `window_max_bytes` (the profile's ceiling) at the top and 2 MB at
    /// the bottom. Seconds rather than bytes because a byte constant is a
    /// different amount of music at every quality. `null` only when the
    /// settings DB could not be read — which is a different fact from `0`.
    pub window_seconds: Option<u8>,
    pub window_max_bytes: usize,
    /// Process-wide cap on the initial buffer a stream waits for before it
    /// starts. The actual figure is chosen per track from measured link speed
    /// and clamped to this.
    pub initial_max_bytes: usize,
    /// Decoded ring depth, as ACTUALLY ALLOCATED when a device is open.
    ///
    /// `audio.pcm_ring_ms` (or the profile's seconds) is only a request: the
    /// ring takes the largest of it, 250 ms, and three times the ALSA hardware
    /// ring. On a box with `alsa_buffer_ms = 1000` at 96 kHz that last floor
    /// wins and the ring is 3000 ms however small the profile asked for, so
    /// reporting the request alone understated a real Pi by 50 %.
    ///
    /// With no device open the rate is unknown and the floors cannot be
    /// applied, so this falls back to the requested value.
    pub pcm_ring_ms: u32,
    /// `audio.alsa_buffer_ms`; `null` when it is the rate-derived default
    /// (500 ms at 192 kHz+, 250 ms from 96 kHz, 125 ms below), which is not
    /// known here because it depends on the stream.
    pub alsa_buffer_ms: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioStatus {
    pub backend: Option<String>,
    pub configured_device: Option<String>,
    pub device_present: bool,
    pub device_open: bool,
    /// `BitPerfectMode` serde variants: "DirectHardware"|"PluginFallback"|"Disabled"
    /// (crates/qbz-audio/src/backend.rs:226-233). Kept as a plain string here so
    /// this crate does not need to depend on the exact qbz-audio enum shape yet.
    pub bit_perfect: Option<String>,
    /// Rate the DECODED STREAM runs at — what the file was encoded at.
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    /// Rate the OUTPUT DEVICE runs at. Differs from `sample_rate` when
    /// something in the chain resamples (a shared PipeWire/Pulse/CPAL path,
    /// or an ALSA config that pins a rate). None before any stream exists.
    pub output_sample_rate: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaybackStatus {
    pub state: String,
    pub track_id: Option<u64>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub position: Option<u64>,
    pub duration: Option<u64>,
    pub volume: f32,
    pub muted: bool,
    pub queue_len: usize,
    /// How much of the streaming track has been downloaded (0.0-1.0). `null`
    /// when not streaming, or once the track is complete.
    pub buffer_progress: Option<f32>,
    /// The audio thread has the next track pre-queued for a gapless handoff.
    pub gapless_ready: bool,
    pub gapless_next_track_id: Option<u64>,
    /// Replay-gain factor being applied; `null` when normalization is off.
    pub normalization_gain: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkStatus {
    pub online: bool,
}

// ============================ live handlers (T6) ============================

/// `GET /api/info` (02 §3.3.2) — identity for the CLI's version-skew diagnosis
/// (§1.6, the single sanctioned second request). Deliberately minimal.
pub fn info(state: &super::ApiState) -> Response<Cursor<Vec<u8>>> {
    let uptime = state
        .shared
        .lock()
        .ok()
        .map(|s| s.started_at.elapsed().as_secs())
        .unwrap_or(0);
    super::json(
        200,
        serde_json::json!({
            "app": "pibuz",
            "version": crate::VERSION,
            "api_version": crate::API_VERSION,
            "bind": state.bind,
            "uptime_secs": uptime,
            "data_root": state.roots.data.display().to_string(),
        }),
    )
}

/// `GET /api/status` (02 §3.3.3) — the composite daemon status. ALWAYS 200;
/// the CLI maps degradation (a missing device) to exit codes.
pub fn status(state: &super::ApiState) -> Response<Cursor<Vec<u8>>> {
    let doc = assemble_live(state);
    let mut value = serde_json::to_value(&doc).unwrap_or_else(|_| serde_json::json!({}));
    // `StatusDoc.playback.volume` is f32; plain `to_value` widens it via
    // `Number::from_f32` (f32→f64, `0.8` → `0.800000011920929`). Overwrite
    // with the canonical form — see `super::canon_volume`.
    if let Some(vol) = value.pointer_mut("/playback/volume") {
        *vol = super::canon_volume(doc.playback.volume);
    }
    // Same widening, same fix, for the other two f32s on the wire.
    if let (Some(slot), Some(p)) = (
        value.pointer_mut("/playback/buffer_progress"),
        doc.playback.buffer_progress,
    ) {
        *slot = super::canon_f32(p);
    }
    if let (Some(slot), Some(g)) = (
        value.pointer_mut("/playback/normalization_gain"),
        doc.playback.normalization_gain,
    ) {
        *slot = super::canon_f32(g);
    }
    super::json(200, value)
}

/// Compose [`StatusDoc`] from live sources: `DaemonShared` (qconnect/latched
/// errors/tick age), the Player's sync getters via `get_playback_event`, the
/// queue via an async core call (`block_on` on the daemon runtime — this is a
/// plain serving thread, never a tokio worker, so no panic), and the audio
/// store + TTL device cache for the audio block.
fn assemble_live(state: &super::ApiState) -> StatusDoc {
    // 1. snapshot DaemonShared, then DROP the guard before any block_on so the
    //    mutex is never held across an await point.
    let (last_errors, qconnect, tick_age, muted, uptime, network_online) = match state.shared.lock()
    {
        Ok(s) => (
            s.last_errors.clone(),
            s.qconnect.clone(),
            s.driver_last_tick.map(|t| t.elapsed().as_millis() as u64),
            s.muted,
            s.started_at.elapsed().as_secs(),
            s.network_online(),
        ),
        Err(_) => (
            LatchedErrors::default(),
            QconnectStatus::default(),
            None,
            false,
            0,
            true,
        ),
    };

    // 2. live player snapshot (all sync atomics, folded into one PlaybackEvent).
    let player = state.runtime.core().player();
    let ev = player.get_playback_event();
    let device_open = player.state.current_device().is_some();

    // 3. queue — async core read, driven from this non-worker thread.
    let queue = state.rt.block_on(state.runtime.core().get_queue_state());

    // 4. audio config from the store; device_present from the TTL cache. An OPEN
    //    device counts as present: CPAL enumerates DESCRIPTIONS ("HiFiBerry DAC+
    //    ..."), never `hw:CARD=...` ids, so a playing direct-hw stream would
    //    otherwise report `not present` (false negative).
    let settings = state.audio.get_settings().ok();
    let backend = settings
        .as_ref()
        .and_then(|s| backend_label(s.backend_type));
    let configured_device = settings.as_ref().and_then(|s| s.output_device.clone());
    let device_present = match &configured_device {
        None => true, // system default is always "present"
        Some(dev) => device_open || device_is_present(state, dev),
    };

    // 5. cache tiers and the host's memory profile. `memory_profile()` is a
    //    process-wide `OnceLock` the player already resolved at start, so this
    //    is a read, not a detection; `cache_report` takes each tier's lock
    //    briefly and drops it before returning.
    let cache = player.cache_report();
    let profile = qbz_models::system_capabilities::memory_profile();

    // 6. playback block. `stopped` when nothing is loaded and the queue has no
    //    current track; otherwise `playing`/`paused`.
    let has_track = queue.current_track.is_some();
    let pstate = if ev.is_playing {
        "playing"
    } else if has_track || player.has_loaded_audio() {
        "paused"
    } else {
        "stopped"
    };
    let stopped = pstate == "stopped";
    let (title, artist, track_id) = match &queue.current_track {
        Some(t) => (Some(t.title.clone()), Some(t.artist.clone()), Some(t.id)),
        None if ev.track_id != 0 => (None, None, Some(ev.track_id)),
        None => (None, None, None),
    };

    StatusDoc {
        version: crate::VERSION.to_string(),
        api_version: crate::API_VERSION,
        uptime_secs: uptime,
        data_root: state.roots.data.display().to_string(),
        driver_tick_age_ms: tick_age,
        audio: AudioStatus {
            backend,
            configured_device,
            device_present,
            device_open,
            bit_perfect: bitperfect_label(ev.bit_perfect_mode),
            sample_rate: ev.sample_rate,
            bit_depth: ev.bit_depth,
            output_sample_rate: ev.output_sample_rate,
        },
        playback: PlaybackStatus {
            state: pstate.to_string(),
            track_id: if stopped { None } else { track_id },
            title: if stopped { None } else { title },
            artist: if stopped { None } else { artist },
            position: if stopped { None } else { Some(ev.position) },
            duration: if stopped { None } else { Some(ev.duration) },
            volume: ev.volume,
            muted,
            queue_len: queue.total_tracks,
            buffer_progress: if stopped { None } else { ev.buffer_progress },
            gapless_ready: ev.gapless_ready,
            gapless_next_track_id: match ev.gapless_next_track_id {
                0 => None,
                id => Some(id),
            },
            normalization_gain: ev.normalization_gain,
        },
        qconnect,
        network: NetworkStatus {
            online: network_online,
        },
        memory: MemoryStatus {
            class: match profile.class {
                qbz_models::system_capabilities::MemoryClass::LowMemory => "low",
                qbz_models::system_capabilities::MemoryClass::Normal => "normal",
            }
            .to_string(),
            total_kb: match profile.mem_total_kb {
                u64::MAX => None,
                kb => Some(kb),
            },
            gapless_prefetch: player.gapless_prefetch_possible(),
            hires_prefetch: profile.allow_hires_prefetch,
        },
        cache: CacheStatus {
            l1: L1CacheStatus {
                tracks: cache.l1_tracks,
                bytes: cache.l1_bytes,
                budget_bytes: cache.l1_budget_bytes,
                fetching: cache.l1_fetching,
            },
            l2: cache.l2.map(|d| L2CacheStatus {
                tracks: d.tracks,
                bytes: d.bytes,
                budget_bytes: d.budget_bytes,
                dir: d.dir,
            }),
        },
        buffers: BufferStatus {
            window_seconds: settings.as_ref().map(|s| s.stream_window_seconds),
            window_max_bytes: profile.stream_window_max_bytes,
            initial_max_bytes: qbz_player::player::max_initial_buffer_bytes(),
            // `0` in the setting means "the profile decides", and the profile
            // is in turn only a request — see the field's doc. Resolve the whole
            // thing here rather than making every reader know the rule, and use
            // `qbz_audio`'s own function so there is one definition of it.
            pcm_ring_ms: {
                let requested = settings.as_ref().map(|s| s.pcm_ring_ms).unwrap_or(0);
                let alsa_ms = settings
                    .as_ref()
                    .map(|s| u32::from(s.alsa_buffer_ms))
                    .unwrap_or(0);
                match ev.output_sample_rate.filter(|r| *r > 0) {
                    // Channels only trims a ring at the byte ceiling, which
                    // stereo never reaches; the device is stereo on every path
                    // this daemon opens.
                    Some(rate) => qbz_audio::ring_depth_ms(
                        rate,
                        2,
                        qbz_audio::alsa_buffer_frames(rate, alsa_ms),
                        requested,
                        profile.pcm_ring_seconds,
                    ),
                    None if requested > 0 => requested,
                    None => u32::from(profile.pcm_ring_seconds) * 1000,
                }
            },
            alsa_buffer_ms: settings
                .as_ref()
                .map(|s| s.alsa_buffer_ms)
                .filter(|ms| *ms > 0),
        },
        last_errors,
    }
}

/// `BitPerfectMode` → its serde variant string (02 §3.3.3, plus
/// `"DirectNamedDevice"` for a named PCM whose chain we cannot see into).
/// `None` = no active stream.
fn bitperfect_label(m: Option<qbz_audio::BitPerfectMode>) -> Option<String> {
    use qbz_audio::BitPerfectMode as M;
    m.map(|m| {
        match m {
            M::DirectHardware => "DirectHardware",
            M::DirectNamedDevice => "DirectNamedDevice",
            M::PluginFallback => "PluginFallback",
            M::Disabled => "Disabled",
        }
        .to_string()
    })
}

/// Configured backend → the lowercase label the status block shows. `None`
/// (auto-detect) stays `null` until a stream picks a concrete backend.
fn backend_label(b: Option<qbz_audio::AudioBackendType>) -> Option<String> {
    use qbz_audio::AudioBackendType as B;
    b.map(|b| {
        match b {
            B::PipeWire => "pipewire",
            B::Alsa => "alsa",
            B::Pulse => "pulse",
            B::Jack => "jack",
            B::SystemDefault => "system",
        }
        .to_string()
    })
}

/// Best-effort presence check against the TTL-cached device enumeration. Exact
/// device identity is refined in T10; here a substring match on either side
/// tolerates the CPAL-name vs `hw:` mismatch without false negatives on a match.
fn device_is_present(state: &super::ApiState, dev: &str) -> bool {
    cached_device_names(state)
        .iter()
        .any(|n| n == dev || n.contains(dev) || dev.contains(n.as_str()))
}

/// Device names, re-enumerated at most every 5 s (a `status` poll must not
/// re-scan CPAL on every call). On enumeration failure the timestamp is still
/// bumped so a broken audio stack is not hammered.
fn cached_device_names(state: &super::ApiState) -> Vec<String> {
    use std::time::{Duration, Instant};
    let mut cache = match state.devices.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let fresh = cache
        .at
        .map(|t| t.elapsed() < Duration::from_secs(5))
        .unwrap_or(false);
    if !fresh {
        if let Ok(sinks) = qbz_audio::output_sinks::list_output_sinks() {
            cache.names = sinks.into_iter().map(|s| s.name).collect();
        }
        cache.at = Some(Instant::now());
    }
    cache.names.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An idle [`StatusDoc`] built directly (no live runtime) — the serde-shape
    /// contract these tests pin.
    fn idle_doc() -> StatusDoc {
        StatusDoc {
            version: "2.1.0".into(),
            api_version: crate::API_VERSION,
            uptime_secs: 261_360,
            data_root: "/home/pi/.local/share/pibuz".into(),
            driver_tick_age_ms: Some(210),
            audio: AudioStatus {
                backend: Some("alsa".into()),
                configured_device: None,
                device_present: true,
                device_open: false,
                bit_perfect: None,
                sample_rate: None,
                bit_depth: None,
                output_sample_rate: None,
            },
            playback: PlaybackStatus {
                state: "stopped".into(),
                track_id: None,
                title: None,
                artist: None,
                position: None,
                duration: None,
                volume: 0.0,
                muted: false,
                queue_len: 0,
                buffer_progress: None,
                gapless_ready: false,
                gapless_next_track_id: None,
                normalization_gain: None,
            },
            qconnect: QconnectStatus::default(),
            network: NetworkStatus { online: true },
            memory: MemoryStatus {
                class: "normal".into(),
                total_kb: Some(3_998_000),
                gapless_prefetch: true,
                hires_prefetch: true,
            },
            cache: CacheStatus {
                l1: L1CacheStatus {
                    tracks: 0,
                    bytes: 0,
                    budget_bytes: 644 * 1024 * 1024,
                    fetching: 0,
                },
                l2: Some(L2CacheStatus {
                    tracks: 37,
                    bytes: 1_288_490_188,
                    budget_bytes: 800 * 1024 * 1024,
                    dir: "/home/pi/.cache/pibuz/audio".into(),
                }),
            },
            buffers: BufferStatus {
                window_seconds: Some(8),
                window_max_bytes: 32 * 1024 * 1024,
                initial_max_bytes: 2 * 1024 * 1024,
                pcm_ring_ms: 6000,
                alsa_buffer_ms: None,
            },
            last_errors: LatchedErrors {
                stream: None,
                auth: Some("token rejected by Qobuz (401) — cleared".into()),
                transport: None,
            },
        }
    }

    #[test]
    fn status_doc_mirrors_the_top_level_contract_keys() {
        // 02-cli-and-api.md §3.3.3 top-level keys, exactly.
        let json = serde_json::to_value(idle_doc()).unwrap();
        let obj = json.as_object().unwrap();
        for key in [
            "version",
            "api_version",
            "uptime_secs",
            "data_root",
            "driver_tick_age_ms",
            "audio",
            "playback",
            "qconnect",
            "network",
            "memory",
            "cache",
            "buffers",
            "last_errors",
        ] {
            assert!(obj.contains_key(key), "missing top-level key: {key}");
        }
    }

    #[test]
    fn the_new_sections_are_additive_only() {
        // The §3.3.3 keys an existing reader knows must all still be there and
        // still mean what they meant — `memory`/`cache`/`buffers` are ADDED
        // beside them, which is why this needs no `api_version` bump and why
        // the moOde overlay (which polls /api/now-playing, not this) is
        // untouched.
        let json = serde_json::to_value(idle_doc()).unwrap();
        assert_eq!(json["playback"]["state"], "stopped");
        assert_eq!(json["audio"]["device_present"], true);
        assert_eq!(json["network"]["online"], true);
        // And the new ones carry the daemon's own figures, not a reader's guess.
        assert_eq!(json["memory"]["class"], "normal");
        assert_eq!(json["cache"]["l2"]["tracks"], 37);
        assert_eq!(json["buffers"]["pcm_ring_ms"], 6000);
    }

    /// On a dev box `memory_profile()` has no `/proc/meminfo` to read and
    /// falls back to Normal by passing `u64::MAX` — a sentinel, not a size.
    #[test]
    fn a_host_with_no_meminfo_reports_no_ram_figure() {
        let mut doc = idle_doc();
        doc.memory.total_kb = None;
        let json = serde_json::to_value(&doc).unwrap();
        assert!(json["memory"]["total_kb"].is_null());
        // The class is still the daemon's real answer, and still useful.
        assert_eq!(json["memory"]["class"], "normal");
    }

    #[test]
    fn a_disk_cache_that_is_off_is_null_not_zero() {
        // `cache_to_disk = false` and "the disk cache holds nothing" are
        // different facts, and the status block renders them differently:
        // `null` says nothing is written to the card at all.
        let mut doc = idle_doc();
        doc.cache.l2 = None;
        let json = serde_json::to_value(&doc).unwrap();
        assert!(json["cache"]["l2"].is_null());
    }

    #[test]
    fn the_f32_fields_all_serialize_canonically() {
        // Same `Number::from_f32` widening as `volume`: `buffer_progress` and
        // `normalization_gain` would land as `0.9800000190734863` raw.
        let mut doc = idle_doc();
        doc.playback.buffer_progress = Some(0.98f32);
        doc.playback.normalization_gain = Some(0.75f32);
        let mut value = serde_json::to_value(&doc).unwrap();
        for (ptr, v) in [
            ("/playback/buffer_progress", 0.98f32),
            ("/playback/normalization_gain", 0.75f32),
        ] {
            *value.pointer_mut(ptr).unwrap() = crate::api::canon_f32(v);
        }
        let rendered = serde_json::to_string(&value).unwrap();
        assert!(rendered.contains("\"buffer_progress\":0.98"), "{rendered}");
        assert!(
            rendered.contains("\"normalization_gain\":0.75"),
            "{rendered}"
        );
        assert!(!rendered.contains("0.98000"), "{rendered}");
    }

    #[test]
    fn a_latched_auth_error_is_still_reported() {
        // `last_errors.auth` is the error CHANNEL, not the account state
        // machine that went with the login path: a QConnect handoff whose
        // `jwt_api` is rejected still latches here, and `/api/status` is the
        // only place it surfaces.
        let doc = idle_doc();
        assert_eq!(
            doc.last_errors.auth.as_deref(),
            Some("token rejected by Qobuz (401) — cleared")
        );
    }

    #[test]
    fn status_doc_playback_volume_serializes_canonically() {
        // Pins the `status()` pointer-overwrite: `to_value(&doc)` widens the
        // f32 `playback.volume` via `Number::from_f32`; the fix must land
        // `0.8` on the wire, never `0.800000011920929`.
        let mut doc = idle_doc();
        doc.playback.volume = 0.8f32;
        let mut value = serde_json::to_value(&doc).unwrap();
        if let Some(vol) = value.pointer_mut("/playback/volume") {
            *vol = crate::api::canon_volume(doc.playback.volume);
        }
        let rendered = serde_json::to_string(&value).unwrap();
        assert!(rendered.contains("\"volume\":0.8"), "got: {rendered}");
        assert!(!rendered.contains("0.80000"), "got: {rendered}");
    }

    #[test]
    fn audio_block_serializes_the_documented_keys() {
        // 02 §3.3.3 audio object — the shape the live assembler fills.
        let json = serde_json::to_value(idle_doc()).unwrap();
        let audio = json["audio"].as_object().unwrap();
        for key in [
            "backend",
            "configured_device",
            "device_present",
            "device_open",
            "bit_perfect",
            "sample_rate",
            "bit_depth",
        ] {
            assert!(audio.contains_key(key), "missing audio key: {key}");
        }
    }
}
