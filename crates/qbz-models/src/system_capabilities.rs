//! System capability detection.
//!
//! Probes the host environment at startup to derive a runtime profile that
//! tunes resource-heavy behaviors (prefetch depth, streaming buffer size,
//! prefetch quality cap) for memory-constrained machines like the
//! Raspberry Pi 3B (1 GB RAM, issue #331).
//!
//! Detection is one-shot, cached in a `OnceLock`, and pure once given the
//! `/proc/meminfo` contents — making it trivial to test by passing
//! synthetic input.

use std::sync::OnceLock;

/// Memory class bucket the runtime adapts behavior to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryClass {
    /// >= 2 GB RAM. Default behavior, no caps applied.
    Normal,
    /// < 2 GB RAM. Reduces prefetch and buffer footprints to keep room
    /// for the WebView and avoid swap thrash on Raspberry Pi-class
    /// devices.
    LowMemory,
}

/// Derived runtime profile applied to memory-sensitive subsystems.
#[derive(Debug, Clone, Copy)]
pub struct MemoryProfile {
    pub class: MemoryClass,
    pub mem_total_kb: u64,
    /// How many upcoming Qobuz tracks to prefetch. Hi-Res tracks are
    /// ~60 MB each held in memory, so this is the dominant source of
    /// RSS growth during normal playback.
    pub prefetch_count: usize,
    /// Maximum allowed initial streaming buffer size in bytes. Caps the
    /// dynamic-buffer growth that `from_speed_mbps` would otherwise
    /// inflate to 2 MB on slow connections — exactly the wrong direction
    /// on a memory-pressured Pi where slow downloads are themselves a
    /// symptom of swap thrash.
    pub max_initial_buffer_bytes: usize,
    /// Concurrency cap for prefetch downloads.
    pub max_concurrent_prefetch: usize,
    /// When false, prefetch downgrades from HiRes/UltraHiRes to Lossless
    /// (44.1 kHz / 16-bit FLAC) so each cached track stays under ~15 MB
    /// instead of ~60 MB.
    pub allow_hires_prefetch: bool,
    /// Upper bound for the L1 (in-memory) audio cache. The 400 MB figure
    /// this is capped at is sized for Normal-class desktops; on a Pi 3B
    /// (1 GB total) that single subsystem could consume 40 % of RAM, which
    /// guarantees swap thrash before the watchdog can react. Scaled by
    /// [`l1_cache_bytes_for_total_kb`] so a 2 GB Pi 5 — nominally
    /// Normal-class — does not get a desktop's cache either.
    pub audio_cache_l1_max_bytes: usize,
    /// Whether this host may prefetch the NEXT track at all — i.e. whether
    /// gapless is possible here. See [`GAPLESS_MIN_TOTAL_KB`].
    pub allow_gapless_prefetch: bool,
    /// Seconds of DECODED audio to hold between the decoder and the DAC.
    ///
    /// This is the renderer's entire tolerance for a stall — a WiFi dropout, an
    /// SD-card seek, a slow FLAC frame. Anything longer than this is an audible
    /// click, so the number is the reliability budget, not a tuning knob.
    ///
    /// It is cheap where it matters. The ring holds interleaved `f32`, so a
    /// second costs `rate * channels * 4` bytes: 353 KB at 44.1 kHz stereo and
    /// 1.5 MB at 192 kHz. Six seconds of Hi-Res is ~9 MB, against the ~120-220 MB
    /// the same track already occupies as compressed bytes — a rounding error on
    /// a board that can play Hi-Res at all.
    ///
    /// Two seconds on a small board rather than six for the same reason
    /// `allow_hires_prefetch` is false there: a 512 MB Pi has no 9 MB to spare,
    /// and two seconds still covers every stall short of a genuine network
    /// outage. Reference points: squeezelite's output buffer is ~10 s of CD
    /// audio, MPD's decoded-chunk pipe a few seconds.
    pub pcm_ring_seconds: u8,
    /// Ceiling for the COMPRESSED streaming window, in bytes.
    ///
    /// The window is normally derived from the track's own byte-rate in
    /// seconds (`audio.stream_window_seconds`), which is the only denomination
    /// that means the same thing at 16/44 and 24/192. This caps what that
    /// derivation may ask for on a small board, where a long window on a
    /// high-bitrate track would reintroduce the problem it exists to solve.
    ///
    /// Unlike `pcm_ring_seconds` this is a ceiling rather than a target: it
    /// binds only when the track's bitrate is high enough for the requested
    /// seconds to exceed it. At 8 s, 24/192 asks for ~4.8 MB and never reaches
    /// even the low-memory cap.
    pub stream_window_max_bytes: usize,
}

