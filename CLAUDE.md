# μqbzd — notes for coding agents

Headless Qobuz Connect daemon. The binary is `qbzd`; the project is `μqbzd`
(`muqbzd` in ASCII). It began as a fork of vicrodh/qbz and has diverged into a
daemon-only tree — **there is no desktop UI here and none is planned**. Outside
`README.md`, the repo does not refer to the project it forked from; keep it that
way when editing docs and CI.

## Layout

A standard Cargo workspace: manifest at the repo root, members under `crates/`,
artifacts in `target/`. Plain `cargo build` / `cargo test` from the root work.

The workspace is `qbzd` plus **exactly** its dependency closure — 15 crates, the
set `cargo tree -p qbzd` resolves. If you find yourself adding a workspace member,
check whether `qbzd` really needs it.

A lot of this tree is a desktop app that was never fully unwound, and orphans
keep turning up: whole crates have gone because their only entry points took an
argument every call site passed `None`. If a feature here looks orphaned, check
whether anything actually calls it before assuming it is load-bearing.

### Finding dead code: the probe

`pub fn` has NO dead-code lint — anything reachable from the crate root counts
as used — so a grep is the only thing most sweeps have to go on, and a grep
cannot see through `Type::name(...)` call syntax, serde attribute strings, or a
same-named method on another type. Get the compiler to answer instead:

1. rewrite the candidates `pub fn` -> `pub(crate) fn` (a PROBE, reverted after),
2. `cargo clippy --all-targets -- -D warnings`; dead_code now names the truly
   unreachable ones, transitively and through serde attribute paths,
3. **run the probe in the arm64 container too, and delete only the
   INTERSECTION.** `dsd_mode` was reported dead on macOS and was live on Linux.

For a LIBRARY crate the same trick needs one more turn, because `pub` there is
also the cross-crate API. Rewrite every `pub` item in the crate to `pub(crate)`,
build the WHOLE workspace, and iterate: each round, put back to `pub` every name
rustc names in an error (`E0603`, `E0364`/`E0365` for re-exports, and the plain
`type ... is private` of `private_interfaces`), then rebuild. **Only once the
workspace is at zero errors is the warning list meaningful** — before that, a
still-private root drags its whole live call tree into the "dead" list with it.

Two classes the lint names that you should NOT delete: an item reachable only
from other dead code (it goes when that does, not before), and an item whose
only caller is a test that covers real behaviour — move the coverage rather than
dropping it.

## Commands

```bash
cargo build --release -p qbzd
./scripts/cargo-test.sh          # whole workspace, same command CI runs
./scripts/build-aarch64-qbzd.sh  # Pi binary: native on ARM, cross via Docker on x86-64
./scripts/qbzd-to-pi.sh          # copy to the Pi, restart the service
./scripts/qbzd-acceptance.sh     # end-to-end against a running daemon
```

## Tests and lints: the suite is GREEN, keep it that way

`./scripts/cargo-test.sh` passes clean — 0 failures — on macOS and
linux/arm64. `cargo clippy --workspace --all-targets -- -D warnings` is clean on
both too, and CI enforces fmt + clippy + tests. **A failure is a regression.**

## The audio behaviour tests

`crates/qbz-player/src/player/playback_engine.rs` → `mod engine_behaviour_tests`
runs the REAL decoder thread, writer thread and ring against
`qbz_audio::VirtualAudioOut` — a software DAC that clocks at `rate x speed`,
models `start_threshold`, counts underruns and can tape every frame handed over.
The whole suite is ~0.25 s: `cargo test -p qbz-player engine_behaviour`.

**Every bug found on hardware gets a case there BEFORE it is fixed.** A suite
nobody adds to is dead weight; the discipline is the point. It has already earned
it — it caught an end-of-stream bug on its first run that hardware testing had
only half-revealed.

Do NOT reach for ALSA's `null` PCM instead. Measured: it takes 3 s of audio in
2 ms and reports no delay, so every timing-dependent path degenerates.

Two traps in the harness itself:
- Sample `underruns()` MID-TRACK. The end of a tail runs the ring dry and counts
  as one, exactly as real ALSA reaches XRun there — `drain` relies on that.
