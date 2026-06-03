use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::error::{ProfileError, SUPPORTED_BACKENDS_M5};

/// Native capture sample rates the pipeline can record. The FLAC
/// artifact is written at this rate (full fidelity); the ASR branch
/// always normalizes to 16 kHz mono regardless.
pub const SUPPORTED_SAMPLE_RATES: &[u32] = &[16_000, 44_100, 48_000];

/// `IDEA.md` § 6 sources mode. `MonoMix` is the only mode the engine
/// honours in M2; `StereoSplit` deserializes cleanly so v1 files that
/// declare it parse, but the engine returns `UnsupportedMode` before
/// any `GStreamer` state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    MonoMix,
    StereoSplit,
}

/// `IDEA.md` § 6 codec. Only FLAC ships in M0/M1/M2; `opus` and `wav`
/// land later. Keeping this an enum (vs a free string) makes adding
/// future codecs a single-variant patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Flac,
}

/// Backend identifier. Strings on the wire match `IDEA.md` § 4 verbatim;
/// the hyphen in `whisper-cpp` is preserved by `serde(rename)` so the
/// TOML stays human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    #[serde(rename = "whisper-cpp")]
    WhisperCpp,
    #[serde(rename = "deepgram")]
    Deepgram,
    #[serde(rename = "parakeet")]
    Parakeet,
    #[serde(rename = "assemblyai")]
    AssemblyAi,
    #[serde(rename = "openai")]
    OpenAi,
}

