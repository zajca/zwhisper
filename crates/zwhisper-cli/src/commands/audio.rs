//! `zwhisper audio …` — guided microphone setup & calibration
//! (RFC-mic-setup, Phases 1+2).
//!
//! This is the thin CLI half of the feature: all analysis and tooling
//! indirection lives in `zwhisper_core::setup` behind the
//! `PipewireControl` trait (`SystemPipewire` shells out to
//! `pw-dump` / `wpctl` with **no shell**, numeric-id validation,
//! finite/clamped volumes, and a size-capped dump). This module owns
//! only the parts that cannot live in core: spawning `pw-cat` to read
//! raw PCM, the `\r`-refreshed ASCII VU meter, the interactive speak
//! prompt, and the calibration control loop.
//!
//! Security (RFC "External Tools & Security"): every external argument
//! is passed via `Command::arg(...)` — never a shell string. Device
//! selectors are resolved to a numeric id by
//! [`zwhisper_core::setup::resolve_selector`] (which validates a
//! `node.name` against the shared allow-list and a bare integer as
//! purely numeric) *before* it reaches `pw-cat --target`. Volumes are
//! clamped + finiteness-checked inside core's `set_volume`. The `pw-cat`
//! child has a hard timeout, is killed on completion, and its stdout
//! read is byte-bounded so a hostile / wedged stream can neither hang
//! the CLI nor exhaust memory. `set-default` mutates global state and is
//! gated behind the explicit `--set-default` flag.

use std::io::Write;
use std::time::Duration;

use color_eyre::eyre::{WrapErr, eyre};
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::info;
use zwhisper_core::profile::error::ProfileError;
use zwhisper_core::profile::listing::{SourcesUpdate, clone_to_user, set_outputs, update_sources};
use zwhisper_core::profile::load;
use zwhisper_core::profile::schema::{Backend, Mode, OutputDest};
use zwhisper_core::setup::config::SetupConfig;
use zwhisper_core::setup::volume::format_linear;
use zwhisper_core::setup::{
    AudioDevice, LevelStats, PipewireControl, SetupError, SystemPipewire, Volume, analyze,
    build_devices, recommend_volume, resolve_selector, within_tolerance,
};

use crate::cli::{AudioCalibrateArgs, AudioCmd, AudioSetupArgs};

use super::build_runtime;

/// Width (characters) of the ASCII VU bar. A presentation constant for
/// terminal rendering only — it carries no audio semantics, so it lives
/// with the renderer rather than in `SetupConfig`.
const VU_BAR_WIDTH: usize = 40;

/// Bytes per `f32` sample on the `pw-cat --format=f32` stream. The PCM
/// is little-endian IEEE-754 single precision (verified on the ALC1220
/// box, 2026-06-03).
const F32_BYTES: usize = 4;

/// Entry point for the `audio` command group (mirrors
/// [`super::model::run`]). Returns `color_eyre::Result` so the binary's
/// `main` renders a typed error and exits non-zero.
pub(crate) fn run(cmd: &AudioCmd) -> color_eyre::Result<()> {
    match cmd {
        AudioCmd::Devices { json } => devices(*json),
        AudioCmd::Meter { source } => meter(source.as_deref()),
        AudioCmd::Calibrate(args) => calibrate(args),
        AudioCmd::Setup(args) => setup(args),
    }
}

// ===========================================================================
// audio devices
// ===========================================================================

/// One row of the `audio devices` JSON output. Kept `pub(crate)` and
/// `Serialize`-only (the human table is built separately) so the script
/// contract is explicit and stable.
#[derive(Debug, Serialize)]
pub(crate) struct DeviceJson {
    /// `object.id` — what `wpctl` / `pw-cat --target` consume.
    id: u32,
    /// `node.name` — the stable routing identifier written into profiles.
    node_name: String,
    /// `node.description` — the human-readable label.
    description: String,
    /// `media.class == "Audio/Source"` (an input) vs a sink (output).
    is_source: bool,
    /// `node.name` ends with `.monitor` (a sink-monitor capture target).
    is_monitor: bool,
    /// Cross-referenced against `wpctl inspect @DEFAULT_AUDIO_SOURCE@`.
    is_default: bool,
    /// Current linear volume (`0.0..`), or `null` when unknown (sinks are
    /// not probed). `1.0` ≈ unity.
    volume_linear: Option<f32>,
    /// Whether the node is muted, or `null` when the volume was not read.
    muted: Option<bool>,
}

/// `zwhisper audio devices [--json]` — enumerate inputs and outputs.
///
/// Sources are listed first (the common case for mic setup), then sinks.
/// Source volume is enriched via `wpctl get-volume` (best-effort: a node
/// that disappears between the dump and the volume read is shown without
/// a level rather than failing the whole listing).
fn devices(json: bool) -> color_eyre::Result<()> {
    let pw = SystemPipewire::default();
    let rows = enumerate(&pw)?;

    if json {
        let payload: Vec<DeviceJson> = rows.iter().map(device_to_json).collect();
        let text = serde_json::to_string_pretty(&payload)
            .wrap_err("failed to serialize audio devices to JSON")?;
        println!("{text}");
        return Ok(());
    }

    if rows.is_empty() {
        println!("(no audio devices found)");
        return Ok(());
    }

    println!(
        "{:<6}  {:<8}  {:<8}  {:<7}  description",
        "id", "kind", "default", "volume"
    );
    println!("{}", "-".repeat(72));
    for d in &rows {
        let kind = if d.is_source { "source" } else { "sink" };
        let default = if d.is_default { "DEFAULT" } else { "" };
        let monitor = if d.is_monitor { " [monitor]" } else { "" };
        let volume = d
            .volume
            .map_or_else(|| "-".to_owned(), |v| format_percent(v.linear));
        println!(
            "{:<6}  {:<8}  {:<8}  {:<7}  {}{}",
            d.id, kind, default, volume, d.description, monitor
        );
    }
    Ok(())
}

/// Enumerate audio devices and enrich each **source** with its current
/// volume. Sinks are left without a volume (the mic-setup flow does not
/// need playback levels, and probing every sink doubles the `wpctl`
/// calls for no benefit). Sources sort before sinks; within a group the
/// `pw-dump` order is preserved.
fn enumerate(pw: &dyn PipewireControl) -> color_eyre::Result<Vec<AudioDevice>> {
    let raw = pw.dump_nodes().map_err(setup_err)?;
    let default = pw.default_source_name().map_err(setup_err)?;
    let mut devices = build_devices(&raw, &default);

    // Sources first, then sinks; stable within each group.
    devices.sort_by_key(|d| !d.is_source);

    for d in &mut devices {
        if d.is_source {
            // Best-effort: a transient failure to read one source's volume
            // must not sink the whole listing. The level simply shows `-`.
            if let Ok(v) = pw.get_volume(d.id) {
                d.volume = Some(v);
            }
        }
    }
    Ok(devices)
}

/// Convert an enriched [`AudioDevice`] into its JSON row.
fn device_to_json(d: &AudioDevice) -> DeviceJson {
    DeviceJson {
        id: d.id,
        node_name: d.node_name.clone(),
        description: d.description.clone(),
        is_source: d.is_source,
        is_monitor: d.is_monitor,
        is_default: d.is_default,
        volume_linear: d.volume.map(|v| v.linear),
        muted: d.volume.map(|v| v.muted),
    }
}

// ===========================================================================
// audio meter
// ===========================================================================

/// `zwhisper audio meter [--source <sel>]` — live VU meter until Ctrl+C.
///
/// Read-only: spawns `pw-cat` to read raw mono `f32` PCM, decodes each
/// `meter_stdout_chunk_bytes` window, computes peak/RMS dBFS, and
/// re-renders an ASCII bar on the same line (`\r`). Ctrl+C (or `pw-cat`
/// EOF) ends the loop cleanly; the child is always killed on exit.
fn meter(source: Option<&str>) -> color_eyre::Result<()> {
    let cfg = SetupConfig::default();
    cfg.validate()
        .map_err(|e| eyre!("invalid setup config: {e}"))?;

    let pw = SystemPipewire::new(cfg.clone());
    let id = resolve_target(&pw, source)?;

    let rt = build_runtime()?;
    rt.block_on(meter_loop(id, &cfg))
}

/// The async metering loop. Spawns `pw-cat`, then selects between the
/// next PCM chunk and Ctrl+C. Returns when the user interrupts or the
/// stream ends; the child is killed and reaped on the way out.
async fn meter_loop(id: u32, cfg: &SetupConfig) -> color_eyre::Result<()> {
    let mut child = spawn_pw_cat(id, cfg).wrap_err("failed to start pw-cat for metering")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("pw-cat produced no stdout pipe"))?;

    eprintln!(
        "metering source id {id} at {} Hz — speak normally; press Ctrl+C to stop.",
        cfg.meter_rate_hz
    );

    // A reusable read buffer plus a carry for the (rare) case where a
    // read splits an f32 across chunk boundaries: we keep the leftover
    // 1-3 bytes and prepend them to the next decode.
    let mut buf = vec![0u8; cfg.meter_stdout_chunk_bytes];
    let mut carry: Vec<u8> = Vec::with_capacity(F32_BYTES);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let outcome = loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => break Ok(()),
            read = stdout.read(&mut buf) => {
                match read {
                    Ok(0) => break Ok(()), // pw-cat exited / EOF
                    Ok(n) => {
                        let samples = decode_f32_le(&mut carry, &buf[..n]);
                        if !samples.is_empty() {
                            let stats = analyze(&samples);
                            render_meter_line(&stats);
                        }
                    }
                    Err(e) => break Err(eyre!("error reading pw-cat stdout: {e}")),
                }
            }
        }
    };

    // Always tear the child down — never leave a stray pw-cat recording.
    let _ = child.start_kill();
    let _ = child.wait().await;
    // Finish the in-place bar with a newline so the shell prompt is clean.
    eprintln!();
    outcome
}

