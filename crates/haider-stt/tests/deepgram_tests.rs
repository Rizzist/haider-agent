//! Deepgram engine laws: URL/auth pins, key validation, the streaming-model
//! filter, message semantics, and the live WS session contract.

#![allow(clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{CannedResponse, spawn_http_fixture};
use futures_util::{SinkExt as _, StreamExt as _};
use haider_stt::capture::encode_linear16;
use haider_stt::deepgram::{
    CLOSE_TIMEOUT_SECS, CONNECT_TIMEOUT_SECS, DEEPGRAM_LISTEN_WS_URL, DEFAULT_MODEL,
    DeepgramSessionConfig, KEEPALIVE_INTERVAL_SECS, MAX_API_KEY_LENGTH, MAX_SESSION_SECS,
    clean_api_key, clean_language, error_from_body, fetch_streaming_models, handle_realtime_text,
    is_flux_model, realtime_url, start_session, validate_key,
};
use haider_stt::{EngineKind, SttError, TranscriptFrame};

/// The production endpoint constants are pinned (doc-verified surface).
#[test]
fn deepgram_constants_are_pinned() {
    assert_eq!(DEEPGRAM_LISTEN_WS_URL, "wss://api.deepgram.com/v1/listen");
    assert_eq!(DEFAULT_MODEL, "nova-3");
    assert_eq!(CONNECT_TIMEOUT_SECS, 10);
    assert_eq!(CLOSE_TIMEOUT_SECS, 8);
    assert_eq!(MAX_API_KEY_LENGTH, 512);
    assert_eq!(
        MAX_SESSION_SECS, 900,
        "a stuck-open stream bills per minute"
    );
    assert!(
        (3..=5).contains(&KEEPALIVE_INTERVAL_SECS),
        "KeepAlive must ride Deepgram's documented 3-5 s window"
    );
}

/// The `/v1/listen` URL is the ADE construction with the SELECTED model:
/// exact parameter set, order, and values.
///
/// MUTATION CHECK: drop `interim_results=true` (no partials) or hardcode
/// `nova-3` (ignore the selection). Expected runtime failure: the literal
/// URL pin below.
#[test]
fn realtime_url_pins_the_exact_query() {
    assert_eq!(
        realtime_url(DEEPGRAM_LISTEN_WS_URL, "nova-3-medical", "en-US", 48_000),
        "wss://api.deepgram.com/v1/listen?model=nova-3-medical&language=en-US&encoding=linear16&sample_rate=48000&channels=1&interim_results=true&smart_format=true"
    );
    // Reserved characters in a model name are percent-encoded, never raw.
    assert_eq!(
        realtime_url("wss://h", "a b&c", "en", 16_000),
        "wss://h?model=a%20b%26c&language=en&encoding=linear16&sample_rate=16000&channels=1&interim_results=true&smart_format=true"
    );
}

/// Key hygiene (ADE `clean_deepgram_api_key`): trimmed, ≤512, no control
/// bytes — and the refusal text never echoes the key.
#[test]
fn api_key_hygiene_is_enforced_without_echoing_the_key() {
    assert_eq!(clean_api_key("  sk-ok  ").expect("trimmed"), "sk-ok");
    assert!(clean_api_key("").is_err());
    assert!(clean_api_key(&"x".repeat(513)).is_err());
    let error = clean_api_key("bad\u{7}key-sentinel-1f2e").expect_err("control byte");
    let message = error.to_string();
    assert!(
        !message.contains("sentinel-1f2e"),
        "refusals must never echo key material: {message}"
    );
}

/// Language hygiene (ADE `clean_deepgram_language`).
#[test]
fn language_hygiene_matches_the_ade() {
    assert_eq!(clean_language(None).expect("default"), "en");
    assert_eq!(
        clean_language(Some("  ".into())).expect("blank → default"),
        "en"
    );
    assert_eq!(clean_language(Some("en-US".into())).expect("tag"), "en-US");
    assert!(clean_language(Some("english language".into())).is_err());
    assert!(clean_language(Some("x".repeat(25))).is_err());
}

/// Error extraction precedence: `err_msg` → `message` → `error`.
#[test]
fn error_body_precedence_is_err_msg_then_message_then_error() {
    let body = serde_json::json!({"err_msg": "first", "message": "second", "error": "third"});
    assert_eq!(error_from_body(&body).as_deref(), Some("first"));
    let body = serde_json::json!({"message": "second", "error": "third"});
    assert_eq!(error_from_body(&body).as_deref(), Some("second"));
    let body = serde_json::json!({"error": "third"});
    assert_eq!(error_from_body(&body).as_deref(), Some("third"));
    assert_eq!(error_from_body(&serde_json::json!({})), None);
}

