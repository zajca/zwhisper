//! The single serialized history writer/reader task (F2.2).
//!
//! "The daemon is the single writer" is made **structural** here: one
//! task owns the in-memory `Vec<HistoryEntry>` and the on-disk
//! `history.json` exclusively. Every mutation and every query arrives
//! over one mpsc channel. There is no independent read-modify-write +
//! rename from multiple callers, so two concurrent jobs (a post-record
//! auto-transcribe and a `TranscribeFile`) can never lose each other's
//! updates. Reads are served by the same task via oneshot replies, so
//! the in-memory view never diverges from disk.

use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};
use zwhisper_ipc::{HistorySession, RpcError};

use super::{
    HistoryEntry, HistoryFile, HistoryStatus, history_file_path, load_from, now_unix_ms, reap,
    write_atomic_to,
};

/// Bounded channel depth — generous; updates are tiny and the writer
/// drains them fast. Bounded (not unbounded) so a runaway producer
/// applies backpressure instead of growing memory without limit.
const CHANNEL_DEPTH: usize = 256;

/// A request to the writer task.
#[derive(Debug)]
pub(crate) enum HistoryRequest {
    /// Insert a new entry or replace an existing one with the same id.
    Upsert(Box<HistoryEntry>),
    /// Update only the status (and optional error) of an entry.
    SetStatus {
        session_id: String,
        status: HistoryStatus,
        last_error: Option<String>,
    },
    /// Record transcript paths + backend and mark `Done`.
    SetTranscript {
        session_id: String,
        transcript_paths: Vec<String>,
        backend: String,
    },
    /// Record (or clear) the running `whisper-cli` pid for recovery.
    ///
    /// Reserved wiring: the recovery + reap path (F2.3) consumes this,
    /// but the current backends do not surface the subprocess pid (it is
    /// buried inside `cmd.output().await`). `kill_on_drop` already tears
    /// the child down on a graceful abort, so the pid is only needed for
    /// the SIGKILL/OOM-orphan case — populated once a backend exposes
    /// its pid. The request + handle exist now so that wiring is a
    /// one-line producer change later.
    #[allow(dead_code)]
    SetWhisperPid {
        session_id: String,
        pid: Option<u32>,
    },
    /// Drop the entry; with `delete_files`, also remove referenced
    /// audio/transcript files (F2.5).
    Forget {
        session_id: String,
        delete_files: bool,
        ack: oneshot::Sender<Result<(), RpcError>>,
    },
    /// Recent entries, most-recent-first, sliced by offset/limit.
    List {
        limit: u32,
        offset: u32,
        reply: oneshot::Sender<Vec<HistorySession>>,
    },
    /// One entry by id.
    Get {
        session_id: String,
        reply: oneshot::Sender<Option<HistorySession>>,
    },
}

/// Cloneable handle the rest of the daemon uses to talk to the writer.
#[derive(Debug, Clone)]
pub(crate) struct HistoryHandle {
    tx: mpsc::Sender<HistoryRequest>,
}

impl HistoryHandle {
    async fn send(&self, req: HistoryRequest) {
        if let Err(e) = self.tx.send(req).await {
            warn!(error = %e, "history writer channel closed; update dropped");
        }
    }

    pub(crate) async fn upsert(&self, entry: HistoryEntry) {
        self.send(HistoryRequest::Upsert(Box::new(entry))).await;
    }

    pub(crate) async fn set_status(
        &self,
        session_id: &str,
        status: HistoryStatus,
        last_error: Option<String>,
    ) {
        self.send(HistoryRequest::SetStatus {
            session_id: session_id.to_owned(),
            status,
            last_error,
        })
        .await;
    }

    pub(crate) async fn set_transcript(
        &self,
        session_id: &str,
        transcript_paths: Vec<String>,
        backend: &str,
    ) {
        self.send(HistoryRequest::SetTranscript {
            session_id: session_id.to_owned(),
            transcript_paths,
            backend: backend.to_owned(),
        })
        .await;
    }

    #[allow(dead_code)] // Reserved; see HistoryRequest::SetWhisperPid.
    pub(crate) async fn set_whisper_pid(&self, session_id: &str, pid: Option<u32>) {
        self.send(HistoryRequest::SetWhisperPid {
            session_id: session_id.to_owned(),
            pid,
        })
        .await;
    }

