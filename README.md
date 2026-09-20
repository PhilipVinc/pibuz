# Pibuz — headless Qobuz Connect daemon

`pibuz` is a standalone binary that turns any Linux box such as a Raspberry Pi into a bit-perfect **Qobuz Connect endpoint** that appears in the
official Qobuz apps like any other hardware streamer.
`pibuz` is optimized to run on low cost devices (tested on a Pi 3B+, will run on Pi 3A), but will also run on your fancy gaming pc.
It was developed for integration with the moode operating system, but will run pretty much anywhere.

Features:
- Daemon + control CLI + terminal setup wizard (TUI) in one binary
- HiFi wizard with copyable audio-stack config blocks (clipboard works over SSH)
- MPRIS out of the box, live JSON events (`pibuz watch`), service files for systemd/OpenRC/runit
- Event hooks: `pibuz settings set hooks.script /path/to/script` runs your script on
  playback/session events with `QBZ_*` environment variables — push integration for
  audio-box distros (moOde, Volumio, DIY setups), no polling required
- Local pairing (no login required): the daemon advertises itself on the LAN like a
  hardware streamer, so ANY Qobuz account in the household can cast to it from the official app.


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

pibuz supports cross compilation through a few scripts in the scripts directory.

```bash
./scripts/build-aarch64-pibuz.sh      # native on ARM, or cross via Docker on x86-64
./scripts/pibuz-to-pi.sh              # copy the binary to the Pi and restart the service
```

A 4 GB Pi can build the daemon natively. The cross path uses `Cross.toml` to supply the arm64 dev libs inside the `cross` image.


## Known Issues

- **ALSA Direct holds the card exclusively.** A `hw:` device is opened for this
  daemon alone, so nothing else on the box can play while a stream is open —
  that is what makes it bit-perfect. Volume still works: software volume is on
  by default, on a curve matched to MPD's so one slider position means the same
  loudness as everything else on a moOde box, and `audio.alsa_hardware_volume`
  hands volume to the DAC's own mixer instead.

## Legal / Branding

- This application uses the Qobuz API but is not certified by Qobuz.
- Qobuz is a trademark of Qobuz. Pibuz is not affiliated with, endorsed by, or certified by Qobuz.
- Qobuz Terms of Service: https://www.qobuz.com/us-en/legal/terms

## License

MIT
