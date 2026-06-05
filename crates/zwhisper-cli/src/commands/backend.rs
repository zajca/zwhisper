//! `zwhisper-cli backend …` — direct probes against cloud backends.
//!
//! Bypasses the daemon and the `Recorder1` D-Bus surface entirely.
//! Reads the API key from the same resolution chain the recorder
//! uses (`ZWHISPER_<BACKEND>_API_KEY` env → `secrets.toml` mode 0600).
//!
//! M5 ships `health` only. The Deepgram health probe is a single
//! HTTP `GET https://api.deepgram.com/v1/projects` with the resolved
//! key — Deepgram's cheapest authenticated endpoint, so it costs no
//! credit and surfaces the same auth/quota/network classification as
//! `transcribe_file`.

use std::time::Duration;

use color_eyre::eyre::eyre;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{ClientBuilder, StatusCode};
use tracing::info;

use zwhisper_core::secrets::{ResolveSource, SecretString, resolve_api_key};

use super::{EXIT_OK, EXIT_PROTOCOL_ERROR, build_runtime};
use crate::cli::BackendCmd;

const DEEPGRAM_HEALTH_URL: &str = "https://api.deepgram.com/v1/projects";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn run(cmd: &BackendCmd) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    rt.block_on(run_async(cmd))
}

async fn run_async(cmd: &BackendCmd) -> color_eyre::Result<()> {
    match cmd {
        BackendCmd::Health { backend } => health(backend).await,
        BackendCmd::List => {
            list_backends();
            Ok(())
        }
    }
}

/// Print every backend id with its compile-time availability in this
/// build. The discoverable counterpart to the wizard's hard-fail: a user
/// who configures a `parakeet` profile can run this to see, without
/// attempting a transcribe, whether the running binary can actually use
/// it — and exactly which feature flag is missing if not.
fn list_backends() {
    use zwhisper_core::profile::schema::Backend;

    const ALL: [Backend; 5] = [
        Backend::WhisperCpp,
        Backend::Deepgram,
        Backend::Parakeet,
        Backend::AssemblyAi,
        Backend::OpenAi,
    ];

    println!("backends in this build:");
    for backend in ALL {
        let status = if backend.is_compiled_in() {
            "compiled-in".to_owned()
        } else {
            match backend.required_feature() {
                Some(feature) => format!("MISSING — rebuild with `--features {feature}`"),
                None => "not implemented".to_owned(),
            }
        };
        println!("  {:<12} {status}", backend.as_str());
    }
}

async fn health(backend: &str) -> color_eyre::Result<()> {
    if backend != "deepgram" {
        // Whisper-cpp is a local backend; "health" there means
        // "binary discoverable" which `transcribe` already reports
        // via `BackendUnavailable`. Cloud backends other than
        // Deepgram (`assemblyai`, `openai`) are not yet implemented.
        return Err(eyre!(
            "backend `{backend}` does not have a health probe in this build (M5 ships deepgram only)"
        ));
    }

    let (key, source) = match resolve_api_key("deepgram") {
        Ok(pair) => pair,
        Err(err) => {
            // Reuse the same exit-code semantics as the daemon path:
            // missing key is a user-facing protocol error (exit 2),
            // not a backend failure (exit 1).
            print_status("FAIL", "key not resolved", &err.to_string());
            std::process::exit(EXIT_PROTOCOL_ERROR);
        }
    };

    let outcome = probe_deepgram(&key).await;
    drop(key); // SecretString::Drop zeroizes immediately.

    match outcome {
        HealthOutcome::Ok { detail } => {
            print_status("OK", source_label(&source), &detail);
            info!(target: "zwhisper_cli::backend", backend = %backend, "health probe OK");
            std::process::exit(EXIT_OK);
        }
        HealthOutcome::AuthFailed { status } => {
            print_status(
                "AUTH_FAILED",
                source_label(&source),
                &format!("HTTP {status}; rotate the key on console.deepgram.com"),
            );
            std::process::exit(EXIT_PROTOCOL_ERROR);
        }
        HealthOutcome::Quota {
            status,
            retry_after_s,
        } => {
            print_status(
                "QUOTA",
                source_label(&source),
                &format!(
                    "HTTP {status}{}",
                    retry_after_s
                        .map(|s| format!(", retry-after {s}s"))
                        .unwrap_or_default(),
                ),
            );
            std::process::exit(EXIT_PROTOCOL_ERROR);
        }
        HealthOutcome::BadResponse { status, excerpt } => {
            print_status(
                "BAD_RESPONSE",
                source_label(&source),
                &format!("HTTP {status}: {excerpt}"),
            );
            std::process::exit(EXIT_PROTOCOL_ERROR);
        }
        HealthOutcome::Network { reason } => {
            print_status("NETWORK", source_label(&source), &reason);
            std::process::exit(EXIT_PROTOCOL_ERROR);
        }
    }
}

