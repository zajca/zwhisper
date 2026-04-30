// `expect` is used here only when re-locking the recorder's own
// mutexes; a poisoned mutex means an earlier panic happened in our
// own code and the recorder is unrecoverable, so propagating with
// expect rather than building a custom `PoisonedRecorder` error
// keeps the error type set small.
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use super::devices::{WpctlCommandRunner, resolve};
use super::error::RecordingError;
use super::pipeline;
use super::state::{RecorderState, SessionId, StopReason};
use super::watchdog::{self, Classification};

const EOS_TIMEOUT_SECS: u64 = 5;
const BUS_POLL_TIMEOUT_MS: u64 = 100;
/// Hard cap on the number of bus warnings the recorder retains. Past
/// this, additional warnings are still logged through `tracing` but
/// not stored in `RecordingReport.warnings`. Without the cap a
/// pathological `pipewire` backend could grow this vec unbounded over
/// a 60-min soak and trip `DoD` #2.
const MAX_WARNINGS: usize = 100;

/// Plain-Rust input to the audio façade. No `gst::*` types here so
/// callers in M3 can reuse this struct as the D-Bus input shape.
#[derive(Debug, Clone)]
pub(crate) struct RecordOptions {
    pub mic: String,
    pub monitor: String,
    pub output: PathBuf,
}

/// Result of a successful recording. Carries the output path because
/// IDEA.md § 2 `RecordingComplete(s session_id, s audio_path)` needs
/// it; threading the path through the caller after the fact is
/// painful.
#[derive(Debug, Clone)]
pub(crate) struct RecordingReport {
    pub session_id: SessionId,
    pub duration: Duration,
    /// Number of audio samples encoded into the FLAC. Computed from
    /// the pipeline's running time at EOS (rate × time / 1 s). Locked
    /// in by `docs/M0-plan.md` as the natural backing field for `DoD`
    /// #3 ("declared length matches wall-clock duration ± one buffer").
    pub samples_written: u64,
    pub underruns: u32,
    pub warnings: Vec<String>,
    pub audio_path: PathBuf,
}

/// Reasons the caller wants the recorder to stop. Translated to a
/// `StopReason` inside `Recorder::request_stop` — the recorder owns
/// the EOS finalisation path.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StopRequest {
    UserRequested,
    DurationElapsed,
}

/// Outcome of the `record_blocking` race between Ctrl+C, the duration
/// timer, and the bus watchdog. Distinguishes caller-initiated stops
/// (which must call `request_stop` to write a `StopReason`) from
/// bus-initiated stops (where the bus thread has already written the
/// real reason — `DeviceLost` / `BusError` / `EosObserved` — and
/// `request_stop` would clobber it).
#[derive(Debug, Clone, Copy)]
enum RaceOutcome {
    Caller(StopRequest),
    BusInitiated,
}

#[derive(Debug)]
pub(crate) struct Recorder {
    pipeline: gst::Pipeline,
    #[allow(dead_code)] // kept for forensics/D-Bus probes once watch-channel becomes the canonical signal source.
    bus: gst::Bus,
    bus_thread: Option<JoinHandle<()>>,
    stop_tx: watch::Sender<StopReason>,
    /// Receiver kept alive so subscribers stay valid; the recorder
    /// also reads its current value to assemble the final
    /// `RecordingReport` / `RecordingError`.
    stop_rx_anchor: watch::Receiver<StopReason>,
    /// Flips `true` when `Recorder::stop` (or `Drop`) wants the bus
    /// thread to exit. The bus thread polls this on every iteration
    /// because waiting on watch-channel closure is fragile when the
    /// recorder itself holds a receiver.
    bus_shutdown: Arc<AtomicBool>,
    state: Arc<Mutex<RecorderState>>,
    underruns: Arc<AtomicU32>,
    warnings: Arc<Mutex<Vec<String>>>,
    session_id: SessionId,
    output_path: PathBuf,
    started_at: Instant,
}

