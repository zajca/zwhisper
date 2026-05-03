//! M7 — Profile editor tab. Two-pane layout (M7-plan § 3.1):
//! `HoldBrowser` (left, grouped by source) + form (right). Save
//! validates → atomic-writes via `tempfile::persist` → conditionally
//! calls `Profiles1.reload` (skipped during recording per D6 and
//! on `ServiceUnknown` per A4). Clone reuses `listing::clone_to_user`.
//! Diff is hand-rolled (`DoD` #5). Helpers are `pub(crate)` for
//! direct unit-test access; dead-code allowed until Group D wires UI.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use fltk::enums::{Color, FrameType};
use fltk::frame::Frame;
use fltk::group::{Group, Pack, PackType, Tabs};
use fltk::input::Input;
use fltk::prelude::*;
use tracing::{error, info, warn};
use zwhisper_core::profile::listing::{ProfileEntry, clone_to_user};
use zwhisper_core::profile::schema::Profile;
use zwhisper_core::profile::{
    self as core_profile, ProfileSource, shipped_path, user_override_path, validate_name,
};

use crate::client::{GetRecording, RecordingState, ReloadCall, ReloadOutcome};
use crate::error::SettingsError;
use crate::runtime::UiBridge;

/// Cross-thread messages from the profile editor pipeline.
/// `daemon_off` = `ServiceUnknown` (A4); `deferred_reload` = user
/// confirmed save during recording (`DoD` #3 / D6).
#[derive(Debug)]
pub(crate) enum ProfileMsg {
    ListLoaded(Vec<ProfileEntry>),
    ListLoadFailed(String),
    SaveSucceeded {
        name: String,
        daemon_off: bool,
        deferred_reload: bool,
    },
    SaveFailed {
        name: String,
        error: String,
    },
    ValidationFailed {
        error: String,
    },
}

/// FLTK widget handles owned by the tab.
pub(crate) struct ProfileTab {
    #[allow(dead_code, reason = "kept alive for the FLTK widget tree")]
    group: Group,
    pub(crate) inline_label: Frame,
    pub(crate) browser: fltk::browser::HoldBrowser,
    widgets: Arc<FormWidgets>,
}

impl std::fmt::Debug for ProfileTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileTab").finish_non_exhaustive()
    }
}

/// Aggregate of editable form widgets, shared via `Arc`.
struct FormWidgets {
    name_label: Frame,
    description: Input,
    mic: Input,
    system_output: Input,
    mode: Input,
    codec: Input,
    sample_rate: Input,
    max_duration_minutes: Input,
    backend: Input,
    model: Input,
    language: Input,
    auto: Input,
}

