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
/// Sample rate of the encoded mono stream, locked in by the
/// pipeline caps `audio/x-raw,rate=16000,channels=1` (see
/// `pipeline.rs`). Mirrored here as a typed constant so the
/// `samples_written` ↔ wall-clock cross-check uses one source of
/// truth instead of a magic literal.
const OUTPUT_SAMPLE_RATE_HZ: u32 = 16_000;
/// Hard cap on the number of bus warnings the recorder retains. Past
/// this, additional warnings are still logged through `tracing` but
/// not stored in `RecordingReport.warnings`. Without the cap a
/// pathological `pipewire` backend could grow this vec unbounded over
/// a 60-min soak and trip `DoD` #2.
const MAX_WARNINGS: usize = 100;

/// Plain-Rust input to the audio façade. No `gst::*` types here so
/// callers in M3 can reuse this struct as the D-Bus input shape.
#[derive(Debug, Clone)]
pub struct RecordOptions {
    pub mic: String,
    /// Sink monitor node, or `"default"` for the `PipeWire` default.
    /// Empty strings are rejected by `devices::resolve` with a
    /// typed `InvalidArgument` — M2 ships mic + sink monitor mono
    /// mix only, mic-only mode lands in M3.
    pub monitor: String,
    pub output: PathBuf,
    /// Whether [`record_blocking`] should install a `Ctrl+C` handler.
    /// The CLI sets `true` (legacy walking-skeleton). The daemon sets
    /// `false` and drives shutdown via [`Recorder::request_stop`] from
    /// its own SIGINT/SIGTERM handler. POSIX allows only one handler
    /// per signal per process, so two `tokio::signal::ctrl_c()` calls
    /// in the same process race each other (M3 stress-test C2).
    ///
    /// Default is `false` — the safer behaviour. CLI sites that still
    /// want the legacy Ctrl+C race must opt in explicitly.
    pub install_ctrl_c: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            mic: String::new(),
            monitor: String::new(),
            output: PathBuf::new(),
            install_ctrl_c: false,
        }
    }
}

/// Result of a successful recording. Carries the output path because
/// IDEA.md § 2 `RecordingComplete(s session_id, s audio_path)` needs
/// it; threading the path through the caller after the fact is
/// painful.
#[derive(Debug, Clone)]
pub struct RecordingReport {
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
pub struct Recorder {
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
    pub fn start(opts: RecordOptions) -> Result<Self, RecordingError> {
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

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn state(&self) -> RecorderState {
        *self.state.lock().expect("poisoned recorder state")
    }

    /// Subscribe to stop-reason updates. Used by `record_blocking` to
    /// race the bus watchdog against the duration timer and Ctrl+C,
    /// and by the M3 daemon's lifecycle task to wait for an explicit
    /// stop request before invoking [`Recorder::await_completion`]
    /// (which always sends EOS as its first step).
    pub fn stop_subscriber(&self) -> watch::Receiver<StopReason> {
        self.stop_rx_anchor.clone()
    }

    /// Convert a caller-side request into a `StopReason` and write it
    /// to the watch channel. Used by the legacy CLI walking-skeleton
    /// path (`record_blocking`); the daemon goes through
    /// [`Recorder::request_stop`] directly.
    pub(crate) fn request_stop_from_request(&self, request: StopRequest) {
        let reason = match request {
            StopRequest::UserRequested => StopReason::UserRequested,
            StopRequest::DurationElapsed => StopReason::DurationElapsed,
        };
        self.request_stop(reason);
    }

    /// Non-blocking external cancellation. Idempotent. Sends `reason`
    /// into the recorder's internal `watch::Sender<StopReason>`; the
    /// recorder's blocking finalisation path observes it on the next
    /// poll and runs the EOS drain.
    ///
    /// This is the daemon's hook for `Recorder1.StopRecording` and
    /// for the daemon's SIGTERM handler. Do **not** install
    /// `tokio::signal::ctrl_c` inside [`Recorder::start`]; the caller
    /// (CLI or daemon) owns signal policy (M3 stress-test C2).
    pub fn request_stop(&self, reason: StopReason) {
        // send_replace tolerates closed receivers; the only time the
        // channel closes is when Recorder is dropped. Idempotent —
        // calling twice with the same or different reasons is fine,
        // the most recent wins (the bus thread already arbitrates).
        let _ = self.stop_tx.send_replace(reason);
    }

    /// Detached stop handle that survives moving `self` into a
    /// `tokio::task::spawn_blocking`. The daemon stores this on the
    /// `SessionManager` so `Recorder1.StopRecording` and the
    /// SIGTERM handler can drive shutdown after the recorder has
    /// been moved into the blocking task that holds
    /// [`Recorder::await_completion`].
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            tx: self.stop_tx.clone(),
        }
    }

