//! M7 — Model downloader tab.
//!
//! One row per model in the embedded `checksums.toml` manifest.
//! Each row carries a status label, a `Progress` widget, and a
//! single action button (Download / Resume / Cancel / Retry).
//!
//! Threading: the FLTK widgets stay on the main thread. Download
//! work runs on the shared tokio runtime via `bridge.rt_handle`.
//! Progress flows back through `bridge.tx` as `UiMessage::Models`,
//! drained by `app::App::run`'s awake_callback dispatcher.
//!
//! See `docs/M7-plan.md` § "Definition of done" #6–#13 and § C3/C4.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fltk::button::Button;
use fltk::enums::{Color, Font, FrameType};
use fltk::frame::Frame;
use fltk::group::{Group, Pack, PackType, Scroll, Tabs};
use fltk::misc::Progress;
use fltk::prelude::*;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::app::UiMessage;
use crate::checksums::{ChecksumManifest, Entry};
use crate::config::ModelsConfig;
use crate::download::{DownloadState, FailReason, ModelDownloader};
use crate::runtime::UiBridge;

/// Vertical pixel size for one model row.
const ROW_HEIGHT: i32 = 28;
/// Horizontal padding inside the tab.
const PADDING: i32 = 8;
/// Width allotted to the action button.
const BUTTON_WIDTH: i32 = 96;
/// Width allotted to the progress widget.
const PROGRESS_WIDTH: i32 = 200;
/// Banner area at the top showing the configured base URL.
const HEADER_HEIGHT: i32 = 36;

/// Minimum interval between UI progress updates per active
/// download. With 4 MiB flush chunks a `large-v3` (3 GB) emits
/// roughly 750 updates without throttling — at 10 Hz we cap the
/// FLTK awake/redraw round-trips to one every 100 ms. The
/// downloader still runs at full I/O speed; only the UI wake
/// rate is bounded.
const PROGRESS_UI_THROTTLE: Duration = Duration::from_millis(100);

/// Cross-thread messages produced by the model downloader.
#[derive(Debug, Clone)]
pub(crate) enum ModelsMsg {
    /// User clicked the action button — UI updates immediately,
    /// the worker spawns its own task. Carried purely for tracing.
    DownloadStarted { model: String },
    /// One progress update from `ModelDownloader::run`.
    Progress {
        model: String,
        bytes_done: u64,
        total: u64,
    },
    /// Download finished successfully.
    Installed { model: String },
    /// Download finished with a failure. `reason` is a human-
    /// readable summary the row label shows verbatim.
    Failed { model: String, reason: String },
    /// User-issued cancellation observed by the worker.
    Cancelled { model: String },
}

/// Holds the FLTK widgets for a single model row.
#[derive(Clone, Debug)]
pub(crate) struct ModelRow {
    /// Static label: model name + size in MB.
    pub(crate) name_label: Frame,
    /// Mutable status text: "installed", "downloading", "failed: …".
    pub(crate) status_label: Frame,
    /// Progress bar. `value() / maximum() == bytes_done / total`.
    pub(crate) progress: Progress,
    /// Action button — toggles between Download / Cancel / Retry.
    pub(crate) action_button: Button,
}