- Use `snapshot()` for anything asserting `played + queued == written`. Reading
  `frames_played()` and `delay_frames()` separately is a race.

What it cannot test at all: clicks, real xruns, the stop ramp, `start_threshold`
against a real driver, RT scheduling, whether moOde's `_audioout` really passes
through. Green here means the state machine is right, not that the Pi sounds
right.

## The bytes-layer behaviour tests

`crates/qbz-player/src/player/streaming_source.rs` → `mod buffer_behaviour_tests`
runs a scripted feeder (tokio) and a scripted reader (its own thread) at
INDEPENDENT rates against the real `BufferedMediaSource`/`BufferWriter` pair.
`cargo test -p qbz-player buffer_behaviour`, well under a second.

The existing unit tests above it push one chunk and read it back in lockstep.
That is the one regime where the download head cannot run ahead of the reader,
and therefore the one regime where an unbounded buffer looks bounded — which is
how a memory cap that was 28x out shipped with a passing test asserting it
worked. **A test for anything in this layer must let the two sides move at
different speeds**, or it is testing the wrong thing.

Assert only what is visible from outside: bytes held, bytes fetched, bodies
opened, whether a read returned. `fetched` is the important one — a re-download
loop has no other external symptom.

There are TWO scripted feeders because there are two real ones, and they differ
in the smallest thing they can restart at. `feed` mirrors
`qbzd/src/qconnect/remote_stream.rs` and restarts at a byte. `feed_cmaf` mirrors
`Player::cmaf_stream_segments` and restarts at a whole CMAF segment, so its
seeks land up to a segment early, its window overshoots by a segment rather than
a chunk, and a reader waiting inside the segment in flight is a case the byte
feeder does not have.

Three fidelity limits, same spirit as the audio harness: both feeders are
stand-ins for the code they mirror rather than that code, so a change there
needs a change here; there is no socket, so nothing exercises TCP back-pressure,
a stalled body, or this CDN's ~5 s cold-offset time-to-first-byte; and an
unpaced feeder means "as fast as this machine allows", which is useful for
ratios and meaningless for absolute timings.

## The CMAF segment table is a byte index

`crates/qbz-cmaf/src/map.rs` has the evidence in full, and it is load-bearing:
the init segment's `byte_len` per segment is EXACTLY what that segment
assembles to, so prefix sums over it are the assembled FLAC's own offsets. That
is what lets the CMAF path be `RangeRequests` — seek by fetching the segment
holding the byte — and therefore what lets its buffer be windowed instead of
holding the whole compressed track (120-220 MB at Hi-Res).

Do not weaken this to "approximately". If it ever stops being true the feeder
fails the track loudly, by comparing each decrypted segment against the length
the table declared; a silent mismatch would serve audio at offsets it does not
belong to, which decodes as noise rather than as an error.

## The controller-sync tests

`crates/qconnect-app/src/controller_harness.rs` puts a scripted phone in a test.
A `VirtualController` expands gestures (`tap_pause`, `drag_seek`,
`push_queue_and_play`) into the exact inbound frames the cloud sends for them; a
`FakeCloud` relays them, folds the renderer's reports into the screen it would
push back, and ECHOES each state report at the renderer the way the real cloud
does; a `ControllerView` models the screen, where `Buffering` is a spinner and
`total_ms: None` is a blanked progress bar. Under it runs the real `QconnectApp`,
the real `qconnect_app::renderer` orchestration, and a `FakeEngine` that MODELS
the player rather than canning answers. Cases live in `controller_sync_tests.rs`;
the suite is 0.01 s: `cargo test -p qconnect-app controller_sync`.

**Same discipline as the audio tests: a bug seen on a phone gets a case here
BEFORE it is fixed.** Assert against the SCREEN, not against protocol fields —
the screen is what was wrong every time. The shared invariants in
`assert_invariants` (the progress display never blanks, the echo never reaches
the player, the cursor names the audible track, one gesture is not a report
storm) are checked by every case, so a rule added for one bug catches the next
one for free.

