//! Server-side `cz.zajca.Zwhisper1.Recorder1` interface.
//!
//! Mirrors the proxy trait declared in `zwhisper-ipc::recorder` —
//! same method set, same signal set, same wire signatures (the
//! M3-plan locks them in). The mirror is mandatory because zbus's
//! `#[interface]` macro can only decorate an `impl` on a server-
//! owned struct that holds state, while `#[proxy]` can only
//! decorate a free `trait`. The two attributes do not share a
//! definition source — see `zwhisper-ipc/src/recorder.rs` for the
//! architectural rationale.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, error, info, warn};
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zwhisper_core::audio::recorder::{RecordOptions, Recorder};
use zwhisper_core::audio::state::{SessionId, StopReason};
use zwhisper_core::profile;
use zwhisper_ipc::{OBJECT_PATH, RpcError, Status};

use crate::lifecycle::{LifecycleHooks, spawn_lifecycle};
use crate::session::SessionManager;

/// Result of [`RecorderInterface::output_path_for_session`] — pinned
/// to a single helper so the path scheme is in one place.
fn default_output_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zwhisper")
        .join("recordings")
}

/// Lazily initialise `GStreamer`. Runs at most once per process; the
/// daemon does **not** call this at startup so a missing
/// `libgstreamer-1.0` does not prevent the bus name from being
/// claimed (M3-plan correction C7). The first successful
/// `StartRecording` call takes the cost.
fn ensure_gstreamer_init() -> Result<(), RpcError> {
    static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();
    match GST_INIT.get_or_init(|| gstreamer::init().map_err(|e| e.to_string())) {
        Ok(()) => Ok(()),
        Err(e) => Err(RpcError::RecordingFailed {
            reason: format!("gstreamer init: {e}"),
        }),
    }
}

/// State held by the `Recorder1` interface impl. Cloned cheaply via
/// the inner `Arc`s — zbus drops one copy per dispatched method
/// when the future resolves, so the interface state must outlive
/// any single call.
#[derive(Debug)]
pub(crate) struct RecorderInterface {
    sessions: Arc<SessionManager>,
    /// In-memory "last used" profile hint, shared with the
    /// `Profiles1` interface. Wrapped in a `tokio::sync::Mutex`
    /// because both interfaces hold `&mut self`-bearing methods on
    /// it that may straddle await points.
    active_profile: Arc<AsyncMutex<String>>,
    /// Serialises the body of `StartRecording` so two concurrent
    /// callers cannot both pass the `try_reserve` race-window when
    /// the first is still mid-spawn. C5 (release-before-transcribe)
    /// already covers the post-recording side; this lock covers the
    /// startup side.
    start_lock: Arc<AsyncMutex<()>>,
}

