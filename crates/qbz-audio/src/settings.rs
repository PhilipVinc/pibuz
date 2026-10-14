//! Audio settings persistence
//!
//! Stores user preferences for audio output device, exclusive mode, and DAC passthrough.
//!
//! NOTE: Tauri command wrappers remain in qbz-nix. This module contains only
//! the core types and persistence logic.

use crate::{AlsaPlugin, AudioBackendType};
use qbz_models::settings_file::{self, Sections};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub output_device: Option<String>, // None = system default
    pub exclusive_mode: bool,
    pub dac_passthrough: bool,
    pub preferred_sample_rate: Option<u32>,     // None = auto
    pub backend_type: Option<AudioBackendType>, // None = auto-detect
    pub alsa_plugin: Option<AlsaPlugin>,        // Only used when backend is ALSA
    pub alsa_hardware_volume: bool,             // Use ALSA mixer for volume (only with hw: devices)
    /// ALSA control device carrying the DAC's mixer, e.g. "hw:0" or
    /// "hw:CARD=Modius". Empty means derive it from `output_device`, which is
    /// right whenever that names a card.
    ///
    /// Set it when the output goes through a virtual PCM that has no mixer of
    /// its own -- moOde's `_audioout`, a dmix, a user's `.asoundrc` chain --
    /// and `alsa_hardware_volume` would otherwise have no mixer to open.
    #[serde(default)]
    pub alsa_mixer_device: String,
    /// When true, uncached tracks start playing via streaming instead of waiting for full download
    pub stream_first_track: bool,
    /// Initial buffer size in seconds before starting streaming playback (1-10, default 3)
    pub stream_buffer_seconds: u8,
    /// Seconds of COMPRESSED audio the downloader may run ahead of playback
    /// before it is parked until the decoder catches up.
    ///
    /// This is the memory bound for a playing track. Without it the feeder
    /// downloads at link speed — measured at 4.5 MB/s against ~0.26 MB/s of
    /// 24/96 playback — and the whole compressed track, 120-220 MB at Hi-Res,
    /// is resident within seconds.
    ///
    /// In seconds rather than bytes because a byte constant means a different
    /// amount of music at every quality: OwnTone's 384 KB is 2.18 s at CD and
    /// 0.33 s at 24/192. `0` takes the default.
    #[serde(default = "default_stream_window_seconds")]
    pub stream_window_seconds: u8,
    /// When true, skip L1+L2 cache writes (streaming-only mode). Offline cache still works.
    pub streaming_only: bool,
    /// Hard budget for the L1 (in-memory) audio cache, in megabytes.
    ///
    /// `0` means auto: derive it from the host's RAM via
    /// `qbz_models::system_capabilities` (17 %, capped at 400 MB), which is
    /// right for every box we know of. An explicit value overrides that — for
    /// a headless renderer sharing a Pi with MPD and a web stack, where the
    /// integrator knows the budget better than a fraction does.
    ///
    /// Read once, when the player builds its cache: changing it needs a
    /// daemon restart.
    ///
    /// Note that a value below one track's size stops that track being cached
    /// at all (`AudioCache::insert` refuses anything over the cap), and gapless
    /// needs the NEXT track cached — so setting this under ~120 MB disables
    /// Hi-Res gapless, and under ~35 MB disables gapless entirely.
    #[serde(default)]
    pub memory_cache_mb: u16,
    /// How a volume percentage becomes an amplitude multiplier: `perceptual`
    /// (MPD's exponential mapping, the default) or `linear` (the fraction
    /// scales amplitude directly). See [`crate::volume_curve`] for why linear
    /// makes a slider that is unusable below its top tenth. Software volume
    /// only — an ALSA hardware mixer owns its own, already dB-scaled, mapping.
    #[serde(default = "default_volume_curve")]
    pub volume_curve: String,
    /// ALSA buffer length in milliseconds for the direct (bit-perfect) path.
    /// `0` keeps the rate-derived default (500 ms at 192 kHz+, 250 ms from
    /// 96 kHz, 125 ms below); anything else is clamped to 50-4000 ms.
    ///
    /// Worth raising on a host where the writer thread competes with WiFi, an
    /// SD card and a shared USB bus: an underrun there is an audible click and
    /// a `Recovered from PCM error` line. Costs a few MB of DMA buffer and adds
    /// latency no renderer listener can perceive.
    #[serde(default)]
    pub alsa_buffer_ms: u16,
    /// Milliseconds of silence to keep queued while there is nothing to play;
    /// `0` (the default) turns the keep-alive off.
    ///
    /// Between two tracks that are not a gapless hand-off — an album ending, a
    /// prefetch that has not finished, a pause — the PCM is drained and left
    /// stopped, and a DAC that hears its clock stop often answers with a click
    /// and a drop into standby, then another click when the next track starts
    /// it again. A floor of silence keeps the clock running through the gap.
    ///
    /// Shairport Sync ships the same thing as `disable_standby_mode` (silence
    /// fed whenever the output buffer falls below a threshold) and MPD as
    /// `always_on`. Both default it off, and so does this: it holds the device
    /// open, which keeps other clients off a shared card and a DAC out of the
    /// standby some users want.
    ///
    /// Keep it SMALL — 50 ms is plenty. It is a floor, not a target, but
    /// whatever silence is queued is latency the next track waits behind.
    ///
    /// **Known broken as it stands; leave it off.** Measured on a Pi at 50 ms
    /// against a 24/96 stream: `resolve_keepalive_depth_ms` widened it to 62 ms,
    /// the ALSA ring was 250 ms, and `stop_threshold` is left at the ring
    /// length — so the device drains the silence it was given, finds the buffer
    /// empty, and the driver calls that an underrun. Top up, start, drain,
    /// XRUN, once every 76 ms: 32 `Recovered from PCM error (keep-alive)` lines
    /// in two seconds, against one real PCM error in the whole run.
    ///
    /// Two things follow. The feature does not do its job — the clock is
    /// stopped and restarted ~13 times a second rather than held running — and
    /// it is worst for its own audience, since a DAC that clicks when the clock
    /// stops would click at that rate instead of once per gap. It also buries
    /// the genuine underruns this log line exists to surface.
    ///
    /// Any depth below the ring underruns by construction while `stop_threshold`
    /// is the whole ring, so the fix is in that relationship or in the top-up
    /// cadence — not in choosing a different number here. moOde deliberately
    /// does not offer this control for that reason.
    #[serde(default)]
    pub dac_keepalive_ms: u16,
    /// Milliseconds of DECODED audio to keep buffered ahead of the DAC; `0`
    /// (the default) takes the host's memory profile figure.
    ///
    /// Distinct from [`Self::alsa_buffer_ms`], and the more important of the
    /// two. That one sizes the hardware ring the DAC drains; this one sizes the
    /// ring the DECODER fills, which is what stands between a network stall and
    /// a click. Before it existed the hardware ring was the only cushion, and a
    /// WiFi hiccup longer than 125 ms at CD rate was audible.
    ///
    /// Clamped to 250-30000 ms, and never allowed below three times the ALSA
    /// ring (the writer would deadlock against a hardware queue deeper than its
    /// own supply). Costs `rate * channels * 4` bytes per second: 353 KB/s at
    /// 44.1 kHz stereo, 1.5 MB/s at 192 kHz.
    #[serde(default)]
    pub pcm_ring_ms: u32,
    /// `SCHED_FIFO` priority for the ALSA writer thread; `0` disables the
    /// promotion and leaves it at ordinary priority.
    ///
    /// Needs `LimitRTPRIO=` in the service unit (both generated units set 20).
    /// Without it the request returns `EPERM`, which is logged once and
    /// otherwise harmless — playback is unaffected, just more exposed to a
    /// scheduling delay becoming a click on a busy Pi.
    ///
    /// Clamped to 0-20. See `qbz_audio::rt` for why the default is as low as 5.
    #[serde(default = "default_writer_rt_priority")]
    pub writer_rt_priority: u8,
    /// Write cached tracks to the L2 disk cache.
    ///
    /// False keeps caching in memory only: gapless still works (the next track
    /// is held in RAM), and the card is never written. Only sane on a host with
    /// room for the current AND next track — a Hi-Res pair is ~450 MB — which is
    /// why moOde offers it above 2 GB and not below. `streaming_only` remains the
    /// setting that disables caching altogether.
    #[serde(default = "default_cache_to_disk")]
    pub cache_to_disk: bool,
    /// Hard budget for the L2 (on-disk) audio cache, in megabytes. Ignored
    /// entirely when [`Self::cache_to_disk`] is false — there is no L2 then.
    ///
    /// `0` means take [`DEFAULT_DISK_CACHE_MB`], which is what this was before
    /// it was a setting: a constant compiled into the player, invisible and
    /// unchangeable. That is the wrong shape for the one number that decides how
    /// much of the card this daemon occupies, on exactly the small boards that
    /// route their cache to disk because they have no RAM for it.
    ///
    /// The cache enforces it by LRU, and sweeps down to it at startup when the
    /// budget has been LOWERED since the last run, so shrinking it takes effect
    /// on the next daemon start rather than waiting for the next eviction.
    ///
    /// Read once, when the player builds its cache: changing it needs a daemon
    /// restart. Same floor logic as the L1 budget — a cap below one track's size
    /// means nothing is ever cached, so gapless from disk stops working under
    /// ~120 MB at Hi-Res.
    #[serde(default)]
    pub disk_cache_mb: u16,
    /// When true, cap the REQUESTED streaming quality tier at the local output
    /// device's detected ceiling (#638 fix 3; consumed by the desktop's
    /// request-time resolution, never by the audio backends). Applies to local
    /// playback only — never to casting, where the local DAC is not in the
    /// signal path. Default: false (opt-in).
    pub limit_quality_to_device: bool,
    /// Cached max sample rate of the selected device (set when device is selected)
    /// Used when limit_quality_to_device is true
    pub device_max_sample_rate: Option<u32>,
    /// Per-device sample rate limits: device_id -> max_sample_rate
    /// Allows different DACs to have independent max sample rate configurations
    #[serde(default)]
    pub device_sample_rate_limits: HashMap<String, u32>,
    /// When true, apply volume normalization using ReplayGain metadata.
    /// When false (default), the audio pipeline is 100% bit-perfect — no sample modification.
    pub normalization_enabled: bool,
    /// Target loudness in LUFS for normalization.
    /// Common values: -14.0 (Spotify/YouTube), -18.0 (audiophile), -23.0 (EBU broadcast)
    pub normalization_target_lufs: f32,
    /// When true, consecutive same-format tracks play without gap.
    /// Works on Rodio (PipeWire/Pulse) and ALSA Direct backends. Requires cached tracks.
    pub gapless_enabled: bool,
    /// When true, force PipeWire clock.force-quantum alongside clock.force-rate for bit-perfect.
    /// Reset both to 0 on stop. PipeWire-only, requires dac_passthrough.
    pub pw_force_bitperfect: bool,
    /// When true, reload audio settings from DB into the player on app startup.
    /// Useful when Player::new() may hold stale settings (e.g., after Flatpak updates).
    /// Default: false (most users don't need this).
    pub sync_audio_on_startup: bool,
    /// User preference for what happens when all quality retries fail.
    /// Values: "ask" (default), "always_fallback", "always_skip"
    /// Protected by ADR-003: must survive reset_all() and migrations.
    pub quality_fallback_behavior: String,
    /// When true, skip `pactl set-default-sink` on stream creation.
    /// Preserves external routing (JACK, qjackctl, Reaper).
    /// Mutually exclusive with dac_passthrough.
    pub skip_sink_switch: bool,
    /// When true, automatically try lower quality tiers if the requested one fails.
    /// When false (default), playback or download fails if the exact quality is unavailable.
    pub allow_quality_fallback: bool,
    /// When true, hold a per-process ALSA device reservation (Lifetime B) for the
    /// configured output device while QBZ is running, so other PulseAudio/PipeWire
    /// clients won't grab the DAC and break exclusive playback. Off by default.
    /// See `crate::device_reservation` for the protocol and the lifetime model.
    #[serde(default)]
    pub reserve_dac_while_running: bool,
}