/// Construct the profile editor tab. Pane split: 35/65.
pub(crate) fn build(parent: &mut Tabs, bridge: UiBridge) -> ProfileTab {
    let (x, y, w, h) = parent.client_area();
    let group = Group::new(x, y, w, h, "Profiles");
    let pane_split = (w * 35) / 100;
    let mut browser = fltk::browser::HoldBrowser::new(x + 4, y + 4, pane_split - 8, h - 8, "");
    populate_browser(&mut browser);

    let form_x = x + pane_split;
    let form_y = y + 4;
    let form_w = w - pane_split - 8;
    let form_h = h - 8;
    let mut form = Pack::new(form_x, form_y, form_w, form_h, "");
    form.set_type(PackType::Vertical);
    form.set_spacing(4);

    let row_h = 24;
    let label_w = 160;
    let input_w = form_w - label_w - 4;

    let name_label = make_row_label(row_h, label_w, input_w, "name (filename):");
    let description = make_row_input(row_h, label_w, input_w, "description:");
    let mic = make_row_input(row_h, label_w, input_w, "sources.mic:");
    let system_output = make_row_input(row_h, label_w, input_w, "sources.system_output:");
    let mode = make_row_input(row_h, label_w, input_w, "sources.mode:");
    let codec = make_row_input(row_h, label_w, input_w, "recording.codec:");
    let sample_rate = make_row_input(row_h, label_w, input_w, "recording.sample_rate:");
    let max_duration_minutes =
        make_row_input(row_h, label_w, input_w, "recording.max_duration_minutes:");
    let backend = make_row_input(row_h, label_w, input_w, "transcription.backend:");
    let model = make_row_input(row_h, label_w, input_w, "transcription.model:");
    let language = make_row_input(row_h, label_w, input_w, "transcription.language:");
    let auto = make_row_input(row_h, label_w, input_w, "transcription.auto:");

    // Buttons row: real Buttons in a horizontal Pack so each
    // gets its own callback. Width and ordering match the static
    // placeholder used at A-stage so manual verification screenshots
    // remain comparable.
    let mut buttons_row = Pack::new(0, 0, form_w, row_h + 4, "");
    buttons_row.set_type(PackType::Horizontal);
    buttons_row.set_spacing(8);
    let button_w = (form_w - 4 * 8) / 4;
    let mut save_btn = fltk::button::Button::new(0, 0, button_w, row_h, "Save");
    let mut revert_btn = fltk::button::Button::new(0, 0, button_w, row_h, "Revert");
    let mut diff_btn = fltk::button::Button::new(0, 0, button_w, row_h, "Show diff");
    let mut clone_btn = fltk::button::Button::new(0, 0, button_w, row_h, "Clone…");
    buttons_row.end();

    // Inline error/success label below the buttons. Hidden until
    // a callback writes to it.
    let mut inline_label = Frame::new(form_x, form_y, form_w, row_h, "");
    inline_label.set_label_color(Color::Red);
    inline_label.set_frame(FrameType::FlatBox);
    inline_label.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);

    form.end();
    group.end();
    parent.add(&group);

    let widgets = Arc::new(FormWidgets {
        name_label,
        description,
        mic,
        system_output,
        mode,
        codec,
        sample_rate,
        max_duration_minutes,
        backend,
        model,
        language,
        auto,
    });

    // ── Wire callbacks ─────────────────────────────────────────

    // Browser selection → load profile into form.
    {
        let cb_widgets = Arc::clone(&widgets);
        let mut cb_inline = inline_label.clone();
        browser.set_callback(move |b| {
            let Some(name) = selected_profile_name(b) else {
                return;
            };
            match core_profile::load(&name) {
                Ok(profile) => {
                    populate_form(&cb_widgets, &profile);
                    cb_inline.set_label("");
                    cb_inline.redraw();
                }
                Err(e) => {
                    set_inline_error(&mut cb_inline, &format!("load {name}: {e}"));
                }
            }
        });
    }

    // Save button.
    {
        let cb_widgets = Arc::clone(&widgets);
        let mut cb_inline = inline_label.clone();
        let cb_bridge = bridge.clone();
        save_btn.set_callback(move |_btn| {
            let profile = match form_to_profile(&cb_widgets) {
                Ok(p) => p,
                Err(e) => {
                    set_inline_error(&mut cb_inline, &format!("form: {e}"));
                    return;
                }
            };
            run_save_blocking(&cb_bridge, &profile, &mut cb_inline);
        });
    }

    // Revert button.
    {
        let cb_widgets = Arc::clone(&widgets);
        let mut cb_inline = inline_label.clone();
        revert_btn.set_callback(move |_btn| {
            let name = cb_widgets.name_label.label();
            if name.is_empty() {
                set_inline_error(&mut cb_inline, "no profile selected");
                return;
            }
            match core_profile::load(&name) {
                Ok(profile) => {
                    populate_form(&cb_widgets, &profile);
                    set_inline_ok(&mut cb_inline, "reverted");
                }
                Err(e) => set_inline_error(&mut cb_inline, &format!("revert {name}: {e}")),
            }
        });
    }

    // Diff button.
    {
        let cb_widgets = Arc::clone(&widgets);
        let mut cb_inline = inline_label.clone();
        diff_btn.set_callback(move |_btn| {
            let name = cb_widgets.name_label.label();
            if name.is_empty() {
                set_inline_error(&mut cb_inline, "no profile selected");
                return;
            }
            match diff_bodies(&name) {
                Ok((user, shipped)) => {
                    let body = diff_lines(&shipped, &user);
                    if body.trim().is_empty() {
                        fltk::dialog::message_default(
                            "No differences against shipped/embedded template.",
                        );
                    } else {
                        fltk::dialog::message_default(&format!("Diff for {name}:\n\n{body}"));
                    }
                    cb_inline.set_label("");
                    cb_inline.redraw();
                }
                Err(e) => set_inline_error(&mut cb_inline, &format!("diff: {e}")),
            }
        });
    }

    // Clone button.
    {
        let mut cb_browser = browser.clone();
        let mut cb_inline = inline_label.clone();
        clone_btn.set_callback(move |_btn| {
            let Some(src_name) = selected_profile_name(&cb_browser) else {
                set_inline_error(&mut cb_inline, "no source profile selected");
                return;
            };
            let Some(dst_name) = fltk::dialog::input_default("Name for cloned profile:", "") else {
                return; // user cancelled
            };
            match perform_clone(&src_name, &dst_name) {
                Ok(_path) => {
                    populate_browser(&mut cb_browser);
                    set_inline_ok(&mut cb_inline, &format!("cloned → {dst_name}"));
                }
                Err(e) => set_inline_error(&mut cb_inline, &format!("clone: {e}")),
            }
        });
    }

    ProfileTab {
        group,
        inline_label,
        browser,
        widgets,
    }
}

