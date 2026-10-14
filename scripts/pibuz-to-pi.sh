#!/usr/bin/env bash
#
# pibuz-to-pi.sh — build the aarch64 pibuz HERE and install it on the Pi,
# without going through GitHub Actions.
#
# The CI round trip is ~7 minutes: tag, push, wait for the runner, download the
# release tarball. This is ~35 s for an incremental build plus the copy, which
# is the difference between testing an idea and abandoning it. CI stays the
# thing that builds what other people install; this is for the loop where the
# only listener is the person running it.
#
# HOW IT BUILDS
#   A native arm64 Linux container under colima — not a cross toolchain. pibuz
#   links ALSA, and matching the Pi's own library is far easier than
#   persuading a cross linker to find an aarch64 copy of it.
#   `zig cc` was tried and rejected for exactly that reason.
#
#   Rust artifacts live in a docker VOLUME, never on the virtiofs mount: cargo
#   writes tens of thousands of small files and doing that across the mount is
#   slower than the compile.
#
# REQUIREMENTS
#   colima running, with a mount covering this checkout. The mount is the usual
#   failure: colima only shares the paths in ~/.colima/default/colima.yaml, and
#   a path outside them appears INSIDE the VM as an empty directory rather than
#   as an error — a build that "succeeds" against no source.
#
# USAGE
#   ./scripts/pibuz-to-pi.sh              # build and install on $PIBUZ_PI (default: moode)
#   ./scripts/pibuz-to-pi.sh --build-only # leave the binary in dist/, install nothing
#   ./scripts/pibuz-to-pi.sh --deb        # install as a dpkg package moOde can see
#
# --deb
#   moOde's renderer page (10.3.6 on) counts pibuz as installed only when dpkg
#   has a `pibuz` package whose version contains "moode"; a bare binary in
#   /usr/local/bin reads as "not installed". --deb wraps the build in a package
#   laid out like moOde's own (just /usr/bin/pibuz), versioned
#   `<version>~local.<sha>-1moode1` so a real `<version>-1moode1` still
#   upgrades over it, and removes the /usr/local/bin copy that would shadow it.
#   The page will then offer to "upgrade" to whatever release its plugin table
#   names, which may be older: do not click it.
set -euo pipefail

PI="${PIBUZ_PI:-${QBZD_PI:-moode}}"
IMAGE="${PIBUZ_BUILD_IMAGE:-${QBZD_BUILD_IMAGE:-pibuz-aarch64-build:ubuntu22.04}}"
HOST_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The directory colima shares, and this checkout's path within it.
MOUNT_ROOT="$(dirname "$HOST_ROOT")"
REL="$(basename "$HOST_ROOT")"
BUILD_ONLY=0
DEB=0
for arg in "$@"; do
  case "$arg" in
    --build-only) BUILD_ONLY=1 ;;
    --deb) DEB=1 ;;
    *) echo "unknown option: $arg (expected --build-only or --deb)" >&2; exit 2 ;;
  esac
done

say() { printf '\n[pibuz-to-pi] %s\n' "$*"; }

docker info >/dev/null 2>&1 || { echo "docker is not reachable — is colima running?" >&2; exit 1; }

# Stamp the build so `pibuz --version`, /api/status and the Connect device name
# say where the binary came from. CI sets this from the tag; a local build says
# "local" and the commit, because a Pi running an untagged binary you cannot
# identify later is worse than no local build loop at all.
SHA="$(git -C "$HOST_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
DIRTY=""
git -C "$HOST_ROOT" diff --quiet 2>/dev/null || DIRTY="-dirty"
BASE="$(grep -m1 '^version = ' "$HOST_ROOT/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"
export PIBUZ_BUILD_ID="${BASE}.local-${SHA}${DIRTY}"

say "building pibuz $PIBUZ_BUILD_ID (release, aarch64) in $IMAGE"
docker run --rm \
  -e PIBUZ_BUILD_ID \
  -v "$MOUNT_ROOT:/work" \
  -v pibuz-cargo-registry:/usr/local/cargo/registry \
  -v pibuz-target-aarch64:/target \
  -w "/work/$REL" \
  "$IMAGE" \
  bash -lc '
    set -e
    # The mount trap: an unshared host path shows up as an empty directory,
    # not as an error, so a build can "succeed" against no source at all.
    test -f Cargo.toml || { echo "no Cargo.toml at $PWD — colima is not sharing this checkout"; exit 1; }
    CARGO_TARGET_DIR=/target PIBUZ_BUILD_ID="$PIBUZ_BUILD_ID" cargo build --release -p pibuz
    mkdir -p "/work/'"$REL"'/dist"
    cp /target/release/pibuz "/work/'"$REL"'/dist/pibuz-aarch64-linux"
  '

BIN="$HOST_ROOT/dist/pibuz-aarch64-linux"
say "built $(ls -lh "$BIN" | awk '{print $5}') -> $BIN"
file "$BIN" | grep -q aarch64 || { echo "not an aarch64 binary — refusing to install" >&2; exit 1; }