/// Perceptual by default: it is what every other player on the host does, and a
/// linear amplitude fader is unusable above its bottom tenth
/// (see [`crate::volume_curve`]).
/// Caching to disk is the default: it is what makes gapless work on the small
/// players that are most of the fleet.
fn default_cache_to_disk() -> bool {
    true
}

/// The L2 disk budget used when [`AudioSettings::disk_cache_mb`] is `0`.
///
/// 800 MB because that is the figure this shipped with as a hard-coded literal,
/// and it is a reasonable middle: ~4 Hi-Res albums, comfortably inside the free
/// space on the 8 GB card that is the smallest moOde supports, and large enough
/// that ordinary listening rarely evicts. Making it a named constant rather than
/// a magic number at the construction site is half the point of the setting.
pub const DEFAULT_DISK_CACHE_MB: u16 = 800;

/// On by default. The promotion needs an rlimit the service unit grants, and
/// degrades to a single info line where it is absent, so defaulting it on costs
/// nothing on a host that cannot use it and is the right answer on every host
/// that can. See [`crate::rt`].
/// See [`AudioSettings::stream_window_seconds`]. Eight seconds is ~4.8 MB at
/// 24/192 and ~2.1 MB at 24/96 — the same order as ohPipeline's 1.5 MB
/// reservoir, which is a shipping renderer against this same CDN.
fn default_stream_window_seconds() -> u8 {
    8
}

