# Changelog

Notable changes per release. Versions are plain semver; releases are `vX.Y.Z`
tags on `main`.

## 2.4.2 — unreleased

### Changed

- **The disk cache is encrypted, and no longer survives a restart.**
  `~/.cache/qbz/playback/` held playable Hi-Res FLACs: the CMAF path decrypts
  each segment before it assembles, the legacy path downloads plaintext, and
  both landed on the card as-is. Every route into the cache — the streaming
  tee, the straight-to-disk download and an in-memory track spilling out — now
  seals its file with AES-128-CTR.

  The key is generated when the daemon starts and never written down, so the
  cache cannot be read across a restart and the directory is wiped on the way
  up. On moOde that is every renderer toggle, because `stopQobuz()` kills the
  daemon: a toggle now means re-fetching whatever was cached. A key stored
  beside the ciphertext would protect nothing, so this was the trade taken.

  Nothing else changes — same budget (`audio.disk_cache_mb`), same cap
  behaviour, same `audio.cache_to_disk false` if you want no disk at all, and
  the reported cache size still counts audio rather than file bytes.

## 2.4.1 — 2026-09-16

### Fixed

- `playback.quality` now caps the cast path, not just local playback — on a
  cast-only daemon it was inert. Advertised as a capability and clamped at both
  load seams; read fresh per load.
- The loudness cache no longer panics on a full or read-only card, killing
  playback for the life of the process. It degrades to memory, opens on first
  use, and honors `--profile`; a stale `~/.local/share/qbz/loudness_cache.db`
  can be deleted.
- Signed stream URLs are redacted in logs. They are bearer credentials, and
  `reqwest` errors put them in `pibuz.log`.
- `bump-version.sh` opens the new CHANGELOG section again.
- The 2.4.0 *Removed* entry is corrected: quality did not come from
  `playback.quality`.


## 2.4.0 — 2026-09-15

Renamed to **Pibuz**, binary `pibuz` — update the unit file, everything else
carries over. Streaming is now bounded: on a Pi 3B the held buffer stays at 2.9
MB instead of growing to the whole track, and RSS at 24/96 went ~125 → ~40 MB.

### Renamed

- Binary, crate and unit are `pibuz` / `pibuz.service`; update `ExecStart=`.
- Profile is `~/.config/pibuz` and siblings, but an existing `qbzd` profile is
  still used when absent — same Connect device, same bit-perfect settings.
- `QBZ_*` and `QBZD_*` environment variables are unchanged.
- MPRIS is `org.mpris.MediaPlayer2.pibuz`, not the desktop app's
  `com.blitzfc.qbz`.
- Connect brand and model are `Pibuz`, default name `Pibuz (<hostname>)`, same
  `device_uuid`.

### Changed

- JACK output is off by default, behind the `qbz-audio/jack` feature. Drops the
  `libjack-jackd2-dev` build dependency.
- The disk cache no longer evicts the track being played or staged.
- The streaming buffer is bounded to `audio.stream_window_seconds` (default 8)
  of the track's byte-rate. Hi-Res held 120–220 MB before.
- Gapless works on 512 MB boards when `audio.cache_to_disk` is on.
- The queue prefetch no longer allocates a whole Hi-Res track on a small board.
- `audio.disk_cache_mb` sizes the L2 cache; `auto` is 800 MB, under 100 MB
  refused.
- `audio.memory_cache_mb` accepts up to 4096 MB, was 1024.
- A low-memory board's decoded ring is 4 s, was 2.
- `pibuz status` is a sectioned block, and adds device-open, active-renderer and
  pairing-listener state, plus `memory` and `buffers` under `-v`.
- `/api/status` gained `memory`, `cache`, `buffers` and four `playback.*` keys.
- The cast path reads and writes the cache.
- Gapless arms on spare bandwidth, not on a completed download.
- CMAF playback holds one copy of a track instead of three.
- Gapless defaults agree; the ALSA backend no longer switches it off.

### Removed

- Six settings keys the daemon never read: `audio.quality_fallback_behavior`,
  `.allow_quality_fallback`, `.limit_quality_to_device`,
  `.device_max_sample_rate`, `.sync_audio_on_startup`,
  `playback.show_context_icon`. Setting one gets exit 2; `settings import` still
  accepts them.

  **Correction:** this entry originally claimed quality comes straight from
  `playback.quality`. Not true of the cast path; fixed in 2.4.1.

### Fixed (release)

- A `vX.Y.Z` tag actually publishes a Release. The `.moodeN` tag series is
  retired.
- A tag that disagrees with `Cargo.toml` is rejected; `bump-version.sh --check`
  verifies the tree.
- `QBZD_BUILD_ID` is `PIBUZ_BUILD_ID`, compile-time only; runtime `QBZD_*` names
  are unchanged.

### Fixed

- `qconnect.volume_mode locked` no longer leaves a fixed-output player quiet
  with a slider that cannot raise it.
