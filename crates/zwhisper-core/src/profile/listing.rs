//! Data-producing helpers for `zwhisper profile {list, clone, migrate}`.
//!
//! Phase 1 of M3 split the original `commands` module into two halves:
//! pure data operations (this file) and CLI pretty-printers (which
//! live in `zwhisper-cli`'s `profile_commands` module). The daemon
//! consumes the data half via D-Bus once Phase 2 lands; the CLI
//! consumes the same data and renders a human-readable table.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};
use tracing::info;

use super::error::ProfileError;
use super::schema::{Mode, OutputDest};
use super::{Profile, ProfileSource, embedded, loader, paths, resolve};

/// One row in the `profile list` table. `source` is the precedence
/// label (`"user"`, `"shipped"`, `"embedded"`); `schema_version` is
/// `None` when the TOML did not carry an integer at the
/// `schema_version` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    pub name: String,
    pub source: String,
    pub schema_version: Option<u32>,
    pub description: Option<String>,
    /// `[transcription].backend`, parsed from the TOML when present.
    /// `None` when the file is malformed or the backend field is
    /// absent; callers (`Profiles1.list_v2` wire emit) substitute
    /// `"whisper-cpp"` as the legacy default in that case so the
    /// tray gets a deterministic value.
    pub backend: Option<String>,
}

/// Aggregate every visible profile, honouring the
/// user > shipped > embedded precedence from IDEA.md § 6.
///
/// I/O failures on individual files are silently treated as missing
/// entries — a corrupted user override should not hide the shipped
/// fallback from `profile list`. Caller-facing errors here would
/// only confuse users who can already see the broken file in `ls`.
pub fn list_entries() -> Result<Vec<ProfileEntry>, ProfileError> {
    let mut entries: BTreeMap<String, ProfileEntry> = BTreeMap::new();

    if let Ok(dir) = paths::user_profiles_dir() {
        if dir.is_dir() {
            scan_dir(&dir, "user", &mut entries);
        }
    }

    let shipped_dir = shipped_profiles_dir();
    if shipped_dir.is_dir() {
        scan_dir(&shipped_dir, "shipped", &mut entries);
    }

    for name in embedded::names() {
        entries.entry(name.to_owned()).or_insert_with(|| {
            let body = embedded::lookup(name).unwrap_or_default();
            entry_from_body(name, "embedded", body)
        });
    }

    Ok(entries.into_values().collect())
}

/// Clone a profile from any source into a user override. The
/// destination filename is `${XDG_CONFIG_HOME}/zwhisper/profiles/<dst>.toml`
/// and the file is opened with `create_new` to refuse silent
/// overwrites — the M2 review's TOCTOU-safe pattern.
///
/// Returns the resolved destination path so callers can include it
/// in a "cloned `<src>` -> `<path>`" message without re-deriving it.
pub fn clone_to_user(src: &str, dst: &str) -> Result<PathBuf, ProfileError> {
    paths::validate_name(dst)?;
    let target = paths::user_override_path(dst)?;

    let mut profile = super::load(src)?;
    dst.clone_into(&mut profile.name);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }

    let body =
        toml_edit::ser::to_string_pretty(&profile).map_err(|e| ProfileError::Validation {
            profile: dst.to_owned(),
            message: format!("could not serialize cloned profile: {e}"),
        })?;

    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ProfileError::OverwriteRefused { path: target });
        }
        Err(source) => {
            return Err(ProfileError::Io {
                path: target,
                source,
            });
        }
    };
    f.write_all(body.as_bytes())
        .map_err(|source| ProfileError::Io {
            path: target.clone(),
            source,
        })?;
    f.sync_all().map_err(|source| ProfileError::Io {
        path: target.clone(),
        source,
    })?;
    info!(
        src = src,
        dst = dst,
        path = %target.display(),
        "profile cloned"
    );
    Ok(target)
}