/// Message semantics (ADE port): finals accumulate, interims overwrite,
/// empty transcripts are skipped, `type: Error` is a typed failure.
#[test]
fn realtime_text_semantics_accumulate_finals_and_overwrite_interims() {
    let mut finals = Vec::new();
    let mut interim = String::new();
    let frame = handle_realtime_text(
        r#"{"type":"Results","channel":{"alternatives":[{"transcript":"hel"}]},"is_final":false}"#,
        &mut finals,
        &mut interim,
    )
    .expect("interim parses")
    .expect("interim frame");
    assert_eq!((frame.text.as_str(), frame.is_final), ("hel", false));
    assert_eq!(interim, "hel");
    let frame = handle_realtime_text(
        r#"{"type":"Results","channel":{"alternatives":[{"transcript":"hello world"}]},"is_final":true,"speech_final":true}"#,
        &mut finals,
        &mut interim,
    )
    .expect("final parses")
    .expect("final frame");
    assert!(frame.is_final && frame.speech_final);
    assert_eq!(finals, vec!["hello world"]);
    assert!(interim.is_empty(), "a final clears the interim");
    // Empty transcripts and metadata frames emit nothing.
    assert!(
        handle_realtime_text(
            r#"{"type":"Results","channel":{"alternatives":[{"transcript":"  "}]}}"#,
            &mut finals,
            &mut interim,
        )
        .expect("empty parses")
        .is_none()
    );
    assert!(
        handle_realtime_text(r#"{"type":"Metadata"}"#, &mut finals, &mut interim)
            .expect("metadata parses")
            .is_none()
    );
    // A server error is typed and carries the err_msg text.
    let error = handle_realtime_text(
        r#"{"type":"Error","err_msg":"NET-0001 timeout"}"#,
        &mut finals,
        &mut interim,
    )
    .expect_err("server error is a failure");
    assert!(matches!(error, SttError::Endpoint(message) if message.contains("NET-0001")));
}

const MODELS_FIXTURE: &str = r#"{
  "stt": [
    {"name": "nova-3", "canonical_name": "nova-3-general", "architecture": "nova-3",
     "languages": ["en"], "version": "2026-01-01", "uuid": "u-1", "batch": true, "streaming": true},
    {"name": "whisper-large", "canonical_name": "whisper-large", "architecture": "whisper",
     "languages": ["en"], "version": "1", "uuid": "u-2", "batch": true, "streaming": false},
    {"name": "flux-general-en", "canonical_name": "flux-general-en", "architecture": "flux",
     "languages": ["en"], "version": "1", "uuid": "u-3", "batch": false, "streaming": true},
    {"name": "nova-2", "canonical_name": "nova-2-general", "architecture": "nova-2",
     "languages": ["en"], "version": "1", "uuid": "u-4", "batch": true, "streaming": true}
  ],
  "tts": [{"name": "aura-2", "streaming": true}]
}"#;

/// THE MODEL-FILTER LAW: `/v1/models` keeps only `streaming: true` STT rows
/// and excludes Flux — batch-only `whisper-*` and `/v2/listen`-only Flux
/// must never reach the dictation picker.
///
/// MUTATION CHECK: drop the `streaming` filter or the Flux exclusion.
/// Expected runtime failure: `whisper-large` or `flux-general-en` appears
/// in the surviving list below.
#[tokio::test]
async fn model_fetch_filters_streaming_true_and_excludes_flux() {
    let fixture = spawn_http_fixture(vec![(
        "/v1/models".to_owned(),
        CannedResponse::ok_json(MODELS_FIXTURE),
    )])
    .await;
    let client = reqwest::Client::new();
    let models = fetch_streaming_models(&client, &fixture.origin, "sk-test")
        .await
        .expect("fetch succeeds");
    let names: Vec<&str> = models.iter().map(|model| model.name.as_str()).collect();
    assert_eq!(names, vec!["nova-3", "nova-2"]);
    // The request carried Token auth (never Bearer for raw keys).
    let seen = fixture.seen.lock().expect("seen requests");
    assert!(
        seen.iter()
            .any(|head| head.contains("Authorization: Token sk-test")
                || head.contains("authorization: Token sk-test")),
        "the models fetch must authenticate with `Token <key>`: {seen:?}"
    );
}

/// Flux detection covers architecture and name spellings.
#[test]
fn flux_detection_covers_architecture_and_names() {
    let flux = serde_json::from_value::<haider_stt::deepgram::DeepgramModel>(serde_json::json!({
        "name": "flux-general-multi", "architecture": "flux", "streaming": true
    }))
    .expect("decode");
    assert!(is_flux_model(&flux));
    let nova = serde_json::from_value::<haider_stt::deepgram::DeepgramModel>(serde_json::json!({
        "name": "nova-3", "architecture": "nova-3", "streaming": true
    }))
    .expect("decode");
    assert!(!is_flux_model(&nova));
}

