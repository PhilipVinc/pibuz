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


## Install

```bash
curl -fsSL https://philipvinc.github.io/pibuz/install.sh | sudo sh
```

Fetches the latest release for this architecture, verifies its published
checksum, replaces whatever `pibuz` is already on `PATH` (or installs to
`/usr/local/bin`), and reboots. Re-run the same line to upgrade.

**On moOde that is all it does.** moOde starts the daemon itself — Renderer
Config pushes the audio device, the quality cap and the event hook, then runs
`pibuz run` — so the script installs no service and leaves that alone; the
reboot is what hands moOde the new binary. A *first* install on moOde belongs in
Renderer Config → Qobuz Connect → Install, which builds moOde's own package and
wires all of that up; this script is for jumping to a release moOde does not
offer yet, and it says so if dpkg still records an older packaged version.

**Anywhere else** it also generates a systemd *system* unit for your user and
enables it, so pibuz starts at boot without `loginctl enable-linger` and does
not die with your SSH session.

Flags go after `sh -s --`:

```bash
curl -fsSL https://philipvinc.github.io/pibuz/install.sh | sudo sh -s -- --no-reboot
```

`--version X.Y.Z` pins a release, `--user NAME` sets the account the daemon runs
as, `--standalone` forces the systemd path on a moOde box. Every release tarball
also carries a README with the manual steps.

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


## Credit

Pibuz began as a fork of **[QBZ](https://github.com/vicrodh/qbz)** by
**[@vicrodh](https://github.com/vicrodh)**.
I decided to keep the git history here from the past.

While qbz worked great, making it talk with moode, and running reliably on low power/memory devices, required substantial work to rewrite all the internals.
Eventually I decided that the rewrite was so profound that it was not worth to try to upstream all the changes, and here we are.
Nevertheless, I am grateful to those who attempted this before me :).

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