impl RecorderInterface {
    pub(crate) fn new(
        sessions: Arc<SessionManager>,
        active_profile: Arc<AsyncMutex<String>>,
    ) -> Self {
        Self {
            sessions,
            active_profile,
            start_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Resolve the on-disk audio path for a given session id.
    /// Recordings land under `$XDG_DATA_HOME/zwhisper/recordings/`
    /// with the session UUID as the file stem, mirroring the M0
    /// CLI's default layout.
    fn output_path_for_session(session_id: SessionId) -> PathBuf {
        default_output_dir().join(format!("{session_id}.flac"))
    }
}

#[zbus::interface(name = "cz.zajca.Zwhisper1.Recorder1")]
impl RecorderInterface {
    /// Start a new recording. See `zwhisper_ipc::recorder` for the
    /// canonical signature.
    async fn start_recording(
        &self,
        profile_name: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        // SAFETY-AGAINST-DEADLOCK: this method must take `&self`,
        // not `&mut self`. zbus's dispatcher takes a read-lock for
        // `&self` methods and a write-lock for `&mut self`. Inside
        // the body we look up our own interface via
        // `conn.object_server().interface(...).await`, which itself
        // takes a read-lock — under a held write-lock that
        // deadlocks. Phase 5 of the M3 milestone surfaced this hang
        // via `start_recording_emits_state_changed_starting`. None
        // of the state mutated below requires `&mut self`: every
        // touched field is `Arc<...>` already.
        // Defensive lock: a flood of concurrent StartRecording calls
        // could otherwise interleave their try_reserve / GStreamer
        // init / spawn_blocking phases and leave the SessionManager
        // in a confusing state. The outer lock is held only for the
        // synchronous prelude — released before we return so signal
        // emission and lifecycle spawn run unblocked.
        let _start_guard = self.start_lock.lock().await;

        info!(profile = %profile_name, "Recorder1.StartRecording");

        // Profile resolution happens on every call (M3 lock-in § 8) —
        // no caching. Per C11, an empty `profile_name` is a normalised
        // ProfileNotFound { name: "(empty)" }.
        if profile_name.is_empty() {
            return Err(RpcError::ProfileNotFound {
                name: "(empty)".into(),
            }
            .into());
        }
        let profile = profile::load(profile_name).map_err(|e| {
            map_profile_error(profile_name, e)
        })?;
        if let Err(e) = profile.validate() {
            return Err(RpcError::ProfileLoadFailed {
                name: profile_name.to_owned(),
                reason: e.to_string(),
            }
            .into());
        }

        let session_id = SessionId::new();
        self.sessions
            .try_reserve(session_id, &profile.name)
            .map_err(zbus::fdo::Error::from)?;

        // Update active-profile hint while we hold the slot; this
        // gives `GetActive` a sticky value across daemon lifetime.
        *self.active_profile.lock().await = profile.name.clone();

        // Lazy GStreamer init now that we are about to need it.
        if let Err(e) = ensure_gstreamer_init() {
            self.sessions.release();
            return Err(e.into());
        }

        // Ensure output directory exists before starting the
        // recorder; the pipeline itself uses `create_new` and would
        // surface a confusing ENOENT if the parent dir is missing.
        let output = Self::output_path_for_session(session_id);
        if let Some(parent) = output.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                self.sessions.release();
                return Err(RpcError::RecordingFailed {
                    reason: format!("failed to create recordings dir {}: {e}", parent.display()),
                }
                .into());
            }
        }

        // C9 lifecycle ordering: emit StateChanged "starting" before
        // the recorder construction so the client sees the daemon
        // accepted the request even if pipeline build fails. This
        // signal pairs with the StateChanged "failed" the lifecycle
        // emits on Recorder::start error.
        let session_id_str = session_id.to_string();
        if let Err(e) =
            Self::state_changed(&emitter, "starting", &session_id_str).await
        {
            warn!(error = %e, "failed to emit StateChanged starting");
        }

        let opts = RecordOptions {
            mic: profile.sources.mic.clone(),
            monitor: profile.sources.system_output.clone(),
            output: output.clone(),
            install_ctrl_c: false, // Daemon owns SIGINT/SIGTERM (C2).
        };

        let recorder = match Recorder::start(opts) {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Recorder::start failed");
                self.sessions.release();
                if let Err(em) =
                    Self::state_changed(&emitter, "failed", &session_id_str).await
                {
                    warn!(error = %em, "failed to emit StateChanged failed");
                }
                return Err(RpcError::RecordingFailed {
                    reason: e.to_string(),
                }
                .into());
            }
        };

        // Emit StateChanged "recording" after the pipeline is up.
        if let Err(e) =
            Self::state_changed(&emitter, "recording", &session_id_str).await
        {
            warn!(error = %e, "failed to emit StateChanged recording");
        }

        // Build the lifecycle handle from the live connection's
        // object server so the spawned task can emit signals
        // independently of any incoming method call.
        let iface_ref: InterfaceRef<RecorderInterface> =
            conn.object_server().interface(OBJECT_PATH).await?;