    pub(crate) async fn forget(
        &self,
        session_id: &str,
        delete_files: bool,
    ) -> Result<(), RpcError> {
        let (ack, rx) = oneshot::channel();
        self.send(HistoryRequest::Forget {
            session_id: session_id.to_owned(),
            delete_files,
            ack,
        })
        .await;
        rx.await.unwrap_or(Err(RpcError::Transient {
            reason: "history writer dropped the forget request".to_owned(),
        }))
    }

    pub(crate) async fn list(&self, limit: u32, offset: u32) -> Vec<HistorySession> {
        let (reply, rx) = oneshot::channel();
        self.send(HistoryRequest::List {
            limit,
            offset,
            reply,
        })
        .await;
        rx.await.unwrap_or_default()
    }

    pub(crate) async fn get(&self, session_id: &str) -> Option<HistorySession> {
        let (reply, rx) = oneshot::channel();
        self.send(HistoryRequest::Get {
            session_id: session_id.to_owned(),
            reply,
        })
        .await;
        rx.await.ok().flatten()
    }
}

/// Spawn the writer task. Loads `history.json` (or starts empty), runs
/// startup recovery (F2.3), then serves requests until the channel
/// closes. Returns the handle and the task's `JoinHandle` so shutdown
/// can await the final in-flight write.
pub(crate) fn spawn_writer() -> (HistoryHandle, tokio::task::JoinHandle<()>) {
    let path = history_file_path();
    spawn_writer_at(path)
}

/// Test-friendly variant targeting an explicit path.
pub(crate) fn spawn_writer_at(path: PathBuf) -> (HistoryHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    let handle = tokio::spawn(writer_loop(path, rx));
    (HistoryHandle { tx }, handle)
}

async fn writer_loop(path: PathBuf, mut rx: mpsc::Receiver<HistoryRequest>) {
    // Load + recover on the blocking pool — file I/O at startup.
    let mut file = match load_from(&path) {
        Ok(f) => f,
        Err(e) => {
            // A corrupt/newer index must not crash the daemon: log and
            // start from empty. The FLAC files remain the source of
            // truth and the index can be rebuilt. (We do NOT overwrite
            // the existing file until the first mutation, so a manual
            // recovery is still possible.)
            warn!(error = %e, path = %path.display(), "could not load history.json; starting empty");
            HistoryFile::default()
        }
    };

    let recovered = recover(&mut file.sessions);
    if recovered > 0 {
        info!(
            count = recovered,
            "startup recovery marked interrupted sessions"
        );
        persist(&file, &path);
    }

    while let Some(req) = rx.recv().await {
        let mut dirty = true;
        match req {
            HistoryRequest::Upsert(entry) => upsert(&mut file.sessions, *entry),
            HistoryRequest::SetStatus {
                session_id,
                status,
                last_error,
            } => set_status(&mut file.sessions, &session_id, status, last_error),
            HistoryRequest::SetTranscript {
                session_id,
                transcript_paths,
                backend,
            } => set_transcript(&mut file.sessions, &session_id, transcript_paths, &backend),
            HistoryRequest::SetWhisperPid { session_id, pid } => {
                set_whisper_pid(&mut file.sessions, &session_id, pid);
            }
            HistoryRequest::Forget {
                session_id,
                delete_files,
                ack,
            } => {
                dirty = forget(&mut file.sessions, &session_id, delete_files);
                let _ = ack.send(Ok(()));
            }
            HistoryRequest::List {
                limit,
                offset,
                reply,
            } => {
                dirty = false;
                let _ = reply.send(list(&file.sessions, limit, offset));
            }
            HistoryRequest::Get { session_id, reply } => {
                dirty = false;
                let _ = reply.send(
                    file.sessions
                        .iter()
                        .find(|e| e.session_id == session_id)
                        .map(HistoryEntry::to_wire),
                );
            }
        }
        if dirty {
            persist(&file, &path);
        }
    }
    info!("history writer task exiting (channel closed)");
}

fn persist(file: &HistoryFile, path: &std::path::Path) {
    if let Err(e) = write_atomic_to(file, path) {
        warn!(error = %e, path = %path.display(), "failed to persist history.json");
    }
}

