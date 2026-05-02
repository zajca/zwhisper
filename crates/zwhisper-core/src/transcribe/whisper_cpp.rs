//! `whisper.cpp` backend: spawns `whisper-cli`, parses JSON output.
//!
//! The runner is structured around three indirections so unit tests
//! can exercise every branch without a real `whisper-cli` install:
//!
//! 1. Binary path comes from a [`Locator`-equivalent] override or
//!    [`super::discovery::locate_whisper_cli`].
//! 2. Subprocess execution goes through the [`Runner`] trait so
//!    tests can simulate filesystem effects + exit codes.
//! 3. Per-call working directory is a `tempfile::TempDir`, never
//!    the user's `$PWD`.
//!
//! Production wires up [`SystemRunner`].

// The runner is consumed by the CLI in M1 phase 4. Until then the
// public façade in `mod.rs` exposes it via `transcribe_file`. Unit
// tests below exercise every branch.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use super::error::TranscribeError;
use super::{Capabilities, TranscribeOpts, Transcriber, TranscriptArtifacts};
use super::{discovery, models};

const STEM: &str = "transcript";
const LOG_TRUNCATE_BYTES: usize = 4096;

/// Output of a [`Runner::run`] invocation. Mirrors the relevant
/// `std::process::Output` fields without dragging in the std type
/// directly so the trait stays mockable.
#[derive(Debug)]
pub(crate) struct RunOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Indirection over subprocess execution so unit tests can simulate
/// the filesystem side-effects of a `whisper-cli` run (writing
/// `<stem>.txt` / `<stem>.json` into the cwd).
#[async_trait]
pub(crate) trait Runner: Send + Sync {
    /// Run `cmd` to completion. `workdir` is provided alongside
    /// because tests need to know where to drop the simulated
    /// output files; the production runner already configured
    /// `cmd.current_dir(workdir)` and ignores the extra arg.
    async fn run(&self, cmd: Command, workdir: &Path) -> Result<RunOutput, TranscribeError>;
}

/// Production runner: spawns the command via `tokio::process` and
/// awaits stdout/stderr capture.
#[derive(Debug, Default)]
pub(crate) struct SystemRunner;

#[async_trait]
impl Runner for SystemRunner {
    async fn run(&self, mut cmd: Command, _workdir: &Path) -> Result<RunOutput, TranscribeError> {
        // Use `output()` to await the process and capture both
        // pipes in one shot. Spawn errors map straight to
        // `BackendSpawn`; the caller already populated `tool` via
        // `cmd.get_program()` so we don't repeat it here — instead
        // we pass the responsibility back as a pure io::Error and
        // let the caller wrap it.
        match cmd.output().await {
            Ok(o) => Ok(RunOutput {
                status: o.status,
                stdout: o.stdout,
                stderr: o.stderr,
            }),
            Err(e) => Err(TranscribeError::BackendSpawn {
                // Best-effort tool path — caller-side wrap is more
                // accurate, but we don't have access here. Use an
                // empty PathBuf as a sentinel; the production code
                // path in [`WhisperCppLocal::transcribe_file`]
                // re-wraps spawn failures with the resolved binary
                // path before returning.
                tool: PathBuf::new(),
                source: e,
            }),
        }
    }
}

/// `whisper.cpp` post-process backend. Runs `whisper-cli` as a
/// subprocess per the contract in `M1-plan.md` § "Subprocess
/// invocation contract".
pub(crate) struct WhisperCppLocal {
    /// Optional override for the binary path. When `None`, the
    /// runner falls back to [`super::discovery::locate_whisper_cli`].
    locator_override: Option<PathBuf>,
    /// Optional override for the model path. When `None`, the
    /// runner falls back to [`super::models::resolve_model`]. Test
    /// seam only — production never sets this.
    #[cfg(test)]
    model_path_override: Option<PathBuf>,
    runner: Box<dyn Runner>,
}

impl std::fmt::Debug for WhisperCppLocal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("WhisperCppLocal");
        d.field("locator_override", &self.locator_override);
        #[cfg(test)]
        d.field("model_path_override", &self.model_path_override);
        d.field("runner", &"<dyn Runner>").finish()
    }
}

impl WhisperCppLocal {
    /// Production constructor — discovers the binary and uses the
    /// real subprocess runner.
    pub(crate) fn new() -> Self {
        Self {
            locator_override: None,
            #[cfg(test)]
            model_path_override: None,
            runner: Box::new(SystemRunner),
        }
    }

    /// Test constructor — injects a mock runner plus binary +
    /// model path overrides. The locator/model paths may point at
    /// non-existent files; tests that don't need real artefacts
    /// use arbitrary `PathBuf`s.
    #[cfg(test)]
    pub(crate) fn with_runner_and_locator(
        runner: Box<dyn Runner>,
        locator_override: PathBuf,
        model_path_override: PathBuf,
    ) -> Self {
        Self {
            locator_override: Some(locator_override),
            model_path_override: Some(model_path_override),
            runner,
        }
    }