/// In-place edits to a profile's `[sources]` table
/// (RFC-mic-setup, "Profile Changes"). Each field is an
/// `Option`: `None` leaves the existing value untouched, `Some(_)`
/// overwrites it. `input_gain_db` is doubly wrapped so the caller can
/// distinguish "set to x" (`Some(Some(x))`) from "remove the key
/// entirely" (`Some(None)`) from "leave as-is" (`None`).
#[derive(Debug, Default)]
pub struct SourcesUpdate<'a> {
    /// New `sources.mic` node name (validated via the shared node-name
    /// allow-list before any write).
    pub mic: Option<&'a str>,
    /// New `sources.system_output`. An empty string `""` is permitted
    /// here — it is the mic-only marker (RFC Phase 5) — and is written
    /// verbatim without node-name validation. A non-empty value is
    /// validated like `mic`.
    pub system_output: Option<&'a str>,
    /// New `sources.mode`.
    pub mode: Option<Mode>,
    /// `Some(Some(x))` sets `input_gain_db = x`; `Some(None)` removes
    /// the key; `None` leaves it as-is.
    pub input_gain_db: Option<Option<f32>>,
}

/// Snake-case TOML token for a [`Mode`] (matches the schema's
/// `#[serde(rename_all = "snake_case")]`). Centralised here so the
/// writer never hardcodes the string in more than one place.
fn mode_token(mode: Mode) -> &'static str {
    match mode {
        Mode::MonoMix => "mono_mix",
        Mode::StereoSplit => "stereo_split",
    }
}

/// Validate the non-structural inputs of a [`SourcesUpdate`] before any
/// document is touched: node names against the shared allow-list, gain
/// finite and within the shared range. Empty `system_output` is the
/// intentional mic-only marker and skips the node-name check.
fn validate_sources_update(name: &str, update: &SourcesUpdate<'_>) -> Result<(), ProfileError> {
    if let Some(mic) = update.mic {
        crate::node_name::validate_node_name(mic).map_err(|e| ProfileError::Validation {
            profile: name.to_owned(),
            message: format!("sources.mic {mic:?} invalid: {}", e.reason()),
        })?;
    }
    if let Some(out) = update.system_output {
        // "" == mic-only: written verbatim, not validated as a node name.
        if !out.is_empty() {
            crate::node_name::validate_node_name(out).map_err(|e| ProfileError::Validation {
                profile: name.to_owned(),
                message: format!("sources.system_output {out:?} invalid: {}", e.reason()),
            })?;
        }
    }
    if let Some(Some(gain)) = update.input_gain_db {
        if !gain.is_finite() {
            return Err(ProfileError::Validation {
                profile: name.to_owned(),
                message: "sources.input_gain_db must be finite".into(),
            });
        }
        if !(crate::gain::MIN_INPUT_GAIN_DB..=crate::gain::MAX_INPUT_GAIN_DB).contains(&gain) {
            return Err(ProfileError::Validation {
                profile: name.to_owned(),
                message: format!(
                    "sources.input_gain_db {gain} out of range [{}, {}] dB",
                    crate::gain::MIN_INPUT_GAIN_DB,
                    crate::gain::MAX_INPUT_GAIN_DB
                ),
            });
        }
    }
    Ok(())
}

/// Set `key = value` in a table-like, **preserving the key's existing
/// decor** (notably a leading `# comment`) when the key is already
/// present. `toml_edit::TableLike::insert` recreates the key entry and
/// drops its prefix decor; reassigning the existing `Item` in place
/// keeps the key (and its comment) intact. A new key is inserted
/// normally — there is no prior decor to preserve.
fn set_value(table: &mut dyn toml_edit::TableLike, key: &str, value: toml_edit::Item) {
    if let Some(item) = table.get_mut(key) {
        *item = value;
    } else {
        table.insert(key, value);
    }
}

/// Apply a [`SourcesUpdate`] onto a parsed `DocumentMut`'s `[sources]`
/// table, preserving the rest of the document (comments, formatting,
/// key order) untouched. Inputs are assumed already validated by
/// [`validate_sources_update`].
fn apply_sources_update(
    doc: &mut DocumentMut,
    name: &str,
    update: &SourcesUpdate<'_>,
) -> Result<(), ProfileError> {
    // `[sources]` must already exist — every valid profile has it. If a
    // user hand-deleted it the round-trip validation below would fail
    // anyway, but error early with a clear message.
    let sources = doc
        .get_mut("sources")
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or_else(|| ProfileError::Validation {
            profile: name.to_owned(),
            message: "profile has no [sources] table to update".into(),
        })?;

    if let Some(mic) = update.mic {
        set_value(sources, "mic", toml_edit::value(mic));
    }
    if let Some(out) = update.system_output {
        set_value(sources, "system_output", toml_edit::value(out));
    }
    if let Some(mode) = update.mode {
        set_value(sources, "mode", toml_edit::value(mode_token(mode)));
    }
    match update.input_gain_db {
        Some(Some(gain)) => {
            set_value(sources, "input_gain_db", toml_edit::value(f64::from(gain)));
        }
        Some(None) => {
            sources.remove("input_gain_db");
        }
        None => {}
    }
    Ok(())
}