fn default_writer_rt_priority() -> u8 {
    crate::rt::DEFAULT_WRITER_RT_PRIORITY
}

fn default_volume_curve() -> String {
    crate::volume_curve::VolumeCurve::Perceptual
        .as_key()
        .to_string()
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            output_device: None,
            exclusive_mode: false,
            dac_passthrough: false,
            preferred_sample_rate: None,
            // OOTB default is "System" (Some(SystemDefault)): play through the OS
            // default output, shared with other apps like any normal player — no
            // bit-perfect, no `pactl`. See AudioBackendType::default(). "Auto"
            // (None) and explicit backends (PipeWire / ALSA) are honored as-is;
            // this only sets what a fresh install and the Reset action land on.
            //
            // History: this defaulted to Some(PipeWire) on Linux to dodge a rodio
            // DeviceSink-drop-on-resume race on the CPAL path (#375), but that
            // hard-required `pactl` and froze OOTB playback without it (#470). The
            // #375 race is covered by the cpal 0.17.3 / alsa 0.11 stream-drop
            // fixes, and "System" is the app-like default audiophiles override.
            backend_type: Some(AudioBackendType::default()),
            alsa_plugin: Some(AlsaPlugin::Hw), // Default to hw (bit-perfect)
            alsa_hardware_volume: false,       // Disabled by default (maximum compatibility)
            alsa_mixer_device: String::new(),  // Empty = derive from output_device
            stream_first_track: true,          // On by default (opt-out)
            stream_buffer_seconds: 2,          // 2 seconds initial buffer
            stream_window_seconds: default_stream_window_seconds(),
            streaming_only: false, // Disabled by default (cache tracks for instant replay)
            memory_cache_mb: 0,    // 0 = auto-size from host RAM
            volume_curve: default_volume_curve(),
            alsa_buffer_ms: 0,   // 0 = derive from the sample rate
            dac_keepalive_ms: 0, // 0 = off, like shairport-sync and MPD default it
            pcm_ring_ms: 0,      // 0 = take the host memory profile's figure
            writer_rt_priority: default_writer_rt_priority(),
            cache_to_disk: default_cache_to_disk(),
            disk_cache_mb: 0,                          // 0 = DEFAULT_DISK_CACHE_MB
            limit_quality_to_device: false, // Opt-in. Off since 1.1.9 (#45); wired to the read-only probe in #638 fix 3
            device_max_sample_rate: None,   // Set when device is selected
            device_sample_rate_limits: HashMap::new(), // Per-device limits (empty = no limit)
            normalization_enabled: false,   // Off by default — preserves bit-perfect pipeline
            normalization_target_lufs: -14.0, // Spotify/YouTube standard
            gapless_enabled: true, // On by default — works for same-format tracks on all backends
            pw_force_bitperfect: false, // Off by default — experimental PipeWire feature
            sync_audio_on_startup: false, // Off by default — opt-in for stale-settings edge case
            quality_fallback_behavior: "ask".to_string(),
            skip_sink_switch: false, // Off by default — only for JACK/DAW routing setups
            allow_quality_fallback: false, // Off by default — fail rather than silently downgrade
            reserve_dac_while_running: false, // Off by default — opt-in DAC reservation (Lifetime B)
        }
    }
}

