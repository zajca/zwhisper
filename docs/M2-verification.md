# M2 — Profile system: verification

> Closes [docs/M2-plan.md](./M2-plan.md). M2 ships TOML profiles with
> mandatory `schema_version`, two-stage parse + version gate,
> in-place migration framework with atomic backups, replace-not-merge
> semantics, embedded shipped templates, and a `profile` subcommand
> surface. All 12 DoD items below are ticked with file:line + test
> evidence.

**Verdict: READY.** Verified on the maintainer's host on
2026-05-01: `cargo build --workspace` clean, `cargo clippy --workspace
--all-targets --all-features -- -D warnings` clean, `cargo test
--workspace` 155/155 green (135 unit + 7 cli + 11 profile + 2
transcribe), `zwhisper record --profile meeting` records 60 s of
mic + sink-monitor mono FLAC and emits a valid `.flac.txt` +
`.flac.json` transcript pair end-to-end.

## DoD checklist

### 1. `zwhisper record --profile meeting` records and transcribes

- Engine entry point: `crates/zwhisper-cli/src/cli.rs:run_record_with_profile` —
  resolves profile, expands `[[output]].path`, drives
  `record_blocking`, then runs `transcribe_file` when
  `transcription.auto = true`.
- End-to-end test: `crates/zwhisper-cli/tests/profile.rs:record_with_meeting_profile_runs_end_to_end`
  — passes 60-second mic + sink-monitor capture, asserts
  `flac -t` validity, asserts `.flac.txt` exists.
- Test runtime-skips on hosts without PipeWire or whisper-cli or
  any `~/.local/share/zwhisper/models/ggml-*.bin`; pattern matches
  M0/M1 runtime-skip discipline.
- Live verification log on maintainer's host (60s mic + sink monitor
  → mono 16 kHz FLAC, samples_written = 960 131, underruns = 0).

### 2. `--profile` xor raw flags on `record`

- `crates/zwhisper-cli/src/cli.rs:RecordArgs` declares `--profile`
  with `conflicts_with_all = ["mic", "monitor", "output", "duration",
  "max_duration_minutes", "transcribe", "model", "lang"]`, plus a
  `clap::ArgGroup("source-mode").required(true)` over `["profile",
  "output"]`.
- Tests: `record_with_profile_parses`,
  `record_profile_conflicts_with_output`,
  `record_profile_conflicts_with_transcribe_chain`,
  `record_either_profile_or_output_required` (in
  `crates/zwhisper-cli/src/cli.rs::tests`).

### 3. `--profile` xor `--backend / --model / --language` on `transcribe`

- `crates/zwhisper-cli/src/cli.rs:TranscribeArgs` declares
  `--profile` with `conflicts_with_all = ["backend", "model",
  "language"]`.
- Test: `transcribe_with_profile_conflicts_with_backend_flags`
  (`crates/zwhisper-cli/src/cli.rs::tests`).
- Profile-driven transcribe path: `cli.rs:run_transcribe_async`
  branches on `args.profile` and pulls the
  `[transcription]` block before delegating to
  `transcribe::transcribe_file`.

### 4. `zwhisper profile list`

- Implementation: `crates/zwhisper-cli/src/profile/commands.rs:list`
  — scans `${XDG_CONFIG_HOME}/zwhisper/profiles`,
  `${ZWHISPER_DATA_DIR:-/usr/share/zwhisper}/profiles`, then
  embedded names; renders a table with `name | source | ver |
  description` columns.
- `*.toml.bak.<ts>_<pid>_<seq>` files are filtered:
  `crates/zwhisper-cli/src/profile/commands.rs:scan_dir` uses an
  ASCII-case-insensitive `.toml` extension match plus a
  `contains(".toml.bak.")` short-circuit; covered by
  `scan_dir_filters_backup_suffix`.
- Black-box test:
  `crates/zwhisper-cli/tests/profile.rs:profile_list_shows_three_embedded_templates_on_clean_host`
  asserts `default` / `meeting` / `voicememo` + `embedded` source
  visible on a synthetic clean `XDG_CONFIG_HOME`.
- Live snapshot (clean home):
  ```
  name                      source      ver     description
  ------------------------------------------------------------------------
  default                   embedded    1       Plain mic capture, no auto-transcription. Backstop for users without a customized profile.
  meeting                   embedded    1       Mic + system sink monitor mono mix, auto-transcribed via whisper.cpp. M2 ships mono_mix; stereo_split is the M3+ roadmap.
  voicememo                 embedded    1       Mic-only mono mix optimized for short voice memos with auto-transcription.
  ```