/// Key validation rides `GET /v1/auth/token`: 200 valid, 401 typed
/// Unauthorized, other statuses a typed endpoint error.
#[tokio::test]
async fn key_validation_maps_statuses_to_typed_errors() {
    let fixture = spawn_http_fixture(vec![(
        "/v1/auth/token".to_owned(),
        CannedResponse::ok_json(r#"{"key":"ok"}"#),
    )])
    .await;
    let client = reqwest::Client::new();
    validate_key(&client, &fixture.origin, "sk-valid")
        .await
        .expect("200 is valid");
    let seen = fixture.seen.lock().expect("seen").join("\n");
    assert!(seen.contains("GET /v1/auth/token"), "{seen}");

    let rejecting = spawn_http_fixture(vec![(
        "/v1/auth/token".to_owned(),
        CannedResponse::status_only(401),
    )])
    .await;
    let error = validate_key(&client, &rejecting.origin, "sk-bad")
        .await
        .expect_err("401 fails");
    assert!(matches!(error, SttError::Unauthorized(_)));

    let broken = spawn_http_fixture(vec![(
        "/v1/auth/token".to_owned(),
        CannedResponse::status_only(500),
    )])
    .await;
    let error = validate_key(&client, &broken.origin, "sk-any")
        .await
        .expect_err("500 fails");
    assert!(matches!(error, SttError::Endpoint(_)));
}

struct WsFixture {
    url: String,
    captured: Arc<std::sync::Mutex<Option<(String, String)>>>,
    binary: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    keepalives: Arc<std::sync::atomic::AtomicUsize>,
    close_stream_seen: Arc<std::sync::atomic::AtomicBool>,
}

/// A scripted `/v1/listen` fixture. `speak` controls whether the server
/// pushes the canned interim/final transcript sequence after the handshake.
#[allow(clippy::result_large_err)] // tungstenite's ErrorResponse rides the accept callback signature
async fn spawn_ws_fixture(speak: bool) -> WsFixture {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::protocol::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback WS fixture");
    let addr = listener.local_addr().expect("ws addr");
    let captured: Arc<std::sync::Mutex<Option<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let binary: Arc<std::sync::Mutex<Vec<Vec<u8>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let keepalives = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let close_stream_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let captured_for_task = Arc::clone(&captured);
    let binary_for_task = Arc::clone(&binary);
    let keepalives_for_task = Arc::clone(&keepalives);
    let close_seen_for_task = Arc::clone(&close_stream_seen);
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let captured = Arc::clone(&captured_for_task);
        let callback = move |request: &Request, response: Response| {
            let auth = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            if let Ok(mut slot) = captured.lock() {
                *slot = Some((request.uri().to_string(), auth));
            }
            Ok(response)
        };
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
            return;
        };
        if speak {
            let _ = ws
                .send(Message::Text(
                    r#"{"type":"Results","channel":{"alternatives":[{"transcript":"hello"}]},"is_final":false}"#.into(),
                ))
                .await;
            let _ = ws
                .send(Message::Text(
                    r#"{"type":"Results","channel":{"alternatives":[{"transcript":"hello world"}]},"is_final":true,"speech_final":true}"#.into(),
                ))
                .await;
        }
        while let Some(Ok(message)) = ws.next().await {
            match message {
                Message::Binary(bytes) => {
                    if let Ok(mut sink) = binary_for_task.lock() {
                        sink.push(bytes.to_vec());
                    }
                }
                Message::Text(text) => {
                    if text.contains("KeepAlive") {
                        keepalives_for_task.fetch_add(1, Ordering::SeqCst);
                    }
                    if text.contains("CloseStream") {
                        close_seen_for_task.store(true, Ordering::SeqCst);
                        if speak {
                            let _ = ws
                                .send(Message::Text(
                                    r#"{"type":"Results","channel":{"alternatives":[{"transcript":"and goodbye"}]},"is_final":true,"speech_final":true}"#.into(),
                                ))
                                .await;
                        }
                        let _ = ws.send(Message::Close(None)).await;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    WsFixture {
        url: format!("ws://{addr}/v1/listen"),
        captured,
        binary,
        keepalives,
        close_stream_seen,
    }
}

fn session_config(fixture_url: &str) -> DeepgramSessionConfig {
    let mut config =
        DeepgramSessionConfig::new("sk-ws-test", "nova-3", "en", 48_000).expect("valid config");
    config.ws_url = fixture_url.to_owned();
    config.close_timeout = std::time::Duration::from_secs(2);
    config
}

/// THE SESSION CONTRACT: Token-auth handshake with the pinned query, i16 LE
/// binary audio frames, interim + final transcript frames on the event
/// stream, CloseStream + bounded drain collecting late finals, and the
/// assembled joined-finals result.
///
/// MUTATION CHECK: send audio as text frames, skip the CloseStream drain,
/// or join finals with the interim included. Expected runtime failure: the
/// server-side byte pin, the missing `and goodbye` late final, or the
/// result-text pin below.
#[tokio::test]
async fn ws_session_streams_audio_and_assembles_joined_finals() {
    let fixture = spawn_ws_fixture(true).await;
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<TranscriptFrame>();
    let session = start_session(session_config(&fixture.url), events_tx)
        .await
        .expect("handshake succeeds");
    let pcm = encode_linear16(&[0.5, -0.25, 0.125]);
    session.send_audio(pcm.clone());
    // Let the transcript frames arrive before closing.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let result = session.finish().await.expect("session settles");
    assert_eq!(result.text, "hello world and goodbye");
    assert_eq!(result.segments, 2);
    // Handshake truth: pinned query + Token auth.
    let (uri, auth) = fixture
        .captured
        .lock()
        .expect("captured")
        .clone()
        .expect("handshake captured");
    assert!(
        uri.contains("model=nova-3")
            && uri.contains("language=en")
            && uri.contains("encoding=linear16")
            && uri.contains("sample_rate=48000")
            && uri.contains("channels=1")
            && uri.contains("interim_results=true")
            && uri.contains("smart_format=true"),
        "{uri}"
    );
    assert_eq!(auth, "Token sk-ws-test");
    // The audio arrived as ONE binary frame with the exact PCM bytes.
    assert_eq!(fixture.binary.lock().expect("binary").as_slice(), &[pcm]);
    assert!(fixture.close_stream_seen.load(Ordering::SeqCst));
    // Event stream: interim then finals, all Deepgram-tagged.
    let mut frames = Vec::new();
    while let Ok(frame) = events_rx.try_recv() {
        frames.push(frame);
    }
    assert!(
        frames
            .iter()
            .all(|frame| frame.provider == EngineKind::Deepgram)
    );
    let texts: Vec<(&str, bool)> = frames
        .iter()
        .map(|frame| (frame.text.as_str(), frame.is_final))
        .collect();
    assert_eq!(
        texts,
        vec![
            ("hello", false),
            ("hello world", true),
            ("and goodbye", true),
        ]
    );
}

/// THE COST-CAP LAW: a session that nobody finishes still self-finalizes at
/// the configured cap (production 900 s), sending KeepAlives while open and
/// CloseStream at the cap — an abandoned socket can never bill forever.
///
/// The load-bearing observation happens BEFORE `finish()` is ever called:
/// the server must have seen CloseStream from the cap alone (`finish`
/// would settle the session even without a cap, so asserting after it
/// would be vacuous).
///
/// MUTATION CHECK: remove the cap arm from the session select (or the
/// KeepAlive timer). Expected runtime failure: the pre-finish CloseStream
/// assertion below, or the KeepAlive count stays zero.
/// Verified by revert on 2026-08-05.
#[tokio::test]
async fn abandoned_session_self_finalizes_at_the_cap_with_keepalives() {
    let fixture = spawn_ws_fixture(false).await;
    let mut config = session_config(&fixture.url);
    config.keepalive_interval = std::time::Duration::from_millis(100);
    config.max_session = std::time::Duration::from_millis(600);
    let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel::<TranscriptFrame>();
    let session = start_session(config, events_tx)
        .await
        .expect("handshake succeeds");
    // Nobody sends audio and nobody calls finish: the cap must settle the
    // session ON ITS OWN. Poll (bounded) for the server-observed
    // CloseStream before finish is ever invoked.
    let started = std::time::Instant::now();
    let mut cap_closed = false;
    while started.elapsed() < std::time::Duration::from_secs(5) {
        if fixture.close_stream_seen.load(Ordering::SeqCst) {
            cap_closed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        cap_closed,
        "the cap alone must send CloseStream — finish() was never called"
    );
    assert!(
        fixture.keepalives.load(Ordering::SeqCst) >= 2,
        "KeepAlives must flow while the stream is open"
    );
    // Finishing afterwards still settles cleanly with the empty result.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), session.finish())
        .await
        .expect("capped session settles")
        .expect("empty session is a success");
    assert!(result.text.is_empty());
    assert_eq!(result.segments, 0);
}
