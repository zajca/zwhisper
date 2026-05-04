# M2 — Profile system: implementation plan

> Target milestone from [IDEA.md § 11](../IDEA.md#11-roadmap). Builds
> on M0 (audio walking skeleton) and M1 (whisper.cpp post-process)
> and turns the bag of `--mic / --monitor / --output / --transcribe
> / --model / --lang` flags into a TOML profile system with
> schema versioning, in-place migrations, replace-not-merge
> semantics, and a small `profile` subcommand surface.

## Status snapshot (2026-05-01)

| Area | State | Evidence |
|---|---|---|
| `RecordArgs` accepts a `--profile` selector | not done | `crates/zwhisper-cli/src/cli.rs:17` (no `profile` field) |
| `TranscribeArgs` accepts a `--profile` selector | not done | `crates/zwhisper-cli/src/cli.rs:65` (no `profile` field) |
| `Profile` struct / TOML schema | not done | no `crates/zwhisper-cli/src/profile/` module yet |
| `ProfileError` typed errors | not done | mirrors `transcribe::error::TranscribeError` shape from M1 |
| Schema-version gate (missing/`>current`/`<current`/`==current`) | not done | IDEA.md § 6 mandates the four-branch behaviour |
| Migration framework (chain of `fn(toml::Value) -> Value`) | not done | even though no migrations exist for v1, the framework lands in M2 so v2 is mechanical |
| Shipped templates (`meeting`, `voicememo`) | not done | none committed; IDEA.md § 6 schema is the source of truth |
| Embedded fallback templates | not done | required so `cargo install` users (no system package) get working defaults |
| `profile list / show / clone / migrate` CLI | not done | DoD includes `zwhisper profile clone meeting my-meeting` |
| Hardcoded `DEFAULT_MODEL` / `DEFAULT_LANGUAGE` | present | `crates/zwhisper-cli/src/cli.rs:12-13` — must be sourced from a shipped profile, not a literal |

**Verdict:** M2 is greenfield. Module wiring + clap surface + TOML
deserialisation + migration runner all need to be built. M0/M1
plumbing (`audio::recorder::RecordOptions`, `transcribe::TranscribeOpts`)
stays as the engine input shape; the profile loader produces those
structs, it does not replace them.

## Definition of done (verbatim from IDEA.md § 11 + § 6)

1. `zwhisper record --profile meeting` records a FLAC and runs the
   profile's transcribe step end-to-end without any other CLI flags
   needed.
2. `zwhisper record --profile <x>` is **mutually exclusive** with the
   raw flags (`--mic / --monitor / --output / --duration /
   --max-duration-minutes / --transcribe / --model / --lang`) — clap
   rejects mixed invocations with a clear error.
3. `zwhisper transcribe <audio> --profile <x>` reuses the profile's
   `[transcription]` block. `--profile` mutually exclusive with
   `--backend / --model / --language`.
4. `zwhisper profile list` lists the resolved set, marking source
   (`[user]`, `[shipped]`, `[embedded]`). `*.toml.bak.*` files are
   filtered out.
5. `zwhisper profile show <name>` prints the resolved TOML body and
   the source path.
6. `zwhisper profile clone <src> <dst>` copies the resolved profile
   into `${XDG_CONFIG_HOME:-~/.config}/zwhisper/profiles/<dst>.toml`,
   rewriting the `name` field. Refuses to overwrite an existing
   destination (no `--force` in M2).
7. `zwhisper profile migrate <name>` re-runs the migration chain
   against the user override; no-op if `schema_version ==
   CURRENT_SCHEMA_VERSION`. Idempotent.
8. **Schema versioning enforced** (the four branches from IDEA.md
   § 6):
   - `schema_version` missing → typed `MissingSchemaVersion` error
     pointing at the file.
   - `schema_version > CURRENT` → typed `UnsupportedSchemaVersion`
     error: "from a newer zwhisper, please upgrade".
   - `schema_version < CURRENT` → backup as
     `<file>.bak.<unix_ms>_<pid>` (atomic create-new, fails if a
     parallel process already wrote one), then run the migration
     chain in-place, then load.
   - `schema_version == CURRENT` → load.
9. **Replace-not-merge** is honest: a migration that adds a new
   required field writes the explicit default *inside the migration
   function*. The runtime profile loader never silently fills holes
   with hardcoded constants; missing fields after migration are
   `Validation` errors. (Closes the "hidden merge" attack from the
   devils-advocate review.)
10. Shipped templates (`meeting`, `voicememo`) are embedded into the
    binary via `include_dir!` so `cargo install zwhisper-cli` works
    on a host without `/usr/share/zwhisper/`. Filesystem search
    (`/usr/share/zwhisper/profiles/`) still happens first for
    distro-installed builds.
11. `crates/zwhisper-cli/src/cli.rs:12-13`'s `DEFAULT_MODEL` /
    `DEFAULT_LANGUAGE` constants are removed; defaults come from
    the bundled `default.toml` profile (or from the active profile
    when one is selected).
12. `docs/M2-verification.md` ticks off all of the above with
    file:line evidence (test name, log line, actual TOML output).

## Out of scope (deferred to later milestones)

- Profile-driven daemon orchestration / D-Bus `SetActive` / `Reload`
  signals (M3 — IDEA.md § 2 D-Bus interface).
- `[hotkey]` table is **parsed and ignored** in M2; the field round-trips
  but no key binding is registered (M6 — `xdg-desktop-portal`).
- `[[output]]` table is parsed; only `type = "file"` is honoured
  (FileSink lives in the engine today). `type = "clipboard"` /
  `"notification"` are valid in TOML but produce a `tracing::warn!`
  about deferral to M4 (tray).
- Cloud backends (`deepgram`, `assemblyai`, `openai`) are valid
  identifiers in `[transcription].backend`, but the runner returns
  `BackendUnknown` for anything other than `whisper-cpp` (M5).
- Stereo-split mode is parsed (`mode = "stereo_split"`); engine
  returns `UnsupportedMode` typed error (M2.5 / M3 — pipeline
  interleave path). `meeting` template ships with `mode = mono_mix`
  so DoD #1 passes.
- Settings GUI / template downloader (M7).
- Schema migrations beyond the v1 framework (no v2 ships in M2 —
  the framework is exercised by tests, not real migrations).

## Non-goals for M2

- **No `merge` semantics, ever.** A user override is a full profile.
  Adding new fields in v2 is a migration responsibility (the
  migration function writes the explicit default), not a runtime
  fallback.
- **No silent schema_version defaults.** A missing
  `schema_version` is a typed error, not "assume v1". This protects
  against future v2 fields landing in a v1 file by accident.
- **No `.bak` cleanup.** Backups stay until the user removes them.
  `profile list` filters them; that is enough.
- **No path-traversal-tolerant lookup.** Profile names are validated
  against `[A-Za-z0-9._-]+`; `..`, `/`, and shell metacharacters
  are rejected before any filesystem call.
- **No interactive prompts.** Migration runs unconditionally on
  load if `schema_version < CURRENT`; the only user-visible signal
  is a `tracing::info!` log line and the `.bak` file.

## Architecture for M2

Single binary `zwhisper`, same workspace as M0/M1. New module under
`zwhisper-cli/src/profile/`, plus a sibling `profiles/` directory
inside the crate that gets embedded via `include_dir!`:

```
crates/zwhisper-cli/
├── profiles/                    # NEW: shipped templates, embedded into the bin
│   ├── default.toml             # absorbs DEFAULT_MODEL / DEFAULT_LANGUAGE constants
│   ├── meeting.toml             # mic + sink monitor mono_mix → whisper-cpp small
│   └── voicememo.toml           # mic only mono_mix → whisper-cpp small
├── src/
│   ├── main.rs                  # entrypoint
│   ├── cli.rs                   # extended in M2: --profile flag, mutex groups, ProfileCmd
│   ├── audio/                   # M0 (unchanged surface; new From<&Profile> impl)
│   ├── transcribe/              # M1 (unchanged surface; new From<&Profile> impl)
│   └── profile/                 # NEW
│       ├── mod.rs               # public façade: load(), resolve(), Profile re-export
│       ├── schema.rs            # Profile + nested types + Mode/Codec/Backend/OutputDest
│       ├── error.rs             # ProfileError (thiserror) — variants stable across M2→M3
│       ├── paths.rs             # search order: user → shipped → embedded; name validator
│       ├── loader.rs            # read → version-gate → migrate-if-needed → deserialize → validate
│       ├── migrations.rs        # registry [(from, to, fn)] + apply_chain; backup naming
│       ├── embedded.rs          # include_dir! macro + lookup by name
│       └── commands.rs          # `profile list/show/clone/migrate` handlers
└── Cargo.toml                   # adds toml_edit, include_dir, shellexpand (workspace deps)
```

Rationale: same separation pattern as `transcribe/` (one module per
concern, `error.rs` next to `schema.rs`, `commands.rs` keeps
`cli.rs` thin). Migrations live in their own file because M3 daemon
will register the same chain at startup — keeping it import-free of
`audio::` / `transcribe::` is what allows the eventual `zwhisper-core`
crate split to be mechanical.

### Public API rules (M3 lock-ins)

These are non-negotiable for M2 because reversing them later means
breaking the M3 D-Bus surface (IDEA.md § 2 `cz.zajca.Zwhisper1.Profiles`
interface):

1. **`Profile` struct is the canonical wire shape** — `pub`, derives
   `Serialize + Deserialize + Clone + Debug`. M3 D-Bus
   `Profiles.List() -> a(ssu)` returns `(name, description,
   schema_version)` tuples; the `Profile` struct must produce all
   three without ad-hoc adapters.

2. **`CURRENT_SCHEMA_VERSION` is a `pub const u32`** in
   `profile::mod.rs`. Daemon and CLI must agree at handshake time
   (M3); diverging versions are a failed-startup condition, not a
   runtime warning.

3. **`load_profile(name) -> Result<Profile, ProfileError>`** is the
   only public entry point that takes a profile name. M3 daemon
   calls it; M3 CLI sends the *name* across D-Bus, the daemon
   resolves locally. We do **not** ship a "load by path" API on the
   public surface — paths are an implementation detail of the
   resolver.

4. **`ProfileError` is `thiserror`-based, one variant per failure
   class.** No `String`-blob errors:
   - `NotFound { name: String, searched: Vec<PathBuf> }` — name did
     not resolve in any of the three sources.
   - `InvalidName { name: String }` — name failed `[A-Za-z0-9._-]+`
     validation. Surfaces *before* any I/O.
   - `Io { path: PathBuf, source: io::Error }` — read/write failed.
   - `TomlParse { path: PathBuf, source: toml_edit::TomlError }` —
     file is not valid TOML.
   - `MissingSchemaVersion { path: PathBuf }` — top-level
     `schema_version` key absent or non-integer.
   - `UnsupportedSchemaVersion { path: PathBuf, found: u32, current: u32 }`
     — `found > current`. Forward-compat reject.
   - `MigrationFailed { path: PathBuf, from: u32, to: u32, source: Box<dyn StdError> }`
     — a registered migration function returned `Err`.
   - `BackupFailed { path: PathBuf, source: io::Error }` — could
     not create `.bak.<ms>_<pid>` (e.g., parallel process won the
     race; this is the right typed error).
   - `Validation { profile: String, message: String }` — semantic
     check after deserialisation (rate range, codec enum, mode
     enum, language string shape).
   - `UnsupportedMode { mode: Mode }` — `stereo_split` requested in
     M2 (engine doesn't implement it yet).
   - `BackendUnknown { backend: String, supported: Vec<&'static str> }`
     — `[transcription].backend` is not `whisper-cpp` in M2.
   - `OverwriteRefused { path: PathBuf }` — `profile clone <src>
     <dst>` where `<dst>.toml` already exists in the user dir.

5. **`Profile`-to-engine conversion lives in the call sites, not in
   `profile/`.** `impl From<&Profile> for RecordOptions` is in
   `audio/recorder.rs`; `impl From<&Profile> for TranscribeOpts` is
   in `transcribe/mod.rs`. This keeps `profile/` free of `audio::`
   / `transcribe::` imports — important for the M3 `zwhisper-core`
   extraction.

6. **Shipped template names are stable identifiers.** `meeting`,
   `voicememo`, `default`. M3 daemon profile activation refers to
   these names; renames are M3-breaking. The TOML `name` field
   inside each file matches the filename.

### Schema (TOML, schema_version = 1)

This is the schema all v1 profiles must match. Anything else is a
`Validation` or `MissingSchemaVersion` error.

```toml
schema_version = 1                  # required, integer
name           = "Meeting"          # required, string
description    = "..."              # optional, string

[sources]
mic           = "default"           # required; node name or "default"
system_output = "default"           # optional; "" or absent disables sink monitor
mode          = "mono_mix"          # required; mono_mix | stereo_split

[recording]
codec                  = "flac"     # required; flac (only one accepted in M2)
sample_rate            = 16000      # required; 16000 | 44100 | 48000
max_duration_minutes   = 240        # required; >0 (0 explicitly opts out)

[transcription]
backend  = "whisper-cpp"            # required; whisper-cpp accepted in M2
model    = "small"                  # required; resolves via M1 model resolver
language = "auto"                   # required; ISO 639-1 or "auto"
auto     = true                     # required; run after stop?

# Optional [[output]] tables — M2 honours type = "file" only;
# clipboard/notification log a deferral warning.
[[output]]
type = "file"
path = "~/Recordings/zwhisper/{profile}/{timestamp}"

# Reserved for M6; M2 round-trips these but does not register
# anything with the system.
[hotkey]
toggle = ""                         # optional; "" means none
```

Path expansion rules in `[[output]].path`:
- `~` → `dirs::home_dir()` (via `shellexpand::tilde`); fail with
  `Validation` if `home_dir` is `None`.
- `{timestamp}` → `chrono::Local::now()` formatted as
  `%Y-%m-%dT%H-%M-%S` (filesystem-safe, no colons).
- `{profile}` → the resolved profile name.
- Any other `{token}` is a `Validation` error: surfaces typo
  *before* the recording starts.

### Migration framework

```rust
pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;

pub(crate) type MigrationFn =
    fn(&mut toml_edit::Document) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub(crate) static MIGRATIONS: &[(u32 /*from*/, u32 /*to*/, MigrationFn)] = &[
    // Empty in M2. Framework + tests exist so v2 (M3-or-later) is
    // a 1-line registry append + 1 migration function.
];
```

Loader sequence (`loader::load_from_path`):

1. `fs::read_to_string` → string body. I/O error → `Io`.
2. Parse as `toml_edit::Document`. Parse error → `TomlParse`.
3. Read top-level `schema_version` as `i64`. Missing or non-int →
   `MissingSchemaVersion`. Negative or zero → same error.
4. `found > CURRENT_SCHEMA_VERSION` → `UnsupportedSchemaVersion`.
5. `found < CURRENT_SCHEMA_VERSION`:
   - **Backup first**: write the *original* body to
     `<file>.bak.<unix_ms>_<pid>` via `OpenOptions::create_new` so a
     parallel process collision returns `BackupFailed` instead of
     overwriting a previous backup. The PID guard handles the
     same-millisecond CLI+daemon race documented in the
     devils-advocate review.
   - Walk the registered `MIGRATIONS` chain step-by-step (`from →
     to`). If a step returns `Err`, abort with `MigrationFailed`;
     the `.bak` is the user's recovery.
   - After the chain succeeds, set
     `doc["schema_version"] = CURRENT_SCHEMA_VERSION` and write
     atomically: `tempfile::NamedTempFile` in the same directory →
     `persist`. (Cross-fs is impossible because the temp is in the
     same dir; no `EXDEV` branch needed here.)
6. Deserialize the (now-current) `Document` into `Profile`.
7. `Profile::validate()` — runtime invariants (rate range, codec
   enum, language shape, path expansion preflight). Failure →
   `Validation`.

**Migration idempotency** is not opt-in — the loader runs the chain
*at most once per call*, and only when `found < CURRENT`. A user
who runs `profile migrate` twice gets a no-op the second time
(`found == CURRENT`); a user who triggers a load after a partial
crash sees a fresh `.bak` with a different timestamp. No `.bak`
deduplication; we lean on the timestamp + PID for uniqueness.

**Embedded templates are read-only**: they are `&'static str`
constants, never modified, never backed up. If the embedded TOML
ships with `schema_version != CURRENT`, that is a build-time bug
caught by Phase 5 unit tests, not a runtime migration trigger.

### Profile resolution order

`paths::resolve_source(name) -> Result<ProfileSource, ProfileError>`:

```rust
pub(crate) enum ProfileSource {
    UserOverride(PathBuf),  // ~/.config/zwhisper/profiles/<name>.toml
    Shipped(PathBuf),       // /usr/share/zwhisper/profiles/<name>.toml
    Embedded(&'static str), // crate::profile::embedded::lookup(name).unwrap()
}
```

Search order:

1. **User override**:
   `dirs::config_dir()?.join("zwhisper/profiles").join(format!("{name}.toml"))`
   — exists? return `UserOverride`.
2. **Shipped (system)**: `/usr/share/zwhisper/profiles/<name>.toml`
   (configurable at compile time via `option_env!("ZWHISPER_DATA_DIR")`,
   default `/usr/share/zwhisper`) — exists? return `Shipped`.
3. **Embedded** (compiled into the binary via `include_dir!`):
   exists? return `Embedded`.
4. None matched → `NotFound { name, searched: [user, shipped, "embedded"] }`.

Tie-break: **user override wins**, even if the user copy is *older*
or *less complete* than shipped. This is the v1 IDEA.md contract;
shipped templates are starting points, not authority.

`profile list` performs a directory scan with `OsStr.ends_with(".toml")`
**and** rejects anything matching `*.toml.bak.*`; embedded entries
come from `embedded::names()`. Each entry carries its `ProfileSource`
tag for the table renderer.

### Subcommand contract

All four `profile` subcommands run *without* GStreamer init — pure
config-plane work. `main.rs` does not call `init_gstreamer()` for
the `Profile { … }` arm.

- `profile list` — table: `NAME | SOURCE | SCHEMA_VERSION |
  DESCRIPTION`. Sort: user first, then shipped, then embedded;
  alphabetical inside each group.
- `profile show <name>` — calls `load_profile(name)`, then prints
  the source path (or `<embedded:name>`) followed by the **current
  on-disk** TOML (i.e., post-migration if migration ran). Useful
  for debugging migrations.
- `profile clone <src> <dst>` — resolves `<src>`, materialises a
  fresh TOML body in
  `${XDG_CONFIG_HOME}/zwhisper/profiles/<dst>.toml`, replacing the
  `name = …` field with the new identifier. `<dst>.toml` must not
  exist — `OverwriteRefused` otherwise.
- `profile migrate <name>` — forces the loader against the **user
  override** specifically (refuses if the resolved source is not a
  user override; "shipped/embedded migrations" are bugs, not
  user-driven). No-op if `schema_version == CURRENT`. Idempotent.

### CLI mutual exclusion

`RecordArgs` and `TranscribeArgs` both grow a `--profile` flag and
clap groups that make it mutually exclusive with the bare flags:

```rust
#[derive(Args)]
#[command(group(
    ArgGroup::new("source-mode")
        .required(true)
        .multiple(false)
        .args(["profile", "output"])
))]
pub(crate) struct RecordArgs {
    #[arg(long, conflicts_with_all = ["mic", "monitor", "output", "duration",
                                     "max_duration_minutes", "transcribe",
                                     "model", "lang"])]
    pub(crate) profile: Option<String>,
    // … existing fields, all conflicts_with("profile")
}
```

Resolution in `run_record`:

```rust
let (record_opts, transcribe_opts) = if let Some(name) = &args.profile {
    let profile = profile::load(name)?;
    (RecordOptions::from(&profile), TranscribeOpts::from(&profile))
} else {
    // existing M0/M1 path: assemble from raw flags
};
```

`zwhisper status` stays unchanged; M3 will rewrite it once the
daemon exists.

## Phased plan

Each phase is a single PR-sized commit set. Phases run sequentially;
each builds on the previous one's verification artefacts.

### Phase 0 — Dependency lock-in + design freeze (~30 min)

- Add to `workspace.dependencies`:
  - `toml_edit = "0.22"` — preserves comments + formatting on
    in-place migration rewrite (cargo-edit precedent).
  - `include_dir = "0.7"` — embed `crates/zwhisper-cli/profiles/`
    into the binary so `cargo install` users get working defaults.
  - `shellexpand = "3"` — `~` expansion in `[[output]].path`. No
    env-var expansion (`$VAR` is a `Validation` error in M2).
- No new direct deps in `zwhisper-cli/Cargo.toml` beyond pulling
  the workspace ones.
- Do **not** add `toml = "0.8"`. `toml_edit` exposes a `serde`
  feature that gives us `Deserialize` straight off the `Document`
  via `toml_edit::de`, so we keep one TOML stack.

**Done when**: `cargo build --workspace` clean; no behavioural code
change yet.

### Phase 1 — `profile` module skeleton + types + error enum (~3 h)

- Create `crates/zwhisper-cli/src/profile/{mod,schema,error,paths,loader,migrations,embedded,commands}.rs`
  as stubs wired through `mod.rs` and into `main.rs` (`mod profile;`).
- `schema.rs`: define `Profile`, `Sources`, `Recording`,
  `Transcription`, `OutputDest`, enums `Mode { MonoMix,
  StereoSplit }`, `Codec { Flac }`, `Backend { WhisperCpp,
  Deepgram, AssemblyAi, OpenAi }`. Use `#[serde(rename_all =
  "snake_case")]` and `#[serde(rename = "whisper-cpp")]` for the
  hyphenated backend identifier so the wire format matches IDEA.md.
  Derive `Serialize, Deserialize, Clone, Debug, PartialEq` on
  every public type.
- `error.rs`: `ProfileError` per the variants in "Public API rules".
- `paths.rs`: `validate_name`, `user_override_path`, `shipped_path`,
  `resolve_source` stubs that return `NotFound` until Phase 4 wires
  them in.
- No `toml_edit` parsing yet; the loader is a TODO that returns a
  fixed dummy `Profile` to keep types compiling.

**Done when**:
- `cargo build --workspace` clean.
- `cargo clippy --workspace --all-targets --all-features -- -D
  warnings` clean.
- Unit tests for `validate_name` ("meeting" ✓, "../etc/passwd" ✗,
  "weird name with spaces" ✗) green.

### Phase 2 — TOML loader + schema_version gate + validation (~4 h)

- `loader::load_from_path(path: &Path) -> Result<Profile,
  ProfileError>` implements steps 1–4 + 6–7 from the loader
  sequence above (everything except migration; migration lands in
  Phase 3 once the framework exists).
- Two-stage parse: parse as `toml_edit::Document`, extract
  `schema_version` as `i64`, then deserialize via `toml_edit::de`
  into `Profile`. This avoids serde tagged-enum brittleness (the
  researcher's antipattern #2).
- `Profile::validate()`:
  - `recording.sample_rate ∈ {16000, 44100, 48000}`.
  - `recording.codec == Codec::Flac` (the enum already enforces
    this; the validate step is here for M5 when more codecs land).
  - `transcription.backend == Backend::WhisperCpp` ⇒ allow; else
    `BackendUnknown`.
  - `transcription.language` matches `^(auto|[a-z]{2,3}(-[A-Z]{2})?)$`.
  - `sources.mode == Mode::StereoSplit` ⇒ `UnsupportedMode` (the
    pipeline does not interleave yet).
  - For each `[[output]]` of `type = "file"`: expand `~` and
    validate every `{token}` is in `{timestamp, profile}`. Unknown
    tokens fail.
- Unit tests cover every error variant: missing
  `schema_version`, version too high, parse failure, validation
  failure, and the happy path against an inline TOML literal.

**Done when**:
- `cargo test --workspace --lib profile::loader::` green (≥ 10
  tests).
- Manual smoke: writing a deliberately broken TOML to
  `/tmp/x.toml` and calling `load_from_path` produces the expected
  typed error.

### Phase 3 — Migration framework + backup-first writer (~2 h)

- `migrations::apply_chain(doc: &mut Document, from: u32, to: u32)
  -> Result<(), MigrationFailedSource>` walks the registered chain
  step-by-step.
- `loader::load_from_path` wires step 5: backup the *original*
  body, run the chain, `doc["schema_version"] =
  CURRENT_SCHEMA_VERSION`, then atomic rewrite via
  `tempfile::NamedTempFile::persist` in the same directory.
- Backup naming: `format!("{}.bak.{}_{}",
  path.display(), unix_ms(), std::process::id())` written via
  `OpenOptions::new().create_new(true).write(true)` — a parallel
  process collision returns `BackupFailed` rather than clobbering.
- Test the framework with a **fake** migration registered behind
  `#[cfg(test)]`: a `(0, 1, fn)` migration that adds a default
  field. Confirms the chain runs, the backup gets written, the
  in-place file ends up with `schema_version = 1`, and the
  `Profile::validate` post-check passes.
- No production migrations are registered yet. The constant
  `MIGRATIONS: &[…] = &[]` ships as the prod state of the table.

**Done when**:
- `cargo test --workspace --lib profile::migrations::` green.
- Test demonstrates: pre-migration backup file written, post-load
  TOML body in-place has `schema_version = CURRENT`, the
  registered fake migration ran exactly once.
- Idempotency test: running `load_from_path` again on the now-v1
  file produces no second backup and no migration log line.

### Phase 4 — Resolution: user → shipped → embedded (~3 h)

- `embedded.rs`: `static PROFILES: include_dir::Dir =
  include_dir!("$CARGO_MANIFEST_DIR/profiles");`. Provide `lookup(name)
  -> Option<&'static str>` and `names() -> Vec<&'static str>`.
- `paths::resolve_source(name)` implements the three-step search
  documented above.
- `profile::load(name)` is the public façade:
  `resolve_source` → `UserOverride` / `Shipped` → call
  `loader::load_from_path`; `Embedded` → call
  `loader::load_from_str` (a sibling that reuses everything except
  the file-write path on migration — embedded is read-only, so
  `found < CURRENT` on an embedded template is a panic-worthy
  build-time bug; we surface it as `MigrationFailed` with a
  source-line stating "embedded template at compile time").
- Tests:
  - User override **wins** over shipped over embedded (write a
    user override matching an embedded name, confirm the user copy
    loads).
  - `NotFound` lists all three searched locations.
  - `InvalidName` (`../passwd`, `meeting/../voicememo`) trips
    *before* I/O.

**Done when**:
- `cargo test --workspace --lib profile::paths::` green.
- Manual smoke: with an empty config dir, `profile::load("meeting")`
  returns the embedded copy.

### Phase 5 — Shipped templates (`default`, `meeting`, `voicememo`) (~2 h)

- Create `crates/zwhisper-cli/profiles/default.toml`,
  `meeting.toml`, `voicememo.toml`. All three carry
  `schema_version = 1` and pass `Profile::validate()`.
- `default.toml`:
  - `[transcription].model = "small"`,
    `[transcription].language = "auto"`,
    `[transcription].auto = false` — makes `default` a pure
    "raw recording, no auto-transcribe" preset.
  - `[recording].sample_rate = 16000`,
    `[recording].max_duration_minutes = 240`.
  - `[sources].mic = "default"`,
    `[sources].system_output = ""` (empty disables monitor),
    `[sources].mode = "mono_mix"`.
- `meeting.toml`:
  - mic + sink monitor, `mode = mono_mix` (NOT stereo_split — DoD #1
    must pass on the existing pipeline; stereo is M2.5).
  - `[transcription]`: `whisper-cpp`, `small`, `auto = true`.
  - `[[output]]`: file at
    `~/Recordings/zwhisper/{profile}/{timestamp}`.
- `voicememo.toml`:
  - mic only, `mode = mono_mix`.
  - same transcription settings as `meeting`.
- Phase-5 unit tests round-trip every shipped template through
  `load_from_str` + `validate`. Any breakage trips the build.

**Done when**:
- `cargo test --workspace --lib profile::embedded::` green; one
  test loads each shipped template by name.
- Each shipped template passes `Profile::validate`.

### Phase 6 — CLI integration + replace hardcoded constants (~3 h)

- `cli::RecordArgs` gains `--profile`, with `clap::ArgGroup` and
  `conflicts_with_all` pinning the mutual exclusion (DoD #2). The
  existing `--output` becomes `Option<PathBuf>`; the
  `clap::ArgGroup` enforces `--profile XOR --output`.
- `cli::TranscribeArgs` gains `--profile`, `conflicts_with_all =
  ["backend", "model", "language"]`.
- `cli::run_record` switches on `args.profile`:
  - `Some(name)`: load profile, build `RecordOptions` and
    `TranscribeOpts` via `From<&Profile>`, run.
  - `None`: existing M0/M1 path.
- `cli::run_transcribe` similar.
- **Remove** `DEFAULT_MODEL` / `DEFAULT_LANGUAGE` constants
  (`cli.rs:12-13`). The post-record transcribe path now requires
  either `--profile` (sources from profile) or both `--model` and
  `--lang` (no longer optional). This closes the CLAUDE.md
  no-hardcoded-values gap flagged by the devils-advocate review.
  Pure CLI users keep working: `zwhisper record --output x.flac
  --transcribe whisper-cpp --model small --lang en`.
- New `Command::Profile(ProfileCmd)` arm in `main.rs`. Subcommand
  matches dispatch into `profile::commands::{list, show, clone,
  migrate}`.

**Done when**:
- `zwhisper record --profile meeting` records + transcribes
  end-to-end on the maintainer's box (or runtime-skips with
  `[SKIP]` if PipeWire isn't reachable, mirroring M0).
- `zwhisper profile list` prints `default`, `meeting`,
  `voicememo` with source `[embedded]` on a clean host.
- `zwhisper profile clone meeting custom-meeting` writes
  `~/.config/zwhisper/profiles/custom-meeting.toml`; running it a
  second time errors with `OverwriteRefused`.
- `zwhisper profile clone meeting custom-meeting; zwhisper record
  --profile custom-meeting` loads the user override (verified via
  `profile show`).
- Compilation flags out `--profile` + any of `--mic / --output / …`
  with a clap error.

### Phase 7 — Tests + verification fixtures (~3 h)

- Unit tests recap (each module already has its own; this phase
  adds the cross-module integration + CLI parsing tests):
  - `cli`: `--profile X` parses with a single `RecordArgs.profile`
    field set; `--profile X --output Y` is rejected by clap.
  - `cli`: `--profile X --backend Y` likewise on `TranscribeArgs`.
- Integration tests (`tests/profile.rs`):
  - `record_with_profile_runs_end_to_end` — parallels M1's
    `record_then_transcribe_end_to_end`. Runtime-skips on hosts
    without PipeWire **or** without `whisper-cli`.
  - `profile_list_includes_embedded` — black-box `assert_cmd` test
    that runs `zwhisper profile list` on a tempdir-isolated
    `XDG_CONFIG_HOME` and asserts the three embedded names are
    present.
  - `profile_clone_then_load_user_override` — `clone meeting x`,
    then `profile show x` reports `[user]` source.
  - `profile_migrate_no_op_at_current_version` — running
    `profile migrate meeting` against a v1 user override produces
    no `.bak` file and exits 0.
- Migration end-to-end test (still no real migrations registered):
  - Write a fake `schema_version = 0` TOML with the same body as
    the current `meeting.toml`. Register a test-only `(0, 1, fn)`
    migration. Confirm the loader writes the backup, runs the
    migration, rewrites the file, and the second `load_from_path`
    is a no-op.

**Done when**:
- `cargo test --workspace` green; runtime skips visible (no silent
  gaps).
- `cargo clippy --workspace --all-targets --all-features -- -D
  warnings` clean.
- The migration end-to-end test produces a `.bak.<ms>_<pid>` file
  with the original v0 body.

### Phase 8 — Verification + sign-off (~1 h)

- Add `docs/M2-verification.md` mirroring the M1-verification
  shape:
  - DoD items 1–12 each linked to evidence (test name, log line,
    file path).
  - Frozen snapshot of `zwhisper profile list` output on a clean
    host.
  - Frozen snapshot of an end-to-end `zwhisper record --profile
    meeting` run (artifacts: `.flac`, `.flac.txt`, `.flac.json`).
  - Note any deviations from the plan (e.g., if the meeting
    profile's path expansion needs adjusting on the maintainer's
    `XDG_CONFIG_HOME`).
- Update `docs/M0-plan.md` and `docs/M1-plan.md` status snapshot
  tables to mark M2 as in-progress / done. Add the M2 cross-link
  the same way M1 cross-linked from M0.

**Done when**: `M2-verification.md` is committed with all twelve
DoD items ticked and links to artefacts.

## Risks (what could push us back)

- **`toml_edit::de` quirks** vs straight `toml`. We pick
  `toml_edit` for the migration rewrite path; if its serde adapter
  has unexpected gaps (e.g., enum representations), Phase 2 falls
  back to a two-crate setup: parse with `toml_edit::Document` for
  rewrites, deserialize with `toml = "0.8"` for `serde`. Locked
  decision in Phase 0.
- **Embedded paths in tests**. `include_dir!` resolves at compile
  time relative to `$CARGO_MANIFEST_DIR`; running tests from a
  workspace root must not trip path resolution. Phase 5 adds an
  explicit "load every embedded profile" test that catches this.
- **`shellexpand` vs daemon `ProtectHome`**. M2 expands `~` in the
  CLI process where `$HOME` is reachable. M3 daemon may have
  `ProtectHome=read-only`; the expansion result must still be
  inside `ReadWritePaths`. Out of scope for M2 but flagged in
  `docs/M2-verification.md` so M3 doesn't get blindsided.
- **Migration backup race** — `unix_ms + pid` is unique enough for
  realistic CLI/daemon timing, but two threads inside the *same*
  process at the *same* millisecond would collide. We use
  `OpenOptions::create_new(true)` so the loser surfaces
  `BackupFailed` and the user retries — this is the right typed
  error for a busy-state collision.
- **`--profile` removing `DEFAULT_MODEL` / `DEFAULT_LANGUAGE`**
  breaks scripts that relied on those defaults. Mitigation: the
  removal is in Phase 6, documented in `M2-verification.md`, and
  the embedded `default` profile preserves the same effective
  values for `--profile default` users.
- **Stereo-split rejection at runtime** is a footgun if a user
  cargo-cults a stereo profile from an old IDEA.md draft. The
  validate step rejects it *before* GStreamer touches anything,
  with an error pointing at IDEA.md § 11 (M2.5 / M3) so the user
  knows when to expect it.
- **Path tokens** — adding `{date}` / `{time}` later requires care
  to keep the `{token}` lexer non-ambiguous. M2 keeps the set
  small (`{timestamp}`, `{profile}`); future tokens are an
  additive change.
- **Cross-fs persist** — `tempfile::NamedTempFile::persist` is
  same-fs (we create the temp in the target's parent dir on
  purpose). If the target dir is `noatime, ro` or whatever, write
  fails; loader surfaces `Io { path, source }`.

## Out of scope, on purpose (re-statement)

- Stereo-split mode pipeline (M2.5 / M3 — interleave caps).
- D-Bus `Profiles.Reload()` signal (M3).
- Cloud backends + secret-service (M5).
- Tray-bound output sinks (`clipboard`, `notification`) — M4.
- Hotkey registration via `xdg-desktop-portal` — M6.
- Settings GUI / model downloader — M7.
- `profile clone --force` (overwrite) — explicit non-goal in M2.
- Per-profile retention / auto-purge — IDEA.md § 5 calls out
  `retention_days`; landed in M2 as a *parsed* field, no enforcement
  yet (M3+ daemon owns the timer).

## Definition-of-done sign-off

M2 is closed only when `docs/M2-verification.md` is committed with
all twelve DoD items ticked and links to artefacts (test logs,
TOML fixtures, sample `meeting` recording + transcript pair, the
`.bak.<ms>_<pid>` from a migration test, `zwhisper profile list`
snapshot on a clean host, `cargo clippy` + `cargo test` reports).
Until then, M2 stays open.