impl Backend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WhisperCpp => "whisper-cpp",
            Self::Deepgram => "deepgram",
            Self::Parakeet => "parakeet",
            Self::AssemblyAi => "assemblyai",
            Self::OpenAi => "openai",
        }
    }

    /// Inverse of [`Self::as_str`]. Returns `None` for an unknown id so
    /// callers surface a typed error instead of guessing a default.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "whisper-cpp" => Some(Self::WhisperCpp),
            "deepgram" => Some(Self::Deepgram),
            "parakeet" => Some(Self::Parakeet),
            "assemblyai" => Some(Self::AssemblyAi),
            "openai" => Some(Self::OpenAi),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sources {
    /// Mic source node name, or `"default"` for the `PipeWire` default.
    pub mic: String,
    /// Sink monitor node name. Required and non-empty in M2 — empty
    /// (mic-only) mode is rejected by `Profile::validate` until the
    /// pipeline branch lands in M3.
    #[serde(default)]
    pub system_output: String,
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recording {
    pub codec: Codec,
    pub sample_rate: u32,
    /// Auto-stop guard from `IDEA.md` § 7. `0` disables (matches the
    /// `--max-duration-minutes 0` CLI opt-out).
    pub max_duration_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcription {
    pub backend: Backend,
    pub model: String,
    pub language: String,
    /// Run transcription automatically after recording stops.
    pub auto: bool,

    /// Per-cloud-backend tuning. M5 ships only the `[transcription.deepgram]`
    /// sub-table; future cloud backends will land alongside it as
    /// optional siblings. Absent block → Deepgram defaults are used
    /// when `backend = "deepgram"`. Round-trips unchanged for
    /// whisper-cpp profiles (the field is `Option`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepgram: Option<DeepgramSettings>,

    /// Per-whisper.cpp tuning. Omitted block keeps upstream defaults
    /// except for zwhisper-owned arguments (`--model`, `--language`,
    /// `--output-*`, and the input audio path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whisper_cpp: Option<WhisperCppSettings>,
}

/// whisper.cpp-specific knobs read from `[transcription.whisper_cpp]`.
/// All fields are optional; absent values let `whisper-cli` keep its
/// own defaults.
// Each bool maps to a distinct `whisper-cli` boolean flag, so the count
// mirrors the upstream CLI surface rather than indicating a state that
// should be modelled as an enum. Grouping them would only obscure the
// 1:1 flag mapping in `whisper_cpp::apply_settings`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WhisperCppSettings {
    pub threads: Option<u32>,
    pub processors: Option<u32>,
    pub offset_ms: Option<u64>,
    pub offset_n: Option<u64>,
    pub duration_ms: Option<u64>,
    pub max_context: Option<i32>,
    pub max_len: Option<u32>,
    pub split_on_word: bool,
    pub best_of: Option<u32>,
    pub beam_size: Option<u32>,
    pub audio_ctx: Option<u32>,
    pub word_threshold: Option<f32>,
    pub entropy_threshold: Option<f32>,
    pub logprob_threshold: Option<f32>,
    pub no_speech_threshold: Option<f32>,
    pub temperature: Option<f32>,
    pub temperature_inc: Option<f32>,
    pub translate: bool,
    pub diarize: bool,
    pub tinydiarize: bool,
    pub no_fallback: bool,
    pub no_prints: bool,
    pub print_special: bool,
    pub print_colors: bool,
    pub print_confidence: bool,
    pub print_progress: bool,
    pub no_timestamps: bool,
    pub prompt: Option<String>,
    pub carry_initial_prompt: bool,
    pub openvino_device: Option<String>,
    pub dtw: Option<String>,
    pub log_score: bool,
    pub no_gpu: bool,
    pub device: Option<u32>,
    pub flash_attn: Option<bool>,
    pub suppress_nst: bool,
    pub suppress_regex: Option<String>,
    pub grammar: Option<String>,
    pub grammar_rule: Option<String>,
    pub grammar_penalty: Option<f32>,
    pub vad: bool,
    pub vad_model: Option<String>,
    pub vad_threshold: Option<f32>,
    pub vad_min_speech_duration_ms: Option<u64>,
    pub vad_min_silence_duration_ms: Option<u64>,
    pub vad_max_speech_duration_s: Option<f32>,
    pub vad_speech_pad_ms: Option<u64>,
    pub vad_samples_overlap: Option<f32>,
    pub extra_args: Vec<String>,
}

impl WhisperCppSettings {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("word_threshold", self.word_threshold),
            ("entropy_threshold", self.entropy_threshold),
            ("logprob_threshold", self.logprob_threshold),
            ("no_speech_threshold", self.no_speech_threshold),
            ("temperature", self.temperature),
            ("temperature_inc", self.temperature_inc),
            ("grammar_penalty", self.grammar_penalty),
            ("vad_threshold", self.vad_threshold),
            ("vad_max_speech_duration_s", self.vad_max_speech_duration_s),
            ("vad_samples_overlap", self.vad_samples_overlap),
        ] {
            if let Some(v) = value {
                if !v.is_finite() {
                    return Err(format!("transcription.whisper_cpp.{name} must be finite"));
                }
            }
        }

        if let Some(v) = self.temperature {
            if !(0.0..=1.0).contains(&v) {
                return Err("transcription.whisper_cpp.temperature must be between 0 and 1".into());
            }
        }
        if let Some(v) = self.temperature_inc {
            if !(0.0..=1.0).contains(&v) {
                return Err(
                    "transcription.whisper_cpp.temperature_inc must be between 0 and 1".into(),
                );
            }
        }
        if let Some(v) = self.vad_threshold {
            if !(0.0..=1.0).contains(&v) {
                return Err(
                    "transcription.whisper_cpp.vad_threshold must be between 0 and 1".into(),
                );
            }
        }
        if let Some(v) = self.vad_samples_overlap {
            if !(0.0..=1.0).contains(&v) {
                return Err(
                    "transcription.whisper_cpp.vad_samples_overlap must be between 0 and 1".into(),
                );
            }
        }
        for value in [self.threads, self.processors, self.best_of, self.beam_size]
            .into_iter()
            .flatten()
        {
            if value == 0 {
                return Err(
                    "transcription.whisper_cpp thread/search counts must be greater than zero"
                        .into(),
                );
            }
        }

        validate_extra_args(&self.extra_args)
    }
}