/// Render one VU line to stderr in place (`\r`, no newline). stderr keeps
/// the live meter off stdout so a user piping stdout is unaffected.
fn render_meter_line(stats: &LevelStats) {
    let bar = render_vu_bar(stats.peak_db, VU_BAR_WIDTH);
    eprint!(
        "\r[{bar}] peak {peak} RMS {rms} {clip}   ",
        peak = format_dbfs(stats.peak_db),
        rms = format_dbfs(stats.rms_db),
        clip = clip_indicator(stats.peak_db),
    );
    // Best-effort flush; a closed stderr is not worth aborting the loop.
    let _ = std::io::stderr().flush();
}

/// A time-bounded live meter for the wizard: render the VU bar for
/// `seconds`, then stop on its own (no Ctrl+C needed). Reuses the same
/// `pw-cat` spawn, `f32` decode, and line renderer as the interactive
/// [`meter`] so there is one metering implementation. The whole thing is
/// wrapped in the same hard timeout / kill discipline as a capture so a
/// wedged `pw-cat` can neither hang the wizard nor leak a child.
async fn live_meter(id: u32, seconds: f32, cfg: &SetupConfig) -> color_eyre::Result<()> {
    let grace = Duration::from_millis(500);
    let window = Duration::from_secs_f32(seconds);
    let hard = Duration::from_secs(cfg.pw_cat_timeout_secs).max(window + grace);

    let result = tokio::time::timeout(hard, live_meter_inner(id, window, cfg)).await;
    // Finish the in-place bar with a newline so the next prompt is clean.
    eprintln!();
    match result {
        Ok(inner) => inner,
        Err(_elapsed) => Err(setup_err(SetupError::Timeout {
            seconds: cfg.pw_cat_timeout_secs,
        })),
    }
}

/// Inner loop of [`live_meter`] (wrapped in a timeout by the caller):
/// spawn `pw-cat`, render each decoded chunk until the window elapses or
/// the stream ends, then kill and reap the child.
async fn live_meter_inner(id: u32, window: Duration, cfg: &SetupConfig) -> color_eyre::Result<()> {
    let mut child = spawn_pw_cat(id, cfg).wrap_err("failed to start pw-cat for the live meter")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("pw-cat produced no stdout pipe"))?;

    let mut buf = vec![0u8; cfg.meter_stdout_chunk_bytes];
    let mut carry: Vec<u8> = Vec::with_capacity(F32_BYTES);
    let deadline = tokio::time::Instant::now() + window;

    let outcome = loop {
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            () = &mut sleep => break Ok(()),
            read = stdout.read(&mut buf) => {
                match read {
                    Ok(0) => break Ok(()), // pw-cat exited / EOF
                    Ok(n) => {
                        let samples = decode_f32_le(&mut carry, &buf[..n]);
                        if !samples.is_empty() {
                            render_meter_line(&analyze(&samples));
                        }
                    }
                    Err(e) => break Err(eyre!("error reading pw-cat stdout: {e}")),
                }
            }
        }
    };

    let _ = child.start_kill();
    let _ = child.wait().await;
    outcome
}

// ===========================================================================
// audio calibrate
// ===========================================================================

/// `zwhisper audio calibrate […]` — measure, recommend, optionally apply.
///
/// Flow (RFC "Calibration Algorithm"):
///   1. Record a short **noise-floor** window *before* prompting, so the
///      idle level is captured while the user is quiet.
///   2. Prompt the user to speak for `--seconds`; capture and analyse.
///   3. Report peak/RMS dBFS for both windows and a noise-floor warning
///      when the idle level exceeds the configured ceiling.
///   4. Compute a recommended volume. With `--apply`, set it and
///      re-measure, iterating up to `cfg.max_iterations` until the peak
///      is within tolerance — or surface a typed "too quiet" message at
///      the cap instead of looping forever. Without `--apply` it is a
///      dry run (recommendation only; no `wpctl` writes).
///   5. With `--set-default`, make the device the default source. With
///      `--profile`, persist `sources.mic` (the concrete node) into the
///      named user-override profile.
fn calibrate(args: &AudioCalibrateArgs) -> color_eyre::Result<()> {
    let cfg = build_calibrate_config(args)?;

    let pw = SystemPipewire::new(cfg.clone());
    let rows = enumerate(&pw)?;
    let id =
        resolve_selector(args.source.as_deref().unwrap_or("default"), &rows).map_err(setup_err)?;
    let device = rows.iter().find(|d| d.id == id);

    if let Some(d) = device {
        println!(
            "calibrating: {} (id {id}, node {})",
            d.description, d.node_name
        );
    } else {
        // A bare numeric id need not appear in the dump (the RFC selector
        // grammar allows it). Proceed — wpctl / pw-cat accept the id.
        println!("calibrating: id {id}");
    }

    let rt = build_runtime()?;
    rt.block_on(calibrate_async(&pw, id, device, args, &cfg))
}

/// Override the relevant [`SetupConfig`] fields from the CLI flags, then
/// fail fast via [`SetupConfig::validate`] (CLAUDE.md: no silent
/// defaults, fail fast on bad config). The capture windows must also fit
/// inside the `pw-cat` hard timeout, otherwise the fixed-duration
/// capture would have to silently truncate — we reject that instead.
fn build_calibrate_config(args: &AudioCalibrateArgs) -> color_eyre::Result<SetupConfig> {
    let mut cfg = SetupConfig::default();
    if let Some(peak) = args.target_peak_db {
        cfg.target_peak_db = peak;
    }
    if let Some(secs) = args.seconds {
        cfg.speech_seconds = secs;
    }
    if let Some(max) = args.max_volume {
        cfg.max_volume = max;
    }
    cfg.validate()
        .map_err(|e| eyre!("invalid calibration settings: {e}"))?;

    // Both capture windows are killed after their duration; the timeout
    // is the overall ceiling on each blocking capture. A window longer
    // than the timeout cannot be honoured without truncation, so reject
    // it with an actionable message rather than silently shortening it.
    let timeout = cfg.pw_cat_timeout_secs as f32;
    if cfg.speech_seconds > timeout {
        return Err(eyre!(
            "--seconds {} exceeds the pw-cat capture timeout of {}s; pick a shorter window",
            cfg.speech_seconds,
            cfg.pw_cat_timeout_secs
        ));
    }
    Ok(cfg)
}

/// Async core of [`calibrate`]: the noise-floor + speech captures and the
/// apply/re-measure loop. Split out so the synchronous flag handling and
/// device resolution stay above the runtime boundary.
async fn calibrate_async(
    pw: &dyn PipewireControl,
    id: u32,
    device: Option<&AudioDevice>,
    args: &AudioCalibrateArgs,
    cfg: &SetupConfig,
) -> color_eyre::Result<()> {
    // 1. Noise floor — sampled while the user is (presumably) quiet, so
    //    we measure it before any "speak now" prompt.
    println!(
        "measuring noise floor — stay quiet for {:.1}s …",
        cfg.noise_floor_seconds
    );
    let floor = capture_window(id, cfg.noise_floor_seconds, cfg).await?;
    println!(
        "noise floor: peak {} RMS {}",
        format_dbfs(floor.peak_db),
        format_dbfs(floor.rms_db)
    );
    if let Some(warning) = noise_floor_warning(floor.peak_db, cfg) {
        println!("  {warning}");
    }

    // 2. Speech window — prompt, then capture.
    prompt_speak(cfg.speech_seconds);
    let speech = capture_window(id, cfg.speech_seconds, cfg).await?;
    println!(
        "speech level: peak {} RMS {}",
        format_dbfs(speech.peak_db),
        format_dbfs(speech.rms_db)
    );

    // 3. Recommend against the current device volume (when known).
    let current = current_linear(pw, id);
    let recommended = recommend_volume(current, speech.peak_db, cfg);
    println!(
        "current volume: {}   recommended: {}  (target peak {})",
        format_percent(current),
        format_percent(recommended),
        format_dbfs(cfg.target_peak_db)
    );

    if !args.apply {
        println!(
            "dry run — no changes made. Re-run with --apply to set the volume{}.",
            if args.profile.is_some() {
                " and write the profile"
            } else {
                ""
            }
        );
        return Ok(());
    }

    // 4. Apply → re-measure → adjust, bounded by cfg.max_iterations.
    apply_loop(pw, id, current, speech, cfg).await?;

    // 5. Side effects gated behind their explicit flags.
    if args.set_default {
        pw.set_default(id).map_err(setup_err)?;
        println!("set default source to id {id}.");
    }

    if let Some(profile) = &args.profile {
        let node = device.map(|d| d.node_name.as_str()).ok_or_else(|| {
            eyre!(
                "cannot persist a profile: selector resolved to id {id}, which is not in the \
                 device list, so its node.name is unknown. Re-run with --source <node.name> or \
                 --source default."
            )
        })?;
        persist_profile(profile, node)?;
    }

    Ok(())
}

/// The result of the apply/re-measure loop: the final linear volume left
/// on the device. The `setup` wizard reports it in its summary, so the
/// loop returns it instead of discarding it; `calibrate` ignores it.
#[derive(Debug, Clone, Copy)]
struct ApplyOutcome {
    /// The linear volume the loop finished on (also the value now live on
    /// the device via `set_volume`).
    final_volume: f32,
}