/// Edit the `[sources]` table of a **user-override** profile in place,
/// preserving comments and formatting (unlike `clone_to_user`'s full
/// reserialize). The resulting document is round-trip deserialized and
/// re-validated as a [`Profile`] *before* it is persisted, and the
/// write is atomic (temp file + `sync_all` + rename), mirroring the
/// `migrations` writer's crash-safety discipline.
///
/// Errors:
/// - [`ProfileError::NotFound`] when no user-override file exists (the
///   CLI should tell the user to clone the profile first — shipped /
///   embedded profiles are not mutable);
/// - [`ProfileError::Validation`] when a node name is invalid, the gain
///   is non-finite / out of range, or the rewritten profile fails
///   `Profile::validate`;
/// - [`ProfileError::TomlParse`] / [`ProfileError::Io`] on a malformed
///   on-disk file or a failed write.
///
/// Returns the path that was written.
pub fn update_sources(name: &str, update: &SourcesUpdate<'_>) -> Result<PathBuf, ProfileError> {
    paths::validate_name(name)?;
    let path = paths::user_override_path(name)?;
    if !path.is_file() {
        return Err(ProfileError::NotFound {
            name: name.to_owned(),
            searched: vec![path.display().to_string()],
        });
    }
    update_sources_at(name, &path, update)
}

/// Path-explicit core of [`update_sources`]. Production callers go
/// through `update_sources` (which resolves and existence-checks the
/// user-override path); tests drive this directly against a tempdir
/// path to stay hermetic, mirroring the existing `clone_into_dir`
/// helper. `name` is used only for error messages.
fn update_sources_at(
    name: &str,
    path: &Path,
    update: &SourcesUpdate<'_>,
) -> Result<PathBuf, ProfileError> {
    // 1. Validate the inputs before touching the file at all — a bad
    //    node name / gain must never reach the document or the disk.
    validate_sources_update(name, update)?;

    // 2. Read + parse, preserving the full document (comments included).
    let body = fs::read_to_string(path).map_err(|source| ProfileError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut doc: DocumentMut = body.parse().map_err(|source| ProfileError::TomlParse {
        path: path.to_owned(),
        source,
    })?;

    // 3. Mutate only the [sources] table.
    apply_sources_update(&mut doc, name, update)?;

    // 4. Re-validate the rewritten document as a full Profile BEFORE
    //    persisting. `load_from_str` parses, gates the schema version,
    //    deserializes, and calls `Profile::validate` — so an edit that
    //    produces an out-of-range gain or otherwise invalid profile is
    //    rejected and the on-disk file is left untouched.
    let rewritten = doc.to_string();
    loader::load_from_str(&rewritten, name)?;

    // 5. Atomic write: temp file in the same dir, fsync, rename over the
    //    original (crash leaves either the old or the new full body).
    atomic_replace(path, &rewritten)?;

    info!(
        name = name,
        path = %path.display(),
        "profile [sources] updated"
    );
    Ok(path.to_owned())
}

/// Snake-case TOML token for an [`OutputDest`] discriminator (matches
/// the schema's `#[serde(tag = "type", rename_all = "snake_case")]`).
/// Centralised here so the writer never hardcodes a discriminator string
/// in more than one place — the symmetric counterpart to [`mode_token`].
fn output_token(dest: &OutputDest) -> &'static str {
    match dest {
        OutputDest::File { .. } => "file",
        OutputDest::Clipboard => "clipboard",
        OutputDest::Notification => "notification",
        OutputDest::TypeAtCursor => "type_at_cursor",
    }
}