#[derive(Debug)]
enum HealthOutcome {
    Ok {
        detail: String,
    },
    AuthFailed {
        status: u16,
    },
    Quota {
        status: u16,
        retry_after_s: Option<u64>,
    },
    BadResponse {
        status: u16,
        excerpt: String,
    },
    Network {
        reason: String,
    },
}

async fn probe_deepgram(key: &SecretString) -> HealthOutcome {
    let client = match ClientBuilder::new()
        .timeout(HEALTH_TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .use_rustls_tls()
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return HealthOutcome::Network {
                reason: e.to_string(),
            };
        }
    };

    let Ok(mut auth) = HeaderValue::from_str(&format!("Token {}", key.expose_secret())) else {
        return HealthOutcome::Network {
            reason: "API key contains characters not valid in an HTTP header".to_owned(),
        };
    };
    auth.set_sensitive(true);

    let response = match client
        .get(DEEPGRAM_HEALTH_URL)
        .header(AUTHORIZATION, auth)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) if err.is_timeout() => {
            return HealthOutcome::Network {
                reason: format!("timeout after {}s", HEALTH_TIMEOUT.as_secs()),
            };
        }
        Err(err) => {
            return HealthOutcome::Network {
                reason: err.to_string(),
            };
        }
    };

    let status = response.status();
    if status.is_success() {
        // Body is small (a few projects); reading it gives the user
        // a concrete confirmation that the key really worked.
        let body = response.text().await.unwrap_or_default();
        let project_hint = parse_project_hint(&body);
        return HealthOutcome::Ok {
            detail: project_hint.unwrap_or_else(|| "key valid".to_owned()),
        };
    }

    classify_failure(status, response).await
}

async fn classify_failure(status: StatusCode, resp: reqwest::Response) -> HealthOutcome {
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.text().await.unwrap_or_default();
    let excerpt = truncate(&body, 256);
    match status.as_u16() {
        401 | 403 => HealthOutcome::AuthFailed {
            status: status.as_u16(),
        },
        402 | 429 => HealthOutcome::Quota {
            status: status.as_u16(),
            retry_after_s: retry_after,
        },
        _ => HealthOutcome::BadResponse {
            status: status.as_u16(),
            excerpt,
        },
    }
}

/// Best-effort grep through the Deepgram response for the first
/// `project_id` and `name` fields so the user gets a confirmation
/// like `OK (project "default", id=…)`. Failure is silent — the
/// outer caller still returns OK because the auth check passed.
fn parse_project_hint(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let projects = value.get("projects")?.as_array()?;
    let first = projects.first()?;
    let name = first.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    Some(format!("project \"{name}\""))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

fn print_status(verdict: &str, source: &str, detail: &str) {
    println!("deepgram: {verdict} (source: {source}) — {detail}");
}

fn source_label(src: &ResolveSource) -> &'static str {
    match src {
        ResolveSource::Env(_) => "env",
        ResolveSource::File(_) => "secrets.toml",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 256), "hello");
    }

    #[test]
    fn truncate_long_string_appends_ellipsis() {
        let s = "x".repeat(300);
        let cut = truncate(&s, 256);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= 260);
    }

    #[test]
    fn parse_project_hint_extracts_first_name() {
        let body = r#"{"projects":[{"project_id":"abc","name":"work"},{"project_id":"def","name":"side"}]}"#;
        assert_eq!(
            parse_project_hint(body).as_deref(),
            Some("project \"work\"")
        );
    }

    #[test]
    fn parse_project_hint_handles_empty_array() {
        assert!(parse_project_hint(r#"{"projects":[]}"#).is_none());
    }

    #[test]
    fn parse_project_hint_handles_unknown_shape() {
        assert!(parse_project_hint(r#"{"unrelated":true}"#).is_none());
        assert!(parse_project_hint("not json").is_none());
    }
}
