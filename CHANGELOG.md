# Changelog

Notable changes per release. Versions are plain semver; releases are `vX.Y.Z`
tags on `main`.

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

`audio.stream_window_seconds` does not exist yet, and the streaming buffer is
still unbounded: it holds the whole compressed track, 120–220 MB at Hi-Res. The
`max_buffer_bytes` cap present in this release only discards bytes *behind* the
reader, so it does not bound the buffer, and enforcing it is expensive. Prefer
`audio.streaming_only` on a 512 MB board until the next release.