/// Run the apply/re-measure loop. Sets the recommended volume, captures a
/// fresh speech window, and repeats until the peak is within tolerance or
/// the iteration cap is hit. If the mic is still below target at the
/// volume cap, surface [`SetupError::TooQuiet`] rather than looping.
async fn apply_loop(
    pw: &dyn PipewireControl,
    id: u32,
    mut current: f32,
    mut last: LevelStats,
    cfg: &SetupConfig,
) -> color_eyre::Result<ApplyOutcome> {
    for iteration in 1..=cfg.max_iterations {
        let target_volume = recommend_volume(current, last.peak_db, cfg);
        pw.set_volume(id, target_volume).map_err(setup_err)?;
        println!(
            "iteration {iteration}/{}: set volume {} ({})",
            cfg.max_iterations,
            format_percent(target_volume),
            format_linear(target_volume)
        );

        // Re-measure with the new volume in effect.
        prompt_speak(cfg.speech_seconds);
        last = capture_window(id, cfg.speech_seconds, cfg).await?;
        current = target_volume;
        println!("  measured peak {}", format_dbfs(last.peak_db));

        if within_tolerance(
            last.peak_db,
            cfg.target_peak_db,
            cfg.target_peak_tolerance_db,
        ) {
            println!(
                "converged: peak {} within {} of target {}.",
                format_dbfs(last.peak_db),
                format_db_delta(cfg.target_peak_tolerance_db),
                format_dbfs(cfg.target_peak_db)
            );
            return Ok(ApplyOutcome {
                final_volume: current,
            });
        }

        // If we are already pinned at the cap and still too quiet, no
        // further iteration can help — stop with a typed error.
        let at_cap = (target_volume - cfg.max_volume).abs() < f32::EPSILON;
        if at_cap && last.peak_db < cfg.target_peak_db {
            return Err(setup_err(SetupError::TooQuiet {
                measured_db: last.peak_db,
                target_db: cfg.target_peak_db,
                max_volume: cfg.max_volume,
            }));
        }
    }

    println!(
        "stopped after {} iterations; closest peak {} (target {} ± {}). \
         Re-run audio calibrate or fine-tune with audio meter.",
        cfg.max_iterations,
        format_dbfs(last.peak_db),
        format_dbfs(cfg.target_peak_db),
        format_db_delta(cfg.target_peak_tolerance_db)
    );
    Ok(ApplyOutcome {
        final_volume: current,
    })
}

/// Write the selected node into a user-override profile's `sources.mic`,
/// leaving `input_gain_db` absent: the PipeWire device volume set above
/// is the primary fix; the software trim stays a separate, opt-in knob
/// (RFC "Profile Changes"). A non-user-override profile yields a typed
/// [`ProfileError::NotFound`], which we translate into an actionable
/// "clone first" hint.
fn persist_profile(profile: &str, node: &str) -> color_eyre::Result<()> {
    let update = SourcesUpdate {
        mic: Some(node),
        ..Default::default()
    };
    match update_sources(profile, &update) {
        Ok(path) => {
            info!(profile, node, path = %path.display(), "wrote sources.mic to profile");
            println!(
                "wrote sources.mic = {node:?} to profile {profile:?} ({}).",
                path.display()
            );
            Ok(())
        }
        Err(ProfileError::NotFound { .. }) => Err(eyre!(
            "profile {profile:?} is not a user override; clone it first: \
             `zwhisper profile clone {profile} {profile}` (or pick another name)."
        )),
        Err(e) => Err(eyre!("{e}")),
    }
}

/// Read the device's current linear volume, defaulting to the
/// configuration's max when it cannot be read. Returning the cap (rather
/// than `0`) keeps [`recommend_volume`]'s ratio math meaningful when the
/// volume probe fails: it errs toward not over-boosting.
fn current_linear(pw: &dyn PipewireControl, id: u32) -> f32 {
    pw.get_volume(id).map(|v: Volume| v.linear).unwrap_or(1.0)
}

// ===========================================================================
// audio setup (interactive wizard — RFC-mic-setup Phase 4)
// ===========================================================================

/// The two capture presets the wizard offers. Each maps to a concrete
/// `sources.system_output` value (and always `mono_mix`):
/// - [`Preset::Dictation`] → `system_output = ""` (mic-only, no system
///   audio mixed in — the dictation path);
/// - [`Preset::Meeting`] → `system_output = "default"` (mic plus the
///   default sink's monitor — the meeting path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preset {
    Dictation,
    Meeting,
}

impl Preset {
    /// The `system_output` token this preset writes into `[sources]`.
    /// `""` is the canonical mic-only marker (honoured verbatim by the
    /// schema, never coerced to `"default"`).
    fn system_output(self) -> &'static str {
        match self {
            Self::Dictation => "",
            Self::Meeting => "default",
        }
    }

    /// A short human label for prompts and the summary.
    fn label(self) -> &'static str {
        match self {
            Self::Dictation => "dictation (mic only)",
            Self::Meeting => "meeting (mic + system)",
        }
    }

    /// The default user-override profile name suggested when `--profile`
    /// is omitted. Matches the shipped profile names so the wizard's
    /// suggestion is one the user likely recognises.
    fn default_profile_name(self) -> &'static str {
        match self {
            Self::Dictation => "dictation",
            Self::Meeting => "meeting",
        }
    }
}

/// Where the transcript is delivered, on top of the always-present `file`
/// output every profile ships. The wizard offers three combinations:
/// - [`OutputChoice::FileOnly`] → just the transcript file (no live
///   delivery; the conservative default for meetings);
/// - [`OutputChoice::FileAndType`] → file **plus** typing the text at the
///   cursor ([`OutputDest::TypeAtCursor`], wlroots only — the natural
///   dictation flow);
/// - [`OutputChoice::FileAndClipboard`] → file **plus** copying the text
///   to the clipboard ([`OutputDest::Clipboard`]).
///
/// The shared `File` prefix is intentional: every choice always keeps the
/// profile's `file` output and only differs in the *additional* live
/// delivery, so the prefix documents that invariant rather than being
/// noise.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputChoice {
    FileOnly,
    FileAndType,
    FileAndClipboard,
}

impl OutputChoice {
    /// A short human label for the menu and the summary.
    fn label(self) -> &'static str {
        match self {
            Self::FileOnly => "file only",
            Self::FileAndType => "file + type at cursor",
            Self::FileAndClipboard => "file + clipboard",
        }
    }

    /// The choice suggested as the empty-input default for a preset:
    /// dictation wants the text typed where the cursor is; meeting capture
    /// just wants the transcript on disk.
    fn default_for(preset: Preset) -> Self {
        match preset {
            Preset::Dictation => Self::FileAndType,
            Preset::Meeting => Self::FileOnly,
        }
    }

    /// The *additional* live output this choice appends to the profile's
    /// `file` output, or `None` for [`OutputChoice::FileOnly`] (file only,
    /// no live delivery).
    fn live(self) -> Option<OutputDest> {
        match self {
            Self::FileOnly => None,
            Self::FileAndType => Some(OutputDest::TypeAtCursor),
            Self::FileAndClipboard => Some(OutputDest::Clipboard),
        }
    }
}

/// What the wizard decided to do, assembled from the interactive prompts
/// and then executed in one place. Kept separate from the I/O so the
/// summary can be rendered by a pure function and unit-tested.
#[derive(Debug, Clone)]
struct WizardPlan {
    /// The chosen device id (what `wpctl` / `pw-cat` consume).
    id: u32,
    /// The chosen device's `node.description` (what the user reads).
    description: String,
    /// The chosen device's `node.name` (written into the profile).
    node_name: String,
    /// The linear volume the calibration loop finished on.
    final_volume: f32,
    /// Whether the user confirmed making this the default source.
    made_default: bool,
    /// The chosen capture preset.
    preset: Preset,
    /// The chosen transcript-delivery output combination (on top of the
    /// always-present `file` output).
    output_choice: OutputChoice,
    /// The user-override profile that was written.
    profile: String,
    /// The on-disk path of the written profile.
    profile_path: std::path::PathBuf,
}

/// `zwhisper audio setup` — the interactive wizard (RFC-mic-setup Phase
/// 4). Composes the Phase 1/2 building blocks:
///   1. enumerate sources and let the user pick one by number
///      ([`enumerate`] + [`prompt_device_choice`]);
///   2. calibrate against the chosen device (noise floor → speak →
///      apply/re-measure), reusing [`capture_window`] / [`apply_loop`]
///      and warning on a high idle floor ([`noise_floor_warning`]);
///   3. choose a dictation/meeting preset;
///   4. confirm, then apply the PipeWire volume (already live after
///      calibration) and — gated behind an explicit yes/no — set the
///      default source ([`PipewireControl::set_default`]);
///   5. resolve the target profile (clone a base to a user override
///      first when needed) and write `[sources]` via [`update_sources`];
///   6. print a plain-language summary ([`render_summary`]).
fn setup(args: &AudioSetupArgs) -> color_eyre::Result<()> {
    let cfg = build_setup_config(args)?;

    let pw = SystemPipewire::new(cfg.clone());
    let rt = build_runtime()?;
    rt.block_on(setup_async(&pw, args, &cfg))
}

/// Override the relevant [`SetupConfig`] fields from the wizard's
/// optional flags, then fail fast via [`SetupConfig::validate`]
/// (CLAUDE.md: no silent defaults). Mirrors [`build_calibrate_config`]
/// but only honours the two overrides the wizard exposes.
fn build_setup_config(args: &AudioSetupArgs) -> color_eyre::Result<SetupConfig> {
    let mut cfg = SetupConfig::default();
    if let Some(peak) = args.target_peak_db {
        cfg.target_peak_db = peak;
    }
    if let Some(max) = args.max_volume {
        cfg.max_volume = max;
    }
    cfg.validate()
        .map_err(|e| eyre!("invalid setup settings: {e}"))?;
    Ok(cfg)
}