/// The `[audio]` section of `settings.toml` (see `qbz_models::settings_file`).
///
/// Holds a path, not a handle: every read goes to the file, so the next read
/// after a `pibuz settings set` from another process sees what it wrote.
pub struct AudioSettingsStore {
    root: PathBuf,
}

const SECTION: &str = "audio";

/// `backend_type` and `alsa_plugin` default to `Some`, and `None` means
/// "auto". TOML has no null and a key left out takes its default, so `None`
/// is written as this word instead of being left out.
const AUTO: &str = "auto";

impl AudioSettingsStore {
    /// The audio settings under `base_dir`. Creates nothing; the file appears
    /// on the first write. Folds in an `audio_settings.db` from before
    /// `settings.toml`, when there is one.
    pub fn new_at(base_dir: &Path) -> Result<Self, String> {
        legacy::migrate(base_dir);
        Ok(Self {
            root: base_dir.to_path_buf(),
        })
    }

    /// Whether anything has ever written audio settings here.
    pub fn has_settings(&self) -> bool {
        settings_file::has_section(&self.root, SECTION)
    }

    /// The current settings, or the defaults when none were ever written. An
    /// error when `settings.toml` is there but does not parse.
    pub fn get_settings(&self) -> Result<AudioSettings, String> {
        from_section(&settings_file::load(&self.root)?)
    }

    fn update(&self, f: impl FnOnce(&mut AudioSettings)) -> Result<(), String> {
        settings_file::update(&self.root, |sections| {
            let mut settings = from_section(sections)?;
            f(&mut settings);
            to_section(sections, &settings)
        })
    }

    pub fn set_output_device(&self, device: Option<&str>) -> Result<(), String> {
        self.update(|s| s.output_device = device.map(str::to_string))
    }