impl Recorder {
    /// Build the pipeline and transition it to `Playing`. On success
    /// the bus thread is already running and forwarding messages into
    /// the watch channel.
    ///
    /// The output-file cleanup is gated on a `BuiltOutput` token
    /// returned from `pipeline::build`: only files we created via
    /// `OpenOptions::create_new` are removed on failure. If
    /// `precreate_output` returns `EEXIST` because the user already
    /// has a file there, the function bails out *before* the token
    /// is issued, so the user's file is never touched.
    pub(crate) fn start(opts: RecordOptions) -> Result<Self, RecordingError> {
        let session_id = SessionId::new();
        info!(%session_id, mic = %opts.mic, monitor = %opts.monitor,
              output = %opts.output.display(), "starting recorder");

        let selection = resolve(&WpctlCommandRunner, &opts.mic, &opts.monitor)
            .map_err(RecordingError::DeviceDiscovery)?;
        debug!(%session_id, mic_node = %selection.mic_node,
               monitor_node = %selection.monitor_node, "resolved devices");

        let (pipeline, output_token) = pipeline::build(&selection, &opts.output)?;

        match Self::finish_start(opts, session_id, pipeline) {
            Ok(recorder) => Ok(recorder),
            Err(e) => {
                output_token.cleanup_on_failure();
                Err(e)
            }
        }
    }

    fn finish_start(
        opts: RecordOptions,
        session_id: SessionId,
        pipeline: gst::Pipeline,
    ) -> Result<Self, RecordingError> {
        let bus = pipeline.bus().ok_or(RecordingError::PipelineFailed {
            stage: "pipeline_bus".into(),
            source: "pipeline returned no bus".into(),
        })?;

        // Bring the pipeline up *before* spawning the bus thread.
        // Spawning earlier and unwinding on `set_state(Playing)`
        // failure leaks the bus thread (its JoinHandle is dropped
        // before the recorder owns it, and the shutdown flag stays
        // false). Any pre-roll messages will queue on the bus and
        // the watchdog will see them once it starts iterating.
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| RecordingError::PipelineFailed {
                stage: "set_state_playing".into(),
                source: Box::new(e),
            })?;