/// Populate the browser with all known profiles, grouped by source.
fn populate_browser(browser: &mut fltk::browser::HoldBrowser) {
    browser.clear();
    match zwhisper_core::profile::listing::list_entries() {
        Ok(entries) => {
            for line in render_grouped_lines(&entries) {
                browser.add(&line);
            }
        }
        Err(e) => {
            browser.add(&format!("> error: {e}"));
        }
    }
}

/// Extract the profile name from the currently-selected browser
/// row. Returns `None` for header rows (`> User profiles` etc.)
/// and when nothing is selected.
fn selected_profile_name(browser: &fltk::browser::HoldBrowser) -> Option<String> {
    let row = browser.value();
    if row <= 0 {
        return None;
    }
    let text = browser.text(row)?;
    let trimmed = text.trim();
    if trimmed.starts_with('>') || trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Schema enums use `serde(rename_all = "snake_case")` and explicit
/// `serde(rename)` for the backend hyphen. They do not implement
/// `FromStr` / `Display`, so we hand-roll bidirectional string
/// conversion. Keeping these as plain `match` expressions makes
/// adding a future codec / backend a single-line change.
fn parse_mode(s: &str) -> Result<zwhisper_core::profile::schema::Mode, String> {
    use zwhisper_core::profile::schema::Mode;
    match s.trim() {
        "mono_mix" => Ok(Mode::MonoMix),
        "stereo_split" => Ok(Mode::StereoSplit),
        other => Err(format!(
            "unknown mode {other:?} (allowed: mono_mix, stereo_split)"
        )),
    }
}

fn mode_str(m: zwhisper_core::profile::schema::Mode) -> &'static str {
    use zwhisper_core::profile::schema::Mode;
    match m {
        Mode::MonoMix => "mono_mix",
        Mode::StereoSplit => "stereo_split",
    }
}

fn parse_codec(s: &str) -> Result<zwhisper_core::profile::schema::Codec, String> {
    use zwhisper_core::profile::schema::Codec;
    match s.trim() {
        "flac" => Ok(Codec::Flac),
        other => Err(format!("unknown codec {other:?} (allowed: flac)")),
    }
}

fn codec_str(c: zwhisper_core::profile::schema::Codec) -> &'static str {
    use zwhisper_core::profile::schema::Codec;
    match c {
        Codec::Flac => "flac",
    }
}

fn parse_backend(s: &str) -> Result<zwhisper_core::profile::schema::Backend, String> {
    use zwhisper_core::profile::schema::Backend;
    match s.trim() {
        "whisper-cpp" | "whisper_cpp" => Ok(Backend::WhisperCpp),
        "deepgram" => Ok(Backend::Deepgram),
        "assemblyai" => Ok(Backend::AssemblyAi),
        "openai" => Ok(Backend::OpenAi),
        other => Err(format!(
            "unknown backend {other:?} (allowed: whisper-cpp, deepgram, assemblyai, openai)"
        )),
    }
}

/// Read all form widgets into a `Profile`. The `name` field comes
/// from the read-only `name_label` populated when the user selected
/// a profile. Parsing errors propagate as `String` (caller surfaces
/// them inline). Outputs and hotkey fields preserve whatever the
/// user previously wrote — we do not edit them in M7.
fn form_to_profile(widgets: &FormWidgets) -> Result<Profile, String> {
    use zwhisper_core::profile::schema::{Recording, Sources, Transcription};

    let name = widgets.name_label.label();
    if name.is_empty() {
        return Err("no profile selected".into());
    }
    let mode = parse_mode(&widgets.mode.value())?;
    let codec = parse_codec(&widgets.codec.value())?;
    let sample_rate: u32 = widgets
        .sample_rate
        .value()
        .trim()
        .parse()
        .map_err(|e| format!("recording.sample_rate: {e}"))?;
    let max_duration_minutes: u64 = widgets
        .max_duration_minutes
        .value()
        .trim()
        .parse()
        .map_err(|e| format!("recording.max_duration_minutes: {e}"))?;
    let backend = parse_backend(&widgets.backend.value())?;
    let auto: bool = widgets
        .auto
        .value()
        .trim()
        .parse()
        .map_err(|e| format!("transcription.auto (true/false): {e}"))?;

    // Re-load the prior profile to preserve fields we do not edit
    // (outputs, hotkey, deepgram block). Settings is an editor
    // overlay, not a writer-from-scratch.
    let prior = core_profile::load(&name).map_err(|e| format!("preserve unknown fields: {e}"))?;

    Ok(Profile {
        schema_version: prior.schema_version,
        name,
        description: widgets.description.value(),
        sources: Sources {
            mic: widgets.mic.value(),
            system_output: widgets.system_output.value(),
            mode,
        },
        recording: Recording {
            codec,
            sample_rate,
            max_duration_minutes,
        },
        transcription: Transcription {
            backend,
            model: widgets.model.value(),
            language: widgets.language.value(),
            auto,
            deepgram: prior.transcription.deepgram,
        },
        outputs: prior.outputs,
        hotkey: prior.hotkey,
    })
}