    pub fn set_exclusive_mode(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.exclusive_mode = enabled)
    }

    pub fn set_dac_passthrough(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.dac_passthrough = enabled)
    }

    pub fn set_sample_rate(&self, rate: Option<u32>) -> Result<(), String> {
        self.update(|s| s.preferred_sample_rate = rate)
    }

    /// Switching to ALSA with no plugin chosen picks `Hw`.
    pub fn set_backend_type(&self, backend: Option<AudioBackendType>) -> Result<(), String> {
        self.update(|s| {
            s.backend_type = backend;
            if backend == Some(AudioBackendType::Alsa) && s.alsa_plugin.is_none() {
                log::info!(
                    "[AudioSettings] ALSA backend selected with no plugin set, defaulting to Hw"
                );
                s.alsa_plugin = Some(AlsaPlugin::Hw);
            }
        })
    }

    pub fn set_alsa_plugin(&self, plugin: Option<AlsaPlugin>) -> Result<(), String> {
        self.update(|s| s.alsa_plugin = plugin)
    }

    pub fn set_alsa_hardware_volume(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.alsa_hardware_volume = enabled)
    }

    pub fn set_stream_first_track(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.stream_first_track = enabled)
    }

    /// Clamped to 1-10.
    pub fn set_stream_buffer_seconds(&self, seconds: u8) -> Result<(), String> {
        self.update(|s| s.stream_buffer_seconds = seconds.clamp(1, 10))
    }

    /// Read when the player builds its cache, so it takes effect on the next
    /// daemon start.
    pub fn set_disk_cache_mb(&self, mb: u16) -> Result<(), String> {
        self.update(|s| s.disk_cache_mb = mb)
    }

    /// Read when the player builds its cache, so it takes effect on the next
    /// daemon start.
    pub fn set_cache_to_disk(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.cache_to_disk = enabled)
    }

    /// The ALSA direct-path buffer length in ms; `0` restores the rate-derived
    /// default. Read when a stream opens, so it applies to the next track.
    pub fn set_alsa_buffer_ms(&self, ms: u16) -> Result<(), String> {
        self.update(|s| s.alsa_buffer_ms = ms)
    }

    /// Clamped 2..=60. Below two seconds a single WiFi hiccup empties the
    /// window; above a minute it stops being a window.
    pub fn set_stream_window_seconds(&self, seconds: u8) -> Result<(), String> {
        self.update(|s| s.stream_window_seconds = seconds.clamp(2, 60))
    }

    /// The decoded-audio ring depth in ms; `0` restores the host memory
    /// profile's figure. Read when a stream opens, so it applies to the next
    /// track.
    pub fn set_pcm_ring_ms(&self, ms: u32) -> Result<(), String> {
        self.update(|s| s.pcm_ring_ms = ms)
    }

    /// The ALSA writer thread's `SCHED_FIFO` priority; `0` turns the promotion
    /// off. Read once when the player starts, so it takes effect on the next
    /// daemon start.
    pub fn set_writer_rt_priority(&self, priority: u8) -> Result<(), String> {
        self.update(|s| s.writer_rt_priority = priority)
    }

    pub fn set_dac_keepalive_ms(&self, ms: u16) -> Result<(), String> {
        self.update(|s| s.dac_keepalive_ms = ms)
    }

    /// `perceptual` or `linear`. Read once when the player starts, so it
    /// takes effect on the next daemon start.
    pub fn set_volume_curve(&self, curve: &str) -> Result<(), String> {
        self.update(|s| s.volume_curve = curve.to_string())
    }

    /// The ALSA control device holding the DAC's mixer. Empty restores
    /// deriving it from the output device.
    pub fn set_alsa_mixer_device(&self, device: &str) -> Result<(), String> {
        self.update(|s| s.alsa_mixer_device = device.to_string())
    }

    /// The L1 audio-cache budget in MB. `0` restores auto-sizing.
    ///
    /// Clamped to 1024 MB: anything larger is a typo, and the L1 cache is not
    /// where a gigabyte of RAM belongs.
    pub fn set_memory_cache_mb(&self, megabytes: u16) -> Result<(), String> {
        self.update(|s| s.memory_cache_mb = megabytes.min(1024))
    }

    pub fn set_streaming_only(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.streaming_only = enabled)
    }

    pub fn set_limit_quality_to_device(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.limit_quality_to_device = enabled)
    }

    pub fn set_device_max_sample_rate(&self, rate: Option<u32>) -> Result<(), String> {
        self.update(|s| s.device_max_sample_rate = rate)
    }

    /// The sample rate limit for one device; `None` removes it.
    pub fn set_device_sample_rate_limit(
        &self,
        device_id: &str,
        rate: Option<u32>,
    ) -> Result<(), String> {
        self.update(|s| match rate {
            Some(r) => {
                s.device_sample_rate_limits.insert(device_id.to_string(), r);
            }
            None => {
                s.device_sample_rate_limits.remove(device_id);
            }
        })
    }

    /// The sample rate limit for one device, if it has one.
    pub fn get_device_sample_rate_limit(&self, device_id: &str) -> Result<Option<u32>, String> {
        Ok(self
            .get_settings()?
            .device_sample_rate_limits
            .get(device_id)
            .copied())
    }

    pub fn set_normalization_enabled(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.normalization_enabled = enabled)
    }

    pub fn set_gapless_enabled(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.gapless_enabled = enabled)
    }

    pub fn set_allow_quality_fallback(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.allow_quality_fallback = enabled)
    }

    pub fn set_skip_sink_switch(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.skip_sink_switch = enabled)
    }

    /// Persist the `reserve_dac_while_running` flag (Lifetime B from the
    /// ALSA exclusive-hardening design spec). Applying the change to the live
    /// `DeviceReservation` guard is the caller's responsibility.
    pub fn set_reserve_dac_while_running(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.reserve_dac_while_running = enabled)
    }

    pub fn set_pw_force_bitperfect(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.pw_force_bitperfect = enabled)
    }

    pub fn set_sync_audio_on_startup(&self, enabled: bool) -> Result<(), String> {
        self.update(|s| s.sync_audio_on_startup = enabled)
    }

    pub fn get_quality_fallback_behavior(&self) -> Result<String, String> {
        let settings = self.get_settings()?;
        let value = &settings.quality_fallback_behavior;
        match value.as_str() {
            "ask" | "always_fallback" | "always_skip" => Ok(value.clone()),
            _ => Ok("ask".to_string()),
        }
    }

    pub fn set_quality_fallback_behavior(&self, behavior: &str) -> Result<(), String> {
        match behavior {
            "ask" | "always_fallback" | "always_skip" => {}
            _ => return Err(format!("Invalid quality_fallback_behavior: {}", behavior)),
        }
        self.update(|s| s.quality_fallback_behavior = behavior.to_string())
    }

    pub fn set_normalization_target_lufs(&self, target: f32) -> Result<(), String> {
        self.update(|s| s.normalization_target_lufs = target)
    }

    /// Reset all audio settings to their default values.
    pub fn reset_all(&self) -> Result<AudioSettings, String> {
        // ADR-003: quality_fallback_behavior must survive reset_all()
        let saved_fallback = self
            .get_quality_fallback_behavior()
            .unwrap_or_else(|_| "ask".to_string());
        let result = AudioSettings {
            quality_fallback_behavior: saved_fallback,
            ..AudioSettings::default()
        };
        let stored = result.clone();
        self.update(move |s| *s = stored)?;
        Ok(result)
    }
}

/// `[audio]` as an [`AudioSettings`], with `auto` read back as `None`.
fn from_section(sections: &Sections) -> Result<AudioSettings, String> {
    let mut table: toml::Table = sections.get(SECTION)?;
    let backend_auto = take_auto(&mut table, "backend_type");
    let plugin_auto = take_auto(&mut table, "alsa_plugin");
    let mut settings: AudioSettings = toml::Value::Table(table)
        .try_into()
        .map_err(|e| format!("{} [{SECTION}]: {e}", sections.file().display()))?;
    if backend_auto {
        settings.backend_type = None;
    }
    if plugin_auto {
        settings.alsa_plugin = None;
    }
    Ok(settings)
}

fn take_auto(table: &mut toml::Table, key: &str) -> bool {
    let auto = table.get(key).and_then(toml::Value::as_str) == Some(AUTO);
    if auto {
        table.remove(key);
    }
    auto
}

/// Write `settings` as `[audio]`, with `None` spelled `auto`.
fn to_section(sections: &mut Sections, settings: &AudioSettings) -> Result<(), String> {
    let mut table = match toml::Value::try_from(settings) {
        Ok(toml::Value::Table(t)) => t,
        Ok(_) => unreachable!("a struct serializes to a table"),
        Err(e) => return Err(format!("[{SECTION}]: {e}")),
    };
    for (key, is_auto) in [
        ("backend_type", settings.backend_type.is_none()),
        ("alsa_plugin", settings.alsa_plugin.is_none()),
    ] {
        if is_auto {
            table.insert(key.to_string(), toml::Value::String(AUTO.to_string()));
        }
    }
    sections.set(SECTION, &table)
}

