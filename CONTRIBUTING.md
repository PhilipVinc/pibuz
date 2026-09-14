# Contributing to Pibuz

This project is actively evolving. Contributions are welcome, but we have a few rules to keep releases stable and avoid regressions (especially around audio output).

## Where the code lives

Pibuz is **daemon-only**: the Rust workspace under `crates/` carries `pibuz` and
exactly the crates it depends on. There is no desktop UI here, and none is
planned — a change that needs one is out of scope for this project.

## Quick rules

- Write clear, concise English (no emojis in code, comments, or commit messages).
- Keep PRs focused and small when possible.
- Do not change app branding or legal disclaimers without discussing it first.
- Do not modify protected audio-backend behavior unless explicitly requested by the maintainer.

## Branch naming

We use a consistent branch naming scheme:

`<type>/<origin>/<branch_name>`

- `type`: `feature` | `bugfix` | `hotfix` | `refactor` | `release` | `chore` | `docs`
- `origin`:
  - `internal`: created/owned by maintainers
  - `external`: branches/commits authored by third-party contributors (PRs)

Examples:

- `feature/internal/offline-cache-encryption`
- `bugfix/internal/login-footer-alignment`
- `docs/internal/contributing-process`
- `feature/external/add-album-to-playlist`

## Branch workflow

`main` is the trunk: it is what releases are cut from, and the only branch CI
will release from. Work happens on topic branches and merges to `main`.

```
feature/xyz ──┐
bugfix/abc  ──┼──> main ──> tag vX.Y.Z ──> release
hotfix/123  ──┘
```

### Branch hierarchy

1. **`main`** — the trunk. PRs target it; CI keeps it green.
2. **`feature/*`, `bugfix/*`, etc.** — individual work branches.

### Releasing

Releases are tags, not merges. Tag a commit **that is already on `main`**:

```bash
./scripts/bump-version.sh 2.5.0     # Cargo.toml + Cargo.lock + CHANGELOG heading
git commit -am 'Version 2.5.0'
git tag v2.5.0
git push origin main v2.5.0
```

Versions are plain semver — `2.1.0`, `2.1.1`, `2.2.0-rc.1` — with no distro
suffixes. `Cargo.toml` is the source of truth, and every crate inherits it with
`version.workspace = true`, so there is one number to change. `release.yml`
refuses a tag that disagrees with it, because the tag is what gets stamped into
the binary: tagging `v2.5.0` on a 2.4.0 tree would ship a `pibuz --version` its
own source never said.

`./scripts/bump-version.sh --check` verifies Cargo.toml, Cargo.lock and the
CHANGELOG agree before you tag.


`release.yml` builds the aarch64 + amd64 tarballs and publishes a
prerelease GitHub Release. Its first job refuses any release tag whose commit is
not an ancestor of `main`, so tagging a topic branch fails loudly instead of
publishing something that is not on the trunk.

`build-arm64.yml` (manual dispatch, any ref) is the way to get a test binary
for the Pi without tagging.

### Procedure (maintainer)

1. **Triage**
   - Confirm scope and that it does not touch protected areas (audio routing/backends, credential storage, etc.) unless requested.
2. **Check out the PR**
   - `gh pr checkout <PR_NUMBER>`
3. **Rename the checked-out branch (local)**
   - Use an `external` branch name so it's obvious these commits are third-party authored:
   - `git branch -m <type>/external/<topic>`
4. **Run checks**
   - Build/validate a touched crate: `cargo check -p <crate>` (run from
     `crates/`), or the whole workspace with `./scripts/cargo-test.sh`.
5. **Merge to main** — `git merge --no-ff <type>/external/<topic>`, then push.

### Merge strategy note (to preserve “external” authorship)

If you want the git history to clearly show third-party authored commits, avoid “squash merge”.
Prefer:

- **Create a merge commit**, or
- **Rebase and merge** (preserves individual commits/authors)

## What to include in PRs

- A short description of the problem and solution.
- Notes about any breaking changes or migrations.

## What not to include

- Large refactors mixed with feature work.
- Desktop-UI changes — Pibuz is headless by design.
