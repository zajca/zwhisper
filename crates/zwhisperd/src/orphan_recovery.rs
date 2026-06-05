//! Startup recovery of an orphaned recording session.
//!
//! ## Why this exists
//!
//! `active-session.json` is written when `StartRecording` begins and
//! cleared on the terminal `StateChanged`. An in-flight recording lives
//! entirely inside the daemon process (the GStreamer pipeline + its
//! watchdog are tokio tasks). So if the daemon is killed or crashes
//! mid-recording, three things are left behind and nothing reaps them:
//!
//! 1. the partially-written FLAC in the recordings dir,
//! 2. a stale `active-session.json` pointing at that session,
//! 3. no transcript, no history entry, no notification.
//!
//! The user observes "nothing was transcribed" with no error anywhere —
//! the silent failure this module fixes.
//!
//! ## What it does (decision: preserve audio + auto-transcribe)
//!
//! Because a recording cannot survive a restart, any `active-session.json`
//! present at startup is *definitionally* orphaned. On daemon start we:
//!
//! 1. read the stale state (session id + profile),
//! 2. derive the recording path and confirm it holds audio,
//! 3. write `last-session.json` phase 1 so the audio is discoverable
//!    even if transcription never completes,
//! 4. enqueue a tracked transcribe job for it (so it lands in history,
//!    delivers via the normal `Jobs1` → `deliver` path, and a
//!    `BackendNotCompiled`/other failure surfaces as a notification),
//! 5. clear `active-session.json`.
//!
//! The FLAC is never deleted — the file is the single source of truth.

use std::path::{Path, PathBuf};

use tracing::{info, warn};
use zwhisper_core::profile::schema::Backend;
use zwhisper_core::transcribe::{BackendSettings, TranscribeOpts};

use crate::active_session::{self, ActiveSession};
use crate::jobs::JobQueue;
use crate::jobs::SubmitMode;
use crate::jobs::queue::{JobSource, JobSpec};
use crate::last_session::{self, LastSession};
use crate::recorder_service::default_output_dir;

/// The recording artifact path for a session id, matching
/// `recorder_service::output_path_for_session`.
fn audio_path_for(session_id: &str) -> PathBuf {
    default_output_dir().join(format!("{session_id}.flac"))
}

/// Whether an orphaned recording is worth recovering: it must exist and
/// carry at least one byte. A missing or empty file means the pipeline
/// never wrote audio (the recording died before the first buffer), so
/// there is nothing to transcribe — we only clear the stale state.
fn worth_recovering(audio_len: Option<u64>) -> bool {
    matches!(audio_len, Some(len) if len > 0)
}

/// Run the orphan-recovery reaper. Best-effort: every failure is logged
/// and never aborts daemon startup. Must be called after the bus
/// connection is live (so the enqueued job can emit `Jobs1` signals).
pub(crate) async fn recover(queue: &JobQueue) {
    let Some(session) = active_session::read() else {
        return; // Steady state: no orphaned recording.
    };

    let audio_path = audio_path_for(&session.session_id);
    let audio_len = std::fs::metadata(&audio_path).ok().map(|m| m.len());

    if !worth_recovering(audio_len) {
        warn!(
            session_id = %session.session_id,
            profile = %session.profile,
            audio_path = %audio_path.display(),
            "orphaned recording has no usable audio; clearing stale active-session.json",
        );
        active_session::clear();
        return;
    }

    warn!(
        session_id = %session.session_id,
        profile = %session.profile,
        audio_path = %audio_path.display(),
        audio_bytes = audio_len.unwrap_or(0),
        "recovering orphaned recording interrupted by a previous daemon exit",
    );

    // Phase 1: make the audio discoverable immediately, before we depend
    // on the transcription succeeding.
    let last = LastSession::audio_only(&session.session_id, &audio_path);
    if let Err(e) = last_session::write_atomic(&last) {
        warn!(error = %e, "could not write last-session.json during recovery");
    }

    // Enqueue the transcription with the session's profile settings, so
    // it flows through history + Jobs1 + deliver exactly like an ordinary
    // job. A profile that no longer loads (renamed/deleted) means we keep
    // the audio but cannot transcribe it.
    match build_job_spec(&session, &audio_path) {
        Ok(spec) => match queue.submit(spec) {
            Ok(job_id) => info!(
                session_id = %session.session_id,
                %job_id,
                "enqueued recovery transcription job",
            ),
            Err(e) => warn!(error = %e, "could not enqueue recovery transcription job"),
        },
        Err(e) => warn!(
            error = %e,
            profile = %session.profile,
            "cannot transcribe orphaned recording (audio preserved via last-session.json)",
        ),
    }

    // Clear the stale state regardless of the transcription outcome — the
    // session is finalized; the job (if any) tracks its own lifecycle.
    active_session::clear();
}

/// Build the [`JobSpec`] for the recovered recording from its profile.
/// Mirrors `recorder_service`'s opts precedence (backend-specific Deepgram
/// model wins over the generic one).
fn build_job_spec(session: &ActiveSession, audio_path: &Path) -> color_eyre::Result<JobSpec> {
    let profile = zwhisper_core::profile::load(&session.profile)?;

    let model = match (
        profile.transcription.backend,
        &profile.transcription.deepgram,
    ) {
        (Backend::Deepgram, Some(dg)) => dg.model.clone(),
        _ => profile.transcription.model.clone(),
    };

    let opts = TranscribeOpts {
        backend: profile.transcription.backend,
        model,
        language: profile.transcription.language.clone(),
        settings: BackendSettings {
            whisper_cpp: profile.transcription.whisper_cpp.clone(),
            deepgram: profile.transcription.deepgram.clone(),
        },
    };

    Ok(JobSpec {
        session_id: session.session_id.clone(),
        source: JobSource::File(audio_path.to_path_buf()),
        opts,
        profile: profile.name.clone(),
        outputs: profile.outputs.clone(),
        submit_mode: SubmitMode::Detached,
        label: format!("recovered:{}", session.session_id),
        done: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn audio_path_uses_session_id_flac_in_recordings_dir() {
        let path = audio_path_for("abc-123");
        assert!(path.ends_with("recordings/abc-123.flac"), "{path:?}");
    }

    #[test]
    fn worth_recovering_requires_non_empty_audio() {
        assert!(worth_recovering(Some(70_000)));
        assert!(!worth_recovering(Some(0)));
        assert!(!worth_recovering(None));
    }
}
