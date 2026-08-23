#![cfg(unix)]
//! T2 — the thin transcription-secret client helpers: request builders
//! against the T1 wire goldens, tolerant response parsing, typed error
//! mapping, and one full async round-trip against an in-process fake
//! daemon (the client_tests harness pattern).
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

use haider_client::transcription::{
    TranscriptionSecretError, present_from_set_response, secret_from_get_response, secret_get,
    secret_get_request, secret_set, secret_set_request,
};
use haider_client::{ClientConfig, connect};
use haider_rpc::{
    Capability, CapabilitySet, DEFAULT_FRAME_LIMIT, LifecyclePhase, RequestBody, ResponseBody,
    WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const LIMIT: usize = DEFAULT_FRAME_LIMIT;

fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("htrx")
        .tempdir_in("/tmp")
        .expect("short temp dir")
}

fn welcome() -> Welcome {
    Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: "fake-instance".into(),
        daemon_generation: 1,
        frame_limit: LIMIT as u32,
        profile_id: "fake-profile".into(),
        daemon_version: "0.0.1-fake".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features: BTreeSet::from([haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned()]),
        user_command_withheld: false,
        encoding: None,
    }
}

async fn write_frame(stream: &mut UnixStream, frame: &WireFrame) {
    let bytes = uds_codec::encode(frame, LIMIT).expect("encode fake frame");
    stream.write_all(&bytes).await.expect("write fake frame");
}

async fn read_frames(stream: &mut UnixStream, decoder: &mut uds_codec::Decoder) -> Vec<WireFrame> {
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.expect("fake server read");
        if read == 0 {
            return Vec::new();
        }
        let batch = decoder.push(&buffer[..read]);
        assert!(batch.error.is_none(), "decode: {:?}", batch.error);
        if !batch.frames.is_empty() {
            return batch.frames;
        }
    }
}

/// A fake daemon holding one in-memory vault slot, answering the two
/// transcription methods exactly as T1's daemon does (set → present,
/// get → the stored secret or an absent field).
fn spawn_vault_daemon(endpoint: &Path) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(endpoint).expect("bind fake daemon");
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut decoder = uds_codec::Decoder::new(LIMIT);
        let frames = read_frames(&mut stream, &mut decoder).await;
        assert!(matches!(frames.first(), Some(WireFrame::Hello(_))));
        write_frame(&mut stream, &WireFrame::Welcome(welcome())).await;
        let mut vault: Option<String> = None;
        loop {
            let frames = read_frames(&mut stream, &mut decoder).await;
            if frames.is_empty() {
                return;
            }
            for frame in frames {
                match frame {
                    WireFrame::Ping { nonce } => {
                        write_frame(&mut stream, &WireFrame::Pong { nonce }).await;
                    }
                    WireFrame::Request { request_id, body } => {
                        let body = match body {
                            RequestBody::TranscriptionSecretGet => {
                                ResponseBody::TranscriptionSecretGet {
                                    secret: vault.clone().map(haider_rpc::SecretWire::new),
                                }
                            }
                            RequestBody::TranscriptionSecretSet { secret, clear } => {
                                if clear {
                                    vault = None;
                                } else {
                                    vault = Some(secret.expose_secret().to_owned());
                                }
                                ResponseBody::TranscriptionSecretSet {
                                    present: vault.is_some(),
                                }
                            }
                            _ => ResponseBody::Error {
                                code: "unsupported".to_owned(),
                                message: "unexpected method".to_owned(),
                                retryable: false,
                                data: None,
                            },
                        };
                        write_frame(&mut stream, &WireFrame::Response { request_id, body }).await;
                    }
                    _ => {}
                }
            }
        }
    })
}

