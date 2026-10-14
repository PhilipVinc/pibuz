#!/usr/bin/env bash
# Narrow every `pub` in the workspace to what another crate actually uses, so
# rustc's dead-code lint can see the rest.
#
# `pub` items have no dead-code lint: anything reachable from a crate root
# counts as used, whether or not another crate ever touches it. This script
# turns every `pub` item, field, module and re-export into `pub(crate)`,
# builds, and puts back `pub` on exactly the names the compiler says another
# crate needs, round after round, until the workspace builds. What is left
# `pub(crate)` is invisible outside its crate, so from then on an unused one is
# a dead_code / unused_imports warning like any private item.
#
# It edits the tree in place: run it on a clean checkout and review the diff.
#
#   scripts/visibility-probe.sh              # narrow, then list what is dead
#   scripts/visibility-probe.sh --resume     # only restore what the build needs
#   scripts/visibility-probe.sh --report     # only list dead items (no narrowing)
#
# Platform code is cfg-gated (ALSA on Linux, CoreAudio on macOS), so a name
# only one platform's code uses is restored only on that platform. Run it on
# Linux (the arm64 container in CLAUDE.md), then with `--resume` on macOS over
# the result: a resumed run only ever turns `pub(crate)` back into `pub`.
#
# Restores are by NAME across the whole workspace, so two items sharing a name
# both stay `pub` when either is needed. That errs towards keeping things
# public, never towards breaking a build.
set -euo pipefail
cd "$(dirname "$0")/.."

FILES=$(find crates -name '*.rs' -not -path '*/target/*')
KINDS='(?:(?:async|const|unsafe) )*(?:fn|struct|enum|const|type|trait|static|mod)'

MODE="${1:-}"
if [[ "$MODE" != "--report" ]]; then
  if [[ "$MODE" != "--resume" ]]; then
    # Items, fields and re-exports. `pub(...)` forms are left as they are.
    perl -pi -e "s/^(\s*)pub ($KINDS) /\$1pub(crate) \$2 /; s/^(\s*)pub (use) /\$1pub(crate) \$2 /; s/^(\s*)pub ([a-z_][a-z0-9_]*\s*:)/\$1pub(crate) \$2/" $FILES
  fi

  for round in $(seq 1 60); do
    out=$(cargo build --workspace --all-targets --message-format short 2>&1 || true)
    # E0603 private item/module, E0624 private method, E0616/E0451 private
    # field, E0364/E0365 private re-export, and the private_interfaces /
    # private_bounds warnings for a pub(crate) type in a pub signature.
    names=$(printf '%s\n' "$out" \
      | grep -E 'error\[E0(603|624|616|451|364|365)\]|is more private than|private type .* in public interface' \
      | grep -oE '`[A-Za-z_][A-Za-z0-9_:]*`' | tr -d '`' | sed 's/.*:://' | sort -u || true)
    if [[ -z "$names" ]]; then
      if printf '%s\n' "$out" | grep -qE '^error|: error'; then
        echo "probe stopped on errors it does not know how to fix:" >&2
        printf '%s\n' "$out" | grep -E '^error|: error' | head -30 >&2
        exit 1
      fi
      echo "round $round: the workspace builds" >&2
      break
    fi
    echo "round $round: restoring $(echo "$names" | wc -l | tr -d ' ') names" >&2
    for n in $names; do
      perl -pi -e "s/pub\(crate\) ($KINDS) $n\b/pub \$1 $n/; s/^(\s*)pub\(crate\) ($n\s*:)/\$1pub \$2/" $FILES
      # A `use` group can span lines, so match the whole statement.
      perl -0pi -e "s/pub\(crate\) (use [^;]*\b$n\b[^;]*;)/pub \$1/g" $FILES
    done
  done
fi

# What the narrowed tree says is unused.
cargo build --workspace --all-targets --message-format short 2>&1 \
  | grep -E 'warning: .*(never used|never read|never constructed|unused import)' \
  | sed 's/^\([^:]*:[0-9]*\):[0-9]*: warning: /\1  /' | sort -u