/// Populate widget values from a loaded `Profile`.
fn populate_form(widgets: &FormWidgets, profile: &Profile) {
    widgets.name_label.clone().set_label(&profile.name);
    widgets.description.clone().set_value(&profile.description);
    widgets.mic.clone().set_value(&profile.sources.mic);
    widgets
        .system_output
        .clone()
        .set_value(&profile.sources.system_output);
    widgets
        .mode
        .clone()
        .set_value(mode_str(profile.sources.mode));
    widgets
        .codec
        .clone()
        .set_value(codec_str(profile.recording.codec));
    widgets
        .sample_rate
        .clone()
        .set_value(&profile.recording.sample_rate.to_string());
    widgets
        .max_duration_minutes
        .clone()
        .set_value(&profile.recording.max_duration_minutes.to_string());
    widgets
        .backend
        .clone()
        .set_value(profile.transcription.backend.as_str());
    widgets
        .model
        .clone()
        .set_value(&profile.transcription.model);
    widgets
        .language
        .clone()
        .set_value(&profile.transcription.language);
    widgets
        .auto
        .clone()
        .set_value(&profile.transcription.auto.to_string());
}

/// Bound on how long the Save button may freeze the FLTK main
/// thread waiting for the daemon to answer two D-Bus calls
/// (`Recorder1.GetStatus` + `Profiles1.reload`). The normal case
/// is < 50 ms; on a stalled session bus we abort with a typed
/// error rather than freeze the UI indefinitely.
const SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `perform_save` on the runtime, blocking the FLTK callback
/// thread for the round-trip. Bounded by `SAVE_TIMEOUT` so a stalled
/// daemon does not freeze the UI.
fn run_save_blocking(bridge: &UiBridge, profile: &Profile, inline: &mut Frame) {
    let outcome = bridge.rt_handle.block_on(async {
        tokio::time::timeout(SAVE_TIMEOUT, async {
            let profiles = crate::client::ProfilesClient::connect().await?;
            let recorder = crate::client::RecorderClient::connect().await?;

            // Pre-check: if recording, ask the user.
            let recording = recorder.is_recording().await?;
            let user_confirmed_defer = matches!(recording, RecordingState::Recording)
                && fltk_main_thread_confirm_recording_dialog();

            if matches!(recording, RecordingState::Recording) && !user_confirmed_defer {
                return Err::<SaveOutcome, SettingsError>(SettingsError::Profile(
                    "save cancelled (daemon recording)".into(),
                ));
            }
            perform_save(profile, &profiles, &recorder, user_confirmed_defer, None).await
        })
        .await
        .map_err(|_| {
            SettingsError::Profile("save timed out (daemon unresponsive after 5s)".into())
        })?
    });

    match outcome {
        Ok(SaveOutcome {
            daemon_off,
            deferred_reload,
        }) => {
            let mut msg = format!("saved {}", profile.name);
            if deferred_reload {
                msg.push_str(" (will apply on next recording)");
            } else if daemon_off {
                msg.push_str(" (daemon not running)");
            }
            set_inline_ok(inline, &msg);
        }
        Err(e) => set_inline_error(inline, &format!("save: {e}")),
    }
}

/// Modal "daemon is recording, continue?" prompt. FLTK's modal
/// `choice2_default` returns `Some(0)` for the first button.
fn fltk_main_thread_confirm_recording_dialog() -> bool {
    matches!(
        fltk::dialog::choice2_default(
            "Daemon is recording. Save will apply on next recording — continue?",
            "Cancel",
            "Save",
            ""
        ),
        Some(1)
    )
}