/// MUTATION CHECK: rename a method string or drop the `clear` field in a
/// builder. Expected failure: the encoded JSON stops matching the T1
/// golden wire bodies (`wire_transcript.json`).
#[test]
fn builders_encode_the_golden_wire_bodies() {
    assert_eq!(
        serde_json::to_value(secret_get_request()).expect("encode"),
        serde_json::json!({"method": "transcription.secret_get"})
    );
    assert_eq!(
        serde_json::to_value(secret_set_request(
            haider_rpc::SecretWire::new("golden-placeholder-deepgram-key"),
            false,
        ))
        .expect("encode"),
        serde_json::json!({
            "method": "transcription.secret_set",
            "secret": "golden-placeholder-deepgram-key",
            "clear": false
        })
    );
    assert_eq!(
        serde_json::to_value(secret_set_request(haider_rpc::SecretWire::new(""), true))
            .expect("encode"),
        serde_json::json!({
            "method": "transcription.secret_set",
            "secret": "",
            "clear": true
        })
    );
}

/// MUTATION CHECK: decode the absent-secret golden through a
/// non-optional field. Expected failure: the `secret`-less golden line
/// stops parsing to `None`.
#[test]
fn parsers_consume_the_golden_response_lines() {
    // The exact golden bodies from the T1 fixture (request_id stripped —
    // the parser takes bodies).
    let with_secret: ResponseBody = serde_json::from_value(serde_json::json!({
        "method": "transcription.secret_get",
        "secret": "golden-placeholder-deepgram-key"
    }))
    .expect("decode get");
    let secret = secret_from_get_response(with_secret).expect("parse get");
    assert_eq!(
        secret.expect("present").expose_secret(),
        "golden-placeholder-deepgram-key"
    );
    let absent: ResponseBody = serde_json::from_value(serde_json::json!({
        "method": "transcription.secret_get"
    }))
    .expect("decode absent get");
    assert!(secret_from_get_response(absent).expect("parse").is_none());
    let stored: ResponseBody = serde_json::from_value(serde_json::json!({
        "method": "transcription.secret_set",
        "present": true
    }))
    .expect("decode set");
    assert!(present_from_set_response(stored).expect("parse set"));
}

/// MUTATION CHECK: collapse the error mapping into `UnexpectedBody`.
/// Expected failure: the typed daemon refusal loses its code + message.
#[test]
fn typed_refusals_and_skewed_bodies_map_distinctly() {
    let refusal = ResponseBody::Error {
        code: "vault_unavailable".to_owned(),
        message: "no credential vault on this platform".to_owned(),
        retryable: false,
        data: None,
    };
    match secret_from_get_response(refusal) {
        Err(TranscriptionSecretError::Refused { code, message }) => {
            assert_eq!(code, "vault_unavailable");
            assert!(message.contains("no credential vault"));
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    // A body from a different method is a skewed daemon, said honestly.
    let skewed = ResponseBody::TranscriptionSecretSet { present: true };
    assert!(matches!(
        secret_from_get_response(skewed),
        Err(TranscriptionSecretError::UnexpectedBody)
    ));
    let skewed = ResponseBody::TranscriptionSecretGet { secret: None };
    assert!(matches!(
        present_from_set_response(skewed),
        Err(TranscriptionSecretError::UnexpectedBody)
    ));
}

/// The async helpers against a real socket: set → present, get → the
/// same secret, clear → absent. One connection, three round trips.
///
/// MUTATION CHECK: make `secret_set` ignore its `clear` flag. Expected
/// failure: the final get still answers the key.
#[tokio::test]
async fn helpers_round_trip_against_a_fake_daemon() {
    let dir = short_dir();
    let endpoint = dir.path().join("d.sock");
    let _daemon = spawn_vault_daemon(&endpoint);
    let connected = connect(&endpoint, ClientConfig::default())
        .await
        .expect("connect fake daemon");
    let client = connected.client;
    assert!(
        secret_get(&client).await.expect("empty get").is_none(),
        "an empty vault answers None"
    );
    let present = secret_set(&client, haider_rpc::SecretWire::new("dg-live-key"), false)
        .await
        .expect("set");
    assert!(present);
    let fetched = secret_get(&client)
        .await
        .expect("get")
        .expect("the stored key comes back");
    assert_eq!(fetched.expose_secret(), "dg-live-key");
    let present = secret_set(&client, haider_rpc::SecretWire::new(""), true)
        .await
        .expect("clear");
    assert!(!present);
    assert!(secret_get(&client).await.expect("cleared get").is_none());
}