/// Least RAM a host needs before it may hold a second whole track.
///
/// A prefetch is not a cache decision, it is an allocation: `cmaf::download_full`
/// returns the ENTIRE track as a `Vec<u8>`, and the L1 budget, the disk staging
/// and the timing all happen downstream of it. At the default 24/192 (~42 MB per
/// minute) a five-minute track is ~210 MB, and it lands beside the playing
/// track's own full buffer. On a 512 MB board — which reports ~439 MB — that is
/// the OOM killer, reported on the moOde forum as a Pi 3A rebooting mid-album.
/// Nothing stood in its way: the `allow_hires_prefetch` flag written for this
/// is referenced only by a log line, and the memory watchdog was never built —
/// its `MemoryPressure` snapshot and `read_memory_pressure` sat here with no
/// caller until they were deleted.
///
/// So the small boards do not prefetch. Playback still advances at the end of a
/// track through the ordinary next-track path; it just has a gap, which is the
/// right trade against a reboot.
///
/// 768 MiB rather than a round 512 MB, for the same reason [`NORMAL_FLOOR_KB`]
/// is 1.75 GiB: a board reports well under its sticker once the kernel and GPU
/// have taken their reservations. A 512 MB Pi 3A/Zero 2 W reports ~439 MB and a
/// 1 GB Pi 3B reports ~905 MB, so the floor sits between them and separates the
/// boards, not the marketing numbers. The 1 GB board is the one gapless is
/// verified on.
pub const GAPLESS_MIN_TOTAL_KB: u64 = 768 * 1024;

/// Share of physical RAM the L1 audio cache may occupy, and the ceiling it is
/// capped at.
///
/// A flat 400 MB was the old value at every size: ~40 % of a Pi 3B+ and ~20 %
/// of a Pi 5 2 GB, both reported on the moOde forum as swapping during ordinary
/// playback. One fraction covers every class — the point is to leave the rest of
/// the box (MPD, nginx, php-fpm, the page cache) the room it needs, which is a
/// proportional question, not a per-class one.
///
/// 17 % rather than a rounder number because of where it lands on real
/// hardware: ~150 MB on a 1 GB Pi 3B+ and ~75 MB on a 512 MB Pi 3A. 150 MB is
/// the number that matters — it still fits one Hi-Res track (~120 MB), so
/// gapless keeps working on a 1 GB box instead of silently degrading, and
/// `AudioCache::insert` refuses anything larger than the cap.
///
/// The CEILING is a separate question from the fraction, and 400 MB was too low
/// for it. It binds only above ~2.35 GB of RAM — a 4 GB Pi's 17 % share is
/// 644 MB — and what it cut into there was room the box plainly had: a 4 GB
/// player sits at ~410 MB used with 2.9 GB free while streaming Hi-Res. It also
/// sat BELOW what `audio.memory_cache_mb` already lets anyone set by hand
/// (clamped at 1024 MB), so the automatic answer was the conservative one.
///
/// 512 MB holds the pair the cache actually exists to hold — the playing track
/// and the prefetched next one, ~220 MB each at Hi-Res — with headroom, and
/// still stops a 32 GB desktop from handing 5 GB to audio bytes. Nothing changes
/// at 2 GB or below, where the fraction binds first.
const L1_CACHE_RAM_FRACTION_PCT: u64 = 17;
const L1_CACHE_MAX_BYTES: usize = 512 * 1024 * 1024;