fn set_inline_error(label: &mut Frame, text: &str) {
    error!(error = text, "profile callback error");
    label.set_label(text);
    label.set_label_color(Color::Red);
    label.redraw();
}

fn set_inline_ok(label: &mut Frame, text: &str) {
    info!(message = text, "profile callback ok");
    label.set_label(text);
    label.set_label_color(Color::DarkGreen);
    label.redraw();
}

/// Apply a `ProfileMsg` arriving via `UiMessage::Profile`. Called
/// from `app::App::run`'s awake_callback dispatcher on the FLTK
/// main thread. Touches only the inline label and (on list reload)
/// the browser widget — form repopulation is driven directly by
/// the browser-selection callback.
pub(crate) fn apply_msg(
    inline: &mut Frame,
    browser: &mut fltk::browser::HoldBrowser,
    msg: &ProfileMsg,
) {
    match msg {
        ProfileMsg::ListLoaded(_entries) => {
            populate_browser(browser);
        }
        ProfileMsg::ListLoadFailed(error) => {
            set_inline_error(inline, &format!("list: {error}"));
        }
        ProfileMsg::SaveSucceeded {
            name,
            daemon_off,
            deferred_reload,
        } => {
            let mut text = format!("saved {name}");
            if *deferred_reload {
                text.push_str(" (will apply on next recording)");
            } else if *daemon_off {
                text.push_str(" (daemon not running)");
            }
            set_inline_ok(inline, &text);
        }
        ProfileMsg::SaveFailed { name, error } => {
            set_inline_error(inline, &format!("save {name}: {error}"));
        }
        ProfileMsg::ValidationFailed { error } => {
            set_inline_error(inline, &format!("validation: {error}"));
        }
    }
}

/// Single row: label on left, value on right.
fn make_row_input(row_h: i32, label_w: i32, input_w: i32, label: &str) -> Input {
    let mut row = Pack::new(0, 0, label_w + input_w, row_h, "");
    row.set_type(PackType::Horizontal);
    Frame::new(0, 0, label_w, row_h, "").set_label(label);
    let input = Input::new(0, 0, input_w, row_h, "");
    row.end();
    input
}

fn make_row_label(row_h: i32, label_w: i32, input_w: i32, label: &str) -> Frame {
    let mut row = Pack::new(0, 0, label_w + input_w, row_h, "");
    row.set_type(PackType::Horizontal);
    Frame::new(0, 0, label_w, row_h, "").set_label(label);
    let value = Frame::new(0, 0, input_w, row_h, "");
    row.end();
    value
}

/// Partition entries by source: `(user, shipped, embedded)`.
pub(crate) fn group_by_source(
    entries: &[ProfileEntry],
) -> (Vec<&ProfileEntry>, Vec<&ProfileEntry>, Vec<&ProfileEntry>) {
    let mut user = Vec::new();
    let mut shipped = Vec::new();
    let mut embedded = Vec::new();
    for e in entries {
        match e.source.as_str() {
            "user" => user.push(e),
            "shipped" => shipped.push(e),
            "embedded" => embedded.push(e),
            // Unknown source — drop. `list_entries` only emits
            // the three known labels.
            _ => {}
        }
    }
    (user, shipped, embedded)
}

/// `HoldBrowser` lines: `>` headers, two-space-indented entries.
pub(crate) fn render_grouped_lines(entries: &[ProfileEntry]) -> Vec<String> {
    let (user, shipped, embedded) = group_by_source(entries);
    let mut lines = Vec::new();
    push_section(&mut lines, "User profiles", &user);
    push_section(&mut lines, "Shipped", &shipped);
    push_section(&mut lines, "Embedded", &embedded);
    lines
}

fn push_section(lines: &mut Vec<String>, header: &str, items: &[&ProfileEntry]) {
    if items.is_empty() {
        return;
    }
    lines.push(format!("> {header}"));
    for e in items {
        lines.push(format!("  {}", e.name));
    }
}

/// Save outcome surfaced via `ProfileMsg::SaveSucceeded`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SaveOutcome {
    pub(crate) daemon_off: bool,
    pub(crate) deferred_reload: bool,
}