- `settings set` accepts a value starting with `-`, so
  `audio.normalization_target_lufs -14` can be written.
- `settings import` applies the nine audio keys it reported as applied.
- The daemon raises its own `RLIMIT_RTPRIO`, so the ALSA writer gets
  `SCHED_FIFO` without a systemd unit — how moOde starts it.
- Seeking on a bounded stream works: the header is pinned and discarded regions
  can be re-requested.
- A cast paused and resumed mid-track no longer wedges on a parked feeder.
- A seek reports its position within the track, not the source.
- The disk cache is usable again; its Hi-Res+ gate demanded more than 96 kHz.
- The cast path no longer sends a stale report for the outgoing track.
- A gapless successor whose track was skipped past is discarded, not queued.
- Gapless is no longer refused by a gate keyed on the download being complete.
- A completed stream that could not be promoted keeps its buffer.
- The HTTP client times out on stalls, not on total request time.

## 2.3.0 — 2026-09-13

First release of μqbzd as its own project rather than a fork branch, and the
first with a real-time audio path.

### Added

- **A buffer of decoded audio between the decoder and the DAC.** Until now the
  ALSA hardware ring was the entire jitter budget, and at 44.1 kHz the writer's
  own work quantum was larger than it — a network stall drained the ring
  directly. There is now a lock-free SPSC ring of decoded frames (2 s on a
  low-memory host, 6 s otherwise, `audio.pcm_ring_ms`) with a decode thread that
  is allowed to block and a writer thread that is not.
- **The ALSA writer runs at `SCHED_FIFO`** (`audio.writer_rt_priority`, default
  5, `0` to disable), degrading to normal priority when the host does not grant
  the rlimit. Only that one thread is promoted, and it never blocks on anything
  but the device.
- **`sw_params` is configured** rather than left at alsa-lib defaults:
  `start_threshold = buffer_size` so the DAC does not begin clocking on a nearly
  empty ring, and `avail_min = period_size`. `stop_threshold` is deliberately
  left alone — the bounded drain detects end-of-tail through it.

### Changed

- **The tree is daemon-only.** The Slint desktop UI and everything that served
  it — 14 crates, the vendored Slint/femtovg forks, the flatpak/snap/AUR/gentoo/
  AppImage packaging, the GUI release workflows and the desktop release notes —
  are gone. What remains is `qbzd` plus exactly its dependency closure, 23
  crates. Tracked files went 1418 → 364; the lockfile went 1008 → 627 packages.
- **Standard Cargo layout.** The workspace manifest moved from
  `crates/Cargo.toml` to the repo root, members are `crates/*`, and artifacts
  land in `target/`. Plain `cargo build` / `cargo test` from the root now work.
- **Plain semver.** No more `.moodeN` build suffixes in the version scheme.
  Release tags are `vX.Y.Z` matching `Cargo.toml`. `QBZD_BUILD_ID` still stamps
  a tag's version into the binary at compile time, which is what makes a
  prerelease tag report itself accurately.
- **Release assets renamed** to `muqbzd-<version>-linux-<arch>.tar.gz`, each
  with a `.sha256` beside it. The archive layout is unchanged: one versioned
  directory holding `qbzd`, `qbzd.service`, `completions/` and `README.md`.
- The daemon calls itself μqbzd where it names itself — `qbzd status`,
  `qbzd version`, `--help`, the setup TUI, and the service units it generates.
  `qbzd --version` still prints `qbzd <version>`, which is what installers parse.
- `main` is the trunk and the only branch releases are cut from; CI refuses a
  release tag whose commit is not an ancestor of `main`.
- The workspace is rustfmt-clean, and CI enforces it.

### Fixed

Everything the fork accumulated before it became its own project — Qobuz
Connect handoff and reporting accuracy, HTTP range requests for seek and
resume, gapless prefetch, ALSA clock and buffer handling, and a large memory
footprint reduction — is in this tree. Several of those changes were also sent
to the project this forked from.

- **A seek no longer leaves the buffer latch stuck.** A resume or a seek past
  the download head re-opens the HTTP body at that offset instead of waiting for
  the download to walk there; verified on hardware across three consecutive
  seeks, each settling in ~300 ms.
- **A session that re-attaches into a pause no longer spins for 90 seconds**
  waiting on a buffering latch that nothing would clear.
- The DoP/native-DSD write path uses the same bounded, interruptible write as
  PCM, so a device that stops draining can no longer wedge the audio thread.

### Known issue

The streaming buffer is unbounded in this release: it holds the whole
compressed track, 120–220 MB at Hi-Res. The `max_buffer_bytes` cap present here
only discards bytes *behind* the reader, so it does not bound the buffer, and
enforcing it is expensive. Prefer `audio.streaming_only` on a 512 MB board.
Fixed after 2.3.0 by `audio.stream_window_seconds`.