Three traps, each of which silently walked the happy path until a mutation
exposed it:
- **Call `let_the_load_windows_expire()`** unless the case is about those
  windows. The 5 s load-dedup and 1.5 s handoff-echo windows both hang off one
  wall-clock `Instant`, so a microsecond-long test sits inside BOTH: every pause
  is swallowed as a peer echo and every load is deduped away.
- **`make_the_audio_thread_lag()`** for anything about a load in flight. By
  default the fake adopts a track the instant the stream opens; the real audio
  thread adopts it only once it has samples, and several guards exist ONLY for
  that gap.
- The fake's queue must hold the REAL tracks. `align_queue_cursor` looks the
  target up in it, so placeholder ids send it down the "not in queue" fallback
  every time and hide every cursor bug behind harness noise.

What it cannot reach: the daemon's own report loop (`qbzd::qconnect::report`),
which is where `buffer_state` is decided and where the periodic position reports
come from — so spinner LIFETIMES are out of scope. It is monomorphic on
`NativeWsTransport` + `AppRuntime` (`DaemonQconnectApp`, `DaemonEventSink`,
`DaemonRendererEngine`); making those generic over transport and engine is what
would let this harness mount the daemon's own sink instead of a copy of its
shape. The daemon-side volume policy (`VolumeMode::Locked`) is in the same
position: its arithmetic is unit-tested, its behaviour is not reachable here.

## The residency tests, and the profile trap

`crates/qbz-app/src/playback_driver.rs` → `mod residency_session_tests` drives
the real `plan_tick`/`advance_state` over a real `qbz_cache::AudioCache` for a
twenty-track gapless playlist and samples residency at EVERY tick. It exists
because the two unit tests either side of it — `plan_tick` emits
`ReleaseCachedTrack`, `AudioCache::release` frees one track — both pass while the
daemon still swaps: the question the Pi asks is how many tracks are resident at
once, twenty tracks in, and that is a property of the two composed over time.

**`memory_profile()` is a process-wide `OnceLock` resolved from the host's RAM,
and it gates four production behaviours.** Anything that calls it in a test gets
`Normal` on every dev machine and every CI runner — so the branch the Pi actually
runs is the one that never executes. Do not reach for a settable global: the
cases share a process, so whichever test set it first would decide for all of
them. Pass the class instead, as `release_finished_track_on` /
`release_finished_track_from` and `should_promote_streaming_buffer` now do.

The ring depth is NOT one of these, despite reading the same singleton: the
sizing lives in the pure `qbz_audio::pcm_ring::ring_capacity_frames`, which takes
both the override and the profile's seconds as arguments, and
`auto_takes_the_hosts_profile` already pins 6 s for a Normal board and 2 s for a
low-memory one. Running the ENGINE at 2 s would test no new logic — the fill,
drain and boundary handling are depth-independent; what a shallower ring changes
is how long the decoder may stall before the ring runs dry, and no test can
settle that honestly, because it depends on real decode speed and real I/O.
Both remaining singleton reads are named functions now — `gapless_prefetch_allowed`
and `should_promote_streaming_buffer` — so the policies are tested even though the
lookup still happens at the call site.

Assert `Arc::strong_count`, not just byte totals. A residency leak here IS one
extra live `TrackBytes` clone, so "the release dropped the last reference" is a
discrete assertion where "peak bytes ≤ budget" is an inequality with slack that
mutations walk straight through.

What was deliberately NOT built, after an adversarial review of the design:
- **A counting `#[global_allocator]`.** It cannot be isolated — the target that
  would host it (`crates/qbz-player/tests/`) cannot see `playback_engine`, which
  is a private module — it perturbs what it measures through `realloc`, and its
  size-class histogram cannot establish provenance: on the CMAF path the
  prefetch `Vec` and the cache `Arc` are the same size class, 16 bytes apart.
