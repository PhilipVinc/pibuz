# Changelog

Notable changes per release. Versions are plain semver; releases are `vX.Y.Z`
tags on `main`.

## 2.4.0-rc.1 — unreleased

**The project is now Pibuz and the binary is `pibuz`** — see *Renamed* below
for what an upgrade has to touch (little: the unit file) and what it keeps (the
profile, the device identity, every `QBZ_*` hook variable).

Also bounded streaming, verified on a 905 MB Pi 3B against a live Qobuz Connect
session: the held buffer stays at 2.9 MB where it used to grow to the whole
track, RSS while playing 24/96 went from ~125 MB to ~40 MB, and the download
is paced at the track's own byte-rate instead of the link's.

### Renamed

The daemon shipped as `μqbzd`/`muqbzd` with a `qbzd` binary. Both names were
unpronounceable, had two spellings between them, and `qbzd` collided with the
binary of the desktop project this tree forked from — installing both put two
different programs at the same path. The project is **Pibuz** and the binary is
**`pibuz`**.

- **Binary, crate and unit file** are `pibuz` / `pibuz.service`. Release assets
  are `pibuz-<version>-linux-<arch>.tar.gz`, unpacking to a directory of the
  same name holding `pibuz`. Update `ExecStart=` to the new path, and the unit
  name you enable.
- **Profile directories** are `~/.config/pibuz`, `~/.local/share/pibuz`,
  `~/.cache/pibuz`, with the config file `pibuz.toml`. **An existing `qbzd`
  profile keeps being used as-is** when the `pibuz` one is absent — that
  directory holds the persisted QConnect `device_uuid` and the audio settings,
  so an upgrade stays the same device in the Qobuz app and keeps its
  bit-perfect configuration. Nothing is copied or moved; rename the directory
  yourself whenever you want to, with the daemon stopped.
- **Every `QBZ_*` and `QBZD_*` environment variable is unchanged** — hook
  scripts written against `QBZ_EVENT`, `QBZ_ARTIST` and friends keep working
  untouched, and so do `QBZD_HOST`, `QBZD_TOKEN`, `QBZD_HOOK` and `QBZD_MPRIS`.
- **MPRIS** now publishes `org.mpris.MediaPlayer2.pibuz`. It used to claim the
  desktop application's `com.blitzfc.qbz` bus name, which meant the two could
  not be on one session bus and a controller could not tell which had answered.
  Anything that addressed the daemon by the old name needs the new one.
- **The Connect device** advertises brand and model `Pibuz`, and a box that
  never set a custom device name is now `Pibuz (<hostname>)` instead of
  `QBZ (<hostname>)`. The device identity is the `device_uuid`, not the name,
  so this renames the existing endpoint rather than creating a second one.
  `QBZ_QCONNECT_DEVICE_BRAND` / `_MODEL` / `_NAME` still override all three.

### Fixed (release)

- **Tagging a release actually publishes one.** Two tag conventions were in use
  — `vX.Y.Z` for the project's own releases, and `qbzd-v2.0.2.moodeNN` for the
  builds the moOde installer downloads — and the workflow ended up half on each:
  it triggered on `v*` while the publish job required a `refs/tags/qbzd-v*`
  ref, which no single tag can satisfy. So a `vX.Y.Z` tag built both
  architectures, uploaded the artifacts and skipped the Release, and a
  `qbzd-v*` tag did not start the workflow at all.

  **The `.moodeN` series is retired**: a build for moOde is a release like any
  other, tagged `vX.Y.Z`. It existed because the version needed a counter the
  Cargo version could not hold, and the cost was a base that stopped being
  updated — the last one went out as `2.0.2.moode57` from a 2.4.0 tree, so the
  binary announced a version its source had not been at for months.

- **A tag that disagrees with `Cargo.toml` is rejected**, so an asset named
  `pibuz-2.5.0-…` can no longer hold a binary that reports 2.4.0.
  `scripts/bump-version.sh` sets the version, the lock and the CHANGELOG
  heading together, and `--check` verifies them before you tag.

- **`QBZD_BUILD_ID` is `PIBUZ_BUILD_ID`, and CI no longer sets it.** It exists
  so a build can report a version `Cargo.toml` cannot hold, which was the
  `.moodeN` suffix; with that gone and the tag pinned, stamping a release would
  only set the value `CARGO_PKG_VERSION` already has. `scripts/pibuz-to-pi.sh`
  still uses it for the case that remains genuinely unrepresentable —
  `2.4.0.local-<sha>-dirty` on a Pi built from a working tree. It is
  compile-time only, which is why it could be renamed at all: the `QBZD_*`
  names read from the environment at RUNTIME (`QBZD_HOOK`, `QBZD_HOST`,
  `QBZD_MPRIS`, `QBZD_TOKEN`) are set by other people's scripts and stay.
  The dev-script variables (`QBZD_PI`, `QBZD_BIN`, `QBZD_TEST_PORT`, the
  build-image and volume overrides) take `PIBUZ_*` names too, each still
  accepting its old name as a fallback.