/// Build a single `[[output]]` table for one [`OutputDest`]. Every table
/// carries the `type = "<token>"` discriminator; `File` additionally
/// carries `path = "<path>"`. The table is constructed explicitly rather
/// than via serde so the rendered shape stays under this module's control
/// (a plain [`toml_edit::Table`] renders as a `[[output]]` header
/// automatically once collected into an [`toml_edit::ArrayOfTables`]).
fn output_table(dest: &OutputDest) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.insert("type", toml_edit::value(output_token(dest)));
    if let OutputDest::File { path } = dest {
        table.insert("path", toml_edit::value(path.as_str()));
    }
    table
}

/// Replace the `[[output]]` array-of-tables of a **user-override**
/// profile, preserving the rest of the document (comments and formatting
/// on every other section are left untouched — only the `output` key is
/// rebuilt). The symmetric counterpart to [`update_sources`]: the
/// rewritten document is round-trip deserialized and re-validated as a
/// full [`Profile`] *before* it is persisted, and the write is atomic
/// (temp file + `sync_all` + rename) so a crash leaves either the old or
/// the new full body.
///
/// An empty `outputs` slice removes the `output` key entirely — a
/// [`Profile`] permits zero outputs.
///
/// Errors mirror [`update_sources`]:
/// - [`ProfileError::NotFound`] when no user-override file exists (the
///   CLI should tell the user to clone the profile first — shipped /
///   embedded profiles are not mutable);
/// - [`ProfileError::Validation`] when the rewritten profile fails
///   `Profile::validate` (e.g. a `File` output whose path token is
///   invalid);
/// - [`ProfileError::TomlParse`] / [`ProfileError::Io`] on a malformed
///   on-disk file or a failed write.
///
/// Returns the path that was written.
pub fn set_outputs(name: &str, outputs: &[OutputDest]) -> Result<PathBuf, ProfileError> {
    paths::validate_name(name)?;
    let path = paths::user_override_path(name)?;
    if !path.is_file() {
        return Err(ProfileError::NotFound {
            name: name.to_owned(),
            searched: vec![path.display().to_string()],
        });
    }
    set_outputs_at(name, &path, outputs)
}

/// Path-explicit core of [`set_outputs`]. Production callers go through
/// [`set_outputs`] (which resolves and existence-checks the user-override
/// path); tests drive this directly against a tempdir path to stay
/// hermetic, mirroring [`update_sources_at`]. `name` is used only for
/// error messages.
fn set_outputs_at(
    name: &str,
    path: &Path,
    outputs: &[OutputDest],
) -> Result<PathBuf, ProfileError> {
    // 1. Read + parse, preserving the full document (comments included).
    let body = fs::read_to_string(path).map_err(|source| ProfileError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut doc: DocumentMut = body.parse().map_err(|source| ProfileError::TomlParse {
        path: path.to_owned(),
        source,
    })?;

    // 2. Rebuild only the `output` array-of-tables. An empty slice drops
    //    the key entirely; otherwise overwrite it with a fresh AoT so any
    //    prior `[[output]]` formatting/comments are intentionally
    //    replaced (the rest of the document is left as-is).
    if outputs.is_empty() {
        doc.remove("output");
    } else {
        let mut aot = toml_edit::ArrayOfTables::new();
        for dest in outputs {
            aot.push(output_table(dest));
        }
        doc.insert("output", Item::ArrayOfTables(aot));
    }

    // 3. Re-validate the rewritten document as a full Profile BEFORE
    //    persisting. `load_from_str` parses, gates the schema version,
    //    deserializes, and calls `Profile::validate` — so an output whose
    //    path token is invalid (or any other broken invariant) is
    //    rejected and the on-disk file is left untouched.
    let rewritten = doc.to_string();
    loader::load_from_str(&rewritten, name)?;

    // 4. Atomic write: temp file in the same dir, fsync, rename over the
    //    original (crash leaves either the old or the new full body).
    atomic_replace(path, &rewritten)?;

    info!(
        name = name,
        path = %path.display(),
        outputs = outputs.len(),
        "profile [[output]] rewritten"
    );
    Ok(path.to_owned())
}

/// Write `body` to a temp file alongside `path`, `sync_all` it, then
/// rename it over `path`. Same crash-safety discipline as
/// `migrations::rewrite_in_place`.
fn atomic_replace(path: &Path, body: &str) -> Result<(), ProfileError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".zwhisper-profile-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|source| ProfileError::Io {
            path: path.to_owned(),
            source,
        })?;
    {
        let mut handle = tmp.as_file();
        handle
            .write_all(body.as_bytes())
            .map_err(|source| ProfileError::Io {
                path: path.to_owned(),
                source,
            })?;
        handle.sync_all().map_err(|source| ProfileError::Io {
            path: path.to_owned(),
            source,
        })?;
    }
    tmp.persist(path).map_err(|persist_err| ProfileError::Io {
        path: path.to_owned(),
        source: persist_err.error,
    })?;
    Ok(())
}