/// Holds the FLTK widgets belonging to the model downloader tab.
#[derive(Clone, Debug)]
pub(crate) struct ModelsTab {
    #[allow(dead_code, reason = "kept alive for the FLTK widget tree")]
    group: Group,
    /// Top-of-tab banner showing the resolved base URL.
    pub(crate) base_url_label: Frame,
    /// Per-model widget handles, keyed by manifest model name
    /// (e.g. "ggml-tiny"). Wrapped in `Arc<Mutex<...>>` so the
    /// dispatcher and per-row callbacks can both reach a row
    /// without cloning every widget upfront.
    pub(crate) rows: Arc<Mutex<HashMap<String, ModelRow>>>,
    /// Per-row cancellation tokens. A `Cancel` button click triggers
    /// `cancel_token.cancel()` for the running download. Kept on the
    /// tab so the cancellation outlives the closure that started it.
    cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

/// Construct the model downloader tab. One row per known model
/// in `ChecksumManifest::embedded()`. The base URL banner
/// reflects whatever `ModelsConfig::load_or_default(None)`
/// returns — settings does not edit `models.toml` in M7 (read-only).
#[allow(
    clippy::needless_pass_by_value,
    reason = "build() takes UiBridge by value to match sibling tabs"
)]
pub(crate) fn build(parent: &mut Tabs, bridge: UiBridge) -> ModelsTab {
    let (gx, gy, gw, gh) = parent.client_area();
    let group = Group::new(gx, gy, gw, gh, "Models");

    let inner_w = gw - PADDING * 2;
    let mut y = gy + PADDING;

    // Header: base URL banner. Loaded from ~/.config/zwhisper/models.toml
    // or falls back to the built-in default. A typed parse error
    // surfaces here in red text — the tab still renders so the
    // user can fix the file and re-open settings.
    let base_url_text = match ModelsConfig::load_or_default(None) {
        Ok(cfg) => format!("Source: {}", cfg.base_url),
        Err(e) => format!("Source: <error: {e}>"),
    };
    let mut base_url_label =
        Frame::new(gx + PADDING, y, inner_w, HEADER_HEIGHT, "");
    base_url_label.set_label(&base_url_text);
    base_url_label.set_label_font(Font::Helvetica);
    base_url_label.set_label_size(11);
    base_url_label.set_frame(FrameType::FlatBox);
    base_url_label.set_align(
        fltk::enums::Align::Left | fltk::enums::Align::Inside,
    );
    y += HEADER_HEIGHT + PADDING;

    // Scrollable list area for model rows. Five known models so
    // the scrollbar is rarely visible, but resizing the window
    // smaller should still work.
    let scroll = Scroll::new(gx + PADDING, y, inner_w, gh - (y - gy) - PADDING, "");
    let mut rows_pack = Pack::new(gx + PADDING, y, inner_w - 16, 0, "");
    rows_pack.set_type(PackType::Vertical);
    rows_pack.set_spacing(4);

    let manifest = ChecksumManifest::embedded();
    let mut rows: HashMap<String, ModelRow> = HashMap::new();
    let cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Resolve `models_dir` once. A failure here is fatal for the
    // tab — silently falling back to an empty path used to make
    // the downloader write to the current working directory. We
    // now hard-fail visibly: the base-URL banner reports the
    // error, the rows are skipped, and Download buttons cannot
    // be clicked.
    let models_dir_result = zwhisper_core::transcribe::models::models_dir();
    let models_dir = match &models_dir_result {
        Ok(dir) => Some(dir.clone()),
        Err(e) => {
            error!(error = %e, "models tab: cannot resolve models_dir; rows disabled");
            base_url_label.set_label(&format!(
                "ERROR: cannot resolve models directory: {e}"
            ));
            base_url_label.set_label_color(Color::Red);
            None
        }
    };

    if let Some(models_dir) = models_dir.as_ref() {
        for model_name in manifest.known_models() {
            let entry = match manifest.lookup(model_name) {
                Some(e) => e.clone(),
                None => continue, // unreachable — known_models() iterates the same map
            };
            let row = build_row(
                inner_w - 16,
                model_name,
                &entry,
                models_dir,
                &bridge,
                Arc::clone(&cancel_tokens),
            );
            rows.insert(model_name.to_owned(), row);
        }
    }
    rows_pack.end();
    scroll.end();

    group.end();
    parent.add(&group);

    ModelsTab {
        group,
        base_url_label,
        rows: Arc::new(Mutex::new(rows)),
        cancel_tokens,
    }
}