/// Async core of the wizard: everything that needs the tokio runtime
/// (the `pw-cat` captures and the apply loop) plus the interactive
/// prompts that gate them. Split out so [`setup`] stays a thin
/// flag-parse + runtime-construct shell.
async fn setup_async(
    pw: &dyn PipewireControl,
    args: &AudioSetupArgs,
    cfg: &SetupConfig,
) -> color_eyre::Result<()> {
    println!("zwhisper audio setup — guided microphone configuration.\n");

    // 1. Enumerate sources and let the user pick one.
    let mut devices = enumerate(pw)?;
    devices.retain(|d| d.is_source);
    if devices.is_empty() {
        return Err(eyre!(
            "no audio input sources found. Plug in a microphone and re-run \
             `zwhisper audio setup`, or check `zwhisper audio devices`."
        ));
    }
    let chosen = prompt_device_choice(&devices)?;
    println!(
        "\nselected: {} (id {}, node {})\n",
        chosen.description, chosen.id, chosen.node_name
    );

    // 2. Calibrate against the chosen device.
    let outcome = calibrate_chosen(pw, chosen.id, cfg).await?;

    // 3. Choose a capture preset, then how the transcript is delivered.
    let preset = prompt_preset()?;
    let output_choice = prompt_output_choice(preset)?;

    // Resolve the target profile name now (the only thing `--profile`
    // and the name prompt feed) so we can refuse *before* mutating any
    // PipeWire state if the profile's transcription backend is not
    // compiled into this build. Configuring a profile that records but
    // can never transcribe is exactly the silent failure we prevent.
    let profile_name = match &args.profile {
        Some(n) => n.clone(),
        None => prompt_profile_name(preset.default_profile_name())?,
    };
    ensure_backend_compiled(&profile_name)?;

    // 4. Confirm, then apply side effects.
    if !prompt_confirm(
        &format!(
            "Apply volume {} to {} and continue?",
            format_percent(outcome.final_volume),
            chosen.description
        ),
        true,
    )? {
        println!("aborted — no profile written. The PipeWire volume set during calibration stays.");
        return Ok(());
    }

    // set-default mutates global state → only with an explicit yes/no.
    let made_default = prompt_confirm(
        "Make this the system default source? (fixes dictation + the daemon + every app)",
        true,
    )?;
    if made_default {
        pw.set_default(chosen.id).map_err(setup_err)?;
        println!("set default source to id {}.", chosen.id);
    }

    // 5. Resolve + write the target profile's `[sources]`, then layer the
    //    chosen delivery output(s) on top of its existing `file` output.
    let (profile, _sources_path) = resolve_and_write_profile(&profile_name, chosen, preset)?;
    let profile_path = write_outputs(&profile, output_choice)?;

    // 6. Summary.
    let plan = WizardPlan {
        id: chosen.id,
        description: chosen.description.clone(),
        node_name: chosen.node_name.clone(),
        final_volume: outcome.final_volume,
        made_default,
        preset,
        output_choice,
        profile,
        profile_path,
    };
    println!("\n{}", render_summary(&plan));
    Ok(())
}

/// Run the calibration flow against an already-chosen device: noise
/// floor, the speak window, then the apply/re-measure loop. Reuses the
/// exact Phase 2 helpers ([`capture_window`], [`apply_loop`],
/// [`recommend_volume`], [`noise_floor_warning`]) so the wizard and
/// `audio calibrate` share one implementation of the math and the loop.
async fn calibrate_chosen(
    pw: &dyn PipewireControl,
    id: u32,
    cfg: &SetupConfig,
) -> color_eyre::Result<ApplyOutcome> {
    // Noise floor — measured while the user is quiet, before any prompt.
    println!(
        "step 1/2 — measuring noise floor; stay quiet for {:.1}s …",
        cfg.noise_floor_seconds
    );
    let floor = capture_window(id, cfg.noise_floor_seconds, cfg).await?;
    println!(
        "noise floor: peak {} RMS {}",
        format_dbfs(floor.peak_db),
        format_dbfs(floor.rms_db)
    );
    if let Some(warning) = noise_floor_warning(floor.peak_db, cfg) {
        println!("  {warning}");
    }

    // A brief live meter so the user sees the bar move before the timed
    // capture — reuses the same pw-cat + decode + render machinery as
    // `audio meter`, just bounded to a few seconds instead of Ctrl+C.
    println!("\nstep 2/2 — speak normally; watch the level, then we calibrate.");
    live_meter(id, cfg.speech_seconds, cfg).await?;

    // First speech measurement at the current volume, then the loop.
    prompt_speak(cfg.speech_seconds);
    let speech = capture_window(id, cfg.speech_seconds, cfg).await?;
    println!(
        "speech level: peak {} RMS {}",
        format_dbfs(speech.peak_db),
        format_dbfs(speech.rms_db)
    );

    let current = current_linear(pw, id);
    let recommended = recommend_volume(current, speech.peak_db, cfg);
    println!(
        "current volume {}, recommended {} (target peak {}). Calibrating …",
        format_percent(current),
        format_percent(recommended),
        format_dbfs(cfg.target_peak_db)
    );

    apply_loop(pw, id, current, speech, cfg).await
}

/// Resolve the target profile (from `--profile` or a prompt) and write
/// the chosen mic + preset into it. If the name is not already a
/// user-override profile, clone the base `default` profile to it first
/// (so the wizard can start from a shipped/embedded base), then
/// `update_sources`. Returns `(name, path)` for the summary.
fn resolve_and_write_profile(
    name: &str,
    device: &AudioDevice,
    preset: Preset,
) -> color_eyre::Result<(String, std::path::PathBuf)> {
    let name = name.to_owned();

    let update = SourcesUpdate {
        mic: Some(device.node_name.as_str()),
        system_output: Some(preset.system_output()),
        mode: Some(Mode::MonoMix),
        input_gain_db: Some(None),
    };

    // First attempt the in-place update. A NotFound means there is no
    // user-override file yet → clone the base `default` profile to this
    // name, then retry the update once.
    match update_sources(&name, &update) {
        Ok(path) => Ok((name, path)),
        Err(ProfileError::NotFound { .. }) => {
            println!(
                "profile {name:?} is not a user override yet — cloning the base `default` profile."
            );
            clone_base_profile(&name)?;
            let path = update_sources(&name, &update).map_err(|e| eyre!("{e}"))?;
            Ok((name, path))
        }
        Err(e) => Err(eyre!("{e}")),
    }
}

/// The base profile a brand-new wizard profile is cloned from. Kept in
/// one place so [`ensure_backend_compiled`] resolves the same backend
/// the clone in [`resolve_and_write_profile`] would inherit.
const BASE_PROFILE: &str = "default";

/// Refuse to configure a profile whose transcription backend is not
/// compiled into this build (decision: hard-fail, no silent fallback).
///
/// The wizard only ever rewrites `[sources]` and `[[output]]`; the
/// transcription backend is inherited — from the profile being edited
/// when it already exists as a user override, otherwise from the
/// [`BASE_PROFILE`] that [`resolve_and_write_profile`] clones. Either
/// way, if that backend cannot run we stop *before* any PipeWire
/// mutation with an actionable rebuild hint, instead of leaving the user
/// a profile that records but never transcribes (the bug this guards).
fn ensure_backend_compiled(name: &str) -> color_eyre::Result<()> {
    let backend = match load(name) {
        Ok(profile) => profile.transcription.backend,
        Err(ProfileError::NotFound { .. }) => {
            load(BASE_PROFILE)
                .map_err(|e| eyre!("{e}"))?
                .transcription
                .backend
        }
        Err(e) => return Err(eyre!("{e}")),
    };

    if backend.is_compiled_in() {
        return Ok(());
    }

    Err(backend_unavailable_error(name, backend))
}

/// Build the hard-fail report for a profile whose backend cannot run in
/// this build. Pure (no I/O) so the message — the user's only guidance
/// out of the silent-failure — is unit-tested. Distinguishes a
/// feature-gated backend (actionable rebuild hint) from a reserved id
/// that has no implementation at all.
fn backend_unavailable_error(name: &str, backend: Backend) -> color_eyre::Report {
    let detail = match backend.required_feature() {
        Some(feature) => format!(
            "the `{}` backend is not compiled into this zwhisper build. Rebuild with \
             `cargo build --release --features {feature} -p zwhisper-cli -p zwhisperd` (or edit \
             the profile to use a backend this build supports) before configuring it",
            backend.as_str(),
        ),
        None => format!(
            "the `{}` backend is not implemented in this build",
            backend.as_str(),
        ),
    };

    eyre!(
        "profile {name:?} uses {detail}.\n\
         Run `zwhisper backend list` to see which backends this build supports."
    )
}

/// Clone the shipped/embedded `default` profile into a user override
/// named `name`. Translates the two relevant typed errors into
/// actionable hints: an existing file we somehow could not update
/// (`OverwriteRefused`) and a bad destination name (`InvalidName`).
fn clone_base_profile(name: &str) -> color_eyre::Result<()> {
    match clone_to_user(BASE_PROFILE, name) {
        Ok(path) => {
            info!(profile = name, path = %path.display(), "cloned base profile for wizard");
            println!("created user override {name:?} ({}).", path.display());
            Ok(())
        }
        Err(ProfileError::OverwriteRefused { path }) => Err(eyre!(
            "a profile file already exists at {} but is not a valid user override; \
             remove it or pick another name with --profile.",
            path.display()
        )),
        Err(ProfileError::InvalidName { name }) => Err(eyre!(
            "invalid profile name {name:?}: only [A-Za-z0-9._-]+ is allowed."
        )),
        Err(e) => Err(eyre!("{e}")),
    }
}

/// Layer the chosen transcript-delivery output onto the profile that
/// `resolve_and_write_profile` just wrote. The profile already declares a
/// `file` output (every shipped/base profile does); we keep all its
/// existing outputs and append `choice.live()` when the user asked for a
/// live delivery and it is not already present (idempotent — re-running the
/// wizard with the same choice does not duplicate the table). For
/// [`OutputChoice::FileOnly`] the existing outputs are written back
/// unchanged, which is a no-op on disk. Returns the (unchanged) profile
/// path (the same user-override file) so the summary can report it.
fn write_outputs(name: &str, choice: OutputChoice) -> color_eyre::Result<std::path::PathBuf> {
    // Start from the freshly-written profile's resolved outputs so we
    // preserve whatever `file` (and any other) destinations it declares.
    let mut outputs = load(name).map_err(|e| eyre!("{e}"))?.outputs;

    // Append the live delivery if requested and not already present.
    if let Some(live) = choice.live() {
        if !outputs.contains(&live) {
            outputs.push(live);
        }
    }

    let path = set_outputs(name, &outputs).map_err(|e| eyre!("{e}"))?;
    info!(profile = name, choice = ?choice, path = %path.display(), "wrote outputs to profile");
    Ok(path)
}

// ===========================================================================
// pw-cat capture (CLI-owned child process)
// ===========================================================================

