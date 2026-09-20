# Pibuz — headless Qobuz Connect daemon (standalone download)

This tarball is the independent daemon download (no dependency on the desktop
`qbz` app, no deb/rpm needed). Install it on the box wired to your DAC — a
Raspberry Pi, an LXC, a living-room NUC — run `pibuz setup` once, and it
appears in the official Qobuz app as a Qobuz Connect device.

## Contents

- `pibuz` — the daemon binary (also its own CLI client and setup TUI)
- `pibuz.service` — a systemd user unit (shipped, not enabled)
- `completions/` — bash/zsh/fish shell completions

## Install

Recommended — matches the shipped unit's `ExecStart=/usr/bin/pibuz run`:

```bash
sudo install -Dm755 pibuz /usr/bin/pibuz
sudo install -Dm644 pibuz.service /usr/lib/systemd/user/pibuz.service
systemctl --user daemon-reload
```

Prefer a user-local install instead? Copy `pibuz` anywhere on your `$PATH`
(e.g. `~/.local/bin/pibuz`), then edit `pibuz.service`'s `ExecStart=` line to
point at that path before copying it to `~/.config/systemd/user/pibuz.service`
and running `systemctl --user daemon-reload`.

Shell completions (optional):

```bash
sudo cp completions/pibuz.bash /usr/share/bash-completion/completions/pibuz
# zsh: copy completions/pibuz.zsh into a directory on your $fpath
# fish: copy completions/pibuz.fish into ~/.config/fish/completions/
```

## Required: enable linger

Without linger, the user unit stops the moment you log out of SSH and the
device vanishes from the Qobuz app:

```bash
sudo loginctl enable-linger $USER
```

`pibuz status` warns when linger is off.

## First run

```bash
pibuz setup
```

`pibuz setup` is the six-screen configurator: log in to Qobuz, pick the audio
device, name the Connect device. It edits the same stores `pibuz run` reads,
so one pass is enough (revisit any time to change a setting).

Then enable and start the daemon:

```bash
systemctl --user enable --now pibuz
systemctl --user status pibuz
```

## Why glibc 2.35

This binary is built on ubuntu-22.04 (glibc 2.35) specifically so it runs on
Raspberry Pi OS bookworm (glibc 2.36) and similarly-aged distros without a
rebuild.
