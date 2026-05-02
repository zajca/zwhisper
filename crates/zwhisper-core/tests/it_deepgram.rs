//! M5 Phase 4 — integration tests for the Deepgram backend.
//!
//! All tests run against a `wiremock` server on a loopback port; no
//! network egress to `api.deepgram.com` ever occurs. The backend is
//! constructed via [`DeepgramBatch::with_base_url`] +
//! [`DeepgramBatch::transcribe_file_with_key`] so process-global env
//! state is untouched and tests can run in parallel.

#![cfg(feature = "transcribe")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use zwhisper_core::profile::schema::DeepgramSettings;
use zwhisper_core::secrets::SecretString;
use zwhisper_core::transcribe::TranscribeError;
use zwhisper_core::transcribe::deepgram::DeepgramBatch;

const FIXTURE_KEY: &str = "sk-fixture-1234567890ABCDEFGHIJ";

fn settings() -> DeepgramSettings {
    DeepgramSettings {
        model: "nova-3".to_owned(),
        diarize: true,
        language_detection: false,
        tier: None,
        timeout_s: 30,
        connect_timeout_s: 5,
        max_retries: 3,
        retry_total_budget_s: 10,
    }
}

fn opts(language: &str) -> zwhisper_core::transcribe::TranscribeOpts {
    zwhisper_core::transcribe::TranscribeOpts {
        backend: "deepgram".to_owned(),
        model: "nova-3".to_owned(),
        language: language.to_owned(),
        ..Default::default()
    }
}

async fn make_audio() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("clip.flac");
    // Body content does not matter — wiremock matchers don't check it
    // beyond accepting any body. We send a recognisable byte pattern
    // so a serialization regression would surface.
    let bytes: Vec<u8> = (0u32..2048).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
    tokio::fs::write(&path, &bytes).await.unwrap();
    (dir, path)
}

fn diarized_response_body() -> String {
    std::fs::read_to_string("tests/fixtures/deepgram/diarized_response.json").unwrap()
}

#[tokio::test]
async fn end_to_end_against_mock_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .and(header("authorization", format!("Token {FIXTURE_KEY}").as_str()))
        .and(header("content-type", "audio/flac"))
        .and(query_param("model", "nova-3"))
        .and(query_param("diarize", "true"))
        .and(query_param("smart_format", "true"))
        .and(query_param("paragraphs", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_string(diarized_response_body()))
        .mount(&server)
        .await;

    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;

    let artifacts = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap();

    // M5 DoD #1 — transcript artifacts produced.
    assert!(artifacts.txt_path.is_file(), "txt missing");
    assert!(artifacts.json_path.is_file(), "json missing");
    assert_eq!(artifacts.language, "en");
    assert_eq!(artifacts.model, "nova-3");
    assert_eq!(artifacts.audio_duration, Duration::from_millis(4320));

    let txt = std::fs::read_to_string(&artifacts.txt_path).unwrap();
    assert!(
        txt.contains("Hello there how are you fine thanks"),
        "transcript text mismatch: {txt}"
    );

    // M5 DoD #6 — speakers populated, JSON envelope contains them.
    let speakers = artifacts.speakers.as_ref().expect("speakers Some");
    assert!(speakers.len() >= 2, "expected ≥2 speaker segments: {speakers:?}");
    assert_eq!(speakers[0].speaker_id, 0);
    assert_eq!(speakers[1].speaker_id, 1);

    let json_body = std::fs::read_to_string(&artifacts.json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_body).unwrap();
    assert!(parsed.get("speakers").is_some(), "envelope missing speakers");
    assert_eq!(parsed["backend"], "deepgram");
    assert_eq!(parsed["model"], "nova-3");
}

/// User feedback #3 (2026-05-02): when the request asked for
/// autodetect, the resulting `TranscriptArtifacts.language` and the
/// JSON envelope MUST reflect the language Deepgram actually used,
/// not the literal "auto" the caller sent in. The previous build
/// echoed back `opts.language`, which is a documented contract
/// violation (`TranscribeOpts.language` is "after autodetect").
#[tokio::test]
async fn detected_language_overwrites_opts_language_when_autodetect() {
    let server = MockServer::start().await;
    let body = r#"{
      "metadata": {"duration": 1.5},
      "results": {
        "channels": [{
          "detected_language": "cs",
          "alternatives": [{
            "transcript": "ahoj",
            "words": [{"word":"ahoj","start":0.0,"end":1.0,"speaker":0}]
          }]
        }]
      }
    }"#;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;
    let artifacts = backend
        .transcribe_file_with_key(
            &audio,
            &opts("auto"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(artifacts.language, "cs");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifacts.json_path).unwrap()).unwrap();
    assert_eq!(json["language"], "cs");
}

#[tokio::test]
async fn opts_language_used_when_response_omits_detected_language() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(200).set_body_string(diarized_response_body()))
        .mount(&server)
        .await;
    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;
    let artifacts = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap();
    // Fixture has no `detected_language` → fall back to opts.language.
    assert_eq!(artifacts.language, "en");
}

#[tokio::test]
async fn auto_language_emits_detect_language_query_param() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .and(query_param("detect_language", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_string(diarized_response_body()))
        .mount(&server)
        .await;

    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;
    backend
        .transcribe_file_with_key(
            &audio,
            &opts("auto"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn auth_failure_maps_to_backend_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"err_code":"INVALID_AUTH"}"#),
        )
        .mount(&server)
        .await;

    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;

    let err = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap_err();
    match err {
        TranscribeError::BackendAuth { backend, status } => {
            assert_eq!(backend, "deepgram");
            assert_eq!(status, 401);
        }
        other => panic!("expected BackendAuth, got {other:?}"),
    }
}