/// Capture a fixed-duration mono `f32` window from `pw-cat` and analyse
/// it. The capture is bounded three ways (RFC security invariants):
///   * **duration** — the child is killed after `seconds`;
///   * **bytes** — the accumulated stdout is capped at
///     `cfg.measure_stdout_cap_bytes`;
///   * **time** — the whole operation is wrapped in a
///     `cfg.pw_cat_timeout_secs` hard timeout so a child that never
///     produces data (or never dies) cannot hang the wizard.
async fn capture_window(
    id: u32,
    seconds: f32,
    cfg: &SetupConfig,
) -> color_eyre::Result<LevelStats> {
    let samples = capture_pcm(id, seconds, cfg).await?;
    Ok(analyze(&samples))
}

/// Spawn `pw-cat`, read its stdout for `seconds` (or until the byte cap),
/// kill + reap the child, and decode the bytes into `f32` samples. All
/// failure modes are typed errors — never a panic on spawn / EOF.
async fn capture_pcm(id: u32, seconds: f32, cfg: &SetupConfig) -> color_eyre::Result<Vec<f32>> {
    // The hard ceiling for the entire capture: the configured timeout, but
    // never shorter than the requested window plus a small grace for the
    // child to flush and exit after we stop reading.
    let grace = Duration::from_millis(500);
    let window = Duration::from_secs_f32(seconds);
    let hard = Duration::from_secs(cfg.pw_cat_timeout_secs).max(window + grace);

    let result = tokio::time::timeout(hard, read_pcm(id, window, cfg)).await;
    match result {
        Ok(inner) => inner,
        Err(_elapsed) => Err(setup_err(SetupError::Timeout {
            seconds: cfg.pw_cat_timeout_secs,
        })),
    }
}

/// The inner capture (wrapped in a timeout by [`capture_pcm`]): spawn the
/// child, read until the window elapses or the byte cap is reached, then
/// kill and reap it.
async fn read_pcm(id: u32, window: Duration, cfg: &SetupConfig) -> color_eyre::Result<Vec<f32>> {
    let mut child = spawn_pw_cat(id, cfg).wrap_err("failed to start pw-cat for capture")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("pw-cat produced no stdout pipe"))?;

    let mut raw: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; cfg.meter_stdout_chunk_bytes];
    let deadline = tokio::time::Instant::now() + window;

    loop {
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            () = &mut sleep => break,
            read = stdout.read(&mut buf) => {
                match read {
                    Ok(0) => break, // pw-cat exited early
                    Ok(n) => {
                        // Enforce the byte cap: take only what fits, then stop.
                        let remaining = cfg.measure_stdout_cap_bytes.saturating_sub(raw.len());
                        if remaining == 0 {
                            break;
                        }
                        let take = n.min(remaining);
                        raw.extend_from_slice(&buf[..take]);
                        if raw.len() >= cfg.measure_stdout_cap_bytes {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(eyre!("error reading pw-cat stdout: {e}"));
                    }
                }
            }
        }
    }

    let _ = child.start_kill();
    let _ = child.wait().await;

    // Decode whole f32 frames; a trailing partial frame (from the kill
    // mid-sample) is dropped — it is at most 3 bytes and never matters.
    let mut carry = Vec::new();
    Ok(decode_f32_le(&mut carry, &raw))
}

/// Spawn `pw-cat` for raw mono `f32` capture of one target id. Every
/// argument is a separate `.arg(...)` — no shell, no interpolation — and
/// the id is rendered numerically, so a selector can never inject a flag
/// or a shell metacharacter (RFC security invariant #1).
fn spawn_pw_cat(id: u32, cfg: &SetupConfig) -> std::io::Result<tokio::process::Child> {
    let args = pw_cat_args(id, cfg.meter_rate_hz);
    Command::new("pw-cat")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

/// Build the `pw-cat` argv for raw mono `f32` capture at `rate` Hz from
/// the given numeric id. Pulled out (and the rate/format pinned) so the
/// invocation is unit-testable and the format assumptions are explicit:
/// `pw-cat`'s defaults are s16 / 48 kHz / 2ch, so format, channels, and
/// rate are all passed explicitly.
fn pw_cat_args(id: u32, rate_hz: u32) -> Vec<String> {
    vec![
        "--record".to_owned(),
        "--raw".to_owned(),
        "--format=f32".to_owned(),
        "--channels=1".to_owned(),
        format!("--rate={rate_hz}"),
        "--target".to_owned(),
        id.to_string(),
        "-".to_owned(),
    ]
}

// ===========================================================================
// shared helpers
// ===========================================================================

/// Resolve a `--source` selector to a numeric id by enumerating devices
/// first (so a `node.name` can be looked up and a `default` resolved).
/// `None` means "the default source".
fn resolve_target(pw: &dyn PipewireControl, source: Option<&str>) -> color_eyre::Result<u32> {
    let rows = enumerate(pw)?;
    resolve_selector(source.unwrap_or("default"), &rows).map_err(setup_err)
}

/// Decode a little-endian `f32` PCM byte slice, carrying any 1-3 trailing
/// bytes that split an `f32` across read-chunk boundaries into the next
/// call. `carry` is updated in place so the caller can stream chunks. A
/// non-finite decoded sample is passed through unchanged — `analyze`
/// already guards against `NaN`/`inf` poisoning its result.
fn decode_f32_le(carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<f32> {
    // Concatenate the carry with the new chunk, decode whole frames, and
    // stash the remainder back into `carry`.
    let mut bytes = std::mem::take(carry);
    bytes.extend_from_slice(chunk);

    let whole = bytes.len() / F32_BYTES;
    let mut samples = Vec::with_capacity(whole);
    for frame in 0..whole {
        let start = frame * F32_BYTES;
        // `start..start + 4` is in bounds because `frame < whole`.
        let arr: [u8; F32_BYTES] = [
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ];
        samples.push(f32::from_le_bytes(arr));
    }

    let consumed = whole * F32_BYTES;
    carry.clear();
    carry.extend_from_slice(&bytes[consumed..]);
    samples
}

/// Prompt the user to speak for the capture window. stdout so it shows in
/// the normal output flow; flushed so it appears before the blocking
/// capture starts.
fn prompt_speak(seconds: f32) {
    print!("speak now for {seconds:.1}s … ");
    let _ = std::io::stdout().flush();
    println!();
}

// ---------------------------------------------------------------------------
// wizard prompt parsing (pure — unit-tested) + thin stdin I/O
// ---------------------------------------------------------------------------

/// Why a menu choice could not be turned into a selection. Kept typed so
/// the prompt loop can render a precise, re-promptable message instead of
/// a generic "bad input".
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuError {
    /// The line was blank and the menu had no default to fall back on.
    EmptyNoDefault,
    /// The line was not a base-10 integer.
    NotANumber,
    /// The number was outside `1..=count`. Carries the count for the
    /// message ("pick 1..N").
    OutOfRange { count: usize },
}

impl MenuError {
    /// A short, user-facing reason for a re-prompt.
    fn reason(&self) -> String {
        match self {
            Self::EmptyNoDefault => "please type a number".to_owned(),
            Self::NotANumber => "that is not a number".to_owned(),
            Self::OutOfRange { count } => format!("pick a number from 1 to {count}"),
        }
    }
}

/// Parse a 1-based menu choice into a 0-based index.
///
/// - A blank line selects `default_index` when one is provided, else
///   [`MenuError::EmptyNoDefault`].
/// - Otherwise the trimmed text must be a base-10 integer in
///   `1..=count`; out-of-range or non-numeric input is a typed error so
///   the caller can re-prompt without exiting.
///
/// Pure (no I/O) so the menu logic is unit-tested directly; the stdin
/// read lives in [`prompt_device_choice`].
fn parse_menu_choice(
    input: &str,
    count: usize,
    default_index: Option<usize>,
) -> Result<usize, MenuError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return default_index.ok_or(MenuError::EmptyNoDefault);
    }
    let n: usize = trimmed.parse().map_err(|_| MenuError::NotANumber)?;
    if n == 0 || n > count {
        return Err(MenuError::OutOfRange { count });
    }
    Ok(n - 1)
}

/// Parse the preset menu choice. `1` (or a blank line — dictation is the
/// default) → [`Preset::Dictation`]; `2` → [`Preset::Meeting`]. Any other
/// input is [`MenuError::OutOfRange`] so the caller re-prompts. Pure.
fn parse_preset_choice(input: &str) -> Result<Preset, MenuError> {
    match parse_menu_choice(input, 2, Some(0))? {
        0 => Ok(Preset::Dictation),
        _ => Ok(Preset::Meeting),
    }
}

/// Parse the output-delivery menu choice into an [`OutputChoice`]. The
/// three rows are fixed: `1` → file only, `2` → file + type at cursor,
/// `3` → file + clipboard. A blank line selects `default` (the
/// preset-derived default the caller marks with `*`). Out-of-range or
/// non-numeric input is a typed [`MenuError`] so the caller re-prompts.
/// Pure (no I/O) so the mapping is unit-tested directly.
fn parse_output_choice(input: &str, default: OutputChoice) -> Result<OutputChoice, MenuError> {
    let default_index = match default {
        OutputChoice::FileOnly => 0,
        OutputChoice::FileAndType => 1,
        OutputChoice::FileAndClipboard => 2,
    };
    match parse_menu_choice(input, 3, Some(default_index))? {
        0 => Ok(OutputChoice::FileOnly),
        1 => Ok(OutputChoice::FileAndType),
        _ => Ok(OutputChoice::FileAndClipboard),
    }
}