/// Startup recovery (F2.3): every `Transcribing` entry becomes
/// `Interrupted` (NOT auto-retried, NOT silently `failed`). When a
/// `whisper_pid` is recorded and still alive, reap its group first so a
/// later retry cannot collide with a surviving writer. Returns the count
/// of entries changed.
fn recover(sessions: &mut [HistoryEntry]) -> usize {
    let mut n = 0;
    for e in sessions.iter_mut() {
        if e.status == HistoryStatus::Transcribing {
            if let Some(pid) = e.whisper_pid {
                reap::reap_group(pid);
            }
            e.whisper_pid = None;
            e.status = HistoryStatus::Interrupted;
            if e.last_error.is_none() {
                e.last_error =
                    Some("daemon stopped while transcribing; not auto-retried".to_owned());
            }
            n += 1;
        }
    }
    n
}

fn upsert(sessions: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    if let Some(slot) = sessions
        .iter_mut()
        .find(|e| e.session_id == entry.session_id)
    {
        *slot = entry;
    } else {
        sessions.push(entry);
    }
}

fn set_status(
    sessions: &mut [HistoryEntry],
    session_id: &str,
    status: HistoryStatus,
    last_error: Option<String>,
) {
    if let Some(e) = sessions.iter_mut().find(|e| e.session_id == session_id) {
        e.status = status;
        if last_error.is_some() {
            e.last_error = last_error;
        }
        if matches!(status, HistoryStatus::Done | HistoryStatus::Failed) {
            e.whisper_pid = None;
        }
    }
}

fn set_transcript(
    sessions: &mut [HistoryEntry],
    session_id: &str,
    transcript_paths: Vec<String>,
    backend: &str,
) {
    if let Some(e) = sessions.iter_mut().find(|e| e.session_id == session_id) {
        e.transcript_paths = transcript_paths;
        backend.clone_into(&mut e.backend);
        e.status = HistoryStatus::Done;
        e.last_error = None;
        e.whisper_pid = None;
    }
}

fn set_whisper_pid(sessions: &mut [HistoryEntry], session_id: &str, pid: Option<u32>) {
    if let Some(e) = sessions.iter_mut().find(|e| e.session_id == session_id) {
        e.whisper_pid = pid;
    }
}

/// Remove the entry; with `delete_files`, also unlink referenced files.
/// Returns `Ok(true)` when an entry was removed, `Ok(false)` when no
/// such entry exists (idempotent). File-deletion failures are logged,
/// never propagated — a failed unlink must not block dropping the index
/// entry (the FLAC is the source of truth, F2.1).
fn forget(sessions: &mut Vec<HistoryEntry>, session_id: &str, delete_files: bool) -> bool {
    let Some(pos) = sessions.iter().position(|e| e.session_id == session_id) else {
        return false;
    };
    let entry = sessions.remove(pos);
    if delete_files {
        let mut paths: Vec<&str> = vec![entry.audio_path.as_str()];
        paths.extend(entry.transcript_paths.iter().map(String::as_str));
        for p in paths {
            if p.is_empty() {
                continue;
            }
            match std::fs::remove_file(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = p, error = %e, "forget: could not delete file");
                }
            }
        }
    }
    true
}

/// Most-recent-first slice. `created_at_ms` descending; `limit == 0`
/// means "all from offset".
fn list(sessions: &[HistoryEntry], limit: u32, offset: u32) -> Vec<HistorySession> {
    let mut ordered: Vec<&HistoryEntry> = sessions.iter().collect();
    ordered.sort_by_key(|e| std::cmp::Reverse(e.created_at_ms));
    ordered
        .into_iter()
        .skip(offset as usize)
        .take(if limit == 0 {
            usize::MAX
        } else {
            limit as usize
        })
        .map(HistoryEntry::to_wire)
        .collect()
}