        let hooks = LifecycleHooks {
            iface_ref,
            sessions: Arc::clone(&self.sessions),
            session_id,
            audio_path: output,
            transcribe_auto: profile.transcription.auto,
            transcribe_backend: profile.transcription.backend.as_str().to_owned(),
            transcribe_model: profile.transcription.model.clone(),
            transcribe_language: profile.transcription.language.clone(),
        };

        spawn_lifecycle(recorder, hooks);

        debug!(session_id = %session_id_str, "lifecycle task spawned");
        Ok(session_id_str)
    }

    /// Stop the recording with the given `session_id`. Returns the
    /// same id on success.
    ///
    /// Validation and stop-hook firing happen under one mutex via
    /// [`SessionManager::try_stop`] — the previous two-call pattern
    /// (`matches` then `request_stop_active`) ignored the second
    /// call's return value, so a request that landed in the brief
    /// window between `try_reserve` and `install_stop_hook` would
    /// reply `Ok` without actually stopping anything.
    #[allow(clippy::unused_async)] // zbus #[interface] requires `async fn`.
    async fn stop_recording(&self, session_id: &str) -> zbus::fdo::Result<String> {
        info!(%session_id, "Recorder1.StopRecording");
        let Some(parsed) = session_id
            .parse::<uuid::Uuid>()
            .ok()
            .map(SessionId::from_uuid)
        else {
            return Err(RpcError::SessionUnknown {
                id: session_id.to_owned(),
            }
            .into());
        };

        // We do not own the `Recorder` here — the lifecycle task
        // does. The lifecycle task subscribes to the same
        // `tokio::sync::watch` channel we wrote into via
        // `request_stop`, so writing the `StopReason` wakes the
        // bus-thread loop and triggers EOS finalisation.
        match self.sessions.try_stop(&parsed, StopReason::UserRequested) {
            crate::session::StopAttempt::Stopped => Ok(session_id.to_owned()),
            crate::session::StopAttempt::Unknown => Err(RpcError::SessionUnknown {
                id: session_id.to_owned(),
            }
            .into()),
            crate::session::StopAttempt::NotReady => Err(RpcError::Transient {
                reason: format!(
                    "session {session_id} is starting up; stop hook not yet installed — retry"
                ),
            }
            .into()),
        }
    }

    /// Snapshot of the daemon's current state.
    async fn get_status(&self) -> zbus::fdo::Result<Status> {
        let snap = self.sessions.snapshot();
        let active_profile = self.active_profile.lock().await.clone();
        let (state, duration_ms) = match snap {
            None => ("idle".to_owned(), 0u64),
            Some(s) => {
                let elapsed = s.started_at.elapsed();
                let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                ("recording".to_owned(), ms)
            }
        };
        Ok(Status {
            state,
            active_profile,
            duration_ms,
        })
    }

    #[zbus(signal)]
    async fn state_changed(
        emitter: &SignalEmitter<'_>,
        new_state: &str,
        session_id: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn recording_complete(
        emitter: &SignalEmitter<'_>,
        session_id: &str,
        audio_path: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn transcript_complete(
        emitter: &SignalEmitter<'_>,
        session_id: &str,
        transcript_path: &str,
        bytes: u64,
        backend: &str,
    ) -> zbus::Result<()>;
}

/// Map `ProfileError` to the typed `RpcError` flavour the CLI knows
/// how to translate into an exit code.
fn map_profile_error(name: &str, err: profile::ProfileError) -> zbus::fdo::Error {
    match err {
        profile::ProfileError::NotFound { .. } => RpcError::ProfileNotFound {
            name: name.to_owned(),
        }
        .into(),
        profile::ProfileError::InvalidName { name: n } => RpcError::ProfileNotFound { name: n }.into(),
        other => RpcError::ProfileLoadFailed {
            name: name.to_owned(),
            reason: other.to_string(),
        }
        .into(),
    }
}