/// Post-review fix (user-feedback #2, 2026-05-02): when the wall-clock
/// budget is exhausted *while we still hold a server response*, the
/// error must classify by status (`BackendQuota` for 429, etc.) — NOT
/// fall through to `BackendTimeout`. The previous build leaked rate
/// limits as local timeouts.
/// User feedback #1 (2026-05-02): the wall-clock retry budget MUST
/// also bound the body read. Mock server delivers headers fast but
/// stalls the body well beyond the configured budget — the call
/// must fail with `BackendTimeout` within the budget window, not
/// after `timeout_s` (which is 30 s in the fixture).
#[tokio::test]
async fn slow_body_does_not_overshoot_retry_budget() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        // 10 s body delay vs. 2 s budget → must trip the budget.
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(10))
                .set_body_string(diarized_response_body()),
        )
        .mount(&server)
        .await;
    let mut s = settings();
    s.retry_total_budget_s = 2;
    s.timeout_s = 60;
    s.max_retries = 0;
    let backend = DeepgramBatch::with_base_url(s, server.uri());
    let (_dir, audio) = make_audio().await;

    let started = std::time::Instant::now();
    let err = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "body read overshot budget; elapsed = {elapsed:?}"
    );
    assert!(
        matches!(err, TranscribeError::BackendTimeout { .. }),
        "expected BackendTimeout, got {err:?}"
    );
}

#[tokio::test]
async fn budget_exhausted_429_classifies_as_quota_not_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30") // long enough to bust the budget
                .set_body_string(r#"{"err_code":"RATE_LIMIT"}"#),
        )
        .mount(&server)
        .await;

    let mut s = settings();
    // Budget too small to ever sleep through Retry-After.
    s.retry_total_budget_s = 1;
    s.max_retries = 5;
    let backend = DeepgramBatch::with_base_url(s, server.uri());
    let (_dir, audio) = make_audio().await;

    let err = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap_err();
    match err {
        TranscribeError::BackendQuota { backend, status, .. } => {
            assert_eq!(backend, "deepgram");
            assert_eq!(status, 429);
        }
        TranscribeError::BackendTimeout { .. } => {
            panic!("regression: 429 + busted budget reclassified as Timeout");
        }
        other => panic!("expected BackendQuota, got {other:?}"),
    }
}

#[tokio::test]
async fn quota_failure_429_is_retried_then_classified() {
    let server = MockServer::start().await;
    // wiremock evaluates mocks in registration order; first-match
    // wins per request. Set up "fail 3 times, then …" via expect()
    // on a single mock with `up_to_n_times`.
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(99)
        .mount(&server)
        .await;

    let mut s = settings();
    s.max_retries = 2; // 1 + 2 retries = 3 attempts max
    s.retry_total_budget_s = 6;
    let backend = DeepgramBatch::with_base_url(s, server.uri());
    let (_dir, audio) = make_audio().await;

    let err = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap_err();
    match err {
        TranscribeError::BackendQuota {
            backend,
            status,
            retry_after_s: _,
            ..
        } => {
            assert_eq!(backend, "deepgram");
            assert_eq!(status, 429);
        }
        other => panic!("expected BackendQuota, got {other:?}"),
    }
}

#[tokio::test]
async fn fivexx_then_success_succeeds_after_retry() {
    let server = MockServer::start().await;
    // First responder: 503 once.
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Then 200.
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(200).set_body_string(diarized_response_body()))
        .mount(&server)
        .await;

    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;
    let artifacts = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap();
    assert!(artifacts.speakers.is_some());
}

#[tokio::test]
async fn bad_request_400_is_not_retried() {
    let server = MockServer::start().await;
    let mock = Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad params"))
        .expect(1) // crucial: exactly one attempt, no retries on 400
        .mount_as_scoped(&server)
        .await;

    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;
    let err = backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap_err();
    match err {
        TranscribeError::BackendBadResponse { status, body_excerpt, .. } => {
            assert_eq!(status, 400);
            assert!(body_excerpt.contains("bad params"));
        }
        other => panic!("expected BackendBadResponse, got {other:?}"),
    }
    drop(mock);
}

/// `DoD` #9: API key must NEVER appear in any captured tracing line at
/// any level. We attach a string subscriber, run a full happy-path
/// transcribe, and grep the captured buffer for the fixture key.
#[tokio::test]
#[tracing_test::traced_test]
async fn api_key_never_appears_in_logs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(200).set_body_string(diarized_response_body()))
        .mount(&server)
        .await;

    let backend = DeepgramBatch::with_base_url(settings(), server.uri());
    let (_dir, audio) = make_audio().await;
    backend
        .transcribe_file_with_key(
            &audio,
            &opts("en"),
            SecretString::new(FIXTURE_KEY.to_owned()),
        )
        .await
        .unwrap();

    // The traced_test macro injects a per-test subscriber; we read
    // back the captured output via `logs_assert` / `logs_contain`.
    // Use the negative form: assert the key NEVER showed up.
    assert!(
        !logs_contain(FIXTURE_KEY),
        "API key fixture leaked into a tracing event"
    );
    assert!(
        !logs_contain("ABCDEFGHIJ"),
        "tail of the API key leaked into tracing"
    );
}