/// L1 audio-cache budget for a host with `mem_total_kb` of RAM.
pub fn l1_cache_bytes_for_total_kb(mem_total_kb: u64) -> usize {
    let share = mem_total_kb
        .saturating_mul(1024)
        .saturating_mul(L1_CACHE_RAM_FRACTION_PCT)
        / 100;
    usize::try_from(share)
        .unwrap_or(L1_CACHE_MAX_BYTES)
        .min(L1_CACHE_MAX_BYTES)
}

/// Decoded-audio ring depth per class. See [`MemoryProfile::pcm_ring_seconds`].
const PCM_RING_SECONDS_NORMAL: u8 = 6;
const PCM_RING_SECONDS_LOW_MEMORY: u8 = 2;

/// Compressed-window ceilings per class. See
/// [`MemoryProfile::stream_window_max_bytes`].
///
/// For scale: ohPipeline — Linn's shipping renderer, against this same CDN,
/// on embedded hardware over WiFi — runs a 1.5 MB encoded reservoir. Eight MB
/// on a 512 MB board is already generous; 32 MB where RAM allows buys a longer
/// ride through a WiFi dropout at a cost that board will not notice.
const STREAM_WINDOW_MAX_NORMAL: usize = 32 * 1024 * 1024;
const STREAM_WINDOW_MAX_LOW_MEMORY: usize = 8 * 1024 * 1024;

impl MemoryProfile {
    /// Derive the profile from a total-memory figure (KB).
    fn from_total_kb(mem_total_kb: u64) -> Self {
        // Threshold: 1.75 GiB, not 2 GiB. A board sold as "2 GB" reports
        // MemTotal slightly BELOW 2 GiB once the kernel and GPU have taken
        // their reservations (a Pi 5 2 GB lands around 1.9 GiB), so a literal
        // 2 GiB floor put real 2 GB hardware in the LowMemory class — and
        // with it an L1 budget smaller than a single Hi-Res track, which
        // stops that track being cached at all and silently kills gapless.
        // 1.75 GiB separates 1 GB boards from 2 GB boards, which is what the
        // split is actually for.
        const NORMAL_FLOOR_KB: u64 = 1792 * 1024;

        if mem_total_kb >= NORMAL_FLOOR_KB {
            Self {
                class: MemoryClass::Normal,
                mem_total_kb,
                prefetch_count: 5,
                max_initial_buffer_bytes: 2 * 1024 * 1024,
                max_concurrent_prefetch: 2,
                allow_hires_prefetch: true,
                audio_cache_l1_max_bytes: l1_cache_bytes_for_total_kb(mem_total_kb),
                allow_gapless_prefetch: mem_total_kb >= GAPLESS_MIN_TOTAL_KB,
                pcm_ring_seconds: PCM_RING_SECONDS_NORMAL,
                stream_window_max_bytes: STREAM_WINDOW_MAX_NORMAL,
            }
        } else {
            Self {
                class: MemoryClass::LowMemory,
                mem_total_kb,
                prefetch_count: 1,
                max_initial_buffer_bytes: 256 * 1024,
                max_concurrent_prefetch: 1,
                allow_hires_prefetch: false,
                audio_cache_l1_max_bytes: l1_cache_bytes_for_total_kb(mem_total_kb),
                allow_gapless_prefetch: mem_total_kb >= GAPLESS_MIN_TOTAL_KB,
                pcm_ring_seconds: PCM_RING_SECONDS_LOW_MEMORY,
                stream_window_max_bytes: STREAM_WINDOW_MAX_LOW_MEMORY,
            }
        }
    }
}

/// Parse the `MemTotal:` line out of `/proc/meminfo` content.
/// Returns None if the field is missing or unparseable.
pub fn parse_meminfo_total_kb(content: &str) -> Option<u64> {
    parse_meminfo_field_kb(content, "MemTotal:")
}