fn validate_extra_args(args: &[String]) -> Result<(), String> {
    for arg in args {
        if arg.is_empty() {
            return Err("transcription.whisper_cpp.extra_args must not contain empty args".into());
        }
        if arg.chars().any(|ch| ch == '\0' || ch.is_control()) {
            return Err(
                "transcription.whisper_cpp.extra_args must not contain control characters".into(),
            );
        }
        if !arg.starts_with('-') || arg == "--" {
            return Err(
                "transcription.whisper_cpp.extra_args must contain only option flags".into(),
            );
        }
        let option = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if is_reserved_whisper_cpp_arg(option) {
            return Err(format!(
                "transcription.whisper_cpp.extra_args must not override zwhisper-owned arg {option:?}"
            ));
        }
    }
    Ok(())
}

fn is_reserved_whisper_cpp_arg(arg: &str) -> bool {
    matches!(
        arg,
        "-h" | "--help"
            | "-m"
            | "--model"
            | "-l"
            | "--language"
            | "-f"
            | "--file"
            | "-otxt"
            | "--output-txt"
            | "-oj"
            | "--output-json"
            | "-of"
            | "--output-file"
    ) || arg.starts_with("--output-")
}

/// Deepgram-specific knobs read from `[transcription.deepgram]`. All
/// fields are optional in TOML; `Default` provides sensible values.
/// IDEA.md § 4 + M5-plan.md Phase 2 lock the field set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeepgramSettings {
    /// Deepgram model identifier. M5 default: `nova-3` (current best
    /// general-purpose model per the researcher's 2026-05-02 sweep).
    pub model: String,
    /// Word-level speaker diarization. Maps to `diarize=true` query
    /// param. Profile UI surfaces this as "speaker labels".
    pub diarize: bool,
    /// When `transcription.language == "auto"`, the daemon also sends
    /// `detect_language=true`; this flag is reserved for future
    /// fine-grained control (e.g., constrain candidate languages).
    pub language_detection: bool,
    /// Optional Deepgram tier override (`enhanced`, `nova`, `base`).
    /// Modern accounts ignore this; kept as escape hatch per
    /// M5-plan.md Risk #8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Per-call HTTP timeout in seconds (covers connect + body
    /// transfer + server processing). Default 600 s — Deepgram batch
    /// can buffer several minutes for long files.
    pub timeout_s: u64,
    /// TCP/TLS connect timeout in seconds (subset of `timeout_s`,
    /// applies only to the connect phase). Default 15 s. Separated
    /// because a connect storm should fail faster than a slow
    /// server-side decode (security review #4, 2026-05-02).
    pub connect_timeout_s: u64,
    /// Cap on the retry attempt count for transient failures
    /// (5xx, 408, 429, connect errors). Hard upper bound.
    pub max_retries: u32,
    /// Wall-clock budget across ALL retry attempts. Set to keep a
    /// flapping network from inflating Deepgram billing
    /// (M5-plan § C4). The retry loop exits early when this elapses.
    pub retry_total_budget_s: u64,
}

impl Default for DeepgramSettings {
    fn default() -> Self {
        Self {
            model: "nova-3".to_owned(),
            diarize: true,
            language_detection: false,
            tier: None,
            timeout_s: 600,
            connect_timeout_s: 15,
            max_retries: 4,
            retry_total_budget_s: 90,
        }
    }
}

/// Output destinations. M2 honours `File`; `Clipboard` and
/// `Notification` parse cleanly but emit a tracing warning at engine
/// time (M4 / tray-bound).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputDest {
    File { path: String },
    Clipboard,
    Notification,
}

