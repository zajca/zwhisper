use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::error::{ProfileError, SUPPORTED_BACKENDS_M2};

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
            Self::AssemblyAi => "assemblyai",
            Self::OpenAi => "openai",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcription {
    pub backend: Backend,
    pub model: String,
    pub language: String,
    /// Run transcription automatically after recording stops.
    pub auto: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        // M2 ships the M0 mono 16 kHz pipeline only — accepting
        // 44.1/48 kHz at the schema level while the recorder
        // hardcodes 16 kHz would silently lie about what was
        // captured. 44.1/48 kHz land in M3 alongside the pipeline
        // rate parameterisation. Until then we reject with a typed
        // error pointing at the right knob.
        if self.recording.sample_rate != 16_000 {
            return Err(ProfileError::Validation {
                profile: self.name.clone(),
                message: format!(
                    "sample_rate {} not supported in this build \
                     (M2 ships 16000 only; 44100/48000 land in M3 \
                     alongside the pipeline rate parameterisation)",
                    self.recording.sample_rate
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

        if !matches!(self.transcription.backend, Backend::WhisperCpp) {
            return Err(ProfileError::BackendUnknown {
                backend: self.transcription.backend.as_str().to_owned(),
                supported: SUPPORTED_BACKENDS_M2,
            });
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
    fn validate_rejects_44100_and_48000_in_m2() {
        // M2 only honours 16 kHz. Accepting higher rates at the
        // schema level while the engine hardcodes 16 kHz silently
        // lies about the captured audio — the M2 review's Medium
        // finding. 44.1/48 kHz land in M3.
        for rate in [44_100, 48_000, 22_050] {
            let mut p = ok_profile();
            p.recording.sample_rate = rate;
            let err = p.validate().unwrap_err();
            assert!(matches!(err, ProfileError::Validation { .. }), "{rate}");
            let msg = err.to_string();
            assert!(msg.contains("M2 ships 16000 only"), "{msg}");
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
    fn validate_rejects_non_whisper_backend_in_m2() {
        let mut p = ok_profile();
        p.transcription.backend = Backend::Deepgram;
        let err = p.validate().unwrap_err();
        assert!(matches!(err, ProfileError::BackendUnknown { .. }));
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