/// Force-load a user-override profile through the migration chain.
/// No-op when the file is already at `CURRENT_SCHEMA_VERSION`.
/// Errors out with a typed `ProfileError` when the named profile is
/// not a user override (the only mutable source).
pub fn migrate_user(name: &str) -> Result<Profile, ProfileError> {
    paths::validate_name(name)?;
    let user_path = paths::user_override_path(name)?;
    if !user_path.is_file() {
        return Err(ProfileError::NotFound {
            name: name.to_owned(),
            searched: vec![user_path.display().to_string()],
        });
    }
    loader::load_from_path(&user_path)
}

/// Re-resolution helper used by CLI `profile show` to print the
/// concrete file path / `<embedded>` marker before dumping the body.
/// Public because the CLI's `profile_commands` module needs it after
/// the carve-out; the daemon does not call this directly.
pub fn resolved_source(name: &str) -> Result<ProfileSource, ProfileError> {
    resolve(name)
}

fn shipped_profiles_dir() -> PathBuf {
    let root = std::env::var_os("ZWHISPER_DATA_DIR")
        .map_or_else(|| PathBuf::from("/usr/share/zwhisper"), PathBuf::from);
    root.join("profiles")
}

fn entry_from_path(name: &str, source: &str, path: &Path) -> ProfileEntry {
    let body = fs::read_to_string(path).unwrap_or_default();
    entry_from_body(name, source, &body)
}

fn entry_from_body(name: &str, source: &str, body: &str) -> ProfileEntry {
    let parsed = body.parse::<toml_edit::DocumentMut>().ok();
    let schema_version = parsed
        .as_ref()
        .and_then(|d| d.get("schema_version")?.as_integer())
        .and_then(|v| u32::try_from(v).ok());
    let description = parsed
        .as_ref()
        .and_then(|d| d.get("description")?.as_str())
        .map(str::to_owned);
    let backend = parsed
        .as_ref()
        .and_then(|d| d.get("transcription")?.as_table_like())
        .and_then(|t| t.get("backend")?.as_str())
        .map(str::to_owned);
    ProfileEntry {
        name: name.to_owned(),
        source: source.to_owned(),
        schema_version,
        description,
        backend,
    }
}

fn scan_dir(dir: &Path, source: &str, entries: &mut BTreeMap<String, ProfileEntry>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for ent in read.flatten() {
        let path = ent.path();
        let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Filter `.bak.<ts>_<pid>` and other suffixes — we list real
        // profiles, not migration backups.
        let lower_ext_toml = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
        if !lower_ext_toml {
            continue;
        }
        if file.contains(".toml.bak.") || file.starts_with('.') {
            continue;
        }
        let name = file.trim_end_matches(".toml").to_owned();
        // Stronger source already wins; only insert if not present.
        entries
            .entry(name.clone())
            .or_insert_with(|| entry_from_path(&name, source, &path));
    }
}

