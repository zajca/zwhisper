use std::path::PathBuf;

use clap::Args;
use color_eyre::eyre::bail;
use tracing::info;

use crate::audio::recorder::{RecordOptions, record_blocking};

#[derive(Debug, Args)]
pub(crate) struct RecordArgs {
    /// `PipeWire` mic source (node name or `default`).
    #[arg(long, default_value = "default")]
    pub(crate) mic: String,

    /// `PipeWire` sink monitor (node name or `default`).
    #[arg(long, default_value = "default")]
    pub(crate) monitor: String,

    /// Output FLAC path.
    #[arg(long)]
    pub(crate) output: PathBuf,

    /// Recording duration in seconds (0 = run until Ctrl+C, but still
    /// capped by `--max-duration-minutes`).
    #[arg(long, default_value_t = 0)]
    pub(crate) duration: u64,

    /// Hard upper bound on a single recording, in minutes. Acts as the
    /// `max_duration_minutes` safeguard from IDEA.md against runaway
    /// captures. Pass `0` to opt out explicitly.
    #[arg(long, default_value_t = 240)]
    pub(crate) max_duration_minutes: u64,
}

#[derive(Debug, Args)]
pub(crate) struct TranscribeArgs {
    /// Input audio file.
    pub(crate) input: PathBuf,

    /// Backend identifier (e.g. `whisper-cpp`).
    #[arg(long, default_value = "whisper-cpp")]
    pub(crate) backend: String,

    /// Model name (backend-specific).
    #[arg(long, default_value = "small")]
    pub(crate) model: String,

    /// Source language (ISO 639-1, e.g. `cs`, `en`).
    #[arg(long, default_value = "auto")]
    pub(crate) language: String,
}

pub(crate) fn run_record(args: &RecordArgs) -> color_eyre::Result<()> {
    info!(
        mic = %args.mic,
        monitor = %args.monitor,
        output = %args.output.display(),
        duration_s = args.duration,
        max_duration_minutes = args.max_duration_minutes,
        "record requested",
    );

    let effective_duration = resolve_duration(args.duration, args.max_duration_minutes)?;
    info!(
        requested_duration_s = args.duration,
        effective_duration_s = effective_duration,
        "duration resolved against safety cap"
    );

    let opts = RecordOptions {
        mic: args.mic.clone(),
        monitor: args.monitor.clone(),
        output: args.output.clone(),
    };

    let report = record_blocking(opts, effective_duration)
        .map_err(|e| color_eyre::eyre::eyre!("recording failed: {e}"))?;

    info!(
        session_id = %report.session_id,
        duration_ms = u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
        samples_written = report.samples_written,
        underruns = report.underruns,
        warnings = report.warnings.len(),
        audio_path = %report.audio_path.display(),
        "recording complete",
    );

    Ok(())
}

/// Apply the runaway-recording safeguard (`max_duration_minutes` from
/// IDEA.md § 1) against the user-supplied `--duration` value.
///
/// Rules:
/// - `max_minutes == 0` → user explicitly disabled the cap; return
///   `duration` verbatim (including 0 = unlimited).
/// - otherwise the cap is `max_minutes * 60` seconds.
///   - `duration == 0` is interpreted as "run until Ctrl+C", which
///     under a non-zero cap means "stop at the cap" → return the cap.
///   - `duration > cap` is rejected with a typed error: silently
///     capping would surprise scripts that pass an explicit duration.
fn resolve_duration(duration_s: u64, max_minutes: u64) -> color_eyre::Result<u64> {
    if max_minutes == 0 {
        return Ok(duration_s);
    }
    let cap_s = max_minutes.saturating_mul(60);
    if duration_s == 0 {
        return Ok(cap_s);
    }
    if duration_s > cap_s {
        bail!(
            "--duration {duration_s}s exceeds --max-duration-minutes {max_minutes} ({cap_s}s); \
             pass --max-duration-minutes 0 to opt out of the safeguard"
        );
    }
    Ok(duration_s)
}

pub(crate) fn run_transcribe(args: &TranscribeArgs) -> color_eyre::Result<()> {
    info!(
        input = %args.input.display(),
        backend = %args.backend,
        model = %args.model,
        language = %args.language,
        "transcribe requested",
    );
    bail!("transcribe: not implemented yet — pending M1 whisper.cpp integration");
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::resolve_duration;

    #[test]
    fn cap_disabled_passes_duration_through() {
        assert_eq!(resolve_duration(0, 0).unwrap(), 0);
        assert_eq!(resolve_duration(99_999, 0).unwrap(), 99_999);
    }

    #[test]
    fn duration_zero_uses_cap() {
        assert_eq!(resolve_duration(0, 240).unwrap(), 240 * 60);
    }

    #[test]
    fn duration_within_cap_is_passed_through() {
        assert_eq!(resolve_duration(60, 240).unwrap(), 60);
        assert_eq!(resolve_duration(240 * 60, 240).unwrap(), 240 * 60);
    }

    #[test]
    fn duration_above_cap_is_rejected() {
        let err = resolve_duration(240 * 60 + 1, 240).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exceeds"), "unexpected message: {msg}");
        assert!(msg.contains("max-duration-minutes"), "unexpected message: {msg}");
    }
}
