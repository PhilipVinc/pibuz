#!/bin/sh
# install.sh — install Pibuz on a moOde box (or any 64-bit Linux) from the
# latest GitHub release, and reboot into it.
#
#   curl -fsSL https://philipvinc.github.io/pibuz/install.sh | sudo sh
#
# What it does, and nothing more: fetch the release tarball for this arch,
# verify its .sha256, put the binary at /usr/bin/pibuz, write a systemd SYSTEM
# unit that runs as the box's own user (the binary generates it — that is where
# User=/HOME= come from), enable it, reboot.
#
# A SYSTEM unit rather than the shipped user unit, because that is what a
# headless appliance wants: it starts at boot with no `loginctl enable-linger`,
# and it does not stop when you log out of SSH.
#
# Options (env or flags):
#   --version X.Y.Z   install this release instead of the latest
#   --user NAME       run the daemon as NAME (default: $SUDO_USER, then uid 1000)
#   --no-reboot       install and enable, but leave the reboot to you
set -eu

REPO="PhilipVinc/pibuz"
# The moOde generation this is tested against. A different major is a warning,
# never a refusal: pibuz talks to ALSA and Qobuz, not to moOde.
MOODE_MAJOR="10"
# Overridable so the script can be exercised without touching a real box.
BIN="${PIBUZ_BIN:-/usr/bin/pibuz}"
UNIT="${PIBUZ_UNIT:-/etc/systemd/system/pibuz.service}"
VERSION=""
RUN_AS="${PIBUZ_USER:-${SUDO_USER:-}}"
REBOOT=1

say() { echo "pibuz: $*"; }
warn() { echo "pibuz: warning: $*" >&2; }
die() {
	echo "pibuz: error: $*" >&2
	exit 1
}

while [ $# -gt 0 ]; do
	case "$1" in
	--version) VERSION="${2:?--version needs X.Y.Z}"; shift 2 ;;
	--user) RUN_AS="${2:?--user needs a name}"; shift 2 ;;
	--no-reboot) REBOOT=0; shift ;;
	-h | --help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
	*) die "unknown option '$1'" ;;
	esac
done

[ "$(id -u)" = "0" ] || die "run this as root: curl -fsSL <url> | sudo sh"

# ---- who the daemon runs as -------------------------------------------------
# It owns the profile (~/.config/pibuz: the Connect device_uuid and the audio
# settings) and needs the audio group, so this must be the box's real user —
# `pi` on a stock moOde image — not root.
[ -n "$RUN_AS" ] || RUN_AS="$(getent passwd 1000 | cut -d: -f1)"
[ -n "$RUN_AS" ] || die "could not tell which user to run as — pass --user NAME"
id "$RUN_AS" >/dev/null 2>&1 || die "no such user: $RUN_AS"

# ---- arch -------------------------------------------------------------------
case "$(uname -m)" in
aarch64 | arm64) ARCH="aarch64" ;;
x86_64 | amd64) ARCH="amd64" ;;
*) die "no prebuilt binary for $(uname -m) — moOde 9+ is 64-bit; build from source" ;;
esac

# ---- moOde release (warn only) ---------------------------------------------
moode_release() {
	if command -v moodeutl >/dev/null 2>&1; then
		moodeutl --mooderel 2>/dev/null | awk '{print $1; exit}'
		return
	fi
	for f in /var/www/footer.min.php /var/www/footer.php; do
		[ -r "$f" ] || continue
		sed -n 's/.*Release: \([0-9][0-9.]*\).*/\1/p' "$f" | head -1
		return
	done
}
MOODE="$(moode_release || true)"
if [ -z "$MOODE" ]; then
	say "no moOde install detected — continuing (pibuz does not need it)"
elif [ "${MOODE%%.*}" != "$MOODE_MAJOR" ]; then
	warn "this is moOde $MOODE; pibuz is tested on moOde $MOODE_MAJOR.x"