/// Hotkey block (M6 only). Round-trips through M2 unchanged so users
/// who write hotkey config now do not lose it after a save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    #[serde(default)]
    pub toggle: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,

    pub sources: Sources,
    pub recording: Recording,
    pub transcription: Transcription,

    /// `[[output]]` table sequence. Renamed from singular `output` per
    /// TOML array-of-tables idiom.
    #[serde(default, rename = "output")]
    pub outputs: Vec<OutputDest>,

    #[serde(default)]
    pub hotkey: Hotkey,
}

impl Profile {
    /// Runtime invariants beyond what serde already enforces. Called
    /// after a successful deserialize / migration.
    pub fn validate(&self) -> Result<(), ProfileError> {
        // The capture pipeline records the FLAC artifact at the native
        // rate (RFC: full-fidelity FLAC) and derives a 16 kHz mono ASR
        // branch from it. The accepted native rates are the common
        // capture rates; other values are rejected so the profile never
        // claims a rate the pipeline cannot honour.
        if !SUPPORTED_SAMPLE_RATES.contains(&self.recording.sample_rate) {
            return Err(ProfileError::Validation {
                profile: self.name.clone(),
                message: format!(
                    "sample_rate {} not supported; allowed native capture rates: {:?}",
                    self.recording.sample_rate, SUPPORTED_SAMPLE_RATES
                ),
            });
        }

        if !is_valid_language(&self.transcription.language) {
            return Err(ProfileError::Validation {
                profile: self.name.clone(),
                message: format!(
                    "language {:?} is not 'auto' nor a BCP-47-ish code",
                    self.transcription.language
                ),
            });
        }

        if self.transcription.model.is_empty() {
            return Err(ProfileError::Validation {
                profile: self.name.clone(),
                message: "model name must not be empty".into(),
            });
        }

        if self.sources.mic.is_empty() {
            return Err(ProfileError::Validation {
                profile: self.name.clone(),
                message: "sources.mic must not be empty".into(),
            });
        }

        if self.sources.system_output.is_empty() {
            // M2 review caught this as a High-severity surprise:
            // empty `system_output` previously got coerced to
            // "default" and silently captured system audio against
            // the profile's intent. M2 rejects the empty value
            // outright — mic-only pipeline lands in M3 alongside
            // the rate parameterisation. Until then, point users at
            // the explicit "default" they almost certainly want.
            return Err(ProfileError::Validation {
                profile: self.name.clone(),
                message: "sources.system_output = \"\" (mic-only mode) is not supported \
                          in this build (M3+); set `system_output = \"default\"` to \
                          capture the sink monitor, or pick a specific node"
                    .into(),
            });
        }

        if matches!(self.sources.mode, Mode::StereoSplit) {
            return Err(ProfileError::UnsupportedMode {
                mode: self.sources.mode,
            });
        }

        if !SUPPORTED_BACKENDS_M5.contains(&self.transcription.backend.as_str()) {
            return Err(ProfileError::BackendUnknown {
                backend: self.transcription.backend.as_str().to_owned(),
                supported: SUPPORTED_BACKENDS_M5,
            });
        }

        if matches!(self.transcription.backend, Backend::Deepgram) {
            if let Some(dg) = &self.transcription.deepgram {
                if dg.model.is_empty() {
                    return Err(ProfileError::Validation {
                        profile: self.name.clone(),
                        message: "transcription.deepgram.model must not be empty".into(),
                    });
                }
                if dg.timeout_s == 0 {
                    return Err(ProfileError::Validation {
                        profile: self.name.clone(),
                        message: "transcription.deepgram.timeout_s must be > 0".into(),
                    });
                }
                if dg.retry_total_budget_s == 0 {
                    return Err(ProfileError::Validation {
                        profile: self.name.clone(),
                        message: "transcription.deepgram.retry_total_budget_s must be > 0".into(),
                    });
                }
            }
        }

        if matches!(self.transcription.backend, Backend::WhisperCpp) {
            if let Some(settings) = &self.transcription.whisper_cpp {
                settings
                    .validate()
                    .map_err(|message| ProfileError::Validation {
                        profile: self.name.clone(),
                        message,
                    })?;
            }
        }

        for out in &self.outputs {
            if let OutputDest::File { path } = out {
                preflight_path_template(path).map_err(|message| ProfileError::Validation {
                    profile: self.name.clone(),
                    message,
                })?;
            }
        }

        Ok(())
    }

