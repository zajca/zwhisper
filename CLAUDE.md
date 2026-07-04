# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

zwhisper is a CLI-first Linux tool for recording PipeWire audio (microphone +
system output) and transcribing it via local
[`whisper.cpp`](https://github.com/ggerganov/whisper.cpp), cloud Deepgram, or an
optional in-process Parakeet backend. A user daemon (`zwhisperd`) owns
recording and transcription; the `zwhisper` CLI is the control surface over a
single D-Bus contract (`cz.zajca.Zwhisper1`). Configuration is file-based
(versioned TOML profiles + secrets).

Target platform: Arch Linux, PipeWire, Wayland. X11, PulseAudio-only systems,
and non-Linux platforms are explicitly out of scope. See `README.md` for the
user-facing story and `IDEA.md` for the full architecture/roadmap.

## Workspace layout

Cargo workspace (`resolver = "3"`, edition 2024, MSRV 1.92). Version is
workspace-wide in `[workspace.package]` of the root `Cargo.toml`.

Active members (built and released):

- `crates/zwhisperd` — the user daemon: recording, transcription jobs, history, D-Bus service.
- `crates/zwhisper-cli` — the `zwhisper` binary (control surface, `deliver`, `model`, `profile`, `audio`, `status`, `toggle`).
- `crates/zwhisper-core` — shared domain: profiles, transcription backends (`transcribe/`), PCM decode, model registry.
- `crates/zwhisper-ipc` — D-Bus wire types and the protocol-version handshake.
- `crates/zwhisper-hotkey` — global hotkey via the XDG GlobalShortcuts portal.

`crates/zwhisper-settings` and `crates/zwhisper-tray` are **excluded** from the
workspace (retired FLTK GUIs) — they are not built, tested, or released. Do not
add them back into the release path.

## Build, test, and quality commands

Match CI (`.github/workflows/ci.yml`) — CI runs with `RUSTFLAGS: -D warnings`,
so warnings fail the build. Run these before pushing:

```sh
cargo fmt --all                                             # format (CI checks with --check)
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --no-fail-fast
cargo build --workspace --release --locked                 # --locked catches Cargo.lock drift
cargo doc --workspace --no-deps --all-features              # CI runs with RUSTDOCFLAGS: -D warnings
```

Building/testing needs the native build deps (GStreamer et al.) installed via
`.github/actions/install-build-deps`. The clippy config in `Cargo.toml` warns on
`unwrap_used`, `expect_used`, `panic`, `todo`, and `dbg_macro` — avoid them in
non-test code. `unsafe_code` is denied workspace-wide.

The `parakeet` feature is opt-in: it pulls ONNX Runtime via `ort` at build time
(network + a large native binary), so the default build excludes it. Only build
with `--features parakeet` when working on the Parakeet backend.

## Releases — what "commit push release" means

"commit push release" is the tagged-release ritual, **not** a plain
`git push`. `docs/RELEASE.md` is the authoritative step-by-step; the `/release`
skill (`.claude/skills/release/`) encodes the full flow including watching CI.
In short, a release is:

1. Move `CHANGELOG.md` `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a fresh `[Unreleased]`.
2. Bump the workspace version in `Cargo.toml`, refresh `Cargo.lock` (`cargo build --locked`).
3. Run the full test suite.
4. Commit `release: vX.Y.Z`, create a signed tag `vX.Y.Z`, push `main` + the tag.
5. Pushing the `vX.Y.Z` tag triggers `.github/workflows/release.yml`, which builds the
   `default` and `parakeet` daemon+CLI tarballs (with `.sha256` sidecars), extracts the
   matching `CHANGELOG.md` section as release notes, and publishes the GitHub Release.
6. After the tag's CI publishes the source tarball, bump `pkgver` and refresh `b2sums`
   in `packaging/arch/PKGBUILD` (`updpkgsums`), then commit
   `packaging: bump pkgver and refresh b2sums for vX.Y.Z` on `main`.

Always confirm the release workflow succeeded (`gh run watch` /
`gh release view vX.Y.Z`) before considering a release done — the packaging
`b2sums` step depends on the tarball CI produces.

## Domain facts worth knowing

- **Transcription language.** Users dictate in Czech or English. With autodetect
  (empty `transcription.language`), whisper.cpp/Deepgram sometimes mis-detect
  Polish. The language is a profile field (`transcription.language`, consumed in
  `crates/zwhisperd/src/jobs_service.rs`); pinning it or otherwise constraining
  detection to CS/EN is the intended fix, not treating the mis-transcription as a
  model bug.
- **Parakeet backend.** In-process, compiled only under the `parakeet` Cargo
  feature (`crates/zwhisper-core/src/transcribe/parakeet.rs`). A binary built
  without the feature must surface a clear "backend not compiled" error when a
  profile selects Parakeet — the audio setup can select it regardless of build
  flavour, so silent failure is a real hazard. Do not paper over it.
- **Wayland / typing limits.** `zwhisper deliver` injects transcripts. Direct
  type-at-cursor relies on `wtype` (wlroots-friendly); on compositors without it
  (GNOME/Mutter is treated as not supported for typing) the honest fallback is
  clipboard injection with a one-shot caveat, never a silent success. The
  clipboard handle (`arboard`) must be held for the process lifetime.

## Conventions

- Communicate with the user in Czech; write all code, comments, and docs in English.
- No emoji in output or commit messages. Commit messages: no `Co-Authored-By` trailer.
- Conventional-commit style prefixes are used (`feat:`, `fix:`, `style:`, `packaging:`, `release:`).
- Secrets resolve from `ZWHISPER_<BACKEND>_API_KEY` or `~/.config/zwhisper/secrets.toml`
  (mode 0600 enforced). Never read or print secret values.
