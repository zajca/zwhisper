//! Transcription job queue (RFC-daemon-role Feature 1).
//!
//! A **sibling** of the recording slot, never the same lane (F1.3):
//! recording is a single-slot resource (`SessionManager`); transcription
//! is a multi-item, serialized queue. Auto-transcribe (post-record),
//! `Jobs1.TranscribeFile`, and (Phase 4) `History1.Retry` are all jobs
//! on this one lane. Recording and transcription proceed concurrently —
//! the C5 invariant (lifecycle releases the recording slot before the
//! transcribe step) is preserved because the job runs after release.
//!
//! See [`queue::JobQueue`] for the worker; this module holds the value
//! types shared between the queue and the `Jobs1` interface.

pub(crate) mod queue;

use std::fmt;

use uuid::Uuid;
use zwhisper_core::profile::schema::OutputDest;

pub(crate) use queue::JobQueue;

/// Identity of a queued transcription job. A DISTINCT namespace from
/// `SessionId` (Finding arch#1 / DA#3): a job id is never a session id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct JobId(Uuid);

impl JobId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        s.parse::<Uuid>().ok().map(Self)
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hyphenated UUID, matching SessionId's rendering.
        write!(f, "{}", self.0)
    }
}

/// How the job was submitted — drives the consumer's intent-based
/// stale-clipboard guard (F3.3). Foreground = the user is actively
/// waiting (synchronous `transcribe --queue` / dictation); Detached =
/// explicit `--detach`; Auto = post-record background auto-transcribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitMode {
    Foreground,
    Detached,
    Auto,
}

impl SubmitMode {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Detached => "detached",
            Self::Auto => "auto",
        }
    }
}

/// Lifecycle state of a job, as surfaced by `JobProgress` / `ListJobs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Encode `profile.outputs` into the `aas` payload carried by
/// `Jobs1.JobCompleted` (F3.1). The daemon resolves outputs at
/// completion time so the session-bound consumer acts on the payload,
/// never on a fresh disk read. Encoding: `File{path}` -> `["file",
/// path]`, `Clipboard` -> `["clipboard"]`, `Notification` ->
/// `["notification"]`.
pub(crate) fn encode_outputs(outputs: &[OutputDest]) -> Vec<Vec<String>> {
    outputs
        .iter()
        .map(|o| match o {
            OutputDest::File { path } => vec!["file".to_owned(), path.clone()],
            OutputDest::Clipboard => vec!["clipboard".to_owned()],
            OutputDest::Notification => vec!["notification".to_owned()],
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn job_id_round_trips_through_string() {
        let id = JobId::new();
        let s = id.to_string();
        assert_eq!(JobId::parse(&s), Some(id));
    }

    #[test]
    fn job_id_parse_rejects_garbage() {
        assert_eq!(JobId::parse("not-a-uuid"), None);
    }

    #[test]
    fn submit_mode_wire_strings() {
        assert_eq!(SubmitMode::Foreground.as_wire(), "foreground");
        assert_eq!(SubmitMode::Detached.as_wire(), "detached");
        assert_eq!(SubmitMode::Auto.as_wire(), "auto");
    }

    #[test]
    fn encode_outputs_covers_all_variants() {
        let outs = vec![
            OutputDest::File {
                path: "/tmp/t.txt".to_owned(),
            },
            OutputDest::Clipboard,
            OutputDest::Notification,
        ];
        let enc = encode_outputs(&outs);
        assert_eq!(enc[0], vec!["file".to_owned(), "/tmp/t.txt".to_owned()]);
        assert_eq!(enc[1], vec!["clipboard".to_owned()]);
        assert_eq!(enc[2], vec!["notification".to_owned()]);
    }
}
