//! `zwhisper transcribe <file>` — local by default, daemon-routed on
//! request (RFC-daemon-role F1.1/F1.2).
//!
//! ## Routing
//!
//! - **default (no `--queue`/`--detach`)**: runs LOCALLY in this process
//!   via `zwhisper-core::transcribe`, with zero daemon dependency. This
//!   preserves the headless/ssh/cron guarantee (IDEA §5) — the single
//!   most dangerous assumption to break, so it stays the default.
//! - **`--detach`**: enqueue a daemon job, print the `job_id`, return.
//! - **`--queue`**: enqueue a daemon job and *wait* for it (bounded by
//!   [`JOB_WAIT_TIMEOUT`]), so it lands in `zwhisper history` and is
//!   retryable. The wait is signal-driven (subscribe to
//!   `Jobs1.JobCompleted`/`JobFailed` BEFORE submitting, mirroring
//!   `record.rs`) and filtered by `job_id`.
//!
//! Only daemon-routed jobs enter history (Feature 2). The local path is
//! unchanged and scriptable.

use color_eyre::eyre::eyre;
use futures_util::StreamExt;
use tracing::{info, warn};
use zwhisper_core::profile;
use zwhisper_core::profile::schema::{Backend, DeepgramSettings};
use zwhisper_core::transcribe::{self, BackendSettings, TranscribeOpts};
use zwhisper_ipc::Jobs1Proxy;

use crate::cli::TranscribeArgs;

use super::{
    DAEMON_DOWN_HINT, DAEMON_TOO_OLD_HINT, EXIT_IPC_FAILURE, EXIT_JOB_TIMEOUT, EXIT_OK,
    EXIT_PROTOCOL_ERROR, EXIT_RECORDING_FAILED, JOB_WAIT_TIMEOUT, build_runtime, classify_error,
    is_daemon_down, is_daemon_too_old,
};

pub(crate) fn run(args: &TranscribeArgs) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    if args.detach || args.queue {
        // Daemon-routed paths return an exit code; translate to process
        // exit so scripts see the F1.2 contract.
        let code = rt.block_on(run_daemon(args));
        if code == EXIT_OK {
            Ok(())
        } else {
            std::process::exit(code);
        }
    } else {
        rt.block_on(run_local(args))
    }
}

/// Resolve `(backend_id, model, language)` for the request, applying the
/// same precedence the local path uses (profile's backend-specific model
/// wins over the generic one).
fn resolve_request(args: &TranscribeArgs) -> color_eyre::Result<(String, String, String)> {
    if let Some(name) = &args.profile {
        let profile = profile::load(name).map_err(|e| eyre!("{e}"))?;
        let model = match (
            profile.transcription.backend,
            &profile.transcription.deepgram,
        ) {
            (Backend::Deepgram, Some(dg)) => dg.model.clone(),
            _ => profile.transcription.model.clone(),
        };
        Ok((
            profile.transcription.backend.as_str().to_owned(),
            model,
            profile.transcription.language.clone(),
        ))
    } else {
        Ok((
            args.backend.clone(),
            args.model.clone(),
            args.language.clone(),
        ))
    }
}

// ---------------------------------------------------------------------
// Local path (default) — unchanged contract.
// ---------------------------------------------------------------------

async fn run_local(args: &TranscribeArgs) -> color_eyre::Result<()> {
    let opts = if let Some(name) = &args.profile {
        let profile = profile::load(name).map_err(|e| eyre!("{e}"))?;
        let model = match (
            profile.transcription.backend,
            &profile.transcription.deepgram,
        ) {
            (Backend::Deepgram, Some(dg)) => dg.model.clone(),
            _ => profile.transcription.model.clone(),
        };
        TranscribeOpts {
            backend: profile.transcription.backend,
            model,
            language: profile.transcription.language.clone(),
            settings: BackendSettings {
                whisper_cpp: profile.transcription.whisper_cpp.clone(),
                deepgram: profile.transcription.deepgram.clone(),
            },
        }
    } else {
        let backend = if args.backend.is_empty() {
            Backend::WhisperCpp
        } else {
            Backend::from_id(&args.backend).ok_or_else(|| {
                eyre!(
                    "unknown backend `{}` (supported: whisper-cpp, deepgram, parakeet)",
                    args.backend
                )
            })?
        };
        let settings = match backend {
            Backend::Deepgram => BackendSettings {
                deepgram: Some(DeepgramSettings::default()),
                ..Default::default()
            },
            _ => BackendSettings::default(),
        };
        TranscribeOpts {
            backend,
            model: args.model.clone(),
            language: args.language.clone(),
            settings,
        }
    };

    info!(
        input = %args.input.display(),
        backend = %opts.backend.as_str(),
        model = %opts.model,
        language = %opts.language,
        "transcribe requested (local)",
    );

    let art = transcribe::transcribe_file(&args.input, &opts)
        .await
        .map_err(|err| eyre!("{err}"))?;

    info!(
        txt = %art.txt_path.display(),
        json = %art.json_path.display(),
        audio_duration_ms = u64::try_from(art.audio_duration.as_millis()).unwrap_or(u64::MAX),
        transcribe_duration_ms = u64::try_from(art.duration.as_millis()).unwrap_or(u64::MAX),
        language = %art.language,
        model = %art.model,
        "transcribe complete (local)",
    );
    println!("transcript: {}", art.txt_path.display());

    Ok(())
}