/// Test-only helper: drive the clone op against a synthesized
/// destination path so unit tests do not pollute the developer's
/// real config dir.
#[cfg(test)]
pub(crate) fn clone_into_dir(src: &str, dst: &str, target: &Path) -> Result<Profile, ProfileError> {
    paths::validate_name(dst)?;
    let mut profile = super::load(src)?;
    dst.clone_into(&mut profile.name);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    let body =
        toml_edit::ser::to_string_pretty(&profile).map_err(|e| ProfileError::Validation {
            profile: dst.to_owned(),
            message: format!("serialize: {e}"),
        })?;
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ProfileError::OverwriteRefused {
                path: target.to_owned(),
            });
        }
        Err(source) => {
            return Err(ProfileError::Io {
                path: target.to_owned(),
                source,
            });
        }
    };
    f.write_all(body.as_bytes())
        .map_err(|source| ProfileError::Io {
            path: target.to_owned(),
            source,
        })?;
    f.sync_all().map_err(|source| ProfileError::Io {
        path: target.to_owned(),
        source,
    })?;
    Ok(profile)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn list_entries_contains_default_meeting_voicememo() {
        let entries = list_entries().unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        for required in ["default", "meeting", "voicememo"] {
            assert!(
                names.contains(&required),
                "list_entries missing {required}: {names:?}"
            );
        }
    }

    #[test]
    fn list_entries_reports_schema_version_for_embedded() {
        let entries = list_entries().unwrap();
        let default_entry = entries
            .iter()
            .find(|e| e.name == "default")
            .expect("default profile present");
        // The shipped/embedded `default.toml` always declares
        // `schema_version = 1`; if the embedded body parsed at all
        // the integer should round-trip.
        assert!(default_entry.schema_version.is_some());
    }

    #[test]
    fn clone_into_dir_writes_user_profile_with_renamed_field() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("custom.toml");
        let profile = clone_into_dir("default", "custom", &target).unwrap();
        assert_eq!(profile.name, "custom");
        let body = fs::read_to_string(&target).unwrap();
        assert!(body.contains("name = \"custom\""), "{body}");
    }

    #[test]
    fn clone_into_dir_refuses_existing_target() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("custom.toml");
        clone_into_dir("default", "custom", &target).unwrap();
        let err = clone_into_dir("default", "custom", &target).unwrap_err();
        assert!(matches!(err, ProfileError::OverwriteRefused { .. }));
    }

    #[test]
    fn scan_dir_filters_backup_suffix() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.toml"),
            "schema_version = 1\nname = \"a\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("a.toml.bak.1700000000000_999"),
            "doesn't matter",
        )
        .unwrap();
        let mut entries = BTreeMap::new();
        scan_dir(dir.path(), "user", &mut entries);
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key("a"));
    }

    // ---- update_sources -------------------------------------------------

    /// A valid v1 user profile body with comments and blank lines, so
    /// the writer's comment/format preservation can be asserted.
    const PROFILE_WITH_COMMENTS: &str = r#"# my custom profile
schema_version = 1
name = "custom"
description = "fixture"

[sources]
# the microphone to capture
mic = "default"
system_output = "default"
mode = "mono_mix" # mono downmix

[recording]
codec = "flac"
sample_rate = 16000
max_duration_minutes = 60

[transcription]
backend = "whisper-cpp"
model = "small"
language = "auto"
auto = true