/// `audio_settings.db`, the store before `settings.toml`. Read once, by
/// [`AudioSettingsStore::new_at`], then deleted.
#[cfg(feature = "sqlite-migrate")]
mod legacy {
    use super::*;
    use rusqlite::types::Value;

    const DB_NAME: &str = "audio_settings.db";

    pub(super) fn migrate(root: &Path) {
        settings_file::migrate_legacy(root, &root.join(DB_NAME), SECTION, |db, sections| {
            to_section(sections, &read(db)?)
        });
    }

    /// The one row, column by column by NAME, over the defaults: a database
    /// last opened by an older version lacks the newer columns, and those
    /// keep their defaults.
    pub(super) fn read(db: &Path) -> Result<AudioSettings, String> {
        // Read-write, not read-only: a read-only open cannot replay a WAL that a
        // crashed writer left behind, and the file is deleted right after.
        let conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT * FROM audio_settings WHERE id = 1")
            .map_err(|e| e.to_string())?;
        let names: Vec<String> = stmt.column_names().iter().map(|n| n.to_string()).collect();
        let row: Vec<(String, Value)> = stmt
            .query_row([], |row| {
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| Ok((n.clone(), row.get::<_, Value>(i)?)))
                    .collect()
            })
            .map_err(|e| e.to_string())?;

        let mut s = AudioSettings::default();
        for (name, value) in row {
            apply(&mut s, &name, value);
        }
        Ok(s)
    }

    fn apply(s: &mut AudioSettings, name: &str, value: Value) {
        let int = || match &value {
            Value::Integer(i) => Some(*i),
            _ => None,
        };
        let text = || match &value {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        };
        let flag = |field: &mut bool| {
            if let Some(i) = int() {
                *field = i != 0;
            }
        };
        match name {
            "output_device" => s.output_device = text(),
            "exclusive_mode" => flag(&mut s.exclusive_mode),
            "dac_passthrough" => flag(&mut s.dac_passthrough),
            "preferred_sample_rate" => s.preferred_sample_rate = int().map(|r| r as u32),
            // NULL is "auto" for both, and stays None.
            "backend_type" => {
                s.backend_type = text().and_then(|j| serde_json::from_str(&j).ok());
            }
            "alsa_plugin" => s.alsa_plugin = text().and_then(|j| serde_json::from_str(&j).ok()),
            "alsa_hardware_volume" => flag(&mut s.alsa_hardware_volume),
            "alsa_mixer_device" => s.alsa_mixer_device = text().unwrap_or_default(),
            "stream_first_track" => flag(&mut s.stream_first_track),
            "stream_buffer_seconds" => {
                if let Some(i) = int() {
                    s.stream_buffer_seconds = i as u8;
                }
            }
            "stream_window_seconds" => {
                if let Some(i) = int() {
                    s.stream_window_seconds = i as u8;
                }
            }
            "streaming_only" => flag(&mut s.streaming_only),
            "memory_cache_mb" => {
                if let Some(i) = int() {
                    s.memory_cache_mb = i as u16;
                }
            }
            "volume_curve" => {
                if let Some(t) = text().filter(|t| !t.is_empty()) {
                    s.volume_curve = t;
                }
            }
            "alsa_buffer_ms" => {
                if let Some(i) = int() {
                    s.alsa_buffer_ms = i as u16;
                }
            }
            "dac_keepalive_ms" => {
                if let Some(i) = int() {
                    s.dac_keepalive_ms = i as u16;
                }
            }
            "pcm_ring_ms" => {
                if let Some(i) = int() {
                    s.pcm_ring_ms = i as u32;
                }
            }
            "writer_rt_priority" => {
                if let Some(i) = int() {
                    s.writer_rt_priority = i as u8;
                }
            }
            "cache_to_disk" => flag(&mut s.cache_to_disk),
            "disk_cache_mb" => {
                if let Some(i) = int() {
                    s.disk_cache_mb = i as u16;
                }
            }
            "limit_quality_to_device" => flag(&mut s.limit_quality_to_device),
            "device_max_sample_rate" => s.device_max_sample_rate = int().map(|r| r as u32),
            "device_sample_rate_limits" => {
                s.device_sample_rate_limits = text()
                    .and_then(|j| serde_json::from_str(&j).ok())
                    .unwrap_or_default();
            }
            "normalization_enabled" => flag(&mut s.normalization_enabled),
            "normalization_target_lufs" => match value {
                Value::Real(r) => s.normalization_target_lufs = r as f32,
                Value::Integer(i) => s.normalization_target_lufs = i as f32,
                _ => {}
            },
            "gapless_enabled" => flag(&mut s.gapless_enabled),
            "pw_force_bitperfect" => flag(&mut s.pw_force_bitperfect),
            "sync_audio_on_startup" => flag(&mut s.sync_audio_on_startup),
            "quality_fallback_behavior" => {
                if let Some(t) = text() {
                    s.quality_fallback_behavior = t;
                }
            }
            "skip_sink_switch" => flag(&mut s.skip_sink_switch),
            "allow_quality_fallback" => flag(&mut s.allow_quality_fallback),
            "reserve_dac_while_running" => flag(&mut s.reserve_dac_while_running),
            _ => {}
        }
    }
}

/// Without the `sqlite-migrate` feature an old `audio_settings.db` cannot be
/// read. Say so rather than silently starting from defaults, and leave the
/// file for a build that can.
#[cfg(not(feature = "sqlite-migrate"))]
mod legacy {
    use std::path::Path;