### 5. `zwhisper profile show <name>`

- `crates/zwhisper-cli/src/profile/commands.rs:show` prints
  `source: <label> (<path>)`, then a `---` separator, then the
  resolved (post-migration) TOML body via
  `toml_edit::ser::to_string_pretty`.
- Test: `tests/profile.rs:profile_show_meeting_prints_source_and_body`.

### 6. `zwhisper profile clone <src> <dst>`

- `crates/zwhisper-cli/src/profile/commands.rs:clone` resolves
  `<src>` (any source), rewrites the `name` field to `<dst>` via
  `dst.clone_into(&mut profile.name)`, and writes
  `${XDG_CONFIG_HOME}/zwhisper/profiles/<dst>.toml`.
- Refuses to overwrite via
  `ProfileError::OverwriteRefused { path }` (no `--force`).
- Tests:
  `tests/profile.rs:profile_clone_creates_user_override_and_refuses_overwrite`,
  `tests/profile.rs:profile_clone_rejects_invalid_destination_name`,
  `tests/profile.rs:profile_clone_unknown_source_returns_not_found`.
- Unit tests: `crates/zwhisper-cli/src/profile/commands.rs::tests:clone_into_dir_writes_user_profile_with_renamed_field`,
  `clone_into_dir_refuses_existing_target`.

### 7. `zwhisper profile migrate <name>` no-op + idempotent

- `crates/zwhisper-cli/src/profile/commands.rs:migrate` only
  operates on the user override path; returns a help-style error
  when the profile is shipped/embedded.
- Loader short-circuits on `from >= to`:
  `crates/zwhisper-cli/src/profile/migrations.rs:run_in_place_with`
  — no backup, no rewrite when no migration is needed.
- Tests: `tests/profile.rs:profile_migrate_no_op_at_current_version`
  (asserts `*.bak.*` absent), `profile_migrate_refuses_when_user_override_missing`,
  unit `idempotency_short_circuits_when_from_ge_to`.

### 8. Schema versioning enforced (four-branch loader)

`crates/zwhisper-cli/src/profile/loader.rs:load_from_path`
implements the four-branch behaviour from IDEA.md § 6:

| Branch | Behaviour | Test |
|---|---|---|
| `schema_version` missing / non-int / `<= 0` | `MissingSchemaVersion { path }` | `loader::tests::missing_schema_version_is_typed_error`, `…schema_version_string_rejected_as_missing`, `…schema_version_zero_rejected_as_missing` |
| `schema_version > CURRENT` | `UnsupportedSchemaVersion { path, found, current }` | `loader::tests::schema_version_too_high_rejected` |
| `schema_version < CURRENT` | backup `<file>.bak.<unix_nanos>_<pid>_<seq>` (atomic `OpenOptions::create_new`), run migration chain, atomic in-place rewrite | `migrations::tests::run_in_place_applies_chain_and_pins_version`, `migrations::tests::backup_uses_create_new_and_unique_suffix`, `migrations::tests::missing_migration_returns_typed_error`, `migrations::tests::failing_migration_propagates_with_chain_step_versions`, `migrations::tests::idempotency_short_circuits_when_from_ge_to` |
| `schema_version == CURRENT` | deserialize + validate | `loader::tests::happy_path_v1_loads_and_validates` |

Embedded templates (`load_from_str`) reject any version mismatch as
`MigrationFailed`: read-only build artefacts cannot be migrated;
caught by `loader::tests::load_from_str_rejects_mismatched_version_as_migration_failed`.

### 9. Replace-not-merge is honest

- The migration framework runs registered migrations top-down; each
  migration sets the values it adds (no runtime fallback inside the
  loader). `crates/zwhisper-cli/src/profile/migrations.rs:apply_chain`
  walks `(from, to)` tuples; missing steps return a typed
  `MigrationFailed { from, to, source: "no registered migration for
  <f> -> <t>" }`.
- The loader never silently fills holes:
  `crates/zwhisper-cli/src/profile/loader.rs:deserialize_validated`
  surfaces serde's `MissingField`-style errors as
  `TomlDeserialize { path, source }`.
- M2 ships zero registered migrations (v1 is the first locked
  version); the framework + tests + atomic backup logic land here
  so v2 is mechanical.

### 10. Embedded shipped templates

- `crates/zwhisper-cli/profiles/{default,meeting,voicememo}.toml`
  embedded via `include_dir!` in
  `crates/zwhisper-cli/src/profile/embedded.rs`.