    /// Resolve the binary path: explicit override > discovery.
    fn resolve_binary(&self) -> Result<PathBuf, TranscribeError> {
        if let Some(p) = &self.locator_override {
            return Ok(p.clone());
        }
        discovery::locate_whisper_cli()
    }

    /// Resolve the model path: test override > production
    /// resolver. Production callers always go through
    /// [`models::resolve_model`].
    #[allow(clippy::unused_self)] // `self` carries the test-only override branch.
    fn resolve_model_path(&self, name: &str) -> Result<PathBuf, TranscribeError> {
        #[cfg(test)]
        if let Some(p) = &self.model_path_override {
            return Ok(p.clone());
        }
        models::resolve_model(name)
    }
}

#[async_trait]
impl Transcriber for WhisperCppLocal {
    fn id(&self) -> &'static str {
        "whisper-cpp"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: false,
            true_diarization: false,
            // Empty list = auto-only at the trait level. We do not
            // enumerate whisper.cpp's 99 supported languages for
            // M1 — `--language auto` covers the common path and
            // M2 profiles will own per-language wiring.
            languages: vec!["auto"],
        }
    }

    #[allow(clippy::too_many_lines)] // Linear pipeline; splitting hurts readability.
    async fn transcribe_file(
        &self,
        audio: &Path,
        opts: &TranscribeOpts,
    ) -> Result<TranscriptArtifacts, TranscribeError> {
        let started_at = Instant::now();

        // 1. Resolve the audio path to an absolute, canonical form
        //    for the subprocess invocation. `current_dir(tempdir)`
        //    (set in step 4) means any relative path passed to
        //    whisper-cli would be resolved against the tempdir and
        //    fail. We canonicalise only the value handed to the
        //    subprocess; the original `audio` path is kept for the
        //    output rename targets so `<audio>.txt` / `<audio>.json`
        //    land where the user expected, even through symlinks.
        //    Canonicalisation doubles as the existence check (same
        //    semantic as `metadata`).
        let audio_for_subprocess = match tokio::fs::canonicalize(audio).await {
            Ok(p) => p,
            Err(source) => {
                let err = TranscribeError::InputAudio {
                    path: audio.to_path_buf(),
                    source,
                };
                write_backtest_log(
                    audio,
                    opts,
                    "err:input-audio",
                    started_at.elapsed(),
                    None,
                    None,
                )
                .await;
                return Err(err);
            }
        };

        // 2. Resolve binary + model.
        let binary = match self.resolve_binary() {
            Ok(p) => p,
            Err(e) => {
                write_backtest_log(
                    audio,
                    opts,
                    "err:backend-unavailable",
                    started_at.elapsed(),
                    None,
                    None,
                )
                .await;
                return Err(e);
            }
        };
        let model_path = match self.resolve_model_path(&opts.model) {
            Ok(p) => p,
            Err(e) => {
                let status = match &e {
                    TranscribeError::ModelNotFound { .. } => "err:model-not-found",
                    TranscribeError::InvalidModelName { .. } => "err:invalid-model-name",
                    _ => "err:model",
                };
                write_backtest_log(audio, opts, status, started_at.elapsed(), None, None).await;
                return Err(e);
            }
        };

        // 3. Per-call tempdir; whisper-cli writes <stem>.txt /
        //    <stem>.json into cwd next to <stem> (`--output-file`
        //    documents `output file path (without file extension)`).
        let tempdir = match tempfile::tempdir() {
            Ok(t) => t,
            Err(source) => {
                let path = std::env::temp_dir();
                let err = TranscribeError::OutputUnreadable { path, source };
                write_backtest_log(audio, opts, "err:tempdir", started_at.elapsed(), None, None)
                    .await;
                return Err(err);
            }
        };
        let stem = tempdir.path().join(STEM);

        // 4. Build the command. Argument order matches the help
        //    snapshot; positional audio path is last.
        let mut cmd = Command::new(&binary);
        cmd.arg("--model")
            .arg(&model_path)
            .arg("--language")
            .arg(&opts.language)
            .arg("--output-txt")
            .arg("--output-json")
            .arg("--output-file")
            .arg(&stem)
            .arg(&audio_for_subprocess)
            .current_dir(tempdir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        info!(
            tool = %binary.display(),
            model = %opts.model,
            language = %opts.language,
            audio = %audio.display(),
            "transcribe start"
        );

        // 5. Run.
        let run_result = self.runner.run(cmd, tempdir.path()).await;
        let output = match run_result {
            Ok(o) => o,
            Err(e) => {
                // Re-wrap spawn errors with the resolved binary
                // path. SystemRunner returns BackendSpawn { tool:
                // empty } as a sentinel.
                let wrapped = match e {
                    TranscribeError::BackendSpawn { tool, source }
                        if tool.as_os_str().is_empty() =>
                    {
                        TranscribeError::BackendSpawn {
                            tool: binary.clone(),
                            source,
                        }
                    }
                    other => other,
                };
                error!(tool = %binary.display(), error = %wrapped, "transcribe spawn failed");
                write_backtest_log(audio, opts, "err:spawn", started_at.elapsed(), None, None)
                    .await;
                return Err(wrapped);
            }
        };

        // 6. Non-zero exit ⇒ BackendExitedNonZero. Stderr is
        //    preserved verbatim for tracing.
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            error!(
                tool = %binary.display(),
                status = ?output.status,
                stderr = %stderr,
                "transcribe failed"
            );
            let err = TranscribeError::BackendExitedNonZero {
                tool: binary.clone(),
                status: output.status,
                stderr,
            };
            write_backtest_log(
                audio,
                opts,
                "err:nonzero-exit",
                started_at.elapsed(),
                None,
                None,
            )
            .await;
            return Err(err);
        }

        // 7. Log truncated stdout/stderr at info on success.
        let stdout_lossy = String::from_utf8_lossy(&output.stdout);
        let stderr_lossy = String::from_utf8_lossy(&output.stderr);
        debug!(
            tool = %binary.display(),
            stdout = %truncate_for_log(&stdout_lossy),
            stderr = %truncate_for_log(&stderr_lossy),
            "transcribe stdio"
        );

        // 8. Verify the expected output files exist in the tempdir.
        let txt_in_temp = tempdir.path().join(format!("{STEM}.txt"));
        let json_in_temp = tempdir.path().join(format!("{STEM}.json"));
        for p in [&txt_in_temp, &json_in_temp] {
            if !p.is_file() {
                let err = TranscribeError::OutputMissing { path: p.clone() };
                write_backtest_log(
                    audio,
                    opts,
                    "err:output-missing",
                    started_at.elapsed(),
                    None,
                    None,
                )
                .await;
                return Err(err);
            }
        }

        // 9. Parse audio_duration from the JSON before moving
        //    files. `Duration::ZERO` for empty / unparseable
        //    transcripts — we don't fail the run on a parse error;
        //    M5 will tighten this once schema validation lands.
        let audio_duration = parse_audio_duration(&json_in_temp).await;

        // 10. Move outputs next to the audio. `<audio>.txt` /
        //     `<audio>.json` — OsString concat preserves non-UTF-8
        //     paths (Linux audio dirs may not be valid UTF-8).
        let txt_target = append_extension(audio, ".txt");
        let json_target = append_extension(audio, ".json");
        if let Err(e) = move_or_copy(&txt_in_temp, &txt_target).await {
            write_backtest_log(
                audio,
                opts,
                "err:move-txt",
                started_at.elapsed(),
                None,
                None,
            )
            .await;
            return Err(e);
        }
        if let Err(e) = move_or_copy(&json_in_temp, &json_target).await {
            write_backtest_log(
                audio,
                opts,
                "err:move-json",
                started_at.elapsed(),
                None,
                None,
            )
            .await;
            return Err(e);
        }

        let duration = started_at.elapsed();
        let artifacts = TranscriptArtifacts {
            txt_path: txt_target.clone(),
            json_path: json_target.clone(),
            duration,
            audio_duration,
            language: opts.language.clone(),
            model: opts.model.clone(),
            // whisper.cpp does not emit speaker labels; M5 callers
            // observing `speakers.is_none()` know diarization is
            // unavailable for this backend. See § Capabilities and
            // M5-plan.md DoD #7.
            speakers: None,
        };

        info!(
            tool = %binary.display(),
            txt = %txt_target.display(),
            json = %json_target.display(),
            audio_duration_ms = u64::try_from(audio_duration.as_millis()).unwrap_or(u64::MAX),
            wall_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            "transcribe ok"
        );

        write_backtest_log(
            audio,
            opts,
            "ok",
            duration,
            Some(&txt_target),
            Some(&json_target),
        )
        .await;

        Ok(artifacts)
    }
}