- **A manually-advanced `VirtualAudioOut`.** "Settle until the device holds `n`
  frames" is unsatisfiable above `buffer_frames` (`accept` blocks there), hangs
  `drain`'s 5 s deadline at every track boundary, and has no observable
  termination condition. It would not buy determinism anyway while the engine's
  five wall-clock timeouts (`WRITER_START_ON_IDLE`, `WRITER_PRIME_DEADLINE`,
  `DECODER_SPACE_WAIT`, the 100 ms source wait, `EXIT_GRACE`) still run on real
  time. Making those test-settable is the cheaper change if flaky underrun
  assertions become a problem.


## Formatting and lints

Run `cargo fmt --all`; the tree is rustfmt-clean and CI checks it.

For clippy, prefer fixing over silencing. When a lint is genuinely wrong for the
code, allow it **at the item**, with the reason in a comment beside it — not with
a blanket module allow. Two lints are allowed workspace-wide in the root
`Cargo.toml` `[workspace.lints.clippy]`, each with its rationale.

### VERIFY ON LINUX — this is not optional

Large parts of the audio stack are `#[cfg(target_os = "linux")]`. A macOS
`cargo check`/`clippy`/`test` **never compiles them**, so it cannot tell you the
truth about them. This has already bitten once: `cargo clippy --fix` on macOS saw
`stream` as unused in `PlaybackEngine::set_volume` — because the only reader is
inside a Linux cfg block — and rewrote it to `stream: _`. That compiles on macOS
and breaks ALSA hardware volume on the Pi.

The container is the check:

```bash
docker run --rm --platform linux/arm64 \
  -v "$PWD:/src:ro" -v qbzd-aarch64-target:/target \
  -v qbzd-aarch64-registry:/usr/local/cargo/registry \
  -w /src qbzd-aarch64-build:ubuntu22.04 \
  bash -c 'cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'
```

(Image: `docker build --platform linux/arm64 -t qbzd-aarch64-build:ubuntu22.04 \
-f packaging/docker/qbzd-aarch64.Dockerfile packaging/docker`. On Apple silicon
this runs natively, no QEMU. Needs colima or Docker Desktop up.)

Also treat clippy's suggestions as drafts, not patches: in this tree three were
wrong — one pasted a literal `<item>` placeholder, one mangled a counter loop into
assigning to an immutable binding, one swapped `&PathBuf` for `&Path` without
adding the import.

## Hard rules in the code

- **Secrets go through `qbz_log::register_secret`** before anything can log them —
  see `crates/qbz-log/src/redact.rs`. Registering after the first log line is too
  late.
- **Every task holding an `Arc<AppRuntime>` must be abort-and-joined in
  `QconnectHandle::shutdown()`** before `drop(booted)` — `crates/qbzd/src/qconnect/mod.rs`.
  This is the #521 clock-release ordering: a surviving clone holds the ALSA device
  open and the next start fails. Adding a task means adding its teardown.
- **`qbzd` must never resolve Slint.** CI gates on it in both workflows. Nothing in
  the tree pulls it today; the gate exists so a crates.io dependency cannot
  reintroduce it.
- **The release asset shape is an API.** `muqbzd-<version>-linux-<arch>.tar.gz`
  unpacks to one versioned directory holding `qbzd`, `qbzd.service`,
  `completions/` and `README.md`, with a `.sha256` beside it. Installers pin
  this; reshaping it breaks them.

## Versioning

Plain semver, no distro suffixes. `[workspace.package] version` in the root
`Cargo.toml` is the source of truth (2.1.0) and release tags are `vX.Y.Z` matching
it. The release workflow stamps the tag's version through the `QBZD_BUILD_ID` env
var at compile time, which `crates/qbzd/src/main.rs` reads into `VERSION` — that is
what `qbzd version`, `--version` and `/api/status` report. Without it you get the
Cargo version, so a plain `cargo build` is unchanged.

## Branches, CI and releases

`main` is the trunk. CI (`test-crates`) runs on PRs into main and pushes to main,
path-filtered to `crates/**`. Releases are **tags on main**: pushing a `vX.Y.Z` tag
triggers `release.yml`, whose first job refuses any tag whose commit is not an
ancestor of `origin/main`. `build-arm64.yml` is manual-dispatch and publishes
nothing — use it for a Pi test binary without tagging.
