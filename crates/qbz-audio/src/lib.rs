//! QBZ Audio - Audio backend system for bit-perfect playback
//!
//! This crate provides the audio backend abstraction layer:
//! - Backend trait and implementations (PipeWire, ALSA, PulseAudio)
//! - Audio device enumeration and selection
//! - Loudness analysis and normalization
//! - Diagnostic tools
//!
//! # What must not change by accident
//!
//! (This header used to read "This code is IMMUTABLE" and point at
//! `qbz-nix-docs/AUDIO_BACKENDS.md`. That file is not in this tree — it went
//! with the desktop UI — and the code below has been rewritten many times
//! since. It was a stale marker of exactly the kind CLAUDE.md warns about, so
//! here are the invariants it was trying to protect, stated where they can be
//! checked.)
//!
//! - **The bit-perfect path does not touch the samples.** On the ALSA-direct
//!   path the only permitted operations between the decoder and the device are
//!   the format conversion in [`alsa_direct::encode_into`] and an explicit
//!   software volume at unity. No resampling, no dither, no mixing.
//! - **Full scale is 2^(N-1), never 2^(N-1) − 1**, so a bit-depth change is an
//!   exact shift. See the doc comment on [`alsa_direct::f32_to_s16`].
//! - **The output follows the track's rate**; a format change rebuilds the
//!   stream rather than resampling into the open one.
//! - **Only one thread may be real-time, and it may not block.** See
//!   [`rt`] for what that costs and why.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     qbz-audio (Tier 1)                      │
//! │  Audio backends, device management, loudness analysis       │
//! └─────────────────────────────────────────────────────────────┘
//!                              ↑
//!                      ┌───────┴───────┐
//!                      │  qbz-models   │
//!                      │   (Tier 0)    │
//!                      └───────────────┘
//! ```

#[cfg(target_os = "linux")]
pub mod alsa_backend;
pub mod alsa_direct;
#[cfg(target_os = "linux")]
pub mod alsa_error_handler;
pub mod analysis;
pub mod analyzer_tap;
pub mod audio_out;
pub mod backend;
pub mod coreaudio_direct;
pub mod dac_capabilities;
pub mod dac_probe;
pub mod device_filter;
pub mod device_reservation;
pub mod diagnostic;
pub mod dynamic_amplify;
pub mod health;
#[cfg(target_os = "linux")]
pub mod jack_backend;
pub mod loudness;
pub mod loudness_analyzer;
pub mod loudness_cache;
pub mod network_throttle;
pub mod output_sinks;
pub mod pcm_ring;
#[cfg(target_os = "linux")]
pub mod pipewire_backend;
#[cfg(target_os = "linux")]
pub mod pulse_backend;
pub mod rt;
pub mod settings;
pub mod virtual_out;
pub mod visualizer;
pub mod volume_curve;

// Re-export commonly used types
#[cfg(target_os = "linux")]
pub use alsa_backend::{
    device_supports_sample_rate, get_device_supported_rates, normalize_device_id_to_stable,
    resolve_stable_to_current_hw,
};
pub use alsa_direct::AlsaDirectStream;
pub use analysis::SpectralAnalyzer;
pub use analyzer_tap::{AnalyzerMessage, AnalyzerTap};
pub use audio_out::AudioOut;
pub use backend::{
    AlsaDirectError, AlsaPlugin, AudioBackend, AudioBackendType, AudioDevice, BackendConfig,
    BackendManager, BackendResult, BitPerfectMode,
};
pub use coreaudio_direct::CoreAudioExclusiveGuard;
pub use dac_capabilities::{query_dac_capabilities, DacCapabilities};
pub use dac_probe::{negotiated_active_rate, negotiated_stream_rate, NegotiatedRate};
pub use device_reservation::{DeviceReservation, ReservationError};
pub use diagnostic::{AudioDiagnostic, BitDepthResult, DiagnosticSource};
pub use dynamic_amplify::DynamicAmplify;
pub use health::{
    audio_stack_health, detect_distro, detect_init, detect_sandbox, AudioStackHealth, Distro,
    InitSystem, Sandbox,
};
#[cfg(target_os = "linux")]
pub use jack_backend::JackStream;
pub use loudness::{calculate_gain_factor, db_to_linear, extract_replaygain, ReplayGainData};
pub use loudness_analyzer::LoudnessAnalyzer;
pub use loudness_cache::LoudnessCache;
pub use output_sinks::{list_output_sinks, OutputSinkInfo};
pub use pcm_ring::{ring_capacity_frames, Boundary, BoundaryKind, RingLink};
pub use rt::{promote_writer_thread_and_log, set_writer_rt_priority, RtOutcome};
pub use settings::AudioSettings;
pub use virtual_out::VirtualAudioOut;
pub use visualizer::{RingBuffer, TappedSource, VisualizerTap};

/// Stub: returns the ID unchanged on non-Linux (no ALSA normalization needed).
#[cfg(not(target_os = "linux"))]
pub fn normalize_device_id_to_stable(id: &str) -> String {
    id.to_string()
}

/// Stub: no ALSA device resolution on non-Linux.
#[cfg(not(target_os = "linux"))]
pub fn resolve_stable_to_current_hw(_stable: &str) -> Option<String> {
    None
}

/// Stub: no ALSA sample rate probing on non-Linux.
#[cfg(not(target_os = "linux"))]
pub fn device_supports_sample_rate(_device_id: &str, _sample_rate: u32) -> Option<bool> {
    None
}

/// Stub: no ALSA rate enumeration on non-Linux.
#[cfg(not(target_os = "linux"))]
pub fn get_device_supported_rates(_device_id: &str) -> Option<Vec<u32>> {
    None
}
