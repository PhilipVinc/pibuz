# Pibuz — headless Qobuz Connect daemon

`pibuz` is a standalone ~25 MB binary that turns any Linux box — a Raspberry Pi, a NAS, or
the living-room mini-PC — into a bit-perfect **Qobuz Connect endpoint** that appears in the
official Qobuz apps like a hardware streamer.

- Daemon + full CLI + terminal setup wizard (TUI) in one binary
- HiFi wizard with copyable audio-stack config blocks (clipboard works over SSH)
- MPRIS out of the box, live JSON events (`pibuz watch`), service files for systemd/OpenRC/runit
- Event hooks: `pibuz settings set hooks.script /path/to/script` runs your script on
  playback/session events with `QBZ_*` environment variables — push integration for
  audio-box distros (moOde, Volumio, DIY setups), no polling required
- Local pairing (no login required): the daemon advertises itself on the LAN like a
  hardware streamer, so ANY Qobuz account in the household can cast to it from the official app.


## Credit

Pibuz began as a fork of **[QBZ](https://github.com/vicrodh/qbz)** by
**[@vicrodh](https://github.com/vicrodh)** — a native hi-fi Qobuz client for Linux and
macOS, and the origin of most of the code in this repository. The git history here *is*
QBZ's history.

It went its own way because I wanted the daemon running on a Raspberry Pi under
[moOde](https://moodeaudio.org/), and kept pushing it in directions that only matter when
there is no screen attached: event hooks so an audio-box distro can react to playback
instead of polling; Qobuz Connect pairing over the LAN; HTTP range requests so a seek or a
resume doesn't re-download the track from zero; gapless prefetch and ALSA clock/buffer
handling tuned for a small board; a much smaller memory footprint. Several of those have
gone back to QBZ and been merged there — upstreaming what fits both trees is still the
preferred outcome.

The split became structural when the desktop player came out: this tree keeps `pibuz` and
exactly the crates it depends on, and has dropped the Slint UI, its packaging and its
release tooling entirely. **QBZ is the project to use if you want a desktop
application**; Pibuz only makes sense if you want a headless box.

## Legal / Branding

- This application uses the Qobuz API but is not certified by Qobuz.
- Qobuz is a trademark of Qobuz. Pibuz is not affiliated with, endorsed by, or certified by Qobuz.
- Qobuz Terms of Service: https://www.qobuz.com/us-en/legal/terms

## Building

Standard Cargo workspace — manifest at the repo root, members under `crates/`.

```bash
cargo build --release -p pibuz        # -> target/release/pibuz
```

System dependencies (Debian/Ubuntu): `build-essential pkg-config libasound2-dev
libjack-jackd2-dev libdbus-1-dev libssl-dev`.

Tests (whole workspace):

```bash
./scripts/cargo-test.sh
```

### aarch64 (Raspberry Pi)

```bash
./scripts/build-aarch64-pibuz.sh      # native on ARM, or cross via Docker on x86-64
./scripts/pibuz-to-pi.sh              # copy the binary to the Pi and restart the service
```

With no UI crate in the graph, a 4 GB Pi can build the daemon natively. The cross path
uses `Cross.toml` to supply the arm64 dev libs inside the `cross` image.

## Repository layout

```
crates/
  pibuz/                  The daemon: CLI, TUI, HTTP API, hooks, MPRIS, Qobuz Connect glue
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
packaging/linux/         pibuz standalone tarball README
scripts/                 Build, deploy and acceptance scripts
```

The daemon's HTTP API is served by `crates/pibuz/src/api/`; `pibuz --help` and the
upstream wiki are the reference for it.

## Known Issues

- **Hi-Res seeking** — seeking in tracks >96kHz can take 10-20s (decoder must scan from start). Use prev/next for instant navigation.
- **ALSA Direct** — exclusive access blocks other apps. Use DAC/amplifier physical volume control.
- **DSD DoP / native mode** — seeking is disabled and volume is fixed while a DoP or native-DSD stream is active (any sample manipulation would corrupt the DSD stream). Convert-to-PCM mode has no such limits.

## Contributing

See `CONTRIBUTING.md`. Pibuz is headless by design; desktop-UI work belongs at
[vicrodh/qbz](https://github.com/vicrodh/qbz).

## License

MIT, as upstream. See `LICENSE`.