/// Concatenate `suffix` (e.g. `.txt`) onto an audio path without
/// going through UTF-8. `Path::with_extension` would *replace*
/// the existing extension, but we want `x.flac` → `x.flac.txt`.
fn append_extension(audio: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = audio.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Truncate a text payload for log embedding. Operates on byte
/// length but respects char boundaries so we never split a UTF-8
/// sequence.
fn truncate_for_log(s: &str) -> String {
    if s.len() <= LOG_TRUNCATE_BYTES {
        return s.to_owned();
    }
    let mut cut = LOG_TRUNCATE_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… [truncated {} bytes]", &s[..cut], s.len() - cut)
}

/// `tokio::fs::rename` first; on cross-filesystem error (Unix
/// `EXDEV`, Windows `ERROR_NOT_SAME_DEVICE`, or
/// `io::ErrorKind::CrossesDevices` once it stabilises in std) fall
/// back to copy + remove. The platform mapping lives in
/// [`is_cross_device`]. All failure paths surface as
/// [`TranscribeError::OutputUnreadable`] with the *target* path so
/// the user sees where we tried to land the file.
async fn move_or_copy(src: &Path, dst: &Path) -> Result<(), TranscribeError> {
    match tokio::fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            // Cross-fs fallback: copy then remove the source.
            if let Err(copy_err) = tokio::fs::copy(src, dst).await {
                return Err(TranscribeError::OutputUnreadable {
                    path: dst.to_path_buf(),
                    source: copy_err,
                });
            }
            if let Err(remove_err) = tokio::fs::remove_file(src).await {
                // The destination is already in place; failing to
                // remove the temp source is recoverable (the
                // tempdir cleanup will sweep it). Warn but don't
                // fail.
                warn!(
                    src = %src.display(),
                    error = %remove_err,
                    "could not remove tempfile after cross-fs copy"
                );
            }
            Ok(())
        }
        Err(e) => Err(TranscribeError::OutputUnreadable {
            path: dst.to_path_buf(),
            source: e,
        }),
    }
}