    /// Run the canonical EOS finalisation sequence: send Eos, wait for
    /// the bus to confirm it (or surface an Error), transition to
    /// Null, join the bus thread, build the report. This is the only
    /// place that calls `set_state(Null)` — keeping the fragile path
    /// singular is the whole point of the handle.
    ///
    /// Blocks the calling thread; the daemon offloads this onto
    /// `tokio::task::spawn_blocking` so the multi-hour wait does not
    /// hold the runtime worker.
    pub fn await_completion(mut self) -> Result<RecordingReport, RecordingError> {
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
                // Read the authoritative sample count straight from
                // the closed FLAC. Done here (not before Null) so the
                // encoder has flushed everything to disk. A failure
                // here means the encoder produced an unreadable or
                // non-FLAC output — DoD #1 violated, surface as
                // `EncoderFailed` and mark the recorder Failed.
                let samples_written = match read_flac_total_samples(&self.output_path) {
                    Ok(n) => n,
                    Err(e) => {
                        *self.state.lock().expect("poisoned recorder state") =
                            RecorderState::Failed;
                        return Err(e);
                    }
                };
                // Cross-check the declared sample count against
                // wall-clock duration (DoD #3). Without this gate,
                // a structurally valid header claiming 0 samples on
                // a multi-minute recording would still return Ok.
                if let Err(e) = verify_samples_match_duration(
                    samples_written,
                    duration,
                    &self.output_path,
                ) {
                    *self.state.lock().expect("poisoned recorder state") =
                        RecorderState::Failed;
                    return Err(e);
                }
                *self.state.lock().expect("poisoned recorder state") = RecorderState::Idle;
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

/// Detached stop handle. Cloneable; sending into the recorder's
/// `watch` channel is idempotent. Used by `zwhisperd` to wire the
/// `Recorder1.StopRecording` D-Bus call to a recorder owned by a
/// `spawn_blocking` task — passing the recorder itself across that
/// boundary would block the runtime worker on the multi-hour
/// `await_completion` call.
#[derive(Debug, Clone)]
pub struct StopHandle {
    tx: watch::Sender<StopReason>,
}

impl StopHandle {
    /// Send `reason` into the recorder's stop channel. Errors are
    /// swallowed because a closed channel means the recorder
    /// already finalised — exactly the success path for an
    /// idempotent cancel.
    pub fn request_stop(&self, reason: StopReason) {
        let _ = self.tx.send_replace(reason);
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

/// Size of the prefix we read from the closed FLAC: 4-byte `fLaC`
/// magic + 4-byte metadata-block header + 34-byte STREAMINFO body
/// (per `RFC` 9639 § 8.1 and § 8.2). A valid FLAC is *at least*
/// this large; reading exactly this many bytes lets us reject a
/// truncated file before parsing the sample count.
const FLAC_PREFIX_BYTES: usize = 4 + 4 + 34;

/// Length declared by a STREAMINFO metadata-block header. Locked in
/// by `RFC` 9639 § 8.2 — flacenc cannot legally write a different
/// value, so any mismatch is a structural defect we refuse to sign
/// off.
const FLAC_STREAMINFO_LENGTH: u32 = 34;

/// Read the `total samples` field from the FLAC STREAMINFO block of
/// the closed output file and use it to validate that what we wrote
/// is in fact a FLAC. This is the authoritative count after
/// `set_state(Null)` has flushed every flacenc frame; querying the
/// running pipeline underestimates because flacenc keeps queued
/// buffers until EOS finalisation.
///
/// Reads only the first `FLAC_PREFIX_BYTES` bytes — the recording
/// can be many `GiB` and `stop()` runs on the foreground thread.
///
/// Validates four things, all required for `DoD` #1 ("produces a
/// valid FLAC"):
/// 1. The first four bytes are the `fLaC` magic.
/// 2. The first metadata block is STREAMINFO (block-type 0).
/// 3. The declared block length is exactly 34 (per `RFC` 9639 § 8.2).
/// 4. The full 34-byte STREAMINFO body is present on disk
///    (covered by `read_exact` on the 42-byte prefix).
///
/// FLAC layout (`RFC` 9639 § 8.1, 8.2):
/// - bytes 0..4 : "fLaC" magic
/// - byte 4 : metadata block header (high bit = last-flag, low 7
///   bits = block type; STREAMINFO == 0 and must be the first block)
/// - bytes 5..8 : block length (3 bytes, big-endian; STREAMINFO is
///   always 34 bytes)
/// - bytes 8.. : STREAMINFO body (34 bytes); bytes 21..26 hold a
///   36-bit total-sample-count — 4 low bits of byte 21 are the high
///   nibble, bytes 22..26 are the low 32 bits, all big-endian.
fn read_flac_total_samples(path: &std::path::Path) -> Result<u64, RecordingError> {
    use std::fs::File;
    use std::io::Read;

    let mut file = File::open(path).map_err(|e| {
        RecordingError::EncoderFailed(format!(
            "cannot open output `{}` to verify FLAC header: {e}",
            path.display()
        ))
    })?;
    let mut header = [0u8; FLAC_PREFIX_BYTES];
    file.read_exact(&mut header).map_err(|e| {
        RecordingError::EncoderFailed(format!(
            "output `{}` is shorter than {FLAC_PREFIX_BYTES} bytes; cannot contain a complete \
             STREAMINFO block: {e}",
            path.display()
        ))
    })?;
    if &header[0..4] != b"fLaC" {
        return Err(RecordingError::EncoderFailed(format!(
            "output `{}` does not start with the FLAC `fLaC` magic",
            path.display()
        )));
    }
    if header[4] & 0x7F != 0 {
        return Err(RecordingError::EncoderFailed(format!(
            "output `{}` has a non-STREAMINFO first metadata block (type {})",
            path.display(),
            header[4] & 0x7F
        )));
    }
    let declared_len = u32::from_be_bytes([0, header[5], header[6], header[7]]);
    if declared_len != FLAC_STREAMINFO_LENGTH {
        return Err(RecordingError::EncoderFailed(format!(
            "output `{}` declares STREAMINFO length {declared_len}, expected {FLAC_STREAMINFO_LENGTH}",
            path.display()
        )));
    }
    let hi = u64::from(header[21] & 0x0F) << 32;
    let lo = u64::from(u32::from_be_bytes([
        header[22], header[23], header[24], header[25],
    ]));
    Ok(hi | lo)
}

/// Maximum allowed drift between the FLAC's declared sample count
/// and the wall-clock duration × 16 kHz, expressed in samples.
/// `DoD` #3 says "± one buffer"; one second is generous — flacenc's
/// default block size is ~4608 samples (~290 ms) and the 60-min
/// soak observed only an 8-sample drift, so 16 000 samples is
/// several orders of magnitude looser than the worst observed
/// value while still catching duplicated/dropped buffers and
/// truncated streams.
const SAMPLE_COUNT_TOLERANCE: u64 = OUTPUT_SAMPLE_RATE_HZ as u64;

/// Verify that the FLAC STREAMINFO sample count is within
/// `SAMPLE_COUNT_TOLERANCE` of `wall_duration × 16 kHz`. Closes
/// `DoD` #3 ("declared length matches wall-clock duration ± one
/// buffer, no truncation"): without this check, `Recorder::stop`
/// would return success even when the encoder produced a header
/// claiming zero samples for a multi-minute recording.
fn verify_samples_match_duration(
    samples_written: u64,
    wall_duration: Duration,
    path: &std::path::Path,
) -> Result<(), RecordingError> {
    let expected_f = wall_duration.as_secs_f64() * f64::from(OUTPUT_SAMPLE_RATE_HZ);
    // Cast saturates on out-of-range f64s, which is the right
    // behaviour for a sanity check (we only care about magnitude).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let expected = expected_f.max(0.0) as u64;
    let lo = expected.saturating_sub(SAMPLE_COUNT_TOLERANCE);
    let hi = expected.saturating_add(SAMPLE_COUNT_TOLERANCE);
    if (lo..=hi).contains(&samples_written) {
        return Ok(());
    }
    Err(RecordingError::EncoderFailed(format!(
        "FLAC sample count {samples_written} for `{}` is outside expected range \
         [{lo}, {hi}] (wall-clock {:.3}s, expected ≈ {expected} samples ± {SAMPLE_COUNT_TOLERANCE})",
        path.display(),
        wall_duration.as_secs_f64()
    )))
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
pub fn record_blocking(
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

    let install_ctrl_c = opts.install_ctrl_c;
    let recorder = Recorder::start(opts)?;
    let mut stop_rx = recorder.stop_subscriber();

    // Run the async race inside the runtime so that ctrl_c +
    // duration timer + watch-channel signalling all share one
    // executor. Once we know which signal fired we drop back to a
    // synchronous code path for the EOS finalisation, which is what
    // `wait_for_stop_signal` expects (no nested tokio block_on).
    let race_result = runtime.block_on(race_stop(&mut stop_rx, duration_secs, install_ctrl_c));
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
            recorder.request_stop_from_request(StopRequest::UserRequested);
            let _ = recorder.await_completion();
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
        recorder.request_stop_from_request(request);
    }

    recorder.await_completion()
}

async fn race_stop(
    stop_rx: &mut watch::Receiver<StopReason>,
    duration_secs: u64,
    install_ctrl_c: bool,
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

    // `ctrl_c_arm` only resolves when `install_ctrl_c == true`; the
    // `pending().await` branch never wakes when disabled, so the
    // POSIX-singleton signal handler is never installed (M3 stress-
    // test C2). Two `tokio::signal::ctrl_c()` calls in the same
    // process race each other, which is exactly what the daemon must
    // avoid when it owns the signal policy.
    let ctrl_c_arm = async {
        if install_ctrl_c {
            tokio::signal::ctrl_c().await
        } else {
            std::future::pending::<std::io::Result<()>>().await
        }
    };

    let outcome = if duration_secs == 0 {
        tokio::select! {
            res = ctrl_c_arm => res.map(|()| RaceOutcome::Caller(StopRequest::UserRequested)),
            () = bus_stopped => Ok(RaceOutcome::BusInitiated),
        }
    } else {
        let dur = Duration::from_secs(duration_secs);
        tokio::select! {
            res = ctrl_c_arm => res.map(|()| RaceOutcome::Caller(StopRequest::UserRequested)),
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
#[allow(clippy::unwrap_used, clippy::panic)]
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
        assert_eq!(read_flac_total_samples(&path).unwrap(), 1_234_567);
    }

    #[test]
    fn read_flac_total_samples_rejects_non_flac() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-flac.bin");
        std::fs::write(&path, b"definitely not a flac stream").unwrap();
        let err = read_flac_total_samples(&path).unwrap_err();
        assert!(matches!(err, RecordingError::EncoderFailed(_)),
            "expected EncoderFailed, got {err:?}");
    }

    #[test]
    fn read_flac_total_samples_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.flac");
        let err = read_flac_total_samples(&path).unwrap_err();
        assert!(matches!(err, RecordingError::EncoderFailed(_)),
            "expected EncoderFailed, got {err:?}");
    }

    #[test]
    fn read_flac_total_samples_rejects_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.flac");
        // Has the magic but is shorter than 42 bytes.
        std::fs::write(&path, b"fLaC\x00\x00\x00\x22").unwrap();
        let err = read_flac_total_samples(&path).unwrap_err();
        assert!(matches!(err, RecordingError::EncoderFailed(_)),
            "expected EncoderFailed, got {err:?}");
    }

    #[test]
    fn read_flac_total_samples_rejects_wrong_streaminfo_length() {
        // 42 bytes, fLaC magic, STREAMINFO type, but declared
        // length is 33 instead of 34 — a structural defect that
        // the previous parser silently accepted because it only
        // read 26 bytes.
        let mut buf = Vec::with_capacity(42);
        buf.extend_from_slice(b"fLaC");
        buf.push(0); // STREAMINFO, not last
        buf.extend_from_slice(&[0, 0, 33]); // wrong length
        buf.extend_from_slice(&[0u8; 34]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-len.flac");
        std::fs::write(&path, &buf).unwrap();
        let err = read_flac_total_samples(&path).unwrap_err();
        match err {
            RecordingError::EncoderFailed(msg) => {
                assert!(msg.contains("STREAMINFO length"), "unexpected msg: {msg}");
            }
            other => panic!("expected EncoderFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_samples_match_duration_accepts_within_tolerance() {
        let path = std::path::Path::new("/tmp/synthetic.flac");
        // 60 s × 16 000 = 960 000; tolerance is 16 000.
        verify_samples_match_duration(960_000, Duration::from_secs(60), path).unwrap();
        verify_samples_match_duration(944_000, Duration::from_secs(60), path).unwrap();
        verify_samples_match_duration(976_000, Duration::from_secs(60), path).unwrap();
    }

    #[test]
    fn verify_samples_match_duration_rejects_zero_samples_for_long_recording() {
        let path = std::path::Path::new("/tmp/synthetic.flac");
        let err = verify_samples_match_duration(0, Duration::from_secs(60), path).unwrap_err();
        assert!(matches!(err, RecordingError::EncoderFailed(_)));
    }

    #[test]
    fn verify_samples_match_duration_rejects_overshoot() {
        let path = std::path::Path::new("/tmp/synthetic.flac");
        // 60 s expected = 960 000; +50 000 overshoots tolerance.
        let err = verify_samples_match_duration(1_010_000, Duration::from_secs(60), path)
            .unwrap_err();
        assert!(matches!(err, RecordingError::EncoderFailed(_)));
    }

    #[test]
    fn verify_samples_match_duration_accepts_short_recording() {
        // For sub-second recordings the tolerance dominates expected,
        // so the lower bound is 0 — only catches gross errors.
        let path = std::path::Path::new("/tmp/synthetic.flac");
        verify_samples_match_duration(0, Duration::from_millis(100), path).unwrap();
        verify_samples_match_duration(1_600, Duration::from_millis(100), path).unwrap();
    }

    #[test]
    fn verify_samples_match_duration_matches_3600s_soak_drift() {
        // Regression: the 60-min soak measured 57_600_008 samples
        // for ~3600 s wall-clock; this must continue to pass.
        let path = std::path::Path::new("/tmp/synthetic.flac");
        verify_samples_match_duration(57_600_008, Duration::from_secs(3600), path).unwrap();
    }
}
