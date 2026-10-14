# Pibuz — headless Qobuz Connect daemon

`pibuz` is a standalone binary that turns any Linux box such as a Raspberry Pi into a bit-perfect **Qobuz Connect endpoint** that appears in the
official Qobuz apps like any other hardware streamer.
`pibuz` is optimized to run on low cost devices (tested on a Pi 3B+, will run on Pi 3A), but will also run on your fancy gaming pc.
It was developed for integration with [moOde audio](https://moodeaudio.org), but will run pretty much anywhere.

Features:
- Daemon + control CLI in one binary: playback, queue, volume, settings and status
- Local pairing (no login required): the daemon advertises itself on the LAN like a
  hardware streamer, so ANY Qobuz account in the household can cast to it from the official app
- Live JSON events (`pibuz watch`) and generated service files for systemd/OpenRC/runit
- Event hooks: `pibuz settings set hooks.script /path/to/script` runs your script on
  playback/session events with `QBZ_*` environment variables — push integration for
  audio-box distros (moOde, Volumio, DIY setups), no polling required


## Running

```bash
pibuz service systemd > ~/.config/systemd/user/pibuz.service   # or: openrc | runit
systemctl --user daemon-reload && systemctl --user enable --now pibuz
sudo loginctl enable-linger "$USER"                              # keep it up after you log out
```

or simply `pibuz run` in the foreground. The box then shows up in the Qobuz apps on
the same network; pick it as the output.

```bash
pibuz status                                    # is it up, connected, what is it playing
pibuz settings show                             # every setting and its current value
pibuz qconnect name "Living Room"               # the name the Qobuz apps show
pibuz settings set qconnect.device_type speaker # the icon next to it (default: streamer)
pibuz --help                                    # everything else
```

Settings live in `settings.toml` in the data root; `settings set` says when each change
takes effect.


## Building

Standard Cargo workspace — manifest at the repo root, members under `crates/`.

```bash
cargo build --release -p pibuz        # -> target/release/pibuz
```

System dependencies (Debian/Ubuntu): `build-essential pkg-config libasound2-dev`.
The optional JACK output backend (`--features qbz-audio/jack`) also needs
`libjack-jackd2-dev`.

Tests (whole workspace):

```bash
cargo test --workspace --no-fail-fast
```

### aarch64 (Raspberry Pi)

```bash
./scripts/build-aarch64-pibuz.sh      # -> dist/pibuz-aarch64-linux
./scripts/pibuz-to-pi.sh              # build, copy to the Pi ($PIBUZ_PI, default `moode`) and restart it
./scripts/pibuz-to-pi.sh --deb        # same, as a package moOde's renderer page detects
```

`build-aarch64-pibuz.sh` picks the build by host: native on ARM Linux (a 4 GB Pi can
build it), `cross` on x86-64 Linux (`Cross.toml` supplies the arm64 dev libs), and an
arm64 Linux container on macOS, which runs natively on Apple silicon.


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