/// Detect cross-device-link errors. `io::ErrorKind::CrossesDevices`
/// is unstable on stable Rust, so we map to the platform-native
/// errno: `EXDEV` on Unix (libc constant; differs in principle
/// between OSes even though Linux/macOS/BSD all happen to use 18),
/// `ERROR_NOT_SAME_DEVICE` (17) on Windows. Other targets fall
/// through to `false` — `move_or_copy` will propagate the original
/// error as `OutputUnreadable` instead of attempting a fallback.
/// `winerror.h` constant. Windows surfaces this raw OS error code
/// when `MoveFile` (which `tokio::fs::rename` calls into) is asked
/// to cross volumes. `std` does not re-export it and a single
/// `i32` doesn't justify pulling in `windows-sys`, so we name it
/// here. Used by [`is_cross_device`].
#[cfg(windows)]
const ERROR_NOT_SAME_DEVICE: i32 = 17;

fn is_cross_device(e: &std::io::Error) -> bool {
    let Some(raw) = e.raw_os_error() else {
        return false;
    };
    #[cfg(unix)]
    {
        raw == libc::EXDEV
    }
    #[cfg(windows)]
    {
        raw == ERROR_NOT_SAME_DEVICE
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = raw;
        false
    }
}

// ----- JSON segment parsing ---------------------------------------
//
// Mirrors the shape `whisper-cli --output-json` writes: a top-level
// object with a `transcription` array of segment objects. The
// fixture committed at
// `crates/zwhisper-cli/tests/fixtures/whisper-cpp-segments.json`
// locks the shape; if upstream whisper.cpp changes its JSON layout,
// the shape-validation tests below will fail loudly.
//
// Extra top-level fields (`systeminfo`, `model`, `params`,
// `result`, …) are tolerated by serde's default behaviour — we only
// care about `transcription`.

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Transcript {
    #[serde(default)]
    pub(crate) transcription: Vec<Segment>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Segment {
    #[serde(default)]
    pub(crate) offsets: Option<Offsets>,
    #[serde(default)]
    pub(crate) text: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Offsets {
    /// Segment start offset in milliseconds.
    #[serde(default)]
    pub(crate) from: u64,
    /// Segment end offset in milliseconds.
    #[serde(default)]
    pub(crate) to: u64,
}

/// Pure deserialiser for the `whisper-cli --output-json` shape.
///
/// Returns the inner `transcription` array on success. The function
/// is intentionally low-level — it does not carry any path context,
/// so the error type is `serde_json::Error` directly. Callers that
/// have a path (the file the JSON came from) should prefer
/// [`parse_segments_file`], which wraps the error into a
/// [`TranscribeError::JsonShape`] with the offending path attached.
///
/// Tolerates unknown top-level fields (whisper.cpp emits
/// `systeminfo`, `model`, `params`, `result` alongside
/// `transcription`).
pub(crate) fn parse_segments(s: &str) -> Result<Vec<Segment>, serde_json::Error> {
    let parsed: Transcript = serde_json::from_str(s)?;
    Ok(parsed.transcription)
}

/// Read a `whisper-cli --output-json` file and parse its segments.
///
/// On parse failure, wraps the underlying [`serde_json::Error`]
/// into [`TranscribeError::JsonShape`] with the originating path so
/// callers can surface a useful diagnostic. Filesystem read errors
/// are mapped to [`TranscribeError::OutputUnreadable`] for symmetry
/// with the rest of the post-process pipeline.
#[allow(dead_code)] // Wired up in M1 phase 6+; keeps the shape-validation entry point exposed.
pub(crate) async fn parse_segments_file(path: &Path) -> Result<Vec<Segment>, TranscribeError> {
    let bytes =
        tokio::fs::read(path)
            .await
            .map_err(|source| TranscribeError::OutputUnreadable {
                path: path.to_path_buf(),
                source,
            })?;
    let s = std::str::from_utf8(&bytes).map_err(|utf8_err| {
        // Map non-UTF-8 bytes to a `JsonShape` parse error so the
        // user still gets a path-bearing diagnostic.
        let synthetic = serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            utf8_err,
        ));
        TranscribeError::JsonShape {
            path: path.to_path_buf(),
            source: synthetic,
        }
    })?;
    parse_segments(s).map_err(|source| TranscribeError::JsonShape {
        path: path.to_path_buf(),
        source,
    })
}