/// Atomic-write a `Profile` and, when the daemon is reachable
/// and not recording, issue `Profiles1.reload`.
/// `user_confirmed_defer` is the user's answer to the
/// "daemon is recording, continue?" modal.
pub(crate) async fn perform_save<R, G>(
    profile: &Profile,
    profiles: &R,
    recorder: &G,
    user_confirmed_defer: bool,
    target_dir_override: Option<&std::path::Path>,
) -> Result<SaveOutcome, SettingsError>
where
    R: ReloadCall + ?Sized,
    G: GetRecording + ?Sized,
{
    profile
        .validate()
        .map_err(|e| SettingsError::Profile(format!("validation: {e}")))?;

    // Caller surfaces the modal and sets `user_confirmed_defer`;
    // we re-check state here so the policy lives in one place.
    let recording = recorder.is_recording().await?;
    let deferred_reload = match recording {
        RecordingState::Recording if user_confirmed_defer => true,
        RecordingState::Recording => {
            return Err(SettingsError::Profile(
                "save aborted: daemon is recording and user did not confirm".into(),
            ));
        }
        RecordingState::Idle | RecordingState::DaemonOff => false,
    };

    let target = match target_dir_override {
        Some(dir) => dir.join(format!("{}.toml", profile.name)),
        None => user_override_path(&profile.name)
            .map_err(|e| SettingsError::Profile(format!("path resolve: {e}")))?,
    };
    let parent = target
        .parent()
        .ok_or_else(|| SettingsError::Profile("target has no parent dir".into()))?
        .to_owned();
    fs::create_dir_all(&parent)?;

    let body = toml::to_string_pretty(profile)
        .map_err(|e| SettingsError::Profile(format!("serialise: {e}")))?;

    write_atomic(&target, &parent, body.as_bytes())?;

    let daemon_off = if deferred_reload {
        // Recording in progress: skip reload regardless of bus
        // state (D6). Re-probe is purely for toast wording.
        matches!(
            recorder.is_recording().await,
            Ok(RecordingState::DaemonOff) | Err(_)
        )
    } else {
        match profiles.reload().await? {
            ReloadOutcome::Reloaded => false,
            ReloadOutcome::DaemonOff => true,
        }
    };

    info!(
        profile = %profile.name,
        daemon_off,
        deferred_reload,
        "profile saved"
    );

    Ok(SaveOutcome {
        daemon_off,
        deferred_reload,
    })
}

/// Atomic write: tempfile co-located with destination dir
/// (`persist` uses `rename(2)` — must not cross filesystems).
fn write_atomic(
    target: &std::path::Path,
    dir: &std::path::Path,
    body: &[u8],
) -> Result<(), SettingsError> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|e| {
        SettingsError::Profile(format!("persist {}: {}", target.display(), e.error))
    })?;
    Ok(())
}

/// Validate `dst` then invoke `clone_to_user`. Pure helper so
/// unit tests drive the validation path without an FLTK dialog.
pub(crate) fn perform_clone(src: &str, dst: &str) -> Result<PathBuf, SettingsError> {
    validate_name(dst).map_err(|e| SettingsError::Profile(format!("name: {e}")))?;
    let path = clone_to_user(src, dst)
        .map_err(|e| SettingsError::Profile(format!("clone {src} -> {dst}: {e}")))?;
    info!(src = src, dst = dst, path = %path.display(), "profile cloned");
    Ok(path)
}