/// Parse a yes/no answer, falling back to `default` on a blank line.
/// Accepts `y`/`yes`/`n`/`no` (case-insensitive); anything else returns
/// the default rather than erroring — a wizard confirm should not abort
/// on a typo, and the conservative default (passed by the caller) keeps
/// the global `set-default` opt-in safe. Pure.
fn parse_yes_no(input: &str, default: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

/// Build the warning shown when the idle noise floor is above the
/// configured ceiling (the ALC1220 broadband-noise signature). Returns
/// `None` when the floor is acceptable. Pure so the predicate is tested
/// without running a capture; reused by `calibrate` and the wizard.
fn noise_floor_warning(floor_peak_db: f32, cfg: &SetupConfig) -> Option<String> {
    if floor_peak_db > cfg.idle_floor_max_db {
        Some(format!(
            "warning: idle floor {} is above {} — a high broadband floor \
             (e.g. ALC1220) limits usable headroom; consider a lower --max-volume.",
            format_dbfs(floor_peak_db),
            format_dbfs(cfg.idle_floor_max_db)
        ))
    } else {
        None
    }
}

/// Render the numbered source-selection menu as a string (one row per
/// source: `N) description  [DEFAULT] [monitor]  volume%`). `default_idx`
/// is the 0-based row marked as the empty-input default. Pure so the
/// table formatting is unit-tested without a terminal.
fn render_device_menu(devices: &[AudioDevice], default_idx: Option<usize>) -> String {
    let mut out = String::from("Pick a microphone:\n");
    for (i, d) in devices.iter().enumerate() {
        let n = i + 1;
        let default = if d.is_default { " [DEFAULT]" } else { "" };
        let monitor = if d.is_monitor { " [monitor]" } else { "" };
        let volume = d
            .volume
            .map_or_else(|| "-".to_owned(), |v| format_percent(v.linear));
        let star = if Some(i) == default_idx { "*" } else { " " };
        out.push_str(&format!(
            "{star}{n}) {desc}{default}{monitor}  ({volume})\n",
            desc = d.description
        ));
    }
    out
}

/// Render the wizard's closing plain-language summary (RFC step f).
/// Pure (plan in, `String` out) so the wording is unit-tested. Mentions
/// that dictation reads `pactl get-default-source`, so the set-default
/// step is what makes `zwhisper-dictate` pick this mic.
fn render_summary(plan: &WizardPlan) -> String {
    let default_line = if plan.made_default {
        format!(
            "  default source : yes (id {}) — `zwhisper-dictate` reads \
             `pactl get-default-source`, so it now uses this mic.",
            plan.id
        )
    } else {
        "  default source : no (left unchanged) — to make dictation use this \
         mic, re-run and confirm \"make default\", or set ZWHISPER_DICTATE_SOURCE."
            .to_owned()
    };
    let delivery_line = match plan.output_choice {
        OutputChoice::FileOnly => {
            "  transcript     : written to the profile's transcript file.".to_owned()
        }
        OutputChoice::FileAndType => {
            "  transcript     : written to the profile's transcript file and typed at the \
             cursor (wlroots only; falls back to the clipboard elsewhere)."
                .to_owned()
        }
        OutputChoice::FileAndClipboard => {
            "  transcript     : written to the profile's transcript file and copied to the \
             clipboard."
                .to_owned()
        }
    };
    format!(
        "Setup complete.\n  microphone     : {desc} (node {node})\n  PipeWire volume: {vol}\n{default_line}\n  preset         : {preset}\n  delivery       : {delivery}\n{delivery_line}\n  profile written: {profile} ({path})",
        desc = plan.description,
        node = plan.node_name,
        vol = format_percent(plan.final_volume),
        preset = plan.preset.label(),
        delivery = plan.output_choice.label(),
        profile = plan.profile,
        path = plan.profile_path.display(),
    )
}

/// Read one line from stdin, returning a typed error on EOF (a closed
/// stdin means the wizard cannot prompt — it is not interactive). Trims
/// the trailing newline only; the callers trim further as needed.
fn read_line() -> color_eyre::Result<String> {
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .wrap_err("failed to read from stdin")?;
    if n == 0 {
        return Err(eyre!(
            "reached end of input while waiting for a response — \
             `zwhisper audio setup` needs an interactive terminal."
        ));
    }
    // Drop the trailing newline (and a CR on Windows-style input).
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(line)
}

/// Print the numbered source menu and read a choice, re-prompting on bad
/// input. The default source is the empty-input default. Returns the
/// chosen [`AudioDevice`]. The only I/O here is the menu print + line
/// read + re-prompt; the parsing is the pure [`parse_menu_choice`].
fn prompt_device_choice(devices: &[AudioDevice]) -> color_eyre::Result<&AudioDevice> {
    let default_idx = devices.iter().position(|d| d.is_default);
    print!("{}", render_device_menu(devices, default_idx));
    loop {
        match default_idx {
            Some(i) => print!("choice [default {}]: ", i + 1),
            None => print!("choice: "),
        }
        let _ = std::io::stdout().flush();
        let line = read_line()?;
        match parse_menu_choice(&line, devices.len(), default_idx) {
            // `idx` is in 0..devices.len() by the parser's contract.
            Ok(idx) => match devices.get(idx) {
                Some(d) => return Ok(d),
                None => println!("  internal: index {idx} out of range; try again."),
            },
            Err(e) => println!("  {} — try again.", e.reason()),
        }
    }
}

/// Print the preset menu and read a choice, re-prompting on bad input.
/// Dictation is the empty-input default. Pure parsing in
/// [`parse_preset_choice`].
fn prompt_preset() -> color_eyre::Result<Preset> {
    println!("\nChoose a capture preset:");
    println!("* 1) {}", Preset::Dictation.label());
    println!("  2) {}", Preset::Meeting.label());
    loop {
        print!("preset [default 1]: ");
        let _ = std::io::stdout().flush();
        let line = read_line()?;
        match parse_preset_choice(&line) {
            Ok(p) => return Ok(p),
            Err(e) => println!("  {} — try again.", e.reason()),
        }
    }
}

/// Print the transcript-delivery menu and read a choice, re-prompting on
/// bad input. The empty-input default is [`OutputChoice::default_for`] the
/// chosen preset (dictation → type at cursor; meeting → file only). Pure
/// parsing in [`parse_output_choice`].
///
/// When the type-at-cursor option is on offer, an **advisory** note (never
/// a block) flags its requirements: it needs the optional `wtype`
/// dependency and a wlroots compositor (Sway/Hyprland). If the current
/// session looks like a non-wlroots desktop,
/// [`crate::commands::deliver::sink::desktop_hint`]
/// surfaces a reason and we add that typing falls back to the clipboard
/// there. The note is informational only — the choice is still offered.
fn prompt_output_choice(preset: Preset) -> color_eyre::Result<OutputChoice> {
    use crate::commands::deliver::sink;

    let default = OutputChoice::default_for(preset);
    let mark = |c: OutputChoice| if c == default { "*" } else { " " };

    println!("\nWhere should the transcript go?");
    println!(
        "{}1) {}",
        mark(OutputChoice::FileOnly),
        OutputChoice::FileOnly.label()
    );
    println!(
        "{}2) {}",
        mark(OutputChoice::FileAndType),
        OutputChoice::FileAndType.label()
    );
    println!(
        "{}3) {}",
        mark(OutputChoice::FileAndClipboard),
        OutputChoice::FileAndClipboard.label()
    );

    // Advisory for the type-at-cursor option (always shown since it is on
    // the menu) — surface a session-specific hint when we have one.
    let current = std::env::var("XDG_CURRENT_DESKTOP").ok();
    let session = std::env::var("XDG_SESSION_DESKTOP").ok();
    if let Some(hint) = sink::desktop_hint(current.as_deref(), session.as_deref()) {
        println!("  note: {hint}; on those sessions typing falls back to the clipboard.");
    }
    println!(
        "  note: \"type at cursor\" needs the optional `wtype` dependency and a wlroots \
         compositor (Sway/Hyprland)."
    );

    let default_n = match default {
        OutputChoice::FileOnly => 1,
        OutputChoice::FileAndType => 2,
        OutputChoice::FileAndClipboard => 3,
    };
    loop {
        print!("delivery [default {default_n}]: ");
        let _ = std::io::stdout().flush();
        let line = read_line()?;
        match parse_output_choice(&line, default) {
            Ok(c) => return Ok(c),
            Err(e) => println!("  {} — try again.", e.reason()),
        }
    }
}

/// Ask a yes/no question with a default, returning the boolean answer.
/// One line read; parsing is the pure [`parse_yes_no`] (a typo falls back
/// to the conservative default rather than re-prompting).
fn prompt_confirm(question: &str, default: bool) -> color_eyre::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint}: ");
    let _ = std::io::stdout().flush();
    let line = read_line()?;
    Ok(parse_yes_no(&line, default))
}

