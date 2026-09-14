#!/usr/bin/env bash
#
# bump-version.sh — set the workspace version and everything that has to agree
# with it.
#
# The crate versions are NOT the hard part: all 15 crates carry
# `version.workspace = true`, so there is exactly one number, in
# `[workspace.package]`, and the binary reads it through `CARGO_PKG_VERSION`.
# What goes wrong is the things that have to agree with that number and are
# edited by hand at a different moment:
#
#   - `Cargo.lock`, which carries the version once per workspace crate. Nothing
#     is broken if it lags, but the tree is dirty at the moment you want to tag.
#   - the CHANGELOG's `## <version> — unreleased` heading.
#   - the tag. `release.yml` stamps the TAG's version into the binary through
#     QBZD_BUILD_ID, so a tag that disagrees with Cargo.toml ships a binary
#     whose `--version` does not match its own source. The release workflow
#     rejects that now; this keeps you from reaching it.
#
# USAGE
#   ./scripts/bump-version.sh 2.5.0        # set the version
#   ./scripts/bump-version.sh --check      # verify the tree agrees with itself
#   ./scripts/bump-version.sh --moode      # next moOde build id for this version
#
# It does not commit and does not tag: it prints the two commands, because
# "which commit is the release" is a decision, not a step.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo_version() {
	# The FIRST `version = ` under [workspace.package]. Dependency versions
	# appear later in the file and must not match.
	awk '/^\[workspace\.package\]/{f=1; next} f && /^version = /{gsub(/version = "|"/,""); print; exit}' Cargo.toml
}

changelog_version() {
	awk '/^## /{gsub(/^## | —.*| -.*/,""); print; exit}' CHANGELOG.md
}

die() { echo "error: $*" >&2; exit 1; }

CURRENT="$(cargo_version)"
[ -n "$CURRENT" ] || die "no [workspace.package] version in Cargo.toml"

case "${1:-}" in
--check)
	CL="$(changelog_version)"
	echo "Cargo.toml: $CURRENT"
	echo "CHANGELOG:  $CL"
	[ "$CURRENT" = "$CL" ] || die "CHANGELOG's top section is $CL, Cargo.toml is $CURRENT"
	# The lock is compared by CONTENT, not by whether it is committed: this has
	# to pass in the middle of a bump, when nothing is committed yet.
	LOCKED="$(awk '/^name = "pibuz"$/{getline; gsub(/version = "|"/,""); print; exit}' Cargo.lock)"
	[ "$LOCKED" = "$CURRENT" ] || die "Cargo.lock has pibuz at $LOCKED, Cargo.toml says $CURRENT"
	echo "consistent."
	exit 0
	;;
--moode)
	# moOde builds are tagged `pibuz-v<version>.moodeN`, where N is a counter
	# within that version. Deriving the base from Cargo.toml is the point: the
	# scheme previously kept a base of its own, which silently stayed at 2.0.2
	# while the tree moved on, so the binary announced a version the source had
	# not been at for months.
	git fetch --tags --quiet 2>/dev/null || true
	LAST="$(git tag --list "pibuz-v${CURRENT}.moode*" "qbzd-v${CURRENT}.moode*" \
		| sed 's/.*\.moode//' | sort -n | tail -1)"
	echo "${CURRENT}.moode$(( ${LAST:-0} + 1 ))"
	exit 0
	;;
"" | -h | --help)
	echo "usage: $0 <X.Y.Z> | --check | --moode"
	echo "current version: $CURRENT"
	exit 0
	;;
esac

NEW="$1"
[[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
	die "'$NEW' is not plain semver (no distro suffixes here — see --moode)"
[ "$NEW" != "$CURRENT" ] || die "already at $NEW"

# Cargo.toml — the [workspace.package] line only.
awk -v new="$NEW" '
	/^\[workspace\.package\]/ { f=1 }
	f && /^version = / && !done { print "version = \"" new "\""; done=1; next }
	{ print }
' Cargo.toml >Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

# CHANGELOG — rename the top section, whether it is still unreleased or the
# previous release (in which case a new unreleased section is opened above it).
CL="$(changelog_version)"
if head -20 CHANGELOG.md | grep -q "^## ${CL} — unreleased"; then
	perl -i -pe "s/^## \Q${CL}\E — unreleased$/## ${NEW} — unreleased/" CHANGELOG.md
else
	perl -i -pe "s/^(## \Q${CL}\E)/## ${NEW} — unreleased\n\n\n\$1/ if !\$done++" CHANGELOG.md
fi

cargo update --workspace --quiet

echo "$CURRENT -> $NEW  (Cargo.toml, Cargo.lock, CHANGELOG.md)"
echo
echo "next:"
echo "  git commit -am 'Version $NEW'"
echo "  git tag v$NEW && git push origin main v$NEW"