- Filesystem search precedes embedded:
  `crates/zwhisper-cli/src/profile/mod.rs:resolve` honours user
  override > shipped > embedded.
- Tests: `embedded::tests::names_contains_shipped_profiles`,
  `embedded::tests::every_embedded_profile_loads_and_validates`,
  `mod::tests::resolve_falls_back_to_embedded_for_known_name`,
  `mod::tests::resolve_unknown_returns_not_found_with_three_locations`.

### 11. Hardcoded `DEFAULT_MODEL` / `DEFAULT_LANGUAGE` constants gone

- Pre-M2: `crates/zwhisper-cli/src/cli.rs:12-13` declared
  `const DEFAULT_MODEL: &str = "small"` and
  `const DEFAULT_LANGUAGE: &str = "auto"`.
- M2 removes both. `RecordArgs` `--model` / `--lang` are now
  `Option<String>` with
  `required_if_eq("transcribe", "whisper-cpp")` so the user must
  pass them explicitly when `--transcribe` is set, or use
  `--profile <name>` (the embedded `default` profile preserves the
  same effective values).
- Test: `cli::tests::record_with_transcribe_requires_model_and_lang`
  asserts clap rejects the previously-defaulted shape.

### 12. Verification doc

- This file. Cross-linked from
  [docs/M0-plan.md](./M0-plan.md) and [docs/M1-plan.md](./M1-plan.md)
  status snapshots.

## Test runs

```
cargo test --workspace
…
test result: ok. 135 passed; 0 failed; 0 ignored
test result: ok. 7 passed; 0 failed; 0 ignored      (tests/cli.rs)
test result: ok. 11 passed; 0 failed; 0 ignored     (tests/profile.rs)
test result: ok. 2 passed; 0 failed; 0 ignored      (tests/transcribe.rs)
```