/// Shared parser for `<Field>: <number> kB` style /proc/meminfo lines.
fn parse_meminfo_field_kb(content: &str, field_prefix: &str) -> Option<u64> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(field_prefix) {
            let kb_str = rest.split_whitespace().next()?;
            return kb_str.parse::<u64>().ok();
        }
    }
    None
}

/// Pure detection given `/proc/meminfo` content. Falls back to Normal
/// when MemTotal is missing or unparseable so we never accidentally
/// throttle a system whose meminfo we couldn't read.
pub fn detect_profile_from_meminfo(content: &str) -> MemoryProfile {
    parse_meminfo_total_kb(content)
        .map(MemoryProfile::from_total_kb)
        .unwrap_or_else(|| MemoryProfile::from_total_kb(u64::MAX))
}

/// Read `/proc/meminfo` and derive the profile. Returns the Normal-fallback
/// profile on platforms without `/proc/meminfo` (macOS, Windows) or when
/// the file is unreadable for any reason.
fn detect_profile() -> MemoryProfile {
    match std::fs::read_to_string("/proc/meminfo") {
        Ok(content) => detect_profile_from_meminfo(&content),
        Err(_) => MemoryProfile::from_total_kb(u64::MAX),
    }
}

/// Process-wide cached profile. Detection runs once on first access.
static PROFILE: OnceLock<MemoryProfile> = OnceLock::new();