        // Channel must start in `Running`; multiple producers (bus
        // thread, duration timer, ctrl_c) write the actual reason.
        let (stop_tx, stop_rx_anchor) = watch::channel(StopReason::Running);
        let bus_shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(RecorderState::Starting));
        let underruns = Arc::new(AtomicU32::new(0));
        let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let bus_thread = spawn_bus_thread(BusThreadCtx {
            bus: bus.clone(),
            stop_tx: stop_tx.clone(),
            shutdown: Arc::clone(&bus_shutdown),
            underruns: Arc::clone(&underruns),
            warnings: Arc::clone(&warnings),
        });

        *state.lock().expect("poisoned recorder state") = RecorderState::Recording;
        info!(%session_id, "recorder transitioned to Recording");

        Ok(Self {
            pipeline,
            bus,
            bus_thread: Some(bus_thread),
            stop_tx,
            stop_rx_anchor,
            bus_shutdown,
            state,
            underruns,
            warnings,
            session_id,
            output_path: opts.output,
            started_at: Instant::now(),
        })
    }

    #[allow(dead_code)] // M3 surfaces this on D-Bus.
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[allow(dead_code)] // M3 surfaces this on D-Bus GetStatus.
    pub(crate) fn state(&self) -> RecorderState {
        *self.state.lock().expect("poisoned recorder state")
    }

    /// Subscribe to stop-reason updates. Used by `record_blocking` to
    /// race the bus watchdog against the duration timer and Ctrl+C.
    pub(crate) fn stop_subscriber(&self) -> watch::Receiver<StopReason> {
        self.stop_rx_anchor.clone()
    }

    /// Convert a caller-side request into a `StopReason` and write it
    /// to the watch channel.
    pub(crate) fn request_stop(&self, request: StopRequest) {
        let reason = match request {
            StopRequest::UserRequested => StopReason::UserRequested,
            StopRequest::DurationElapsed => StopReason::DurationElapsed,
        };
        // send_replace tolerates closed receivers; the only time the
        // channel closes is when Recorder is dropped.
        let _ = self.stop_tx.send_replace(reason);
    }

    /// Run the canonical EOS finalisation sequence: send Eos, wait for
    /// the bus to confirm it (or surface an Error), transition to
    /// Null, join the bus thread, build the report. This is the only
    /// place that calls `set_state(Null)` — keeping the fragile path
    /// singular is the whole point of the handle.
    pub(crate) fn stop(mut self) -> Result<RecordingReport, RecordingError> {
        *self.state.lock().expect("poisoned recorder state") = RecorderState::Stopping;
        info!(session_id = %self.session_id, "draining pipeline (sending EOS)");

        if !self.pipeline.send_event(gst::event::Eos::new()) {
            warn!("pipeline rejected EOS event — falling through to Null transition");
        }

        // Wait until the bus thread classifies the EOS message (or any
        // stop-worthy event) into the watch channel. Polling the
        // channel here instead of `bus.timed_pop_filtered` prevents a
        // race where the bus thread consumes the EOS first and our
        // direct read times out empty-handed.
        let eos_seen = wait_for_stop_signal(&mut self.stop_rx_anchor.clone());

        // After observing (or failing to observe) the stop signal,
        // shut the bus thread down so we own the bus exclusively for
        // the Null transition.
        self.bus_shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.bus_thread.take() {
            if handle.join().is_err() {
                warn!("bus thread panicked during shutdown");
            }
        }

        let null_result = self
            .pipeline
            .set_state(gst::State::Null)
            .map_err(|e| RecordingError::PipelineFailed {
                stage: "set_state_null".into(),
                source: Box::new(e),
            });

        null_result?;

        let final_reason = self.stop_rx_anchor.borrow().clone();
        let duration = self.started_at.elapsed();
        let warnings = self
            .warnings
            .lock()
            .expect("poisoned warnings vec")
            .clone();
        let underruns = self.underruns.load(Ordering::Relaxed);

        match final_reason {
            StopReason::BusError { stage, message } => {
                *self.state.lock().expect("poisoned recorder state") = RecorderState::Failed;
                Err(RecordingError::PipelineFailed {
                    stage: stage.into(),
                    source: message.into(),
                })
            }
            StopReason::DeviceLost { node } => {
                *self.state.lock().expect("poisoned recorder state") = RecorderState::Failed;
                Err(RecordingError::DeviceDisappeared { node })
            }
            _ => {
                if !eos_seen {
                    return Err(RecordingError::EosTimeout {
                        seconds: EOS_TIMEOUT_SECS,
                    });
                }
                *self.state.lock().expect("poisoned recorder state") = RecorderState::Idle;
                // Read the authoritative sample count straight from
                // the closed FLAC. Done here (not before Null) so the
                // encoder has flushed everything to disk.
                let samples_written = read_flac_total_samples(&self.output_path);
                Ok(RecordingReport {
                    session_id: self.session_id,
                    duration,
                    samples_written,
                    underruns,
                    warnings,
                    audio_path: self.output_path.clone(),
                })
            }
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Best effort: if the caller dropped without `stop()`, force
        // pipeline to Null so the bus thread exits and the file is
        // closed. Errors are logged; we cannot return them.
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            error!("pipeline set_state(Null) failed during drop: {e:?}");
        }
        self.bus_shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.bus_thread.take() {
            if handle.join().is_err() {
                error!("bus thread panicked during drop");
            }
        }
    }
}

struct BusThreadCtx {
    bus: gst::Bus,
    stop_tx: watch::Sender<StopReason>,
    shutdown: Arc<AtomicBool>,
    underruns: Arc<AtomicU32>,
    warnings: Arc<Mutex<Vec<String>>>,
}

fn spawn_bus_thread(ctx: BusThreadCtx) -> JoinHandle<()> {
    thread::Builder::new()
        .name("zwhisper-bus".into())
        .spawn(move || run_bus_thread(ctx))
        .expect("failed to spawn bus thread")
}

#[allow(clippy::needless_pass_by_value)] // ctx must live for the bus thread.
fn run_bus_thread(ctx: BusThreadCtx) {
    let timeout = gst::ClockTime::from_mseconds(BUS_POLL_TIMEOUT_MS);
    loop {
        if ctx.shutdown.load(Ordering::Acquire) {
            return;
        }
        let Some(msg) = ctx.bus.timed_pop(Some(timeout)) else {
            // Timeout — re-check the shutdown flag at the top of the
            // loop. Watch-channel closure is unreliable here because
            // Recorder itself owns a receiver until `stop` returns.
            continue;
        };

        match watchdog::classify(&msg) {
            Classification::Stop(reason) => {
                debug!(?reason, "bus thread observed stop-worthy message");
                // send_replace overwrites earlier non-Running reasons
                // with the latest one, which is the right behaviour:
                // if a DeviceLost arrives after a UserRequested, the
                // device-lost reason should win (it carries error
                // information the user needs to see).
                ctx.stop_tx.send_replace(reason);
            }
            Classification::Underrun { source } => {
                let n = ctx.underruns.fetch_add(1, Ordering::Relaxed) + 1;
                warn!(%source, total = n, "audio underrun detected");
            }
            Classification::Warning { source, message } => {
                warn!(%source, %message, "gstreamer warning");
                if let Ok(mut v) = ctx.warnings.lock() {
                    if v.len() < MAX_WARNINGS {
                        v.push(format!("{source}: {message}"));
                    }
                }
            }
            Classification::Ignore => {}
        }
    }
}

