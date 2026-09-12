# μ-qbz-d — headless Qobuz Connect daemon

**μqbzd started life as a fork of [QBZ](https://github.com/vicrodh/qbz), and has since
diverged into a headless-only project optimized for low power devices like Raspberry Pis.** See [Where this came from](#where-this-came-from)

The binary is `qbzd`, a standalone ~25 MB binary that turns any Linux box — a Raspberry Pi, a NAS, or the 
living-room mini-PC — into a bit-perfect **Qobuz Connect endpoint** that appears in the
official Qobuz apps like a hardware streamer.

- Daemon + full CLI + terminal setup wizard (TUI) in one binary
- HiFi wizard with copyable audio-stack config blocks (clipboard works over SSH)
- MPRIS out of the box, live JSON events (`qbzd watch`), service files for systemd/OpenRC/runit
- Event hooks: `qbzd settings set hooks.script /path/to/script` runs your script on
  playback/session events with `QBZ_*` environment variables — push integration for
  audio-box distros (moOde, Volumio, DIY setups), no polling required
- Local pairing (no login required): the daemon advertises itself on the LAN like a
  hardware streamer, so ANY Qobuz account in the household can cast to it from the official app.


## Where this came from

μqbzd is a fork of **[QBZ](https://github.com/vicrodh/qbz)**, written by
**[@vicrodh](https://github.com/vicrodh)** — a native hi-fi Qobuz client for Linux and
macOS, and the origin of nearly every line of code in this repository, `qbzd` itself
included. The git history here *is* QBZ's history; this is a branch of that work, not a
rewrite of it.

The fork started because I wanted that daemon running on a Raspberry Pi under
[moOde](https://moodeaudio.org/), and kept pushing it in directions that only matter when
there is no screen attached: event hooks so an audio-box distro can react to playback
instead of polling; Qobuz Connect pairing over the LAN; HTTP range requests so a seek or a
resume doesn't re-download the track from zero; gapless prefetch and ALSA clock/buffer
handling tuned for a small board; a much smaller memory footprint. Several of those have
gone back to QBZ and been merged there, and more are in review — upstreaming is the
preferred outcome, and this fork is not a competitor to it.

The divergence became structural with the removal of the desktop player: this tree keeps
`qbzd` and exactly the crates it depends on, and has dropped the Slint UI, its packaging
and its release tooling entirely. That makes the two trees hard to reconcile in the UI
direction, which is the honest reason to call it a separate project rather than a branch
waiting to be merged. **QBZ remains actively developed and is the project to use** if you
want the application; μqbzd only makes sense if you want a headless box.

## Legal / Branding

- This application uses the Qobuz API but is not certified by Qobuz.
- Qobuz is a trademark of Qobuz. QBZ is not affiliated with, endorsed by, or certified by Qobuz.
- Qobuz Terms of Service: https://www.qobuz.com/us-en/legal/terms

## Building

Standard Cargo workspace — manifest at the repo root, members under `crates/`.

```bash
cargo build --release -p qbzd        # -> target/release/qbzd
```

System dependencies (Debian/Ubuntu): `build-essential pkg-config libasound2-dev
libjack-jackd2-dev libdbus-1-dev libssl-dev`.

Tests (whole workspace):

```bash
./scripts/cargo-test.sh
```

### aarch64 (Raspberry Pi)

```bash
./scripts/build-aarch64-qbzd.sh      # native on ARM, or cross via Docker on x86-64
./scripts/qbzd-to-pi.sh              # copy the binary to the Pi and restart the service
```

With no UI crate in the graph, a 4 GB Pi can build the daemon natively. The cross path
uses `Cross.toml` to supply the arm64 dev libs inside the `cross` image.

## Repository layout

```
crates/
  qbzd/                  The daemon: CLI, TUI, HTTP API, hooks, MPRIS, Qobuz Connect glue
  qbz-app/               Application-level orchestration (non-UI)
  qbz-core/              Orchestrator (player + audio + API)
  qbz-player/            Playback engine, streaming, queue
  qbz-audio/             Audio backends, loudness, device management
  qbz-qobuz/             Qobuz API client and auth
  qbz-models/            Shared domain types
  qbz-cmaf/ qbz-dsd/     CMAF demux; DSD (DSF/DFF) decoding, DoP, native DSD packing
  qbz-cache/             L1 memory + L2 disk audio caching
  qbz-offline-cache/     Encrypted offline store
  qbz-library/           Local library scanning and metadata
  qbz-radio/ qbz-reco/   Radio and recommendations
  qbz-integrations/      Last.fm, ListenBrainz, MusicBrainz, Discogs
  qbz-media-controls/    MPRIS
  qbz-credentials/ qbz-secrets/  Auth/token storage
  qbz-log/               Logging with secret redaction
  qconnect-protocol/     Qobuz Connect protobuf wire format
  qconnect-core/         Queue and renderer domain models
  qconnect-app/          Application logic and concurrency
  qconnect-transport-ws/ WebSocket transport with qcloud framing
packaging/linux/         qbzd standalone tarball README
scripts/                 Build, deploy and acceptance scripts
```

The daemon's HTTP API is served by `crates/qbzd/src/api/`; `qbzd --help` and the
upstream wiki are the reference for it.

## Known Issues

- **Hi-Res seeking** — seeking in tracks >96kHz can take 10-20s (decoder must scan from start). Use prev/next for instant navigation.
- **ALSA Direct** — exclusive access blocks other apps. Use DAC/amplifier physical volume control.
- **DSD DoP / native mode** — seeking is disabled and volume is fixed while a DoP or native-DSD stream is active (any sample manipulation would corrupt the DSD stream). Convert-to-PCM mode has no such limits.

## Contributing

See `CONTRIBUTING.md`. UI changes belong upstream at
[vicrodh/qbz](https://github.com/vicrodh/qbz).

## License

MIT, as upstream. See `LICENSE`.