if [[ $DEB == 1 ]]; then
  # Same layout as moOde's cargo-deb package. The version must contain "moode"
  # for the renderer page to see it; `~local` sorts it below the release.
  DEB_VERSION="${BASE}~local.${SHA}${DIRTY:+.dirty}-1moode1"
  DEB_FILE="pibuz_${DEB_VERSION}_arm64.deb"
  say "packaging $DEB_FILE"
  docker run --rm \
    -e DEB_VERSION="$DEB_VERSION" -e DEB_FILE="$DEB_FILE" -e SHA="$SHA" \
    -v "$MOUNT_ROOT:/work" \
    -w "/work/$REL/dist" \
    "$IMAGE" \
    bash -c '
      set -e
      rm -rf deb && mkdir -p deb/DEBIAN deb/usr/bin
      install -m755 pibuz-aarch64-linux deb/usr/bin/pibuz
      # The newest glibc symbol the binary links, so dpkg refuses a board too old for it.
      GLIBC=$(objdump -T deb/usr/bin/pibuz | grep -o "GLIBC_[0-9.]*" | sort -uV | tail -1 | cut -d_ -f2)
      cat > deb/DEBIAN/control <<EOF
Package: pibuz
Version: $DEB_VERSION
Architecture: arm64
Maintainer: Filippo Vicentini <filippovicentini@gmail.com>
Section: sound
Priority: optional
Depends: libasound2 (>= 1.0.16), libc6 (>= $GLIBC)
Description: Headless Qobuz Connect renderer (local build $SHA)
 A local test build from pibuz-to-pi.sh, not a release.
EOF
      dpkg-deb --build --root-owner-group deb "$DEB_FILE" >/dev/null
      rm -rf deb
    '
  PAYLOAD="$HOST_ROOT/dist/$DEB_FILE"
  REMOTE=/tmp/pibuz.deb
  # dpkg owns /usr/bin/pibuz; a /usr/local/bin copy left by a plain run would
  # come first on PATH and shadow it, so it goes. `qbzd` follows the package.
  INSTALL="sudo dpkg -i $REMOTE >/dev/null
  sudo rm -f /usr/local/bin/pibuz
  if [ -L /usr/local/bin/qbzd ]; then sudo ln -sf /usr/bin/pibuz /usr/local/bin/qbzd; fi"
else
  PAYLOAD="$BIN"
  REMOTE=/tmp/pibuz.new
  INSTALL="sudo install -m755 $REMOTE /usr/local/bin/pibuz
  sudo ln -sf /usr/local/bin/pibuz /usr/local/bin/qbzd
  if dpkg-query -W pibuz >/dev/null 2>&1; then
    echo '[pibuz-to-pi] note: a pibuz package is installed; this copy shadows it, and moOde shows the package version (use --deb)' >&2
  fi"
fi

if [[ $BUILD_ONLY == 1 ]]; then
  say "--build-only: stopping here"
  exit 0
fi

say "installing on $PI"
scp -q "$PAYLOAD" "$PI:$REMOTE"
# Install FIRST, over the running daemon: it keeps the old binary's inode, so
# replacing the file is safe, and the restart below is then the only gap.
#
# The gap must stay short. moOde's watchdog treats three seconds with no
# `pibuz` process as a crash and restarts it through stopQobuz(), whose
# `killall pibuz` takes out whatever this script started in the meantime.
# So on moOde, restart the way the watchdog does, `moodeutl -R --qobuz`:
# stopQobuz() returns as soon as the daemon has gone (SIGTERM, then SIGKILL
# after 3 s) and startQobuz() follows at once, with moOde's settings, its
# launch line and its session, so the debug-log setting decides whether the
# output lands in /var/log/moode_pibuz.log. moOde's worker runs its own
# stop/start when it sees the renderer go inactive, so expect the daemon to be
# restarted once more a couple of seconds later; that is moOde, not a crash.
#
# Elsewhere, stop the process (and the sudo that owns it) and relaunch it the
# way startQobuz() does. `pkill -f 'pibuz run'` is wrong here — the pattern
# matches the ssh command line running it and kills the shell mid-script.
# `qbzd` is the pre-rename process and binary name, which a box installed
# before 2.4.0 still runs.
ssh "$PI" "
  set -e
  $INSTALL
  rm -f $REMOTE
  if command -v moodeutl >/dev/null; then
    sudo moodeutl -R --qobuz >/dev/null
  else
    for name in pibuz qbzd; do
      pid=\$(pgrep -x \$name | head -1) || true
      [ -n \"\$pid\" ] || continue
      ppid=\$(ps -o ppid= -p \$pid | tr -d ' ')
      sudo kill \$ppid \$pid 2>/dev/null || true
      for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        pgrep -x \$name >/dev/null || break
        sleep 0.2
      done
      sudo kill -9 \$ppid \$pid 2>/dev/null || true
    done
    sudo sh -c 'LC_ALL=C QBZD_HOOK=/var/local/www/commandw/qbzevent.sh setsid nohup pibuz run >> /var/log/moode_pibuz.log 2>&1 < /dev/null &'
  fi
"
# The daemon needs a moment before the control API answers.
for _ in $(seq 1 20); do
  v=$(ssh "$PI" "curl -s --max-time 2 http://127.0.0.1:8182/api/status" 2>/dev/null | tr ',' '\n' | sed -n 's/.*"version":"\([^"]*\)".*/\1/p') || true
  [[ -n "${v:-}" ]] && { say "running on $PI: pibuz $v"; exit 0; }
  sleep 1
done
echo "[pibuz-to-pi] installed, but the control API did not answer — check /var/log/moode_pibuz.log" >&2
exit 1