// ---------------------------------------------------------------------
// Daemon-routed paths (--detach / --queue).
// ---------------------------------------------------------------------

#[allow(clippy::print_stderr)]
async fn run_daemon(args: &TranscribeArgs) -> i32 {
    let (backend, model, lang) = match resolve_request(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_PROTOCOL_ERROR;
        }
    };
    let path = args.input.display().to_string();

    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{DAEMON_DOWN_HINT}");
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_PROTOCOL_ERROR;
        }
    };
    let proxy = match Jobs1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => return map_daemon_err("build Jobs1 proxy", &err),
    };

    let submit_mode = if args.queue { "foreground" } else { "detached" };

    if args.detach {
        match proxy
            .transcribe_file(&path, &backend, &model, &lang, submit_mode)
            .await
        {
            Ok(job_id) => {
                println!("job: {job_id}");
                println!("(detached — check `zwhisper jobs` / `zwhisper history`)");
                EXIT_OK
            }
            Err(err) => map_daemon_err("Jobs1.TranscribeFile", &err),
        }
    } else {
        run_queue_wait(&proxy, &path, &backend, &model, &lang).await
    }
}

/// `--queue`: subscribe first, submit, then wait (bounded) for the
/// matching `JobCompleted`/`JobFailed`.
#[allow(clippy::print_stderr)]
async fn run_queue_wait(
    proxy: &Jobs1Proxy<'_>,
    path: &str,
    backend: &str,
    model: &str,
    lang: &str,
) -> i32 {
    // SUBSCRIBE FIRST (missed-signal race, as in record.rs): the daemon
    // can finish a tiny job before we would otherwise subscribe.
    let mut completed = match proxy.receive_job_completed().await {
        Ok(s) => s,
        Err(err) => return map_daemon_err("subscribe JobCompleted", &err),
    };
    let mut failed = match proxy.receive_job_failed().await {
        Ok(s) => s,
        Err(err) => return map_daemon_err("subscribe JobFailed", &err),
    };

    let job_id = match proxy
        .transcribe_file(path, backend, model, lang, "foreground")
        .await
    {
        Ok(id) => id,
        Err(err) => return map_daemon_err("Jobs1.TranscribeFile", &err),
    };
    info!(%job_id, "queued transcription job; waiting");

    let wait = async {
        let mut comp_done = false;
        let mut fail_done = false;
        loop {
            if comp_done && fail_done {
                return EXIT_IPC_FAILURE; // both streams closed
            }
            tokio::select! {
                maybe = completed.next(), if !comp_done => {
                    let Some(sig) = maybe else { comp_done = true; continue; };
                    let Ok(a) = sig.args() else { continue; };
                    if a.job_id != job_id { continue; }
                    println!("transcript: {}", a.transcript_path);
                    return EXIT_OK;
                }
                maybe = failed.next(), if !fail_done => {
                    let Some(sig) = maybe else { fail_done = true; continue; };
                    let Ok(a) = sig.args() else { continue; };
                    if a.job_id != job_id { continue; }
                    eprintln!("transcription failed: {}", a.error);
                    return EXIT_RECORDING_FAILED;
                }
            }
        }
    };

    match tokio::time::timeout(JOB_WAIT_TIMEOUT, wait).await {
        Ok(code) if code == EXIT_IPC_FAILURE => {
            eprintln!(
                "daemon disconnected before job {job_id} finished; \
                 it may be recoverable via `zwhisper history` / `zwhisper retry`"
            );
            EXIT_IPC_FAILURE
        }
        Ok(code) => code,
        Err(_) => {
            println!("job: {job_id}");
            eprintln!(
                "still running after {}s; check `zwhisper jobs` / `zwhisper history`",
                JOB_WAIT_TIMEOUT.as_secs()
            );
            EXIT_JOB_TIMEOUT
        }
    }
}

/// Map a daemon-side zbus error to an exit code + user-facing message,
/// distinguishing daemon-down / too-old / typed-error cases.
#[allow(clippy::print_stderr)]
fn map_daemon_err(ctx: &str, err: &zbus::Error) -> i32 {
    if is_daemon_down(err) {
        eprintln!("{DAEMON_DOWN_HINT}");
        return EXIT_PROTOCOL_ERROR;
    }
    if is_daemon_too_old(err) {
        eprintln!("{DAEMON_TOO_OLD_HINT}");
        return EXIT_PROTOCOL_ERROR;
    }
    warn!(context = ctx, error = %err, "daemon call failed");
    eprintln!("{ctx} failed: {err}");
    classify_error(err)
}