/// Read the `total samples` field from the FLAC STREAMINFO block of
/// the closed output file. This is the authoritative count after
/// `set_state(Null)` has flushed every flacenc frame; querying the
/// running pipeline underestimates because flacenc keeps queued
/// buffers until EOS finalisation. Returns `0` if the file cannot be
/// read or does not look like a FLAC — telemetry only, not a
/// correctness gate.
///
/// FLAC layout (RFC 9639 § 8.1, 8.2):
/// - bytes 0..4 : "fLaC" magic
/// - byte 4 : metadata block header (high bit = last-flag, low 7
///   bits = block type; STREAMINFO == 0 and must be the first block)
/// - bytes 5..8 : block length (3 bytes, big-endian; STREAMINFO is
///   always 34 bytes)
/// - bytes 8.. : STREAMINFO body (34 bytes); bytes 21..26 hold a
///   36-bit total-sample-count — 4 low bits of byte 21 are the high
///   nibble, bytes 22..26 are the low 32 bits, all big-endian.
fn read_flac_total_samples(path: &std::path::Path) -> u64 {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    if bytes.len() < 26 || &bytes[0..4] != b"fLaC" {
        return 0;
    }
    if bytes[4] & 0x7F != 0 {
        // First metadata block is not STREAMINFO; we do not search
        // further in M0 — the encoder always writes it first.
        return 0;
    }
    let hi = u64::from(bytes[21] & 0x0F) << 32;
    let lo = u64::from(u32::from_be_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]));
    hi | lo
}

/// Block the calling thread until the watch channel reports a
/// non-`Running` `StopReason` (sent by the bus thread when it
/// classifies an `Eos`/`Error`/`DeviceLost` message), or until
/// `EOS_TIMEOUT_SECS` elapses. Returns `true` when an `EosObserved`
/// reason arrived in time. Any other reason (`BusError`,
/// `DeviceLost`) is also a "stop signal received" — we return `true`
/// to skip the timeout error path and let `stop()` translate the
/// watch value.
fn wait_for_stop_signal(stop_rx: &mut watch::Receiver<StopReason>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(EOS_TIMEOUT_SECS);
    loop {
        if !matches!(*stop_rx.borrow_and_update(), StopReason::Running) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            warn!(
                "EOS not observed within {EOS_TIMEOUT_SECS}s; pipeline will be torn down forcibly"
            );
            return false;
        }
        // tokio::sync::watch::Receiver only exposes async `changed()`.
        // We are on a synchronous code path here (callers are not in
        // a runtime) so we poll with a short blocking sleep instead.
        // The bus thread's stop forward is sub-millisecond once Eos
        // arrives, so 25ms slices are fine.
        let slice = remaining.min(Duration::from_millis(25));
        std::thread::sleep(slice);
    }
}