[[output]]
type = "file"
path = "~/Recordings/zwhisper/{profile}/{timestamp}.flac"
"#;

    fn write_user_profile(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("custom.toml");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn update_sources_sets_mic_and_preserves_comments() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);
        let update = SourcesUpdate {
            mic: Some("alsa_input.real-mic"),
            ..Default::default()
        };
        update_sources_at("custom", &path, &update).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("mic = \"alsa_input.real-mic\""), "{body}");
        // Comments and the unrelated lines survive the single-table edit.
        assert!(body.contains("# my custom profile"), "{body}");
        assert!(body.contains("# the microphone to capture"), "{body}");
        assert!(body.contains("# mono downmix"), "{body}");
        assert!(body.contains("max_duration_minutes = 60"), "{body}");

        // And it still loads + validates.
        let p = loader::load_from_path(&path).unwrap();
        assert_eq!(p.sources.mic, "alsa_input.real-mic");
    }

    #[test]
    fn update_sources_sets_then_removes_input_gain_db() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);

        // Set the gain.
        update_sources_at(
            "custom",
            &path,
            &SourcesUpdate {
                input_gain_db: Some(Some(-2.5)),
                ..Default::default()
            },
        )
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("input_gain_db"), "{body}");
        let p = loader::load_from_path(&path).unwrap();
        assert_eq!(p.sources.input_gain_db, Some(-2.5));

        // Remove it again — key gone, profile still valid.
        update_sources_at(
            "custom",
            &path,
            &SourcesUpdate {
                input_gain_db: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(
            !body.contains("input_gain_db"),
            "key should be gone: {body}"
        );
        let p = loader::load_from_path(&path).unwrap();
        assert!(p.sources.input_gain_db.is_none());
    }

    #[test]
    fn update_sources_leaves_gain_untouched_when_none() {
        let dir = TempDir::new().unwrap();
        // Start with a gain already present in the file.
        let body_with_gain = PROFILE_WITH_COMMENTS.replace(
            "mode = \"mono_mix\" # mono downmix\n",
            "mode = \"mono_mix\"\ninput_gain_db = -3.0\n",
        );
        let path = write_user_profile(&dir, &body_with_gain);

        // Update only the mic — gain must remain.
        update_sources_at(
            "custom",
            &path,
            &SourcesUpdate {
                mic: Some("alsa_input.other"),
                ..Default::default()
            },
        )
        .unwrap();
        let p = loader::load_from_path(&path).unwrap();
        assert_eq!(p.sources.input_gain_db, Some(-3.0));
        assert_eq!(p.sources.mic, "alsa_input.other");
    }

    #[test]
    fn update_sources_writes_mic_only_empty_system_output() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);
        // The raw writer must be able to persist `system_output = ""`
        // even though Wave-1 `Profile::validate` still rejects mic-only.
        // Verify the document mutation itself (decoupled from the
        // validate gate, which Wave 2B relaxes).
        let mut doc: DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        apply_sources_update(
            &mut doc,
            "custom",
            &SourcesUpdate {
                system_output: Some(""),
                ..Default::default()
            },
        )
        .unwrap();
        let rewritten = doc.to_string();
        assert!(rewritten.contains("system_output = \"\""), "{rewritten}");
        // And the input validation accepts the empty marker (no
        // node-name check for "").
        validate_sources_update(
            "custom",
            &SourcesUpdate {
                system_output: Some(""),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn update_sources_changes_mode() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);
        // mono_mix -> mono_mix is a no-op token; assert the token writer
        // emits the right string for both variants via the document.
        let mut doc: DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        apply_sources_update(
            &mut doc,
            "custom",
            &SourcesUpdate {
                mode: Some(Mode::StereoSplit),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(doc.to_string().contains("mode = \"stereo_split\""));
    }

    #[test]
    fn update_sources_refuses_non_user_override() {
        // `update_sources` (the public entry) existence-checks the
        // user-override path. A name with no user file → NotFound, so
        // the CLI can tell the user to clone first. Use a name that is
        // extremely unlikely to have a real user override on the host.
        let err = update_sources(
            "definitely-not-a-real-user-profile-xyz",
            &SourcesUpdate {
                mic: Some("alsa_input.x"),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ProfileError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn update_sources_rejects_bad_node_name_before_write() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);
        let original = fs::read_to_string(&path).unwrap();

        let err = update_sources_at(
            "custom",
            &path,
            &SourcesUpdate {
                mic: Some("bad name with spaces"),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ProfileError::Validation { .. }), "{err:?}");
        // File must be untouched (validation happens before any write).
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn update_sources_rejects_out_of_range_gain_before_write() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);
        let original = fs::read_to_string(&path).unwrap();

        for gain in [100.0_f32, -100.0, f32::NAN, f32::INFINITY] {
            let err = update_sources_at(
                "custom",
                &path,
                &SourcesUpdate {
                    input_gain_db: Some(Some(gain)),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(
                matches!(err, ProfileError::Validation { .. }),
                "{gain}: {err:?}"
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn update_sources_missing_file_is_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("custom.toml"); // not created
        let err = update_sources_at(
            "custom",
            &path,
            &SourcesUpdate {
                mic: Some("alsa_input.x"),
                ..Default::default()
            },
        )
        .unwrap_err();
        // Reading a nonexistent file surfaces as an Io error from the
        // path-explicit core (the existence check lives in the public
        // `update_sources`).
        assert!(matches!(err, ProfileError::Io { .. }), "{err:?}");
    }

    #[test]
    fn update_sources_rejects_malformed_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("custom.toml");
        fs::write(&path, "this is not = valid toml = =").unwrap();
        let err = update_sources_at(
            "custom",
            &path,
            &SourcesUpdate {
                mic: Some("alsa_input.x"),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ProfileError::TomlParse { .. }), "{err:?}");
    }

    #[test]
    fn update_sources_atomic_no_partial_on_validation_failure() {
        // If the edit would produce an invalid profile (round-trip
        // validate fails), the original file must remain byte-for-byte.
        let dir = TempDir::new().unwrap();
        // Make a profile whose sample_rate the edit cannot fix; then try
        // to set a mic — the round-trip validate fails on the bad rate,
        // and the file must be left as-is.
        let bad_rate = PROFILE_WITH_COMMENTS.replace("sample_rate = 16000", "sample_rate = 9001");
        let path = write_user_profile(&dir, &bad_rate);
        let original = fs::read_to_string(&path).unwrap();

        let err = update_sources_at(
            "custom",
            &path,
            &SourcesUpdate {
                mic: Some("alsa_input.x"),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ProfileError::Validation { .. }), "{err:?}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "file must be untouched when round-trip validation fails"
        );
    }

    // ---- set_outputs ----------------------------------------------------

    #[test]
    fn set_outputs_replaces_array_and_preserves_other_sections() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);

        set_outputs_at(
            "custom",
            &path,
            &[
                OutputDest::File {
                    path: "~/Recordings/zwhisper/{profile}/{timestamp}.flac".to_owned(),
                },
                OutputDest::TypeAtCursor,
            ],
        )
        .unwrap();

        // Reparse: exactly two `[[output]]` tables in order, first the
        // file (with its path), second type_at_cursor.
        let p = loader::load_from_path(&path).unwrap();
        assert_eq!(p.outputs.len(), 2, "{:?}", p.outputs);
        assert!(
            matches!(&p.outputs[0], OutputDest::File { path } if path.contains(".flac")),
            "{:?}",
            p.outputs[0]
        );
        assert_eq!(p.outputs[1], OutputDest::TypeAtCursor);

        // A comment on an unrelated section survives the output rewrite.
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("# the microphone to capture"), "{body}");
        assert!(body.contains("type = \"type_at_cursor\""), "{body}");
    }

    #[test]
    fn set_outputs_round_trips_file_and_clipboard_tokens() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);

        set_outputs_at(
            "custom",
            &path,
            &[
                OutputDest::File {
                    path: "~/Recordings/zwhisper/{profile}/{timestamp}.flac".to_owned(),
                },
                OutputDest::Clipboard,
            ],
        )
        .unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("type = \"file\""), "{body}");
        assert!(body.contains("type = \"clipboard\""), "{body}");

        let p = loader::load_from_path(&path).unwrap();
        assert_eq!(p.outputs.len(), 2, "{:?}", p.outputs);
        assert!(matches!(&p.outputs[0], OutputDest::File { .. }));
        assert_eq!(p.outputs[1], OutputDest::Clipboard);
    }

    #[test]
    fn set_outputs_empty_removes_key_and_still_validates() {
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);

        set_outputs_at("custom", &path, &[]).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(!body.contains("[[output]]"), "key should be gone: {body}");
        // Zero outputs is a valid profile; it still loads.
        let p = loader::load_from_path(&path).unwrap();
        assert!(p.outputs.is_empty(), "{:?}", p.outputs);
    }

    #[test]
    fn set_outputs_refuses_non_user_override() {
        // `set_outputs` (the public entry) existence-checks the
        // user-override path. A name with no user file → NotFound, so the
        // CLI can tell the user to clone first. Use a name extremely
        // unlikely to have a real user override on the host.
        let err = set_outputs(
            "definitely-not-a-real-user-profile-xyz",
            &[OutputDest::Clipboard],
        )
        .unwrap_err();
        assert!(matches!(err, ProfileError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn set_outputs_atomic_no_partial_on_validation_failure() {
        // If the rewrite would produce an invalid profile (round-trip
        // validate fails on a bad File path token), the original file
        // must remain byte-for-byte unchanged.
        let dir = TempDir::new().unwrap();
        let path = write_user_profile(&dir, PROFILE_WITH_COMMENTS);
        let original = fs::read_to_string(&path).unwrap();

        let err = set_outputs_at(
            "custom",
            &path,
            &[OutputDest::File {
                path: "/tmp/{bad}".to_owned(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, ProfileError::Validation { .. }), "{err:?}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "file must be untouched when round-trip validation fails"
        );
    }
}