/// Parse the audio duration from the last segment's `offsets.to`
/// (milliseconds). Returns `Duration::ZERO` on empty / malformed
/// input — a parse failure should not fail the run, only log.
///
/// Implementation note: re-uses [`parse_segments`] so a single
/// deserialiser controls the JSON shape contract. Failures are
/// logged and downgraded to `Duration::ZERO`.
async fn parse_audio_duration(json_path: &Path) -> Duration {
    let bytes = match tokio::fs::read(json_path).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path = %json_path.display(), error = %e, "could not read transcript JSON");
            return Duration::ZERO;
        }
    };
    let s = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %json_path.display(), error = %e, "transcript JSON is not valid UTF-8");
            return Duration::ZERO;
        }
    };
    let segments = match parse_segments(s) {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %json_path.display(), error = %e, "could not parse transcript JSON");
            return Duration::ZERO;
        }
    };
    let Some(last) = segments.last() else {
        return Duration::ZERO;
    };
    match &last.offsets {
        Some(o) => Duration::from_millis(o.to),
        None => Duration::ZERO,
    }
}

// ----- Backtest log -----------------------------------------------

/// One JSON line per transcribe call, written to
/// `${XDG_STATE_HOME:-~/.local/state}/zwhisper/transcribe.log`.
/// Honours the IDEA.md § 7 "no transcript text in logs" rule —
/// we record paths and metadata only.
#[derive(Debug, Serialize)]
struct BacktestEntry<'a> {
    ts: String,
    audio: String,
    model: &'a str,
    language: &'a str,
    backend: &'a str,
    status: &'a str,
    duration_ms: u64,
    txt_path: Option<String>,
    json_path: Option<String>,
}

async fn write_backtest_log(
    audio: &Path,
    opts: &TranscribeOpts,
    status: &str,
    duration: Duration,
    txt_path: Option<&Path>,
    json_path: Option<&Path>,
) {
    let Some(state_dir) = backtest_log_dir() else {
        warn!("could not resolve XDG_STATE_HOME; skipping backtest log line");
        return;
    };
    let log_dir = state_dir.join("zwhisper");
    if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
        warn!(dir = %log_dir.display(), error = %e, "could not create backtest log dir");
        return;
    }
    let log_path = log_dir.join("transcribe.log");

    let entry = BacktestEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        audio: audio.display().to_string(),
        model: &opts.model,
        language: &opts.language,
        backend: &opts.backend,
        status,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        txt_path: txt_path.map(|p| p.display().to_string()),
        json_path: json_path.map(|p| p.display().to_string()),
    };

    let line = match serde_json::to_string(&entry) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not serialise backtest entry");
            return;
        }
    };

    let mut buf = line.into_bytes();
    buf.push(b'\n');
    let opened = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await;
    match opened {
        Ok(mut f) => {
            use tokio::io::AsyncWriteExt;
            if let Err(e) = f.write_all(&buf).await {
                warn!(path = %log_path.display(), error = %e, "could not write backtest log line");
            }
        }
        Err(e) => {
            warn!(path = %log_path.display(), error = %e, "could not open backtest log");
        }
    }
}

/// `dirs::state_dir()` falls back to `dirs::data_local_dir()` —
/// neither is available on every platform, so log gracefully when
/// both return `None`.
fn backtest_log_dir() -> Option<PathBuf> {
    dirs::state_dir().or_else(dirs::data_local_dir)
}

