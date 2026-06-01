# Release process

The release procedure for cutting a tagged zwhisper version. Every
step has a single verify command on the same line so the maintainer
can copy-paste into a terminal and check `echo $?`.

> **Convention.** `vX.Y.Z` is the new release tag. The first packaged
> release is `v0.1.0`. Pre-1.0 releases bump the **minor** for new
> features and the **patch** for fixes — the protocol-version
> handshake (M8) is keyed on the workspace version, so any release
> bump propagates to the wire surface automatically.

## 1. Move the changelog forward

Edit `CHANGELOG.md`:

1. Replace the `## [Unreleased]` heading with `## [X.Y.Z] - YYYY-MM-DD`.
2. Add a fresh `## [Unreleased]` heading at the top.
3. Update the link references at the bottom.

Verify with `grep "^## \[X.Y.Z\]" CHANGELOG.md`.

## 2. Bump the workspace version

```sh
sed -i 's/^version = "0\.1\.0"$/version = "X.Y.Z"/' Cargo.toml   # adjust as needed
```

Verify with `grep '^version' Cargo.toml | head -1`.

## 3. Refresh `Cargo.lock`

```sh
cargo build --workspace --release --locked
```

The `--locked` flag verifies no manifest drift; the build also
exercises the Wayland-only FLTK source build so this is the first
place a broken cmake / gcc combination would surface.

Verify with `git diff --stat Cargo.lock` (one line changed: the
workspace `[package]` reference).

## 4. Verify ggml model checksums

```sh
scripts/refresh-checksums.sh
```

The script downloads each model listed in
`crates/zwhisper-settings/checksums.toml`, recomputes the SHA-256,
and exits non-zero on drift. **A non-zero exit blocks the release**
— upstream HuggingFace re-encoded a model and the embedded manifest
must be regenerated before the new version ships, otherwise users
will hit `download::tests::resume_re_hashes_from_zero_then_continues`
mismatches in production.

Verify with `echo $?` (must be `0`).

## 5. Run the full test suite

```sh
cargo test --workspace --release --no-fail-fast
```

Verify with the green summary line. Optionally run the perf gate:

```sh
cargo test -p zwhisperd --release --test m8_perf_gate -- --include-ignored
```

## 6. Commit and tag

```sh
git add CHANGELOG.md Cargo.toml Cargo.lock crates/zwhisper-settings/checksums.toml
git commit -m "release: vX.Y.Z"
git tag -s vX.Y.Z -m "zwhisper vX.Y.Z"
git push origin main vX.Y.Z
```

Verify with `git log --oneline -1` and `git tag --list vX.Y.Z`.

## 7. Regenerate PKGBUILD checksums

GitHub Actions auto-creates the source tarball at the tag URL.
Wait for the tag's CI run to publish the `v$pkgver.tar.gz` artifact,
then:

```sh
cd packaging/arch
updpkgsums
```

`updpkgsums` downloads the tarball and replaces the `b2sums`
placeholder. Commit the result on `main`:

```sh
git add packaging/arch/PKGBUILD
git commit -m "packaging: refresh b2sums for vX.Y.Z"
git push
```

Verify with `grep b2sums packaging/arch/PKGBUILD` (must not contain
`SKIP`).

## 8. Dry-run the package install

```sh
cd packaging/arch
makepkg -si
```

`-s` installs missing build deps (including the FLTK source-build
chain on a clean machine), `-i` installs the resulting `.pkg.tar.zst` so the
maintainer can also walk the manual verification gate
(`docs/M8-verification.md`).

Verify with `pacman -Q zwhisper` (reports `X.Y.Z-1`) and the
MV-1..MV-10 matrix.

## 9. Publish the GitHub release notes

Copy the new `CHANGELOG.md` section into the GitHub release UI for
the `vX.Y.Z` tag.

---

## Rollback

If MV-1..MV-10 surfaces a regression:

1. Revert the release commit on `main`: `git revert <release-commit>`.
2. Delete the tag locally and remotely:
   `git tag -d vX.Y.Z && git push --delete origin vX.Y.Z`.
3. File a tracking issue with the failing MV-N step.
4. Land the fix on `main` and start back at step 1.

A revoked tag must never be re-used for a different commit. If the
fix takes more than one cycle, bump the patch (`vX.Y.Z+1`).
