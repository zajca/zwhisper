---
name: release
description: Cut a tagged zwhisper release end-to-end — changelog, version bump, tag, wait for the GitHub Actions release workflow to publish, then refresh the Arch PKGBUILD checksums. Use when the user says "release", "commit push release", "cut vX.Y.Z", or "push release".
---

# Release a zwhisper version

This is the full ritual behind "commit push release". `docs/RELEASE.md` is the
authoritative maintainer procedure — read it in the checkout you are releasing,
since steps may have moved. This skill adds the agent-facing sequencing: what to
verify, and how to watch CI to completion instead of assuming success.

Releases happen on `main`. Pre-1.0 versioning: bump the **minor** for new
features, the **patch** for fixes. `vX.Y.Z` below is the new tag.

## 0. Preconditions

```sh
git switch main && git pull --ff-only
git status --porcelain    # must be clean before you start
```

Confirm the current version so you know what you are bumping from:
`grep -m1 '^version' Cargo.toml`. Ask the user for the target version if they
did not give one — do not guess the bump level.

## 1. Changelog

Edit `CHANGELOG.md`:

- Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` (use today's date).
- Add a fresh empty `## [Unreleased]` at the top.
- Update the link-reference lines at the bottom.

Verify: `grep "^## \[X.Y.Z\]" CHANGELOG.md`. The release workflow extracts this
exact section as the GitHub Release notes — if it is empty or missing, the
`release` job fails.

## 2. Bump the workspace version + refresh the lockfile

Edit `version = "..."` under `[workspace.package]` in the root `Cargo.toml`, then:

```sh
cargo build -p zwhisperd -p zwhisper-cli --release --locked
```

`--locked` fails on manifest drift and regenerates `Cargo.lock`. Verify:
`grep '^version' Cargo.toml | head -1` and `git diff --stat Cargo.lock`.

## 3. Test gate

```sh
cargo test --workspace --release --no-fail-fast
```

Must be green. (If the M8 perf gate is relevant, also run
`cargo test -p zwhisperd --release --test m8_perf_gate -- --include-ignored`.)

## 4. Commit and tag

```sh
git add CHANGELOG.md Cargo.toml Cargo.lock
git commit -m "release: vX.Y.Z"      # no Co-Authored-By trailer, no emoji
git tag -s vX.Y.Z -m "zwhisper vX.Y.Z"
git push origin main vX.Y.Z
```

Signed tag (`-s`). Verify: `git log --oneline -1` shows the release commit and
`git tag --list vX.Y.Z` lists the tag.

## 5. Watch the release workflow to completion — do not assume

Pushing the tag triggers `.github/workflows/release.yml`. It builds the
`default` and `parakeet` daemon+CLI tarballs, attaches `.sha256` sidecars, and
publishes the GitHub Release with the changelog section as notes. **Wait for it
and confirm** — the PKGBUILD step below depends on the source tarball this run
produces.

```sh
# find the run for the tag and follow it to conclusion
gh run list --workflow release.yml --limit 5 --json databaseId,headBranch,status,conclusion
gh run watch <run_id> --exit-status        # non-zero exit if the run fails
```

Then confirm the release itself:

```sh
gh release view vX.Y.Z   # notes present + 4 assets: 2 tarballs + 2 .sha256
```

If the run fails, stop and fix forward (or roll back per docs/RELEASE.md
"Rollback"); do not proceed to packaging on a failed release.

## 6. Refresh the Arch PKGBUILD checksums

Once the tag's CI has published the `vX.Y.Z` source tarball:

```sh
cd packaging/arch
updpkgsums          # downloads the tarball, replaces the b2sums placeholder
```

`updpkgsums` also needs `pkgver` bumped to the new version. Verify no `SKIP`
remains: `grep b2sums packaging/arch/PKGBUILD`. Commit on `main`:

```sh
git add packaging/arch/PKGBUILD
git commit -m "packaging: bump pkgver and refresh b2sums for vX.Y.Z"
git push
```

Optionally dry-run the install (`cd packaging/arch && makepkg -si`, then
`pacman -Q zwhisper`) and walk the manual verification matrix in
`docs/M8-verification.md`.

## Done criteria

- `main` has both the `release: vX.Y.Z` and `packaging: … for vX.Y.Z` commits, pushed.
- Tag `vX.Y.Z` exists remotely; the release workflow run concluded **success**.
- `gh release view vX.Y.Z` shows notes + all four assets.
- `packaging/arch/PKGBUILD` has real `b2sums` (no `SKIP`) and the bumped `pkgver`.

## Rollback

If verification surfaces a regression: `git revert` the release commit on `main`,
delete the tag locally and remotely
(`git tag -d vX.Y.Z && git push --delete origin vX.Y.Z`), land the fix, and start
over. Never reuse a revoked tag for a different commit — bump the patch instead.