/// Build a fresh `Recorded` entry (used by the lifecycle + Jobs1 paths).
#[allow(clippy::too_many_arguments)]
pub(crate) fn new_entry(
    session_id: &str,
    profile: &str,
    audio_path: &str,
    codec: &str,
    native_rate: u32,
    channels: u16,
    backend: &str,
    model: &str,
    lang: &str,
) -> HistoryEntry {
    HistoryEntry {
        session_id: session_id.to_owned(),
        created_at_ms: now_unix_ms(),
        profile: profile.to_owned(),
        audio_path: audio_path.to_owned(),
        codec: codec.to_owned(),
        native_rate,
        channels,
        transcript_paths: Vec::new(),
        backend: backend.to_owned(),
        model: model.to_owned(),
        lang: lang.to_owned(),
        status: HistoryStatus::Recorded,
        last_error: None,
        whisper_pid: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mk(id: &str, status: HistoryStatus, created: u64) -> HistoryEntry {
        let mut e = new_entry(
            id,
            "default",
            "/tmp/a.flac",
            "flac",
            48_000,
            1,
            "whisper-cpp",
            "small",
            "auto",
        );
        e.status = status;
        e.created_at_ms = created;
        e
    }

    #[test]
    fn recover_marks_transcribing_interrupted_no_retry() {
        let mut s = vec![
            mk("a", HistoryStatus::Transcribing, 1),
            mk("b", HistoryStatus::Done, 2),
        ];
        let n = recover(&mut s);
        assert_eq!(n, 1);
        assert_eq!(s[0].status, HistoryStatus::Interrupted);
        assert!(s[0].last_error.is_some());
        assert_eq!(s[1].status, HistoryStatus::Done);
    }

    #[test]
    fn upsert_replaces_same_id() {
        let mut s = vec![mk("a", HistoryStatus::Recorded, 1)];
        let mut updated = mk("a", HistoryStatus::Done, 5);
        updated.model = "large".to_owned();
        upsert(&mut s, updated);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].status, HistoryStatus::Done);
        assert_eq!(s[0].model, "large");
    }

    #[test]
    fn set_transcript_marks_done_and_clears_error() {
        let mut s = vec![{
            let mut e = mk("a", HistoryStatus::Failed, 1);
            e.last_error = Some("old".to_owned());
            e
        }];
        set_transcript(&mut s, "a", vec!["/t.txt".to_owned()], "deepgram");
        assert_eq!(s[0].status, HistoryStatus::Done);
        assert_eq!(s[0].backend, "deepgram");
        assert_eq!(s[0].transcript_paths, vec!["/t.txt".to_owned()]);
        assert!(s[0].last_error.is_none());
    }

    #[test]
    fn list_is_most_recent_first_with_offset_limit() {
        let s = vec![
            mk("old", HistoryStatus::Done, 10),
            mk("new", HistoryStatus::Done, 30),
            mk("mid", HistoryStatus::Done, 20),
        ];
        let all = list(&s, 0, 0);
        assert_eq!(all[0].session_id, "new");
        assert_eq!(all[1].session_id, "mid");
        assert_eq!(all[2].session_id, "old");
        let one = list(&s, 1, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].session_id, "mid");
    }

    #[test]
    fn forget_removes_entry_idempotent() {
        let mut s = vec![mk("a", HistoryStatus::Done, 1)];
        assert!(forget(&mut s, "a", false));
        assert!(s.is_empty());
        // Idempotent: forgetting again returns false, not an error.
        assert!(!forget(&mut s, "a", false));
    }

    #[test]
    fn forget_delete_files_unlinks() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("a.flac");
        std::fs::write(&audio, b"x").unwrap();
        let mut e = mk("a", HistoryStatus::Done, 1);
        e.audio_path = audio.display().to_string();
        let mut s = vec![e];
        assert!(forget(&mut s, "a", true));
        assert!(!audio.exists());
    }

    #[tokio::test]
    async fn concurrent_updates_do_not_lose_entries() {
        // Two producers upserting distinct ids must both land — the
        // single-writer task serializes them (F2.2).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history.json");
        let (h, join) = spawn_writer_at(path.clone());
        let h2 = h.clone();
        let t1 = tokio::spawn(async move {
            for i in 0..50 {
                h.upsert(mk(&format!("a{i}"), HistoryStatus::Done, i)).await;
            }
        });
        let t2 = tokio::spawn(async move {
            for i in 0..50 {
                h2.upsert(mk(&format!("b{i}"), HistoryStatus::Done, 100 + i))
                    .await;
            }
        });
        t1.await.unwrap();
        t2.await.unwrap();
        // The writer persists after every mutation, so once both
        // producers have finished sending and the writer has drained
        // the channel, all 100 entries are on disk. Give the writer a
        // moment to drain its mpsc backlog, then read the file back.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(join); // detach the task; the file is already persisted
        let f = load_from(&path).unwrap();
        assert_eq!(f.sessions.len(), 100, "all 100 upserts must persist");
    }
}