### Changed

- **`pibuz status` is a sectioned block, and it reports what the daemon knows
  about its own memory.** The five `·`-joined lines ran to 95 columns and wrapped
  on a narrow terminal; the block is now labelled rows under `audio`, `playback`
  and `qconnect`, none wider than 72 columns, and a row the daemon has no answer
  for is omitted rather than printed empty — an idle daemon is a short block.
  Three things it already knew but never showed are now on it: whether the
  device is *open*, whether this box is the session's **active renderer** (a
  controller switching to its own speakers leaves us connected and silent, which
  looked identical on every other field), and whether the pairing listener is
  serving.
  `pibuz status -v` adds `memory` and `buffers`: L1 and L2 cache occupancy
  against the budget actually in force, the host's memory class, the compressed
  window, the initial buffer cap and the decoded ring depth — the figures that
  explain a dropout, resolved daemon-side so asking a Pi from a laptop reports
  the *Pi's* class. `--json` is the whole payload either way.
- **`/api/status` gained `memory`, `cache` and `buffers`** plus
  `playback.buffer_progress`, `.gapless_ready`, `.gapless_next_track_id` and
  `.normalization_gain`. Additive only — every documented key still means what
  it meant, so no `api_version` bump, and `/api/now-playing` (the endpoint an
  overlay actually polls) is untouched.
- **The streaming buffer is bounded.** The downloader now parks when it is
  `audio.stream_window_seconds` (default 8) of the track's own byte-rate ahead
  of the decoder, and resumes at 75 % of that. Before, nothing bounded it: the
  link measured 4.5 MB/s against ~0.26 MB/s of playback, so the whole
  compressed track — 120–220 MB at Hi-Res — was resident within seconds of
  pressing play. A Hi-Res stream now holds single-digit megabytes.
  The window is in *seconds*, not bytes, because a byte constant is a
  different amount of music at every quality.
- **The cast path uses the cache.** Casting from the Qobuz app now checks the
  memory and disk caches before the network, and stages what it streams into
  the disk cache. Previously that path read neither and wrote neither, so
  pressing *previous* re-downloaded a track already on the card.
- **Gapless arms on "this track can spare the bandwidth"** rather than "this
  track has finished downloading", which a bounded window makes true only at
  the very end of a track.
- **CMAF playback holds one copy of a track instead of three**, and no longer
  allocates per FLAC frame.
- **Gapless defaults agree.** The struct default, the database default and
  `reset_all()` gave three different answers; a fresh install had it off while
  `AudioSettings::default()` claimed on. Choosing the ALSA backend no longer
  switches gapless off.

### Fixed

- The daemon now raises its own `RLIMIT_RTPRIO`, so the ALSA writer actually
  gets `SCHED_FIFO` where it is started without a systemd unit — which is how
  moOde starts it, and where the promotion had been silently refused.
- A completed stream that could not be promoted no longer drops its buffer
  anyway, which left the track unresumable after the next pause.
- The HTTP client used a *total* request timeout, which a rate-matched
  download would have hit on every track over five minutes; it now times out
  on stalls instead.
- **Seeking on a bounded stream works.** A seek rebuilds the decoder, which
  re-probes the container from byte 0 — a region the window has long since
  discarded. The pinned header is now readable by decoders rather than only by
  metadata accessors, and a reader may re-request any region the window threw
  away, with the feeder staying up to serve it instead of exiting once the file
  has been walked.
- **A seek reports its position within the track, not within the source.** A
  cached track played from an offset used to start the clock at zero and seek
  afterwards, so the controller saw a position below the one it asked for and
  spun until it caught up. It now starts where it was asked to, which also
  stops it decoding the whole song to throw the result away.
- **The disk cache is usable again.** A quality gate demanded more than 96 kHz
  for the Hi-Res+ tier, which most hi-res masters do not have, so every cached
  copy of such a track was refused and re-downloaded forever.
- **The cast path keeps the controller in step.** A track served from the cache
  no longer lets a stale report for the outgoing track reach the controller,
  which showed as the artwork flicking to the previous song.
- **A gapless successor whose track was skipped past is discarded** rather than
  queued, which used to send the player backwards through the queue.
- **Gapless is no longer refused for the whole track.** A third gate was keyed
  on the download being complete, which a bounded window makes false until the
  very end; the prefetch ran, succeeded, and was discarded at every hand-off.

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