    /// Expand the first `OutputDest::File` entry to a concrete
    /// `PathBuf`, substituting `{timestamp}` and `{profile}` and the
    /// leading `~`. Returns `None` if no file output is declared.
    pub fn primary_output_path(&self) -> Option<PathBuf> {
        let template = self.outputs.iter().find_map(|o| match o {
            OutputDest::File { path } => Some(path.as_str()),
            _ => None,
        })?;
        Some(self.expand_template(template))
    }

    fn expand_template(&self, template: &str) -> PathBuf {
        let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
        let with_tokens = template
            .replace("{timestamp}", &timestamp)
            .replace("{profile}", &self.name);
        let expanded = shellexpand::tilde(&with_tokens).into_owned();
        PathBuf::from(expanded)
    }
}

fn preflight_path_template(template: &str) -> Result<(), String> {
    // Walk the template looking for `{token}` segments. Any token not
    // in the known set is a typo — surface it now rather than at
    // recording time when GStreamer would just write to a literal
    // path including the curly braces.
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated {{ in path template {template:?}"))?;
        let token = &after[..end];
        if !matches!(token, "timestamp" | "profile") {
            return Err(format!(
                "unknown path token {{{token}}} in {template:?} \
                 (allowed: {{timestamp}}, {{profile}})"
            ));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

fn is_valid_language(s: &str) -> bool {
    if s == "auto" {
        return true;
    }
    // ISO 639-1/-3 with optional region: cs / eng / cs-CZ / pt-BR.
    let parts: Vec<&str> = s.split('-').collect();
    let lang_ok = matches!(parts.first(), Some(p) if (2..=3).contains(&p.len())
        && p.chars().all(|c| c.is_ascii_lowercase()));
    let region_ok = parts
        .get(1)
        .is_none_or(|r| r.len() == 2 && r.chars().all(|c| c.is_ascii_uppercase()));
    lang_ok && region_ok && parts.len() <= 2
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ok_profile() -> Profile {
        Profile {
            schema_version: 1,
            name: "test".into(),
            description: String::new(),
            sources: Sources {
                mic: "default".into(),
                system_output: "default".into(),
                mode: Mode::MonoMix,
            },
            recording: Recording {
                codec: Codec::Flac,
                sample_rate: 16_000,
                max_duration_minutes: 60,
            },
            transcription: Transcription {
                backend: Backend::WhisperCpp,
                model: "small".into(),
                language: "auto".into(),
                auto: true,
                deepgram: None,
                whisper_cpp: None,
            },
            outputs: vec![OutputDest::File {
                path: "~/Recordings/zwhisper/{profile}/{timestamp}.flac".into(),
            }],
            hotkey: Hotkey::default(),
        }
    }

    #[test]
    fn validate_accepts_minimal_ok_profile() {
        ok_profile().validate().unwrap();
    }

    #[test]
    fn validate_rejects_bad_sample_rate() {
        let mut p = ok_profile();
        p.recording.sample_rate = 12_345;
        let err = p.validate().unwrap_err();
        assert!(matches!(err, ProfileError::Validation { .. }));
        assert!(err.to_string().contains("sample_rate"));
    }

    #[test]
    fn validate_rejects_empty_system_output_with_mic_only_message() {
        let mut p = ok_profile();
        p.sources.system_output.clear();
        let err = p.validate().unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ProfileError::Validation { .. }));
        assert!(msg.contains("mic-only"), "{msg}");
    }

    #[test]
    fn validate_accepts_native_capture_rates() {
        // RFC: the FLAC artifact is recorded at the native rate
        // (44.1/48 kHz) for full fidelity; the ASR branch normalizes to
        // 16 kHz. All three are valid.
        for rate in [16_000, 44_100, 48_000] {
            let mut p = ok_profile();
            p.recording.sample_rate = rate;
            assert!(p.validate().is_ok(), "{rate} should be accepted");
        }
    }

    #[test]
    fn validate_rejects_unsupported_sample_rate() {
        for rate in [22_050, 8_000, 96_000, 12_345] {
            let mut p = ok_profile();
            p.recording.sample_rate = rate;
            let err = p.validate().unwrap_err();
            assert!(matches!(err, ProfileError::Validation { .. }), "{rate}");
            assert!(err.to_string().contains("sample_rate"), "{rate}");
        }
    }

    #[test]
    fn validate_rejects_stereo_split_with_unsupported_mode() {
        let mut p = ok_profile();
        p.sources.mode = Mode::StereoSplit;
        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            ProfileError::UnsupportedMode {
                mode: Mode::StereoSplit
            }
        ));
    }

    #[test]
    fn validate_accepts_deepgram_backend_with_default_settings() {
        let mut p = ok_profile();
        p.transcription.backend = Backend::Deepgram;
        // deepgram block omitted → defaults are used; profile validates.
        p.validate().unwrap();
    }

    #[test]
    fn validate_accepts_deepgram_backend_with_explicit_settings() {
        let mut p = ok_profile();
        p.transcription.backend = Backend::Deepgram;
        p.transcription.deepgram = Some(DeepgramSettings::default());
        p.validate().unwrap();
    }

    #[test]
    fn validate_rejects_assemblyai_until_m6() {
        let mut p = ok_profile();
        p.transcription.backend = Backend::AssemblyAi;
        let err = p.validate().unwrap_err();
        assert!(matches!(err, ProfileError::BackendUnknown { .. }));
        assert!(err.to_string().contains("assemblyai"));
    }

    #[test]
    fn validate_rejects_openai_until_m6() {
        let mut p = ok_profile();
        p.transcription.backend = Backend::OpenAi;
        let err = p.validate().unwrap_err();
        assert!(matches!(err, ProfileError::BackendUnknown { .. }));
    }

    #[test]
    fn validate_rejects_empty_deepgram_model() {
        let mut p = ok_profile();
        p.transcription.backend = Backend::Deepgram;
        p.transcription.deepgram = Some(DeepgramSettings {
            model: String::new(),
            ..Default::default()
        });
        let err = p.validate().unwrap_err();
        assert!(matches!(err, ProfileError::Validation { .. }));
        assert!(err.to_string().contains("deepgram.model"));
    }

    #[test]
    fn validate_rejects_zero_deepgram_timeout() {
        let mut p = ok_profile();
        p.transcription.backend = Backend::Deepgram;
        p.transcription.deepgram = Some(DeepgramSettings {
            timeout_s: 0,
            ..Default::default()
        });
        let err = p.validate().unwrap_err();
        assert!(err.to_string().contains("timeout_s"));
    }

    #[test]
    fn whisper_profile_unchanged_after_m5() {
        // Existing whisper-cpp profiles must validate without
        // touching the new `deepgram` field — that is the whole
        // point of the field being `Option`.
        let p = ok_profile();
        assert!(matches!(p.transcription.backend, Backend::WhisperCpp));
        assert!(p.transcription.deepgram.is_none());
        p.validate().unwrap();
    }

    #[test]
    fn validate_accepts_whisper_cpp_settings() {
        let mut p = ok_profile();
        p.transcription.whisper_cpp = Some(WhisperCppSettings {
            threads: Some(16),
            processors: Some(1),
            no_gpu: true,
            flash_attn: Some(false),
            vad: true,
            vad_model: Some("/models/silero.bin".to_owned()),
            extra_args: vec!["--zen5-special".to_owned()],
            ..Default::default()
        });

        p.validate().unwrap();
    }

    #[test]
    fn validate_rejects_whisper_cpp_reserved_extra_args() {
        let mut p = ok_profile();
        p.transcription.whisper_cpp = Some(WhisperCppSettings {
            extra_args: vec!["--output-file".to_owned(), "/tmp/elsewhere".to_owned()],
            ..Default::default()
        });

        let err = p.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--output-file"), "{msg}");
    }

    #[test]
    fn validate_rejects_whisper_cpp_positional_extra_args() {
        let mut p = ok_profile();
        p.transcription.whisper_cpp = Some(WhisperCppSettings {
            extra_args: vec!["/tmp/other-input.wav".to_owned()],
            ..Default::default()
        });

        let err = p.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("option flags"), "{msg}");
    }

    #[test]
    fn whisper_cpp_settings_round_trip_via_toml() {
        let settings = WhisperCppSettings {
            threads: Some(16),
            duration_ms: Some(30_000),
            temperature: Some(0.2),
            prompt: Some("technical Czech dictation".to_owned()),
            no_gpu: true,
            vad: true,
            vad_threshold: Some(0.55),
            extra_args: vec!["--zen5-special".to_owned()],
            ..Default::default()
        };

        let toml = toml_edit::ser::to_string(&settings).unwrap();
        let parsed: WhisperCppSettings = toml_edit::de::from_str(&toml).unwrap();

        assert_eq!(settings, parsed);
    }

    #[test]
    fn deepgram_settings_round_trip_via_toml() {
        let dg = DeepgramSettings {
            model: "nova-3".to_owned(),
            diarize: true,
            language_detection: true,
            tier: Some("enhanced".to_owned()),
            timeout_s: 300,
            connect_timeout_s: 10,
            max_retries: 5,
            retry_total_budget_s: 120,
        };
        let toml = toml_edit::ser::to_string(&dg).unwrap();
        let parsed: DeepgramSettings = toml_edit::de::from_str(&toml).unwrap();
        assert_eq!(dg, parsed);
    }

    #[test]
    fn deepgram_settings_rejects_unknown_keys() {
        // deny_unknown_fields prevents typos like `diarise = true`
        // from silently no-op'ing.
        let bad = "model = \"nova-3\"\ndiarise = true\n";
        let res: Result<DeepgramSettings, _> = toml_edit::de::from_str(bad);
        assert!(res.is_err());
    }

    #[test]
    fn validate_rejects_unknown_path_token() {
        let mut p = ok_profile();
        p.outputs = vec![OutputDest::File {
            path: "/tmp/{wat}".into(),
        }];
        let err = p.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("{wat}"), "got: {msg}");
    }

    #[test]
    fn validate_rejects_unterminated_path_token() {
        let mut p = ok_profile();
        p.outputs = vec![OutputDest::File {
            path: "/tmp/{nope".into(),
        }];
        let err = p.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unterminated"), "got: {msg}");
    }

    #[test]
    fn primary_output_path_expands_tokens() {
        let p = ok_profile();
        let resolved = p.primary_output_path().expect("template present");
        let s = resolved.to_string_lossy();
        assert!(s.contains("test"), "{s}");
        assert!(!s.contains("{profile}"));
        assert!(!s.contains("{timestamp}"));
        assert!(!s.starts_with('~'), "tilde must be expanded: {s}");
    }

    #[test]
    fn language_validator_accepts_auto_and_iso_codes() {
        for ok in ["auto", "cs", "en", "eng", "pt-BR", "zh-CN"] {
            assert!(is_valid_language(ok), "{ok}");
        }
        for bad in ["", "CS", "english", "cs_CZ", "cs-cz", "12"] {
            assert!(!is_valid_language(bad), "{bad}");
        }
    }
}