/// Convenience wrapper used by the M0 CLI: starts a `Recorder`, races
/// Ctrl+C against `--duration`, then runs the EOS finalisation. M3
/// will call `Recorder::start`/`stop` directly from the D-Bus handler
/// instead of going through this function.
pub(crate) fn record_blocking(
    opts: RecordOptions,
    duration_secs: u64,
) -> Result<RecordingReport, RecordingError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| RecordingError::PipelineFailed {
            stage: "build_tokio_runtime".into(),
            source: Box::new(e),
        })?;

    let recorder = Recorder::start(opts)?;
    let mut stop_rx = recorder.stop_subscriber();

    // Run the async race inside the runtime so that ctrl_c +
    // duration timer + watch-channel signalling all share one
    // executor. Once we know which signal fired we drop back to a
    // synchronous code path for the EOS finalisation, which is what
    // `wait_for_stop_signal` expects (no nested tokio block_on).
    let race_result = runtime.block_on(race_stop(&mut stop_rx, duration_secs));
    drop(runtime);

    let outcome = match race_result {
        Ok(o) => o,
        Err(race_err) => {
            // The race itself failed (e.g. tokio's signal handler
            // could not be installed). Do *not* propagate before
            // running the canonical EOS drain — bypassing it would
            // leave Drop to call `set_state(Null)` on a still-PLAYING
            // pipeline, which truncates the FLAC header. Treat this
            // as a user-requested stop, drain cleanly, then surface
            // the original error.
            recorder.request_stop(StopRequest::UserRequested);
            let _ = recorder.stop();
            return Err(race_err);
        }
    };

    // Only write a caller-side StopReason when the caller actually
    // initiated the stop. If the bus thread fired first, it has
    // already populated the watch channel with the real reason
    // (DeviceLost / BusError / EosObserved); calling request_stop
    // here would clobber that with UserRequested and turn a real
    // failure into a misleading success.
    if let RaceOutcome::Caller(request) = outcome {
        recorder.request_stop(request);
    }

    recorder.stop()
}

async fn race_stop(
    stop_rx: &mut watch::Receiver<StopReason>,
    duration_secs: u64,
) -> Result<RaceOutcome, RecordingError> {
    let bus_stopped = async {
        loop {
            // Check the *current* value first. If the bus thread
            // wrote a non-Running reason before this future was
            // first polled, awaiting `.changed()` would block
            // forever waiting for a *next* change. `borrow_and_update`
            // consumes the current value as "seen" so the subsequent
            // `.changed()` call is correctly armed for the next
            // notification.
            if !matches!(*stop_rx.borrow_and_update(), StopReason::Running) {
                return;
            }
            if stop_rx.changed().await.is_err() {
                return;
            }
        }
    };

    let outcome = if duration_secs == 0 {
        tokio::select! {
            res = tokio::signal::ctrl_c() => res.map(|()| RaceOutcome::Caller(StopRequest::UserRequested)),
            () = bus_stopped => Ok(RaceOutcome::BusInitiated),
        }
    } else {
        let dur = Duration::from_secs(duration_secs);
        tokio::select! {
            res = tokio::signal::ctrl_c() => res.map(|()| RaceOutcome::Caller(StopRequest::UserRequested)),
            () = tokio::time::sleep(dur) => Ok(RaceOutcome::Caller(StopRequest::DurationElapsed)),
            () = bus_stopped => Ok(RaceOutcome::BusInitiated),
        }
    };

    outcome.map_err(|e| RecordingError::PipelineFailed {
        stage: "install_ctrl_c_handler".into(),
        source: Box::new(e),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_running_is_not_an_error() {
        let r = StopReason::Running;
        assert!(!r.is_error());
    }

    #[test]
    fn stop_reason_device_lost_is_an_error() {
        let r = StopReason::DeviceLost {
            node: "x".into(),
        };
        assert!(r.is_error());
    }

    #[test]
    fn read_flac_total_samples_handles_known_streaminfo() {
        // Hand-crafted FLAC header with STREAMINFO declaring 1 234 567
        // total samples. Only the bytes the parser inspects are real;
        // everything else is filler.
        let mut buf = Vec::with_capacity(42);
        buf.extend_from_slice(b"fLaC");
        buf.push(0); // metadata block header: not last, type STREAMINFO
        buf.extend_from_slice(&[0, 0, 34]); // STREAMINFO body length
        // STREAMINFO body — 34 bytes; we only set the bytes the
        // parser reads, leave the rest zero.
        let mut info = vec![0u8; 34];
        // total_samples = 1_234_567 — fits in 32 bits, so the
        // 4-bit high nibble at body[13] stays 0; body[14..18] holds
        // the BE u32. body offset 13..18 = absolute file offset 21..26.
        info[14..18].copy_from_slice(&1_234_567u32.to_be_bytes());
        buf.extend_from_slice(&info);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synth.flac");
        std::fs::write(&path, &buf).unwrap();
        assert_eq!(read_flac_total_samples(&path), 1_234_567);
    }

    #[test]
    fn read_flac_total_samples_returns_zero_for_non_flac() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-flac.bin");
        std::fs::write(&path, b"definitely not a flac stream").unwrap();
        assert_eq!(read_flac_total_samples(&path), 0);
    }
}