/// Build a single model row inside the rows pack.
fn build_row(
    row_w: i32,
    model_name: &str,
    entry: &Entry,
    models_dir: &PathBuf,
    bridge: &UiBridge,
    cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
) -> ModelRow {
    let mut row_pack = Pack::new(0, 0, row_w, ROW_HEIGHT, "");
    row_pack.set_type(PackType::Horizontal);
    row_pack.set_spacing(8);

    // Name + size column (fixed width, left-aligned text).
    let name_w = row_w - PROGRESS_WIDTH - BUTTON_WIDTH - 16 - 8;
    let size_mb = entry.size_bytes / (1024 * 1024);
    let name_text = format!("{model_name} ({size_mb} MB)");
    let mut name_label = Frame::new(0, 0, name_w, ROW_HEIGHT, "");
    name_label.set_label(&name_text);
    name_label.set_label_font(Font::HelveticaBold);
    name_label.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    name_label.set_frame(FrameType::FlatBox);

    // Progress widget (centre).
    let mut progress = Progress::new(0, 0, PROGRESS_WIDTH, ROW_HEIGHT, "");
    progress.set_minimum(0.0);
    progress.set_maximum(entry.size_bytes as f64);
    progress.set_value(0.0);
    progress.set_color(Color::Background2);
    progress.set_selection_color(Color::DarkGreen);

    // Status label between progress and button — shows "installed",
    // "downloading…", "failed", etc. Kept narrow; full failure
    // reasons go to a tracing log + the user reopens settings.
    let mut status_label = Frame::new(0, 0, 16, ROW_HEIGHT, "");
    let final_path = models_dir.join(format!("ggml-{model_name}.bin"));
    let initial_label = if final_path.exists() {
        "installed"
    } else {
        "ready"
    };
    status_label.set_label(initial_label);
    status_label.set_label_font(Font::Helvetica);
    status_label.set_label_size(11);
    status_label.set_frame(FrameType::FlatBox);
    status_label.set_align(
        fltk::enums::Align::Left | fltk::enums::Align::Inside,
    );

    // Action button (right). Default: Download (or "Installed" if
    // present on disk — disabled in that case).
    let initial_button_label = if final_path.exists() {
        "Installed"
    } else {
        "Download"
    };
    let mut action_button =
        Button::new(0, 0, BUTTON_WIDTH, ROW_HEIGHT, initial_button_label);
    if final_path.exists() {
        action_button.deactivate();
    }

    // Wire the Download click. Captures: model name, bridge clone,
    // models_dir, cancel-tokens map.
    let cb_model_name = model_name.to_owned();
    let cb_bridge = bridge.clone();
    let cb_models_dir = models_dir.clone();
    let cb_cancel_tokens = Arc::clone(&cancel_tokens);
    action_button.set_callback(move |btn| {
        // Inspect the button's current label to decide intent.
        let label = btn.label();
        match label.as_str() {
            "Cancel" => {
                if let Ok(tokens) = cb_cancel_tokens.lock() {
                    if let Some(token) = tokens.get(&cb_model_name) {
                        info!(model = %cb_model_name, "user requested cancel");
                        token.cancel();
                    }
                }
                // UI flips to "Cancelled" via the dispatcher.
            }
            _ => {
                // Download / Retry / Resume — all run the same path.
                spawn_download_task(
                    &cb_model_name,
                    &cb_bridge,
                    &cb_models_dir,
                    Arc::clone(&cb_cancel_tokens),
                );
            }
        }
    });

    row_pack.end();

    ModelRow {
        name_label,
        status_label,
        progress,
        action_button,
    }
}