else
	say "moOde $MOODE"
fi

# ---- fetch ------------------------------------------------------------------
if [ -n "$VERSION" ]; then
	URL="https://github.com/$REPO/releases/download/v$VERSION/pibuz-$VERSION-linux-$ARCH.tar.gz"
else
	URL="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
		grep -om1 "https://[^\"]*-linux-$ARCH\.tar\.gz")" ||
		die "could not reach the GitHub release API"
	[ -n "$URL" ] || die "the latest release has no linux-$ARCH asset"
	VERSION="$(echo "$URL" | sed -n 's|.*/pibuz-\(.*\)-linux-.*|\1|p')"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT HUP INT TERM
say "downloading $VERSION ($ARCH)"
curl -fsSL "$URL" -o "$TMP/pibuz.tar.gz" || die "download failed: $URL"

# The .sha256 is published beside every asset. A truncated download is the
# failure that actually happens on a Pi, and it would install a broken binary.
if curl -fsSL "$URL.sha256" -o "$TMP/pibuz.sha256" 2>/dev/null; then
	if command -v sha256sum >/dev/null 2>&1; then
		want="$(cut -d' ' -f1 "$TMP/pibuz.sha256")"
		got="$(sha256sum "$TMP/pibuz.tar.gz" | cut -d' ' -f1)"
		[ "$want" = "$got" ] || die "checksum mismatch — refusing to install"
	else
		warn "no sha256sum on this box; skipping checksum"
	fi
else
	warn "no published checksum for this asset; skipping"
fi

tar -xzf "$TMP/pibuz.tar.gz" -C "$TMP" || die "the tarball did not unpack"
SRC="$TMP/pibuz-$VERSION-linux-$ARCH/pibuz"
[ -f "$SRC" ] || die "no pibuz binary inside the tarball"

# ---- install ----------------------------------------------------------------
# `install` unlinks the destination first, so this replaces a RUNNING binary
# without "Text file busy". The running process keeps the old inode until the
# reboot below (or a restart) picks up the new one.
install -m 755 "$SRC" "$BIN" || die "could not write $BIN"
say "installed $BIN ($("$BIN" --version 2>/dev/null || echo "$VERSION"))"

# The binary writes its own unit, resolving User=, HOME= and ExecStart= for the
# target user. Its stderr is an install hint meant for humans; drop it.
"$BIN" service systemd --system --user "$RUN_AS" --bin "$BIN" >"$TMP/unit" 2>/dev/null ||
	die "could not generate the unit file"
[ -s "$TMP/unit" ] || die "generated an empty unit file"
install -m 644 "$TMP/unit" "$UNIT"
say "wrote $UNIT (User=$RUN_AS)"

systemctl daemon-reload
systemctl enable pibuz >/dev/null 2>&1 || die "systemctl enable pibuz failed"

# An older recipe installed a systemd USER unit. Two units, one device_uuid,
# one ALSA device: whichever wins, the other logs device-busy errors forever.
HOME_DIR="$(getent passwd "$RUN_AS" | cut -d: -f6)"
if [ -n "$HOME_DIR" ] && [ -e "$HOME_DIR/.config/systemd/user/default.target.wants/pibuz.service" ]; then
	warn "a systemd USER unit is also enabled for $RUN_AS — disable it:"
	warn "  sudo -u $RUN_AS XDG_RUNTIME_DIR=/run/user/$(id -u "$RUN_AS") systemctl --user disable --now pibuz"
fi

echo
say "done. After the reboot pibuz runs at boot and appears in the Qobuz app."
say "To pick a specific DAC or name the device:  pibuz setup   (as $RUN_AS)"
say "Status:  pibuz status"

if [ "$REBOOT" = "1" ]; then
	echo
	say "rebooting in 5 seconds — Ctrl-C to stay up (then: sudo systemctl start pibuz)"
	sleep 5
	systemctl reboot
fi