/// Return the cached memory profile, running detection on first call.
/// Logs the resolved profile at info level on the initial detection.
pub fn memory_profile() -> &'static MemoryProfile {
    PROFILE.get_or_init(|| {
        let profile = detect_profile();
        match profile.class {
            MemoryClass::LowMemory => {
                log::info!(
                    "[system] Low-memory profile active: {} MB total RAM, prefetch={}, max_initial_buffer={}KB, audio_cache_l1={}MB, hires_prefetch=disabled",
                    profile.mem_total_kb / 1024,
                    profile.prefetch_count,
                    profile.max_initial_buffer_bytes / 1024,
                    profile.audio_cache_l1_max_bytes / (1024 * 1024),
                );
            }
            MemoryClass::Normal => {
                log::info!(
                    "[system] Normal memory profile: {} MB total RAM, audio_cache_l1={}MB",
                    profile.mem_total_kb / 1024,
                    profile.audio_cache_l1_max_bytes / (1024 * 1024)
                );
            }
        }
        profile
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring is the stall budget, so the small board must still get a real
    /// one — and the big board must not get so much that the allocation itself
    /// becomes the memory problem.
    #[test]
    fn the_decoded_ring_is_sized_by_class_and_stays_cheap() {
        let pi3a = MemoryProfile::from_total_kb(439 * 1024);
        let pi5 = MemoryProfile::from_total_kb(4 * 1024 * 1024);
        assert_eq!(pi3a.class, MemoryClass::LowMemory);
        assert_eq!(pi5.class, MemoryClass::Normal);
        assert!(
            pi3a.pcm_ring_seconds >= 2,
            "a small board still needs a cushion"
        );
        assert!(pi3a.pcm_ring_seconds < pi5.pcm_ring_seconds);

        // The worst case the ring is ever asked to hold: 24/192 stereo f32.
        let bytes = |secs: u8| usize::from(secs) * 192_000 * 2 * 4;
        assert!(
            bytes(pi3a.pcm_ring_seconds) <= 4 * 1024 * 1024,
            "LowMemory ring is {} bytes at 192 kHz",
            bytes(pi3a.pcm_ring_seconds)
        );
        assert!(
            bytes(pi5.pcm_ring_seconds) <= 16 * 1024 * 1024,
            "Normal ring is {} bytes at 192 kHz",
            bytes(pi5.pcm_ring_seconds)
        );
    }

    #[test]
    fn parse_meminfo_total_kb_extracts_value() {
        let sample = "\
MemTotal:         938196 kB
MemFree:          120000 kB
Buffers:           48000 kB
";
        assert_eq!(parse_meminfo_total_kb(sample), Some(938196));
    }

    #[test]
    fn parse_meminfo_total_kb_ignores_other_fields() {
        let sample = "\
MemFree:          120000 kB
MemTotal:        4194304 kB
SwapTotal:       2097152 kB
";
        assert_eq!(parse_meminfo_total_kb(sample), Some(4194304));
    }

    #[test]
    fn parse_meminfo_total_kb_handles_missing_field() {
        let sample = "\
MemFree:          120000 kB
SwapTotal:       2097152 kB
";
        assert_eq!(parse_meminfo_total_kb(sample), None);
    }

    #[test]
    fn parse_meminfo_total_kb_handles_empty_input() {
        assert_eq!(parse_meminfo_total_kb(""), None);
    }

    #[test]
    fn pi3b_with_1gb_resolves_to_low_memory() {
        // Raspberry Pi 3B = 1 GB RAM = ~938196 kB after kernel reservations.
        let profile = MemoryProfile::from_total_kb(938196);
        assert_eq!(profile.class, MemoryClass::LowMemory);
        assert_eq!(profile.prefetch_count, 1);
        assert_eq!(profile.max_concurrent_prefetch, 1);
        assert!(!profile.allow_hires_prefetch);
        assert!(profile.max_initial_buffer_bytes <= 256 * 1024);
        // L1 cap must be a small fraction of total RAM, not the four-tenths
        // the old flat 400 MB reserved on a 1 GB host — but big enough to
        // still hold one Hi-Res track, or gapless dies here.
        assert!(profile.audio_cache_l1_max_bytes <= 160 * 1024 * 1024);
        assert!(profile.audio_cache_l1_max_bytes >= 140 * 1024 * 1024);
    }

    #[test]
    fn l1_cache_scales_with_ram_and_stays_under_the_ceiling() {
        // Pi 5 2 GB: Normal-class, but must not get a desktop's 400 MB.
        let pi5 = MemoryProfile::from_total_kb(2 * 1024 * 1024);
        assert_eq!(
            pi5.audio_cache_l1_max_bytes,
            2 * 1024 * 1024 * 1024_usize * 17 / 100
        );
        assert!(pi5.audio_cache_l1_max_bytes < 512 * 1024 * 1024);
        // 4 GB and up saturate at the ceiling.
        assert_eq!(
            MemoryProfile::from_total_kb(4 * 1024 * 1024).audio_cache_l1_max_bytes,
            512 * 1024 * 1024
        );
        assert_eq!(
            MemoryProfile::from_total_kb(32 * 1024 * 1024).audio_cache_l1_max_bytes,
            512 * 1024 * 1024
        );
    }

    #[test]
    fn audio_cache_cap_is_significantly_smaller_on_low_memory() {
        let normal = MemoryProfile::from_total_kb(8 * 1024 * 1024);
        let low = MemoryProfile::from_total_kb(938196);
        assert!(low.audio_cache_l1_max_bytes < normal.audio_cache_l1_max_bytes);
    }

    #[test]
    fn pi_zero_2w_512mb_resolves_to_low_memory() {
        let profile = MemoryProfile::from_total_kb(500 * 1024);
        assert_eq!(profile.class, MemoryClass::LowMemory);
        // ~75 MB: too small for a Hi-Res track, which is correct on a box
        // with 512 MB total, and still room for a Lossless one.
        assert!(profile.audio_cache_l1_max_bytes <= 90 * 1024 * 1024);
        assert!(profile.audio_cache_l1_max_bytes >= 70 * 1024 * 1024);
    }

    #[test]
    fn machine_with_2gb_resolves_to_normal() {
        // Exactly the threshold — Normal (>= NORMAL_FLOOR_KB).
        let profile = MemoryProfile::from_total_kb(2 * 1024 * 1024);
        assert_eq!(profile.class, MemoryClass::Normal);
        assert_eq!(profile.prefetch_count, 5);
        assert!(profile.allow_hires_prefetch);
    }

    #[test]
    fn machine_with_just_under_the_floor_resolves_to_low_memory() {
        let profile = MemoryProfile::from_total_kb(1792 * 1024 - 1);
        assert_eq!(profile.class, MemoryClass::LowMemory);
    }

    #[test]
    fn nominal_2gb_pi_resolves_to_normal() {
        // A Pi 5 2 GB reports ~1.94 GiB of MemTotal, not a round 2 GiB. It
        // must land Normal: its L1 budget has to fit a Hi-Res track or
        // gapless stops working on a box that handles it fine.
        let profile = MemoryProfile::from_total_kb(2_033_664);
        assert_eq!(profile.class, MemoryClass::Normal);
        assert!(profile.audio_cache_l1_max_bytes > 200 * 1024 * 1024);
    }

    #[test]
    fn detect_profile_from_meminfo_falls_back_to_normal_when_unparseable() {
        let profile = detect_profile_from_meminfo("garbage\nno memtotal here\n");
        assert_eq!(profile.class, MemoryClass::Normal);
    }

    #[test]
    fn detect_profile_from_meminfo_returns_low_memory_for_pi() {
        let pi_meminfo = "\
MemTotal:         938196 kB
MemFree:          250000 kB
";
        let profile = detect_profile_from_meminfo(pi_meminfo);
        assert_eq!(profile.class, MemoryClass::LowMemory);
        assert_eq!(profile.mem_total_kb, 938196);
    }

    /// The L1 budget on each board this actually ships to. The 926,832 kB row
    /// is measured — it is the moOde test Pi 3B, and 161,342,914 bytes is the
    /// figure its own `Cache size:` log line reports.
    ///
    /// The pair that matters is the playing track plus the prefetched next one,
    /// so half the budget is the size above which a track is handed over as a
    /// file instead (`Player::should_hand_over_as_file`). Hi-Res runs about
    /// 21 MB/min, CD about 5-6.
    #[test]
    fn l1_budget_per_board() {
        let mb = |bytes: usize| bytes / (1024 * 1024);

        // Pi Zero 2 W / 3A+, 512 MB: below one Hi-Res track, by design.
        assert_eq!(mb(l1_cache_bytes_for_total_kb(439_000)), 72);
        // Pi 3B/3B+ and 1 GB Pi 4 — measured on the test player.
        assert_eq!(l1_cache_bytes_for_total_kb(926_832), 161_342_914);
        assert_eq!(mb(l1_cache_bytes_for_total_kb(926_832)), 153);
        // Pi 4 / 5, 2 GB: the fraction still binds, the ceiling does not.
        assert_eq!(mb(l1_cache_bytes_for_total_kb(1_986_000)), 329);
        // Pi 4 / 5, 4 GB: 17 % would be 644 MB, so the ceiling binds here.
        assert_eq!(mb(l1_cache_bytes_for_total_kb(3_881_000)), 512);
        // And it keeps binding, however large the box.
        assert_eq!(mb(l1_cache_bytes_for_total_kb(32 * 1024 * 1024)), 512);
    }

    /// A board that cannot afford a second whole track does not prefetch one.
    /// The 512 MB Pi 3A / Zero 2 W is the case: `cmaf::download_full` allocates
    /// the ENTIRE next track, ~210 MB at the default 24/192, on a box reporting
    /// ~439 MB — and it was the OOM killer, not a slow transition.
    #[test]
    fn gapless_prefetch_needs_a_board_that_can_hold_two_tracks() {
        // 512 MB boards: no prefetch.
        assert!(!MemoryProfile::from_total_kb(439_000).allow_gapless_prefetch);
        // 1 GB Pi 3B, measured — this is the board gapless is verified on.
        assert!(MemoryProfile::from_total_kb(926_832).allow_gapless_prefetch);
        // Everything larger, plainly.
        assert!(MemoryProfile::from_total_kb(1_986_000).allow_gapless_prefetch);
        assert!(MemoryProfile::from_total_kb(3_881_000).allow_gapless_prefetch);
        // The floor is on RAM, not on the memory CLASS: a 1 GB board is
        // LowMemory and still prefetches.
        assert_eq!(
            MemoryProfile::from_total_kb(926_832).class,
            MemoryClass::LowMemory
        );
    }
}