/// Prompt for the target profile name, defaulting to `suggestion` on a
/// blank line. A single read — name validation happens downstream in
/// `update_sources` / `clone_to_user` (which own the `[A-Za-z0-9._-]+`
/// allow-list), so the wizard does not duplicate it.
fn prompt_profile_name(suggestion: &str) -> color_eyre::Result<String> {
    print!("Profile name to write [{suggestion}]: ");
    let _ = std::io::stdout().flush();
    let line = read_line()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(suggestion.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

/// Render an ASCII VU bar of `width` characters for a dBFS peak value.
///
/// The bar maps the usable range `[FLOOR_DB, 0]` linearly onto the width:
/// `0 dBFS` fills the whole bar, the floor (or quieter) leaves it empty.
/// Filled cells are `#`, empty cells `-`. Kept pure (dBFS in, `String`
/// out) so it is unit-testable without a terminal.
fn render_vu_bar(peak_db: f32, width: usize) -> String {
    // Display floor for the bar (not the silence sentinel): −60 dBFS is a
    // sensible bottom for a speech VU so quiet rooms still show movement.
    const FLOOR_DB: f32 = -60.0;
    if width == 0 {
        return String::new();
    }
    let clamped = peak_db.clamp(FLOOR_DB, 0.0);
    let fraction = (clamped - FLOOR_DB) / (0.0 - FLOOR_DB); // 0.0..=1.0
    let filled = (fraction * width as f32).round() as usize;
    let filled = filled.min(width);
    let mut bar = String::with_capacity(width);
    for _ in 0..filled {
        bar.push('#');
    }
    for _ in filled..width {
        bar.push('-');
    }
    bar
}

/// Format a linear volume (`0.0..`) as a whole-number percentage, e.g.
/// `0.25` → `25%`. Used in both the device table and the calibration
/// report. A non-finite input renders as `?%` rather than `NaN%`.
fn format_percent(linear: f32) -> String {
    if !linear.is_finite() {
        return "?%".to_owned();
    }
    let pct = (linear * 100.0).round() as i64;
    format!("{pct}%")
}

/// Format a dBFS value for display, e.g. `−7.5 dBFS`. The silence floor
/// and any non-finite value render as `-inf dBFS` so a quiet capture
/// reads naturally instead of showing a large negative sentinel.
fn format_dbfs(db: f32) -> String {
    use zwhisper_core::setup::config::SILENCE_FLOOR_DB;
    if !db.is_finite() || db <= SILENCE_FLOOR_DB {
        return "-inf dBFS".to_owned();
    }
    format!("{db:.1} dBFS")
}

/// Format a tolerance / delta in dB, e.g. `±1.5 dB`.
fn format_db_delta(db: f32) -> String {
    format!("±{db:.1} dB")
}

/// A short clip indicator for the meter: `CLIP!` once the peak reaches
/// 0 dBFS (full scale), otherwise empty. Helps the user back off the gain
/// during manual `audio meter` fine-tuning.
fn clip_indicator(peak_db: f32) -> &'static str {
    if peak_db.is_finite() && peak_db >= 0.0 {
        "CLIP!"
    } else {
        ""
    }
}

/// Map a [`SetupError`] into a `color_eyre::Report` (mirrors
/// `transcribe`'s `eyre!("{err}")` and `profile`'s `eyre_from`). The
/// typed variant's `Display` already carries an actionable message.
fn setup_err(err: SetupError) -> color_eyre::Report {
    eyre!("{err}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // ---- backend-compiled guard messaging ----------------------------

    #[test]
    fn backend_unavailable_error_for_parakeet_names_the_feature() {
        let msg = backend_unavailable_error("dictation", Backend::Parakeet).to_string();
        assert!(msg.contains("dictation"), "names the profile: {msg}");
        assert!(msg.contains("parakeet"), "names the backend: {msg}");
        assert!(
            msg.contains("--features parakeet"),
            "gives the rebuild flag: {msg}"
        );
        assert!(
            msg.contains("zwhisper backend list"),
            "points at the discovery command: {msg}"
        );
    }

    #[test]
    fn backend_unavailable_error_for_unimplemented_has_no_rebuild_hint() {
        let msg = backend_unavailable_error("x", Backend::OpenAi).to_string();
        assert!(msg.contains("not implemented"), "{msg}");
        assert!(
            !msg.contains("--features"),
            "no rebuild hint when no feature: {msg}"
        );
    }

    // ---- pw-cat argv (shell-free, explicit format) -------------------

    #[test]
    fn pw_cat_args_are_explicit_and_numeric() {
        let args = pw_cat_args(68, 16_000);
        assert_eq!(
            args,
            vec![
                "--record",
                "--raw",
                "--format=f32",
                "--channels=1",
                "--rate=16000",
                "--target",
                "68",
                "-",
            ]
        );
    }

    #[test]
    fn pw_cat_args_render_id_numerically() {
        // The id is rendered with `to_string()` — a u32 can never be a
        // flag or a shell metacharacter, so `--target` is injection-safe.
        let args = pw_cat_args(4_294_967_295, 48_000);
        assert!(args.contains(&"4294967295".to_owned()));
        assert!(args.contains(&"--rate=48000".to_owned()));
    }

    // ---- f32 LE decoding + carry -------------------------------------

    #[test]
    fn decode_f32_le_decodes_whole_frames() {
        let mut carry = Vec::new();
        let mut bytes = Vec::new();
        for s in [0.0_f32, 1.0, -1.0, 0.5] {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let samples = decode_f32_le(&mut carry, &bytes);
        assert_eq!(samples, vec![0.0, 1.0, -1.0, 0.5]);
        assert!(carry.is_empty(), "no leftover for an aligned buffer");
    }

    #[test]
    fn decode_f32_le_carries_partial_frame_across_chunks() {
        let value = 0.75_f32;
        let raw = value.to_le_bytes();
        let mut carry = Vec::new();

        // First chunk: only 3 of the 4 bytes — nothing decodes yet.
        let first = decode_f32_le(&mut carry, &raw[..3]);
        assert!(first.is_empty());
        assert_eq!(carry.len(), 3);

        // Second chunk: the final byte completes the frame.
        let second = decode_f32_le(&mut carry, &raw[3..]);
        assert_eq!(second, vec![value]);
        assert!(carry.is_empty());
    }

    #[test]
    fn decode_f32_le_handles_empty_input() {
        let mut carry = Vec::new();
        assert!(decode_f32_le(&mut carry, &[]).is_empty());
        assert!(carry.is_empty());
    }

    // ---- VU bar rendering --------------------------------------------

    #[test]
    fn vu_bar_full_scale_fills_completely() {
        let bar = render_vu_bar(0.0, 10);
        assert_eq!(bar, "##########");
    }

    #[test]
    fn vu_bar_floor_is_empty() {
        let bar = render_vu_bar(-60.0, 10);
        assert_eq!(bar, "----------");
        // Anything below the display floor also clamps to empty.
        assert_eq!(render_vu_bar(-120.0, 10), "----------");
    }

    #[test]
    fn vu_bar_half_scale_is_about_half_filled() {
        // −30 dBFS is the midpoint of the [−60, 0] display range.
        let bar = render_vu_bar(-30.0, 10);
        let filled = bar.chars().filter(|&c| c == '#').count();
        assert_eq!(filled, 5, "bar was {bar:?}");
        assert_eq!(bar.len(), 10);
    }

    #[test]
    fn vu_bar_zero_width_is_empty_string() {
        assert_eq!(render_vu_bar(-10.0, 0), "");
    }

    #[test]
    fn vu_bar_never_exceeds_width_for_boosted_peak() {
        // A > 0 dBFS reading (boosted / clipped) must not overflow the bar.
        let bar = render_vu_bar(6.0, 8);
        assert_eq!(bar, "########");
    }

    // ---- percent + dBFS formatting -----------------------------------

    #[test]
    fn format_percent_rounds_to_whole_numbers() {
        assert_eq!(format_percent(0.25), "25%");
        assert_eq!(format_percent(1.0), "100%");
        assert_eq!(format_percent(0.0), "0%");
        assert_eq!(format_percent(0.286), "29%");
    }

    #[test]
    fn format_percent_handles_non_finite() {
        assert_eq!(format_percent(f32::NAN), "?%");
        assert_eq!(format_percent(f32::INFINITY), "?%");
    }

    #[test]
    fn format_dbfs_renders_one_decimal() {
        assert_eq!(format_dbfs(-7.5), "-7.5 dBFS");
        assert_eq!(format_dbfs(-12.34), "-12.3 dBFS");
    }

    #[test]
    fn format_dbfs_floor_and_non_finite_render_as_inf() {
        use zwhisper_core::setup::config::SILENCE_FLOOR_DB;
        assert_eq!(format_dbfs(SILENCE_FLOOR_DB), "-inf dBFS");
        assert_eq!(format_dbfs(f32::NEG_INFINITY), "-inf dBFS");
        assert_eq!(format_dbfs(f32::NAN), "-inf dBFS");
    }

    #[test]
    fn format_db_delta_prefixes_plus_minus() {
        assert_eq!(format_db_delta(1.5), "±1.5 dB");
    }

    // ---- clip indicator ----------------------------------------------

    #[test]
    fn clip_indicator_fires_only_at_or_above_full_scale() {
        assert_eq!(clip_indicator(-0.1), "");
        assert_eq!(clip_indicator(0.0), "CLIP!");
        assert_eq!(clip_indicator(3.0), "CLIP!");
        assert_eq!(clip_indicator(f32::NEG_INFINITY), "");
    }

    // ============================================================
    // Wave 3 — interactive `audio setup` wizard. Only the pure
    // logic is tested here (menu parsing, preset → SourcesUpdate
    // mapping, the summary formatter, the noise-floor predicate);
    // the stdin loop itself needs a TTY + hardware and is verified
    // manually, with the I/O kept deliberately thin.
    // ============================================================

    fn device(id: u32, node: &str, desc: &str, is_default: bool) -> AudioDevice {
        AudioDevice {
            id,
            node_name: node.to_owned(),
            description: desc.to_owned(),
            is_source: true,
            is_monitor: false,
            is_default,
            volume: Some(Volume {
                linear: 0.25,
                muted: false,
            }),
        }
    }

    // ---- parse_menu_choice -------------------------------------------

    #[test]
    fn menu_choice_parses_one_based_index() {
        assert_eq!(parse_menu_choice("1", 3, None), Ok(0));
        assert_eq!(parse_menu_choice("3", 3, None), Ok(2));
        // Surrounding whitespace is tolerated.
        assert_eq!(parse_menu_choice("  2 \n", 3, None), Ok(1));
    }

    #[test]
    fn menu_choice_empty_uses_default_when_present() {
        assert_eq!(parse_menu_choice("", 3, Some(1)), Ok(1));
        assert_eq!(parse_menu_choice("   ", 3, Some(0)), Ok(0));
    }

    #[test]
    fn menu_choice_empty_without_default_is_error() {
        assert_eq!(
            parse_menu_choice("", 3, None),
            Err(MenuError::EmptyNoDefault)
        );
    }

    #[test]
    fn menu_choice_rejects_non_numeric() {
        assert_eq!(
            parse_menu_choice("abc", 3, None),
            Err(MenuError::NotANumber)
        );
        assert_eq!(parse_menu_choice("1x", 3, None), Err(MenuError::NotANumber));
    }

    #[test]
    fn menu_choice_rejects_out_of_range_including_zero() {
        assert_eq!(
            parse_menu_choice("0", 3, None),
            Err(MenuError::OutOfRange { count: 3 })
        );
        assert_eq!(
            parse_menu_choice("4", 3, None),
            Err(MenuError::OutOfRange { count: 3 })
        );
    }

    #[test]
    fn menu_choice_index_is_always_in_bounds() {
        // The parser's contract is the load-bearing invariant for the
        // `devices.get(idx)` lookup in `prompt_device_choice`.
        let count = 5;
        for raw in ["1", "5", "", "3"] {
            if let Ok(idx) = parse_menu_choice(raw, count, Some(0)) {
                assert!(idx < count, "idx {idx} must be < {count} for {raw:?}");
            }
        }
    }

    // ---- parse_preset_choice -----------------------------------------

    #[test]
    fn preset_choice_maps_numbers_and_default() {
        assert_eq!(parse_preset_choice("1"), Ok(Preset::Dictation));
        assert_eq!(parse_preset_choice("2"), Ok(Preset::Meeting));
        // Blank line → dictation (the default).
        assert_eq!(parse_preset_choice(""), Ok(Preset::Dictation));
    }

    #[test]
    fn preset_choice_rejects_out_of_range() {
        assert_eq!(
            parse_preset_choice("3"),
            Err(MenuError::OutOfRange { count: 2 })
        );
        assert_eq!(parse_preset_choice("x"), Err(MenuError::NotANumber));
    }

    // ---- preset → sources mapping ------------------------------------

    #[test]
    fn preset_system_output_is_mic_only_or_default() {
        assert_eq!(Preset::Dictation.system_output(), "");
        assert_eq!(Preset::Meeting.system_output(), "default");
    }

    #[test]
    fn preset_maps_to_expected_sources_update() {
        // The wizard always writes mono_mix, drops input_gain_db, sets the
        // concrete mic node, and the preset's system_output marker.
        for (preset, expected_out) in [(Preset::Dictation, ""), (Preset::Meeting, "default")] {
            let update = SourcesUpdate {
                mic: Some("alsa_input.pci-0000_00_1f.3.analog-stereo"),
                system_output: Some(preset.system_output()),
                mode: Some(Mode::MonoMix),
                input_gain_db: Some(None),
            };
            assert_eq!(update.system_output, Some(expected_out));
            assert_eq!(update.mode, Some(Mode::MonoMix));
            assert_eq!(update.input_gain_db, Some(None));
            assert_eq!(
                update.mic,
                Some("alsa_input.pci-0000_00_1f.3.analog-stereo")
            );
        }
    }

    #[test]
    fn preset_default_profile_names_match_shipped() {
        assert_eq!(Preset::Dictation.default_profile_name(), "dictation");
        assert_eq!(Preset::Meeting.default_profile_name(), "meeting");
    }

    // ---- parse_yes_no ------------------------------------------------

    #[test]
    fn yes_no_accepts_common_forms_case_insensitively() {
        for s in ["y", "Y", "yes", "YES", "Yes"] {
            assert!(parse_yes_no(s, false), "{s:?} should be yes");
        }
        for s in ["n", "N", "no", "NO", "No"] {
            assert!(!parse_yes_no(s, true), "{s:?} should be no");
        }
    }

    #[test]
    fn yes_no_blank_or_garbage_uses_default() {
        // Conservative: a typo keeps the caller's default (e.g. the
        // safe-by-default for the global set-default prompt).
        assert!(parse_yes_no("", true));
        assert!(!parse_yes_no("", false));
        assert!(!parse_yes_no("maybe", false));
        assert!(parse_yes_no("  yep ", true)); // not recognised → default
    }

    // ---- noise_floor_warning -----------------------------------------

    #[test]
    fn noise_floor_warning_fires_above_ceiling_only() {
        let cfg = SetupConfig::default(); // idle_floor_max_db = -45.0
        // Quiet floor → no warning.
        assert!(noise_floor_warning(-60.0, &cfg).is_none());
        assert!(noise_floor_warning(cfg.idle_floor_max_db, &cfg).is_none());
        // Above the ceiling → a warning that names a lower --max-volume.
        let warn = noise_floor_warning(-30.0, &cfg).expect("should warn");
        assert!(warn.contains("idle floor"), "{warn}");
        assert!(warn.contains("--max-volume"), "{warn}");
    }

    // ---- render_device_menu ------------------------------------------

    #[test]
    fn device_menu_numbers_rows_and_marks_default() {
        let devices = vec![
            device(68, "alsa_input.builtin", "Built-in Mic", true),
            device(70, "alsa_input.usb", "USB Headset", false),
        ];
        let menu = render_device_menu(&devices, Some(0));
        assert!(menu.contains("1) Built-in Mic"), "{menu}");
        assert!(menu.contains("[DEFAULT]"), "{menu}");
        assert!(menu.contains("2) USB Headset"), "{menu}");
        // The default row carries the `*` marker; the other does not.
        assert!(menu.contains("*1)"), "{menu}");
        assert!(menu.contains(" 2)"), "{menu}");
        // Volume is rendered as a percentage from the fixture's 0.25.
        assert!(menu.contains("(25%)"), "{menu}");
    }

    #[test]
    fn device_menu_marks_monitor_sources() {
        let mut mon = device(72, "alsa_output.hdmi.monitor", "HDMI Monitor", false);
        mon.is_monitor = true;
        let menu = render_device_menu(&[mon], None);
        assert!(menu.contains("[monitor]"), "{menu}");
    }

    // ---- render_summary ----------------------------------------------

    fn plan(made_default: bool, preset: Preset) -> WizardPlan {
        plan_with_output(made_default, preset, OutputChoice::default_for(preset))
    }

    fn plan_with_output(
        made_default: bool,
        preset: Preset,
        output_choice: OutputChoice,
    ) -> WizardPlan {
        WizardPlan {
            id: 68,
            description: "Built-in Mic".to_owned(),
            node_name: "alsa_input.builtin".to_owned(),
            final_volume: 0.27,
            made_default,
            preset,
            output_choice,
            profile: "dictation".to_owned(),
            profile_path: std::path::PathBuf::from(
                "/home/u/.config/zwhisper/profiles/dictation.toml",
            ),
        }
    }

    #[test]
    fn summary_reports_all_decisions() {
        let s = render_summary(&plan(true, Preset::Dictation));
        assert!(s.contains("Built-in Mic"), "{s}");
        assert!(s.contains("alsa_input.builtin"), "{s}");
        assert!(s.contains("27%"), "{s}"); // 0.27 → 27%
        assert!(s.contains("dictation (mic only)"), "{s}");
        assert!(s.contains("dictation.toml"), "{s}");
        // Made default → mentions pactl get-default-source + dictation.
        assert!(s.contains("pactl get-default-source"), "{s}");
        assert!(s.contains("yes"), "{s}");
        // Dictation's default delivery is file + type-at-cursor, and the
        // summary states where the transcript goes.
        assert!(s.contains("file + type at cursor"), "{s}");
        assert!(s.contains("typed at the cursor"), "{s}");
    }

    #[test]
    fn summary_states_clipboard_delivery() {
        let s = render_summary(&plan_with_output(
            false,
            Preset::Meeting,
            OutputChoice::FileAndClipboard,
        ));
        assert!(s.contains("file + clipboard"), "{s}");
        assert!(s.contains("copied to the clipboard"), "{s}");
    }

    #[test]
    fn summary_states_file_only_delivery() {
        let s = render_summary(&plan_with_output(
            false,
            Preset::Meeting,
            OutputChoice::FileOnly,
        ));
        assert!(s.contains("file only"), "{s}");
        assert!(s.contains("transcript file"), "{s}");
    }

    // ---- OutputChoice mapping ----------------------------------------

    #[test]
    fn output_choice_default_matches_preset() {
        // Dictation types where the cursor is; meeting just keeps the file.
        assert_eq!(
            OutputChoice::default_for(Preset::Dictation),
            OutputChoice::FileAndType
        );
        assert_eq!(
            OutputChoice::default_for(Preset::Meeting),
            OutputChoice::FileOnly
        );
    }

    #[test]
    fn output_choice_live_maps_to_expected_output() {
        // FileOnly adds no live delivery; the other two map to their
        // concrete OutputDest variant.
        assert_eq!(OutputChoice::FileOnly.live(), None);
        assert_eq!(
            OutputChoice::FileAndType.live(),
            Some(OutputDest::TypeAtCursor)
        );
        assert_eq!(
            OutputChoice::FileAndClipboard.live(),
            Some(OutputDest::Clipboard)
        );
    }

    // ---- parse_output_choice -----------------------------------------

    #[test]
    fn output_choice_parses_numbers() {
        let d = OutputChoice::FileOnly;
        assert_eq!(parse_output_choice("1", d), Ok(OutputChoice::FileOnly));
        assert_eq!(parse_output_choice("2", d), Ok(OutputChoice::FileAndType));
        assert_eq!(
            parse_output_choice("3", d),
            Ok(OutputChoice::FileAndClipboard)
        );
    }

    #[test]
    fn output_choice_blank_uses_preset_default() {
        // A blank line selects whatever default the caller passes (which is
        // OutputChoice::default_for(preset) at the call site).
        assert_eq!(
            parse_output_choice("", OutputChoice::FileAndType),
            Ok(OutputChoice::FileAndType)
        );
        assert_eq!(
            parse_output_choice("  ", OutputChoice::FileOnly),
            Ok(OutputChoice::FileOnly)
        );
        assert_eq!(
            parse_output_choice("", OutputChoice::FileAndClipboard),
            Ok(OutputChoice::FileAndClipboard)
        );
    }

    #[test]
    fn output_choice_rejects_out_of_range_and_non_numeric() {
        let d = OutputChoice::FileOnly;
        assert_eq!(
            parse_output_choice("4", d),
            Err(MenuError::OutOfRange { count: 3 })
        );
        assert_eq!(
            parse_output_choice("0", d),
            Err(MenuError::OutOfRange { count: 3 })
        );
        assert_eq!(parse_output_choice("x", d), Err(MenuError::NotANumber));
    }

    #[test]
    fn summary_when_not_default_explains_how_to_enable() {
        let s = render_summary(&plan(false, Preset::Meeting));
        assert!(s.contains("meeting (mic + system)"), "{s}");
        assert!(s.contains("no (left unchanged)"), "{s}");
        // Points the user at how to make dictation use this mic later.
        assert!(
            s.contains("ZWHISPER_DICTATE_SOURCE") || s.contains("make default"),
            "{s}"
        );
    }
}