/// Spawn the download state machine on the runtime. Resolves the
/// URL via `ModelsConfig` (re-read each click — cheap), builds a
/// fresh `ModelDownloader`, and forwards every `DownloadState`
/// transition through `bridge.tx` as `UiMessage::Models`.
fn spawn_download_task(
    model_name: &str,
    bridge: &UiBridge,
    models_dir: &PathBuf,
    cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
) {
    let model_name = model_name.to_owned();
    let models_dir = models_dir.clone();
    let bridge_tx = bridge.tx.clone();
    let rt_handle = bridge.rt_handle.clone();

    // Resolve the URL on the calling thread so a config error is
    // surfaced to the user before we spawn anything.
    let cfg = match ModelsConfig::load_or_default(None) {
        Ok(c) => c,
        Err(e) => {
            send_failed(&bridge_tx, &model_name, format!("models.toml: {e}"));
            return;
        }
    };
    let url = match cfg.resolve_url(&model_name) {
        Ok(u) => u,
        Err(e) => {
            send_failed(&bridge_tx, &model_name, format!("url: {e}"));
            return;
        }
    };

    // Per-row cancellation token. Inserted into the map so the
    // Cancel button can reach it.
    let cancel = CancellationToken::new();
    if let Ok(mut tokens) = cancel_tokens.lock() {
        // Replace any prior token for this model — the prior
        // download is either finished or already cancelled.
        tokens.insert(model_name.clone(), cancel.clone());
    }

    let manifest = ChecksumManifest::embedded();
    let mut downloader = match ModelDownloader::new(
        model_name.clone(),
        url,
        manifest,
        models_dir,
        cancel.clone(),
    ) {
        Ok(d) => d,
        Err(e) => {
            send_failed(&bridge_tx, &model_name, e.to_string());
            return;
        }
    };

    // Notify the UI that work has started — the dispatcher flips
    // the button to "Cancel" and the status label to "downloading".
    if let Err(e) = bridge_tx.send(UiMessage::Models(ModelsMsg::DownloadStarted {
        model: model_name.clone(),
    })) {
        warn!(error = %e, "models tab: receiver gone before download start");
        return;
    }
    fltk::app::awake();

    // Spawn the actual work. Internal channel surfaces every
    // `DownloadState` transition; we forward to bridge.tx as
    // `ModelsMsg::Progress` / `Installed` / `Failed` / `Cancelled`.
    let _join = rt_handle.spawn(async move {
        let (state_tx, mut state_rx) = unbounded_channel::<DownloadState>();

        // Drainer: read every state on this same task and forward.
        // Progress events are throttled to PROGRESS_UI_THROTTLE so
        // the FLTK main loop is not woken thousands of times per
        // download (perf review finding 2). The very last Fetching
        // emit (at 100% before Verifying) is forced through so the
        // bar reaches max before transitioning.
        let bridge_tx_drain = bridge_tx.clone();
        let model_name_drain = model_name.clone();
        let drain = tokio::spawn(async move {
            let mut last_progress_emit =
                Instant::now() - PROGRESS_UI_THROTTLE;
            let mut last_progress_state: Option<DownloadState> = None;
            while let Some(state) = state_rx.recv().await {
                if let DownloadState::Fetching { .. } = &state {
                    if last_progress_emit.elapsed() < PROGRESS_UI_THROTTLE {
                        // Coalesce: keep the most recent state to flush
                        // when the throttle window opens or on stream
                        // end.
                        last_progress_state = Some(state);
                        continue;
                    }
                    last_progress_emit = Instant::now();
                    last_progress_state = None;
                }
                forward_state(&bridge_tx_drain, &model_name_drain, state);
            }
            // Stream closed — flush the last coalesced progress so
            // the bar shows 100 % before the terminal-state message
            // arrives.
            if let Some(state) = last_progress_state {
                forward_state(&bridge_tx_drain, &model_name_drain, state);
            }
        });

        let outcome = downloader.run(state_tx).await;
        // Closing the state_tx is implicit when downloader.run drops it;
        // wait for the drainer to finish observing the final transition.
        let _ = drain.await;

        match outcome {
            Ok(DownloadState::Installed) => {
                let _ = bridge_tx.send(UiMessage::Models(ModelsMsg::Installed {
                    model: model_name.clone(),
                }));
            }
            Ok(DownloadState::Cancelled) => {
                let _ = bridge_tx.send(UiMessage::Models(ModelsMsg::Cancelled {
                    model: model_name.clone(),
                }));
            }
            Ok(DownloadState::Failed { reason }) => {
                let _ = bridge_tx.send(UiMessage::Models(ModelsMsg::Failed {
                    model: model_name.clone(),
                    reason: format_fail_reason(&reason),
                }));
            }
            Ok(other) => {
                // Any non-terminal state here is a defensive
                // backstop: the state machine should always exit
                // on Installed / Failed / Cancelled. If it does
                // not, the row would otherwise stay stuck in
                // "downloading…" forever (silent-failure
                // finding 1). Surface as Failed so the user can
                // retry rather than reopen settings.
                warn!(
                    state = ?other,
                    model = %model_name,
                    "downloader returned unexpected non-terminal state"
                );
                let _ = bridge_tx.send(UiMessage::Models(ModelsMsg::Failed {
                    model: model_name.clone(),
                    reason: format!("internal: unexpected state {other:?}"),
                }));
            }
            Err(e) => {
                let _ = bridge_tx.send(UiMessage::Models(ModelsMsg::Failed {
                    model: model_name.clone(),
                    reason: e.to_string(),
                }));
            }
        }
        fltk::app::awake();
    });
}

/// Forward a single `DownloadState` to the cross-thread channel
/// as the right `ModelsMsg` variant. Non-progress transitions are
/// folded into the terminal-state messages emitted by the caller
/// after `downloader.run` returns.
fn forward_state(
    tx: &tokio::sync::mpsc::UnboundedSender<UiMessage>,
    model_name: &str,
    state: DownloadState,
) {
    if let DownloadState::Fetching { bytes_done, total } = state {
        let _ = tx.send(UiMessage::Models(ModelsMsg::Progress {
            model: model_name.to_owned(),
            bytes_done,
            total,
        }));
        fltk::app::awake();
    }
}

fn send_failed(
    tx: &tokio::sync::mpsc::UnboundedSender<UiMessage>,
    model: &str,
    reason: String,
) {
    let _ = tx.send(UiMessage::Models(ModelsMsg::Failed {
        model: model.to_owned(),
        reason,
    }));
    fltk::app::awake();
}