// ===== Tests ======================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;

    use tempfile::TempDir;
    use tokio::fs;

    use super::*;

    /// Mock runner that performs configurable filesystem effects
    /// inside the workdir and returns a configurable status/stderr.
    struct MockRunner {
        /// Files to write into the workdir before returning. Each
        /// tuple is `(filename, contents)`.
        files: Vec<(String, Vec<u8>)>,
        /// Exit status to return.
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        /// If set, the runner returns this error instead of a
        /// successful `RunOutput`. Wrapped in Mutex<Option<…>> so
        /// the trait's `&self` stays object-safe.
        spawn_error: Mutex<Option<TranscribeError>>,
    }

    impl MockRunner {
        fn new() -> Self {
            Self {
                files: Vec::new(),
                status: ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
                spawn_error: Mutex::new(None),
            }
        }

        fn with_file(mut self, name: &str, contents: &[u8]) -> Self {
            self.files.push((name.to_owned(), contents.to_vec()));
            self
        }

        fn with_status(mut self, code: i32) -> Self {
            // ExitStatus::from_raw takes a wait(2)-style status:
            // exit code N is encoded as N << 8.
            self.status = ExitStatus::from_raw(code << 8);
            self
        }

        fn with_stderr(mut self, stderr: &[u8]) -> Self {
            self.stderr = stderr.to_vec();
            self
        }

        fn with_spawn_error(self, err: TranscribeError) -> Self {
            *self.spawn_error.lock().unwrap() = Some(err);
            self
        }
    }

    #[async_trait]
    impl Runner for MockRunner {
        async fn run(&self, _cmd: Command, workdir: &Path) -> Result<RunOutput, TranscribeError> {
            if let Some(e) = self.spawn_error.lock().unwrap().take() {
                return Err(e);
            }
            for (name, contents) in &self.files {
                fs::write(workdir.join(name), contents).await.unwrap();
            }
            Ok(RunOutput {
                status: self.status,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }
    }

    /// 1-segment fixture matching the whisper.cpp JSON schema.
    fn one_segment_json() -> &'static str {
        r#"{
  "transcription": [
    {
      "timestamps": { "from": "00:00:00,000", "to": "00:00:02,500" },
      "offsets": { "from": 0, "to": 2500 },
      "text": " Hello world."
    }
  ]
}"#
    }

    fn empty_segments_json() -> &'static str {
        r#"{ "transcription": [] }"#
    }

    /// Materialise a fake audio file in a tempdir so the
    /// existence check passes.
    async fn make_audio(name: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let audio = tmp.path().join(name);
        fs::write(&audio, b"\x66\x4cAC fake header").await.unwrap();
        (tmp, audio)
    }

    /// Materialise a fake `ggml-<name>.bin` inside a tempdir and
    /// return its path so tests can pass it as the
    /// `model_path_override` on [`WhisperCppLocal`]. Avoids
    /// touching `XDG_DATA_HOME` (which would require process-wide
    /// env mutation under `unsafe`, banned by workspace lints).
    fn make_model_file(name: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("zwhisper").join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let model = models_dir.join(format!("ggml-{name}.bin"));
        std::fs::write(&model, b"fake-model").unwrap();
        (tmp, model)
    }

    fn opts(model: &str) -> TranscribeOpts {
        TranscribeOpts {
            backend: "whisper-cpp".into(),
            model: model.into(),
            language: "auto".into(),
            ..Default::default()
        }
    }

    // ---- Tests ----

    #[tokio::test]
    async fn happy_path_returns_artifacts_pointing_at_renamed_files() {
        let (_model_dir, model_path) = make_model_file("small");
        let (_audio_dir, audio) = make_audio("clip.flac").await;

        let runner = MockRunner::new()
            .with_file("transcript.txt", b"Hello world.\n")
            .with_file("transcript.json", one_segment_json().as_bytes())
            .with_status(0);

        let backend = WhisperCppLocal::with_runner_and_locator(
            Box::new(runner),
            PathBuf::from("/usr/bin/whisper-cli"),
            model_path,
        );

        let artifacts = backend
            .transcribe_file(&audio, &opts("small"))
            .await
            .unwrap();

        let expected_txt = append_extension(&audio, ".txt");
        let expected_json = append_extension(&audio, ".json");
        assert_eq!(artifacts.txt_path, expected_txt);
        assert_eq!(artifacts.json_path, expected_json);
        assert!(expected_txt.is_file(), "txt should land next to audio");
        assert!(expected_json.is_file(), "json should land next to audio");
        assert_eq!(artifacts.audio_duration, Duration::from_millis(2500));
        assert_eq!(artifacts.language, "auto");
        assert_eq!(artifacts.model, "small");
        // M5 DoD #7: whisper.cpp never produces speaker labels, so
        // `speakers` is None and the resulting JSON file (whisper-cli's
        // own output) does not contain a `speakers` array.
        assert!(
            artifacts.speakers.is_none(),
            "whisper-cpp must not synthesise speaker labels"
        );
        let json_body = std::fs::read_to_string(&expected_json).unwrap();
        assert!(
            !json_body.contains("\"speakers\""),
            "whisper-cli output must not contain a `speakers` array"
        );
    }

    #[tokio::test]
    async fn subprocess_exits_nonzero_returns_backend_exited_nonzero() {
        let (_model_dir, model_path) = make_model_file("small");
        let (_audio_dir, audio) = make_audio("clip.flac").await;

        let runner = MockRunner::new()
            .with_status(1)
            .with_stderr(b"model load failed");

        let backend = WhisperCppLocal::with_runner_and_locator(
            Box::new(runner),
            PathBuf::from("/usr/bin/whisper-cli"),
            model_path,
        );

        let err = backend
            .transcribe_file(&audio, &opts("small"))
            .await
            .unwrap_err();
        match err {
            TranscribeError::BackendExitedNonZero { stderr, .. } => {
                assert!(
                    stderr.contains("model load failed"),
                    "stderr should preserve subprocess body, got: {stderr}"
                );
            }
            other => panic!("expected BackendExitedNonZero, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subprocess_produces_only_txt_returns_output_missing() {
        let (_model_dir, model_path) = make_model_file("small");
        let (_audio_dir, audio) = make_audio("clip.flac").await;

        let runner = MockRunner::new()
            .with_file("transcript.txt", b"Hello\n")
            .with_status(0);

        let backend = WhisperCppLocal::with_runner_and_locator(
            Box::new(runner),
            PathBuf::from("/usr/bin/whisper-cli"),
            model_path,
        );

        let err = backend
            .transcribe_file(&audio, &opts("small"))
            .await
            .unwrap_err();
        match err {
            TranscribeError::OutputMissing { path } => {
                let suffix = path.to_string_lossy();
                assert!(
                    suffix.ends_with(".json"),
                    "expected missing .json, got {suffix}"
                );
            }
            other => panic!("expected OutputMissing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_failure_returns_backend_spawn() {
        let (_model_dir, model_path) = make_model_file("small");
        let (_audio_dir, audio) = make_audio("clip.flac").await;

        let injected = TranscribeError::BackendSpawn {
            tool: PathBuf::new(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let runner = MockRunner::new().with_spawn_error(injected);

        let backend = WhisperCppLocal::with_runner_and_locator(
            Box::new(runner),
            PathBuf::from("/usr/bin/whisper-cli"),
            model_path,
        );

        let err = backend
            .transcribe_file(&audio, &opts("small"))
            .await
            .unwrap_err();
        match err {
            TranscribeError::BackendSpawn { tool, .. } => {
                // Sentinel empty path was re-wrapped with the
                // resolved binary path.
                assert_eq!(tool, PathBuf::from("/usr/bin/whisper-cli"));
            }
            other => panic!("expected BackendSpawn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn audio_path_does_not_exist_returns_input_audio_error() {
        let (_model_dir, model_path) = make_model_file("small");

        let missing = PathBuf::from("/tmp/zwhisper-nonexistent-clip.flac");
        // Make sure the path really does not exist.
        let _ = std::fs::remove_file(&missing);

        let runner = MockRunner::new();
        let backend = WhisperCppLocal::with_runner_and_locator(
            Box::new(runner),
            PathBuf::from("/usr/bin/whisper-cli"),
            model_path,
        );

        let err = backend
            .transcribe_file(&missing, &opts("small"))
            .await
            .unwrap_err();
        match err {
            TranscribeError::InputAudio { path, .. } => {
                assert_eq!(path, missing);
            }
            other => panic!("expected InputAudio, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_segments_json_yields_zero_audio_duration() {
        let (_model_dir, model_path) = make_model_file("small");
        let (_audio_dir, audio) = make_audio("clip.flac").await;

        let runner = MockRunner::new()
            .with_file("transcript.txt", b"")
            .with_file("transcript.json", empty_segments_json().as_bytes())
            .with_status(0);

        let backend = WhisperCppLocal::with_runner_and_locator(
            Box::new(runner),
            PathBuf::from("/usr/bin/whisper-cli"),
            model_path,
        );

        let artifacts = backend
            .transcribe_file(&audio, &opts("small"))
            .await
            .unwrap();
        assert_eq!(artifacts.audio_duration, Duration::ZERO);
    }

    #[tokio::test]
    async fn unknown_backend_via_facade_returns_backend_unknown() {
        let opts = TranscribeOpts {
            backend: "vaporware".into(),
            model: "small".into(),
            language: "auto".into(),
            ..Default::default()
        };
        let err = super::super::transcribe_file(Path::new("/tmp/x.flac"), &opts)
            .await
            .unwrap_err();
        match err {
            TranscribeError::BackendUnknown { name, supported } => {
                assert_eq!(name, "vaporware");
                assert_eq!(supported, vec!["whisper-cpp", "deepgram"]);
            }
            other => panic!("expected BackendUnknown, got {other:?}"),
        }
    }

    #[test]
    fn append_extension_is_osstr_safe() {
        let p = PathBuf::from("/tmp/x.flac");
        let txt = append_extension(&p, ".txt");
        assert_eq!(txt, PathBuf::from("/tmp/x.flac.txt"));
        let json = append_extension(&p, ".json");
        assert_eq!(json, PathBuf::from("/tmp/x.flac.json"));
    }

    #[cfg(unix)]
    #[test]
    fn is_cross_device_detects_exdev() {
        let exdev = std::io::Error::from_raw_os_error(libc::EXDEV);
        assert!(
            is_cross_device(&exdev),
            "EXDEV (errno {}) must trigger the cross-fs fallback",
            libc::EXDEV
        );
    }

    #[cfg(windows)]
    #[test]
    fn is_cross_device_detects_error_not_same_device() {
        let err = std::io::Error::from_raw_os_error(super::ERROR_NOT_SAME_DEVICE);
        assert!(
            is_cross_device(&err),
            "ERROR_NOT_SAME_DEVICE ({}) must trigger the cross-fs fallback",
            super::ERROR_NOT_SAME_DEVICE
        );
    }

    #[test]
    fn is_cross_device_ignores_unrelated_errors() {
        let enoent = std::io::Error::from_raw_os_error(2);
        assert!(
            !is_cross_device(&enoent),
            "ENOENT must not trigger fallback"
        );
        let synthetic = std::io::Error::other("no errno");
        assert!(
            !is_cross_device(&synthetic),
            "errors without raw_os_error must not trigger fallback"
        );
    }

    #[tokio::test]
    async fn parse_audio_duration_handles_empty_segments() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.json");
        fs::write(&p, empty_segments_json()).await.unwrap();
        assert_eq!(parse_audio_duration(&p).await, Duration::ZERO);
    }

    #[tokio::test]
    async fn parse_audio_duration_reads_last_segment_offset() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.json");
        fs::write(&p, one_segment_json()).await.unwrap();
        assert_eq!(parse_audio_duration(&p).await, Duration::from_millis(2500));
    }

    // ---- M1 Phase 5a: parse_segments shape validation ----------

    /// Committed fixture mirroring `whisper-cli --output-json`.
    /// Re-loaded at compile time so a missing/renamed file is a
    /// build failure, not a runtime skip.
    const FIXTURE_JSON: &str = include_str!("../../tests/fixtures/whisper-cpp-segments.json");

    #[test]
    fn parse_segments_accepts_valid_fixture() {
        let segments = parse_segments(FIXTURE_JSON).expect("fixture must parse");
        assert_eq!(segments.len(), 3, "fixture has 3 segments");
        let last = segments.last().expect("3 segments ⇒ has last");
        let offsets = last.offsets.as_ref().expect("last segment has offsets");
        assert_eq!(offsets.to, 8200);
        assert_eq!(offsets.from, 5800);
        let text = last.text.as_deref().unwrap_or_default();
        assert!(
            text.contains("zebras"),
            "last segment text should be from the third pangram, got: {text}"
        );
    }

    #[test]
    fn parse_segments_handles_empty_array() {
        let segments =
            parse_segments(r#"{ "transcription": [] }"#).expect("empty array is a valid shape");
        assert!(segments.is_empty());
    }

    #[test]
    fn parse_segments_rejects_missing_transcription_key() {
        // `transcription` defaults to an empty Vec via
        // `#[serde(default)]` for forward-compat with future
        // whisper.cpp output that may omit the key in degenerate
        // cases. So an object without `transcription` parses to
        // an empty Vec — that's intentional.
        //
        // What MUST be rejected: input that isn't an object at
        // all. Use a top-level scalar (number) to lock that.
        let err = parse_segments("42").expect_err("top-level scalar is not the documented shape");
        let msg = err.to_string();
        assert!(
            msg.contains("expected") || msg.contains("invalid"),
            "expected a serde type error, got: {msg}"
        );
    }

    #[test]
    fn parse_segments_object_without_transcription_yields_empty_vec() {
        // Forward-compat: missing `transcription` field defaults
        // to an empty Vec. Pin this so a future contributor doesn't
        // accidentally remove `#[serde(default)]` and break callers
        // that handle empty transcripts gracefully.
        let segments = parse_segments(r#"{ "foo": [] }"#)
            .expect("missing transcription key should default to empty");
        assert!(segments.is_empty());
    }

    #[test]
    fn parse_segments_rejects_wrong_type() {
        let err = parse_segments(r#"{ "transcription": "not an array" }"#)
            .expect_err("string is not Vec<Segment>");
        let msg = err.to_string();
        assert!(
            msg.contains("expected") || msg.contains("sequence") || msg.contains("string"),
            "expected a type-mismatch error, got: {msg}"
        );
    }

    #[test]
    fn parse_segments_ignores_extra_top_level_fields() {
        // The committed fixture carries `systeminfo`, `model`,
        // `params`, `result`. parse_segments must tolerate these
        // — confirmed implicitly by the fixture test above, but
        // pinned explicitly here so a future #[serde(deny_unknown_fields)]
        // is caught.
        let json = r#"{
            "systeminfo": "irrelevant",
            "model": { "type": "small" },
            "params": { "model": "x" },
            "result": { "language": "en" },
            "transcription": [
                {
                    "timestamps": { "from": "00:00:00,000", "to": "00:00:01,000" },
                    "offsets": { "from": 0, "to": 1000 },
                    "text": " hi"
                }
            ]
        }"#;
        let segments = parse_segments(json).expect("extra fields must be ignored");
        assert_eq!(segments.len(), 1);
        assert_eq!(
            segments[0]
                .offsets
                .as_ref()
                .map(|o| o.to)
                .unwrap_or_default(),
            1000
        );
    }

    #[tokio::test]
    async fn parse_segments_file_wraps_bad_shape_with_path() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bad.json");
        fs::write(&p, b"not even json").await.unwrap();
        let err = parse_segments_file(&p)
            .await
            .expect_err("malformed JSON must surface as JsonShape");
        match err {
            TranscribeError::JsonShape { path, .. } => {
                assert_eq!(path, p, "JsonShape must carry the offending path");
            }
            other => panic!("expected JsonShape, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_segments_file_accepts_committed_fixture() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("ok.json");
        fs::write(&p, FIXTURE_JSON).await.unwrap();
        let segments = parse_segments_file(&p)
            .await
            .expect("fixture must round-trip through parse_segments_file");
        assert_eq!(segments.len(), 3);
    }
}