```
cargo clippy --workspace --all-targets --all-features -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

## Post-review fixes round 2 (2026-05-01 evening, after second pass)

The first set of fixes left two High threads dangling that the
second review caught:

- **High — half-wired mic-only abstraction.** Round-1 fixes added
  `RecordOptions::monitor: Option<String>`, `DeviceSelection::monitor_node:
  Option<String>`, and a `pipeline::build` branch that returned
  `PipelineFailed` for the `None` case. The plumbing existed but
  was never reached for any happy path — it was forward-compat for
  M3 mic-only that the comments oversold. Fix: reverted both fields
  to plain `String`; `devices::resolve` now rejects `monitor_arg ==
  ""` with a typed `DeviceError::InvalidArgument` whose message
  points at the M3 land. The pipeline shape is back to the M0
  mic+monitor mono mix only. Regression:
  `audio::devices::tests::empty_monitor_arg_returns_typed_invalid_argument`.
- **High — misleading shipped-profile descriptions.** The round-1
  fix changed `default.toml` and `voicememo.toml` to `system_output =
  "default"` (so the schema validator would accept them) but left
  the descriptions claiming "Plain mic capture" and "Mic-only mono
  mix optimized for short voice memos". A user picking those names
  would not realise system audio was being captured. Fix: rewrote
  both descriptions to spell out the M2 reality — "Mic + system
  sink monitor mono mix …" — and explicitly note that a mic-only
  preset lands in M3. The profile *names* stay because they are
  the M3 lock-in identifiers.

## Post-review fixes round 1 (2026-05-01 evening)

The first M2 sign-off pass was reviewed and four findings landed:

- **High — `system_output = ""` was silently coerced to `"default"`**
  in `cli.rs:run_record_with_profile`. The mapping turned a profile
  intent of "mic-only capture" into "mic + sink monitor" — an
  audio-content surprise. Fix (final state after round 2):
  - `Profile::validate` rejects empty `system_output` with a typed
    error pointing at "M3 mic-only mode" and suggesting
    `system_output = "default"`.
  - `devices::resolve` rejects `monitor_arg == ""` with a typed
    `DeviceError::InvalidArgument`, so the bare-flag CLI path
    (`--monitor ""`) gets the same honest rejection as the profile
    path.
  - Shipped `default.toml` and `voicememo.toml` updated to
    `system_output = "default"`; descriptions rewritten in round 2
    to call out the actual capture shape rather than the original
    "Plain mic" / "Mic-only" promises.
  - Regression: `tests/profile.rs::empty_system_output_rejected_at_validate_time`,
    `audio::devices::tests::empty_monitor_arg_returns_typed_invalid_argument`,
    and the `Profile::validate` unit covers the schema branch.
- **Medium — `sample_rate` propagation gap.** Schema accepted 16000
  / 44100 / 48000 while the recorder was hardcoded to 16 kHz; user
  config was silently ignored. Fix: `Profile::validate` now rejects
  44100 and 48000 with the same "M3 land" message; shipped profiles
  already use 16 kHz so no template change. Regression in
  `src/profile/schema.rs::tests::validate_rejects_44100_and_48000_in_m2`.
- **Medium — TOCTOU in `profile clone`.** The `target.exists()`
  check followed by `fs::create` had a race where a parallel writer
  between the check and the open got truncated. Fix: switched to
  `OpenOptions::new().create_new(true).write(true).open(&target)`
  in both the production `clone` and the test-only `clone_into_dir`
  helpers; the typed `OverwriteRefused { path }` is now produced
  atomically. Existing tests still cover the behaviour.
- **Low — `option_env!` is compile-time.** `paths::shipped_path`
  used `option_env!("ZWHISPER_DATA_DIR")`, so the integration tests'
  `env_remove("ZWHISPER_DATA_DIR")` was a no-op. Tests were green
  on the maintainer's host because `/usr/share/zwhisper/profiles/`
  does not exist there, but on a host with that dir populated the
  `profile show meeting` test would have flipped to `source: shipped`.
  Fix: switched to `std::env::var_os` (runtime) in both
  `paths::shipped_path` and `commands::shipped_profiles_dir`. Same
  fallback default; environment isolation now works for tests and
  for distro packagers that override at install time.

## Deviations from the plan

1. **Backup naming evolved from `<unix_ms>_<pid>` to
   `<unix_nanos>_<pid>_<seq>`.** The plan's millisecond + PID guard
   is enough for parallel-process collisions, but two in-process
   backups inside the same nanosecond are realistic on a fast box
   (the test suite hit it). Adding an `AtomicU64` sequence keeps
   `OpenOptions::create_new` honest without giving up the typed
   `BackupFailed` for cross-process races.
2. **`run_in_place_with` short-circuits when `from >= to`.** The
   plan called for the loader to skip the migration call when
   `found == CURRENT`; the framework now also self-skips, so a
   forced `profile migrate` against an up-to-date profile produces
   no spurious backup. Both callers stay aligned.
3. **`Profile::primary_output_path` returns `Option<PathBuf>`, not
   `Option<Result<PathBuf, ProfileError>>`.** Path expansion now
   always succeeds at this point because the validate step (run
   immediately before during deserialization) has already preflighted
   `{token}` syntax and confirmed `~` expansion is reachable; the
   nested `Result` was redundant and clippy flagged it.
4. **Stereo-split rejection moved to `Profile::validate`.** The
   plan placed it in the engine call site; doing it during
   `validate()` means `zwhisper profile show meeting` (a pure
   config-plane command) catches a stereo profile before the user
   tries to record with it. M0/M1 callsites still see the error
   first because the profile loader runs before any audio init.
5. **`--profile` flag does not gate GStreamer init**: the
   `Profile` arm in `main.rs` skips `init_gstreamer()`, so
   `zwhisper profile list` runs on hosts without GStreamer plugins.
   Documented in the plan, called out here for the M3 daemon split.

## Risks carried into M3

- **`shellexpand::tilde` vs daemon `ProtectHome=read-only`.** The CLI
  process expands `~` against the live `$HOME`. M3 daemon hardening
  may make the expanded path inaccessible from the daemon side;
  the M3 plan must include this in the systemd `ReadWritePaths`
  audit before flipping `ProtectHome` on.
- **Embedded vs system shipped collision.** A user who installs
  zwhisper from a distro package gets `/usr/share/zwhisper/profiles/`
  populated; the build's embedded copy still exists. They share
  filenames; the resolver prefers the system copy. If the distro
  ships an older schema_version, the loader will write a backup
  next to it (the system path is still writable for system users
  but not for normal ones — a `BackupFailed` is the typed error).
  The cleanest fix lands in M3 alongside the daemon split:
  shipped profiles become read-only sources, migrations always
  copy-to-user before mutating.
- **Stereo-split is parsed but rejected.** Future M2.5 / M3 must
  expand the GStreamer pipeline (`interleave` instead of
  `audiomixer`) and remove `Profile::validate`'s
  `UnsupportedMode` branch in the same commit. No schema bump.

## Sign-off

M2 closes 2026-05-01 with all 12 DoD items ticked and the
verification artefacts above. Schema_version = 1 is the first
locked profile schema; subsequent `Profile` field changes go
through `migrations::MIGRATIONS` rather than ad-hoc loader
branches.