/// Hand-rolled LCS line diff. Output prefixes: `- ` removed,
/// `+ ` added, `  ` unchanged. O(n*m) is fine for < 50-line
/// profile bodies; pulling in `similar` is overkill.
#[allow(clippy::many_single_char_names)]
pub(crate) fn diff_lines(left: &str, right: &str) -> String {
    let l: Vec<&str> = left.lines().collect();
    let r: Vec<&str> = right.lines().collect();
    let n = l.len();
    let m = r.len();

    // LCS length table.
    let mut table = vec![vec![0_usize; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            table[i + 1][j + 1] = if l[i] == r[j] {
                table[i][j] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    // Walk back to emit the diff. `i` indexes `l` (left), `j`
    // indexes `r` (right).
    let mut out = String::new();
    let mut i = n;
    let mut j = m;
    let mut rev: Vec<String> = Vec::with_capacity(n + m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && l[i - 1] == r[j - 1] {
            rev.push(format!("  {}", l[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || table[i][j - 1] >= table[i - 1][j]) {
            rev.push(format!("+ {}", r[j - 1]));
            j -= 1;
        } else if i > 0 {
            rev.push(format!("- {}", l[i - 1]));
            i -= 1;
        }
    }
    for line in rev.into_iter().rev() {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Resolve `(user_body, shipped_body)` for a "show diff" against
/// a user override. `shipped_body` is empty when no shipped or
/// embedded profile of that name exists.
pub(crate) fn diff_bodies(name: &str) -> Result<(String, String), SettingsError> {
    let user_path =
        user_override_path(name).map_err(|e| SettingsError::Profile(format!("user path: {e}")))?;
    let user_body = fs::read_to_string(&user_path)
        .map_err(|e| SettingsError::Profile(format!("read {}: {e}", user_path.display())))?;

    // Try the shipped path; fall back to embedded on `NotFound`.
    let shipped_body = match shipped_path(name) {
        Ok(p) if p.is_file() => fs::read_to_string(&p)
            .map_err(|e| SettingsError::Profile(format!("read {}: {e}", p.display())))?,
        _ => match core_profile::resolve(name) {
            Ok(ProfileSource::Embedded(body)) => body.to_owned(),
            _ => String::new(),
        },
    };
    Ok((user_body, shipped_body))
}

/// Wrap a UI callback that may return `Err`. Logs through
/// `tracing` and copies the message into `inline`. `panic = "abort"`
/// in release is the safety net for FFI-unsafe panics.
pub(crate) fn safe_callback<F>(mut inline: Frame, mut f: F)
where
    F: FnMut() -> Result<(), SettingsError> + 'static,
{
    #[allow(clippy::redundant_closure)]
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || f()));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            warn!(error = %err, "profile callback returned Err");
            inline.set_label(&format!("error: {err}"));
            inline.set_label_color(Color::Red);
        }
        Err(panic) => {
            error!(?panic, "profile callback panicked");
            inline.set_label("internal error (see logs)");
            inline.set_label_color(Color::Red);
        }
    }
}

/// Test-only mirror of [`safe_callback`] without an FLTK widget.
#[cfg(test)]
fn capture_callback_error<F>(mut f: F) -> Option<String>
where
    F: FnMut() -> Result<(), SettingsError>,
{
    match f() {
        Ok(()) => None,
        Err(err) => {
            warn!(error = %err, "captured callback Err");
            Some(format!("error: {err}"))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use zwhisper_core::profile::schema::{
        Backend, Codec, Mode, Profile, Recording, Sources, Transcription,
    };

    /// Counter fake; records `reload()` call count so the save
    /// tests can assert the "skip reload during recording" branch.
    struct FakeProfilesClient {
        call_count: AtomicUsize,
        outcome: ReloadOutcome,
    }
    impl FakeProfilesClient {
        fn ok() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                outcome: ReloadOutcome::Reloaded,
            }
        }
        fn daemon_off() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                outcome: ReloadOutcome::DaemonOff,
            }
        }
    }
    #[async_trait::async_trait]
    impl ReloadCall for FakeProfilesClient {
        async fn reload(&self) -> Result<ReloadOutcome, SettingsError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(self.outcome)
        }
    }

    struct FakeRecorderClient(RecordingState);
    #[async_trait::async_trait]
    impl GetRecording for FakeRecorderClient {
        async fn is_recording(&self) -> Result<RecordingState, SettingsError> {
            Ok(self.0)
        }
    }

    fn make_valid_profile(name: &str) -> Profile {
        Profile {
            schema_version: 1,
            name: name.to_owned(),
            description: "test profile".into(),
            sources: Sources {
                mic: "default".into(),
                system_output: "default".into(),
                mode: Mode::MonoMix,
            },
            recording: Recording {
                codec: Codec::Flac,
                sample_rate: 16_000,
                max_duration_minutes: 0,
            },
            transcription: Transcription {
                backend: Backend::WhisperCpp,
                model: "tiny".into(),
                language: "auto".into(),
                auto: true,
                deepgram: None,
            },
            outputs: Vec::new(),
            hotkey: zwhisper_core::profile::schema::Hotkey::default(),
        }
    }

    fn entry(name: &str, source: &str) -> ProfileEntry {
        ProfileEntry {
            name: name.into(),
            source: source.into(),
            schema_version: Some(1),
            description: None,
            backend: None,
        }
    }

    /// `DoD` #1: list groups by source.
    #[test]
    fn list_groups_by_source() {
        let entries = vec![
            entry("a-user", "user"),
            entry("default", "embedded"),
            entry("ship-1", "shipped"),
            entry("b-user", "user"),
        ];
        let (user, shipped, embedded) = group_by_source(&entries);
        assert_eq!(user.len(), 2);
        assert_eq!(shipped.len(), 1);
        assert_eq!(embedded.len(), 1);

        let lines = render_grouped_lines(&entries);
        // Section headers appear in canonical order.
        let headers: Vec<&String> = lines.iter().filter(|l| l.starts_with('>')).collect();
        assert_eq!(headers.len(), 3, "expected three section headers");
        assert!(headers[0].contains("User profiles"));
        assert!(headers[1].contains("Shipped"));
        assert!(headers[2].contains("Embedded"));
    }

    /// `DoD` #2: validate, atomic-write, reload.
    #[tokio::test]
    async fn save_validates_then_atomic_writes_then_reloads() {
        let tmp = TempDir::new().unwrap();
        let profiles = FakeProfilesClient::ok();
        let recorder = FakeRecorderClient(RecordingState::Idle);
        let profile = make_valid_profile("savetest");
        let outcome = perform_save(&profile, &profiles, &recorder, false, Some(tmp.path()))
            .await
            .expect("save succeeds");
        assert!(!outcome.daemon_off);
        assert!(!outcome.deferred_reload);
        assert_eq!(profiles.call_count.load(Ordering::SeqCst), 1);
        let body = fs::read_to_string(tmp.path().join("savetest.toml")).unwrap();
        let parsed: Profile = toml::from_str(&body).expect("re-parse");
        assert_eq!(parsed.name, "savetest");
        assert_eq!(parsed.recording.sample_rate, 16_000);
    }

    /// `DoD` #3 + D6: save during recording defers reload.
    #[tokio::test]
    async fn save_during_recording_warns_and_defers_reload() {
        let tmp = TempDir::new().unwrap();
        let profiles = FakeProfilesClient::ok();
        let recorder = FakeRecorderClient(RecordingState::Recording);
        let profile = make_valid_profile("recsave");
        // confirmed → write proceeds, reload skipped.
        let outcome = perform_save(&profile, &profiles, &recorder, true, Some(tmp.path()))
            .await
            .expect("save during recording when confirmed");
        assert!(outcome.deferred_reload);
        assert_eq!(
            profiles.call_count.load(Ordering::SeqCst),
            0,
            "reload must NOT be called while recording (D6)"
        );
        assert!(tmp.path().join("recsave.toml").is_file());
        // not confirmed → save refused.
        let refused = perform_save(&profile, &profiles, &recorder, false, Some(tmp.path())).await;
        assert!(matches!(refused, Err(SettingsError::Profile(_))));
    }

    /// `DoD` #4: clone rejects path-traversal and empty names.
    #[test]
    fn clone_name_traversal_rejected() {
        match perform_clone("default", "../../etc/passwd") {
            Err(SettingsError::Profile(msg)) => assert!(
                msg.contains("name") || msg.contains("invalid"),
                "expected name-validation error: {msg}"
            ),
            other => panic!("expected Profile error, got {other:?}"),
        }
        assert!(matches!(
            perform_clone("default", ""),
            Err(SettingsError::Profile(_))
        ));
    }

    /// `DoD` #5: diff marks added/removed lines.
    #[test]
    fn diff_marks_added_removed_lines() {
        let diff = diff_lines("a\nb\nc", "a\nx\nc");
        assert!(diff.contains("- b") && diff.contains("+ x"), "{diff}");
        assert!(diff.contains("  a") && diff.contains("  c"), "{diff}");
    }

    /// `DoD` A2: callback returning Err logs and surfaces inline.
    #[tracing_test::traced_test]
    #[test]
    fn callback_returning_err_logs_and_shows_inline_label() {
        let label = capture_callback_error(|| Err(SettingsError::Profile("bang".into())))
            .expect("callback returned Err");
        assert!(label.contains("bang"));
        assert!(
            logs_contain("captured callback Err") || logs_contain("bang"),
            "expected tracing event"
        );
    }

    /// `DoD` A4: `ServiceUnknown` treated as daemon-off.
    #[tokio::test]
    async fn service_unknown_treated_as_daemon_off() {
        let tmp = TempDir::new().unwrap();
        let profiles = FakeProfilesClient::daemon_off();
        let recorder = FakeRecorderClient(RecordingState::Idle);
        let profile = make_valid_profile("svcunknown");
        let outcome = perform_save(&profile, &profiles, &recorder, false, Some(tmp.path()))
            .await
            .expect("save succeeds when daemon is offline");
        assert!(outcome.daemon_off);
        assert!(!outcome.deferred_reload);
        assert_eq!(profiles.call_count.load(Ordering::SeqCst), 1);
        assert!(tmp.path().join("svcunknown.toml").is_file());
    }

    #[test]
    fn diff_identical_inputs_marks_all_unchanged() {
        let diff = diff_lines("a\nb\nc", "a\nb\nc");
        for line in diff.lines() {
            assert!(line.starts_with("  "), "expected unchanged marker: {line}");
        }
        assert_eq!(diff_lines("", ""), "");
    }
}