    pub(super) fn migrate(root: &Path) {
        let db = root.join("audio_settings.db");
        if db.exists() && !qbz_models::settings_file::has_section(root, super::SECTION) {
            log::warn!(
                "[settings] {} is from an older pibuz and this build cannot read it \
                 (built without sqlite-migrate); audio settings are at their defaults",
                db.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("qbz-audio-{name}-{}-{nonce}", std::process::id()))
    }

    fn fresh_store(name: &str) -> (std::path::PathBuf, AudioSettingsStore) {
        let dir = unique_test_dir(name);
        let store = AudioSettingsStore::new_at(&dir).expect("open store in temp dir");
        (dir, store)
    }

    #[test]
    fn audio_settings_default_values_are_stable() {
        let settings = AudioSettings::default();

        // OOTB default is "System" — the OS default output (#470).
        assert_eq!(settings.backend_type, Some(AudioBackendType::SystemDefault));
        assert_eq!(settings.alsa_plugin, Some(AlsaPlugin::Hw));
        assert!(settings.gapless_enabled);
        assert!(!settings.sync_audio_on_startup);
        assert_eq!(settings.quality_fallback_behavior, "ask");
        assert!(!settings.skip_sink_switch);
        assert!(!settings.allow_quality_fallback);
        assert!(!settings.reserve_dac_while_running);
    }

    #[test]
    fn audio_settings_store_returns_current_defaults() {
        let (dir, store) = fresh_store("defaults");

        let settings = store.get_settings().expect("get settings");

        // A store nothing has written to IS the defaults: the OOTB backend
        // "System" (#470), and the struct's own answer for every field. The
        // SQLite store used to disagree with the struct on two of them.
        assert_eq!(settings.backend_type, Some(AudioBackendType::SystemDefault));
        assert_eq!(settings.alsa_plugin, Some(AlsaPlugin::Hw));
        assert!(settings.gapless_enabled);
        assert_eq!(settings.quality_fallback_behavior, "ask");
        assert!(!settings.reserve_dac_while_running);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn backend_null_stays_auto_on_reopen() {
        // A NULL backend_type means "Auto" (system default output). It must be
        // preserved across restarts — never backfilled to a concrete backend on
        // store open (the old #375 backfill hard-coded PipeWire and froze OOTB
        // playback on hosts without `pactl`, #470). Only the explicit Reset
        // action rewrites settings.
        let dir = unique_test_dir("backend-null-auto");
        {
            let store = AudioSettingsStore::new_at(&dir).expect("open store");
            store.set_backend_type(None).expect("Auto backend");
            store.set_alsa_plugin(None).expect("Auto plugin");
        }
        // Spelled out, because a key left out of the file takes its default,
        // and the default is not None.
        let text = std::fs::read_to_string(settings_file::path(&dir)).unwrap();
        assert!(text.contains("backend_type = \"auto\""), "{text}");
        assert!(text.contains("alsa_plugin = \"auto\""), "{text}");

        let reopened = AudioSettingsStore::new_at(&dir).expect("reopen store");
        let settings = reopened.get_settings().expect("get settings");

        assert_eq!(settings.backend_type, None);
        assert_eq!(settings.alsa_plugin, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn switching_to_alsa_sets_default_plugin_when_missing() {
        let (dir, store) = fresh_store("alsa-plugin-default");
        store.set_alsa_plugin(None).expect("clear alsa plugin");

        store
            .set_backend_type(Some(AudioBackendType::Alsa))
            .expect("set backend");
        let settings = store.get_settings().expect("get settings");

        assert_eq!(settings.backend_type, Some(AudioBackendType::Alsa));
        assert_eq!(settings.alsa_plugin, Some(AlsaPlugin::Hw));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stream_buffer_seconds_clamps_to_valid_range() {
        let (dir, store) = fresh_store("stream-buffer-clamp");

        store.set_stream_buffer_seconds(0).expect("set low buffer");
        assert_eq!(
            store
                .get_settings()
                .expect("get settings")
                .stream_buffer_seconds,
            1
        );

        store
            .set_stream_buffer_seconds(99)
            .expect("set high buffer");
        assert_eq!(
            store
                .get_settings()
                .expect("get settings")
                .stream_buffer_seconds,
            10
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quality_fallback_invalid_value_reads_as_ask() {
        let (dir, store) = fresh_store("quality-invalid");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            settings_file::path(&dir),
            "[audio]\nquality_fallback_behavior = \"bad-value\"\n",
        )
        .expect("write invalid fallback behavior");

        assert_eq!(
            store
                .get_quality_fallback_behavior()
                .expect("get quality fallback"),
            "ask"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reset_all_preserves_quality_fallback_behavior() {
        let (dir, store) = fresh_store("reset-preserves-quality");
        store
            .set_quality_fallback_behavior("always_skip")
            .expect("set quality fallback");
        store.set_output_device(Some("hw:4,0")).expect("set device");
        store.set_dac_passthrough(true).expect("set dac");

        let reset = store.reset_all().expect("reset settings");
        let settings = store.get_settings().expect("get settings");

        assert_eq!(reset.quality_fallback_behavior, "always_skip");
        assert_eq!(settings.quality_fallback_behavior, "always_skip");
        assert_eq!(settings.output_device, None);
        assert!(!settings.dac_passthrough);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_persist_and_reopen_all_new_fields() {
        let dir = unique_test_dir("persist-new-fields");
        {
            let store = AudioSettingsStore::new_at(&dir).expect("open store");
            store
                .set_sync_audio_on_startup(true)
                .expect("set sync flag");
            store
                .set_quality_fallback_behavior("always_fallback")
                .expect("set quality fallback");
            store
                .set_reserve_dac_while_running(true)
                .expect("set reserve flag");
            store
                .set_allow_quality_fallback(true)
                .expect("set allow fallback");
            store
                .set_skip_sink_switch(true)
                .expect("set skip sink switch");
        }

        let reopened = AudioSettingsStore::new_at(&dir).expect("reopen store");
        let settings = reopened.get_settings().expect("get settings");

        assert!(settings.sync_audio_on_startup);
        assert_eq!(settings.quality_fallback_behavior, "always_fallback");
        assert!(settings.reserve_dac_while_running);
        assert!(settings.allow_quality_fallback);
        assert!(settings.skip_sink_switch);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deserializes_legacy_json_without_reserve_dac_field() {
        let legacy = r#"{
            "output_device": null,
            "exclusive_mode": false,
            "dac_passthrough": false,
            "preferred_sample_rate": null,
            "backend_type": null,
            "alsa_plugin": null,
            "alsa_hardware_volume": false,
            "stream_first_track": false,
            "stream_buffer_seconds": 3,
            "streaming_only": false,
            "limit_quality_to_device": false,
            "device_max_sample_rate": null,
            "normalization_enabled": false,
            "normalization_target_lufs": -14.0,
            "gapless_enabled": true,
            "pw_force_bitperfect": false,
            "sync_audio_on_startup": false,
            "quality_fallback_behavior": "ask",
            "skip_sink_switch": false,
            "allow_quality_fallback": false
        }"#;

        let settings: AudioSettings =
            serde_json::from_str(legacy).expect("legacy JSON should deserialize");

        assert!(!settings.reserve_dac_while_running);
    }
    /// An `audio_settings.db` as an older pibuz left it: only some of the
    /// columns later versions added, in WAL mode.
    #[cfg(feature = "sqlite-migrate")]
    fn plant_legacy_db(dir: &Path, insert: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let db = dir.join("audio_settings.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE audio_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                output_device TEXT,
                exclusive_mode INTEGER NOT NULL DEFAULT 0,
                dac_passthrough INTEGER NOT NULL DEFAULT 0,
                preferred_sample_rate INTEGER,
                backend_type TEXT,
                alsa_plugin TEXT,
                alsa_hardware_volume INTEGER NOT NULL DEFAULT 0,
                stream_first_track INTEGER NOT NULL DEFAULT 1,
                stream_buffer_seconds INTEGER NOT NULL DEFAULT 2,
                alsa_mixer_device TEXT DEFAULT '',
                gapless_enabled INTEGER DEFAULT 1,
                normalization_target_lufs REAL DEFAULT -14.0,
                disk_cache_mb INTEGER DEFAULT 0
             );
             {insert}"
        ))
        .unwrap();
        db
    }

    #[cfg(feature = "sqlite-migrate")]
    #[test]
    fn a_legacy_database_is_migrated_into_settings_toml_and_deleted() {
        let dir = unique_test_dir("legacy-migrate");
        let db = plant_legacy_db(
            &dir,
            "INSERT INTO audio_settings (id, output_device, backend_type, alsa_plugin,
                alsa_hardware_volume, stream_buffer_seconds, alsa_mixer_device,
                gapless_enabled, normalization_target_lufs, disk_cache_mb)
             VALUES (1, '_audioout', '\"Alsa\"', '\"Hw\"', 1, 5, 'hw:0', 0, -18.0, 2000);",
        );

        let settings = AudioSettingsStore::new_at(&dir)
            .unwrap()
            .get_settings()
            .unwrap();
        assert_eq!(settings.output_device.as_deref(), Some("_audioout"));
        assert_eq!(settings.backend_type, Some(AudioBackendType::Alsa));
        assert_eq!(settings.alsa_plugin, Some(AlsaPlugin::Hw));
        assert!(settings.alsa_hardware_volume);
        assert_eq!(settings.stream_buffer_seconds, 5);
        assert_eq!(settings.alsa_mixer_device, "hw:0");
        assert!(!settings.gapless_enabled);
        assert_eq!(settings.normalization_target_lufs, -18.0);
        assert_eq!(settings.disk_cache_mb, 2000);
        // Columns this database never had keep their defaults.
        let defaults = AudioSettings::default();
        assert_eq!(
            settings.stream_window_seconds,
            defaults.stream_window_seconds
        );
        assert_eq!(settings.writer_rt_priority, defaults.writer_rt_priority);
        assert_eq!(settings.cache_to_disk, defaults.cache_to_disk);

        for suffix in ["", "-wal", "-shm"] {
            assert!(
                !dir.join(format!("audio_settings.db{suffix}")).exists(),
                "audio_settings.db{suffix} must be gone"
            );
        }
        assert!(!db.exists());
        // And it is now the file that answers.
        let reopened = AudioSettingsStore::new_at(&dir).unwrap();
        assert_eq!(reopened.get_settings().unwrap().disk_cache_mb, 2000);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(feature = "sqlite-migrate")]
    #[test]
    fn a_legacy_null_backend_and_plugin_stay_auto() {
        // A NULL backend_type meant "Auto", and a fresh SQLite store left
        // alsa_plugin NULL. Migrating must not turn either into the default.
        let dir = unique_test_dir("legacy-auto");
        plant_legacy_db(&dir, "INSERT INTO audio_settings (id) VALUES (1);");

        let settings = AudioSettingsStore::new_at(&dir)
            .unwrap()
            .get_settings()
            .unwrap();
        assert_eq!(settings.backend_type, None);
        assert_eq!(settings.alsa_plugin, None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