fn format_fail_reason(reason: &FailReason) -> String {
    match reason {
        FailReason::UnknownModel => "unknown model".into(),
        FailReason::ContentTypeMismatch(t) => format!("content-type: {t}"),
        FailReason::ContentLengthMismatch { expected, actual } => {
            format!("size: expected {expected}, got {actual}")
        }
        FailReason::Http(code) => format!("HTTP {code}"),
        FailReason::RateLimited { retry_after_secs } => {
            format!("rate-limited; retry in {retry_after_secs}s")
        }
        FailReason::Network(s) => format!("network: {s}"),
        FailReason::ChecksumMismatch => "checksum mismatch".into(),
        FailReason::Io(s) => format!("io: {s}"),
    }
}

/// Update one row's widgets based on a dispatched `ModelsMsg`.
/// Called on the FLTK main thread by `app::App::run`.
pub(crate) fn apply_msg(
    rows: &Arc<Mutex<HashMap<String, ModelRow>>>,
    msg: &ModelsMsg,
) {
    let Ok(map) = rows.lock() else {
        warn!("models tab: rows mutex poisoned");
        return;
    };
    match msg {
        ModelsMsg::DownloadStarted { model } => {
            if let Some(row) = map.get(model) {
                let mut row = row.clone();
                row.status_label.set_label("downloading…");
                row.action_button.set_label("Cancel");
                row.action_button.activate();
                row.progress.set_value(0.0);
                row.progress.redraw();
            }
        }
        ModelsMsg::Progress { model, bytes_done, total } => {
            if let Some(row) = map.get(model) {
                let mut row = row.clone();
                if *total > 0 {
                    row.progress.set_maximum(*total as f64);
                    row.progress.set_value(*bytes_done as f64);
                }
                let pct = if *total == 0 {
                    0
                } else {
                    (*bytes_done * 100 / *total).min(100)
                };
                row.status_label.set_label(&format!("{pct}%"));
                row.progress.redraw();
                row.status_label.redraw();
            }
        }
        ModelsMsg::Installed { model } => {
            if let Some(row) = map.get(model) {
                let mut row = row.clone();
                row.status_label.set_label("installed");
                row.action_button.set_label("Installed");
                row.action_button.deactivate();
                row.progress.set_value(row.progress.maximum());
                row.progress.redraw();
            }
            info!(%model, "model installed");
        }
        ModelsMsg::Cancelled { model } => {
            if let Some(row) = map.get(model) {
                let mut row = row.clone();
                row.status_label.set_label("cancelled");
                row.action_button.set_label("Resume");
                row.action_button.activate();
            }
        }
        ModelsMsg::Failed { model, reason } => {
            if let Some(row) = map.get(model) {
                let mut row = row.clone();
                row.status_label.set_label(&format!("failed: {reason}"));
                row.status_label.set_label_color(Color::Red);
                row.action_button.set_label("Retry");
                row.action_button.activate();
            }
            warn!(%model, %reason, "model download failed");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `format_fail_reason` covers each `FailReason` variant.
    #[test]
    fn format_fail_reason_covers_each_variant() {
        let cases: &[(FailReason, &str)] = &[
            (FailReason::UnknownModel, "unknown model"),
            (FailReason::ChecksumMismatch, "checksum mismatch"),
            (FailReason::Http(503), "HTTP 503"),
            (
                FailReason::RateLimited {
                    retry_after_secs: 30,
                },
                "rate-limited; retry in 30s",
            ),
        ];
        for (reason, expected) in cases {
            assert_eq!(format_fail_reason(reason), *expected);
        }
    }

    /// `DownloadState::Fetching` forwards as `Progress`.
    #[test]
    fn forward_state_emits_progress_for_fetching() {
        let (tx, mut rx) = unbounded_channel::<UiMessage>();
        forward_state(
            &tx,
            "ggml-tiny",
            DownloadState::Fetching {
                bytes_done: 1024,
                total: 4096,
            },
        );
        let msg = rx.try_recv().expect("forward should send Progress");
        match msg {
            UiMessage::Models(ModelsMsg::Progress {
                model,
                bytes_done,
                total,
            }) => {
                assert_eq!(model, "ggml-tiny");
                assert_eq!(bytes_done, 1024);
                assert_eq!(total, 4096);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    /// Non-`Fetching` states do NOT emit forwarded progress —
    /// terminal states are handled in the caller after `run`.
    #[test]
    fn forward_state_skips_non_progress_states() {
        let (tx, mut rx) = unbounded_channel::<UiMessage>();
        forward_state(&tx, "ggml-tiny", DownloadState::Verifying);
        forward_state(&tx, "ggml-tiny", DownloadState::Resolving);
        forward_state(&tx, "ggml-tiny", DownloadState::Installed);
        assert!(rx.try_recv().is_err(), "no forwarded message expected");
    }
}
