//! T2 — the transcription-secret RPCs through the TUI's link plumbing:
//! request bodies against the T1 wire goldens, response → reply mapping,
//! the operation-tagged error path, driver routing, and the
//! command-identity pins (reads + the deliberately receipt-free set).
#![allow(clippy::expect_used)]

use haider_rpc::{RequestBody, ResponseBody, SecretWire};
use haider_tui::app::{AppRequest, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply, TranscriptionOp};
use haider_tui::talk::{SecretIntent, TalkPhase, TalkShellCommand};

mod common;
use common::launcher_model;

fn get_context() -> CommandContext {
    CommandContext::of(&LiveCommand::TranscriptionSecretGet)
}

fn set_context() -> CommandContext {
    CommandContext::of(&LiveCommand::TranscriptionSecretSet {
        secret: SecretWire::new("k"),
        clear: false,
    })
}

/// MUTATION CHECK: rename either method string in the client helpers.
/// Expected failure: the encoded body stops matching the T1 golden wire
/// bytes (`crates/haider-rpc/tests/fixtures/wire_transcript.json`).
#[test]
fn request_bodies_match_the_t1_wire_goldens() {
    let get = request_body(LiveCommand::TranscriptionSecretGet);
    assert_eq!(
        serde_json::to_value(&get).expect("encode get"),
        serde_json::json!({"method": "transcription.secret_get"})
    );
    let set = request_body(LiveCommand::TranscriptionSecretSet {
        secret: SecretWire::new("golden-placeholder-deepgram-key"),
        clear: false,
    });
    assert_eq!(
        serde_json::to_value(&set).expect("encode set"),
        serde_json::json!({
            "method": "transcription.secret_set",
            "secret": "golden-placeholder-deepgram-key",
            "clear": false
        })
    );
    let clear = request_body(LiveCommand::TranscriptionSecretSet {
        secret: SecretWire::new(""),
        clear: true,
    });
    assert_eq!(
        serde_json::to_value(&clear).expect("encode clear"),
        serde_json::json!({
            "method": "transcription.secret_set",
            "secret": "",
            "clear": true
        })
    );
}

/// MUTATION CHECK: map the get response through the SET parser. Expected
/// failure: the present secret comes back as an UnexpectedBody failure
/// reply.
#[test]
fn secret_responses_map_to_their_replies() {
    let replies = map_response(
        &get_context(),
        ResponseBody::TranscriptionSecretGet {
            secret: Some(SecretWire::new("vaulted")),
        },
    );
    match replies.as_slice() {
        [
            LiveReply::TranscriptionSecret {
                secret: Some(secret),
            },
        ] => {
            assert_eq!(secret.expose_secret(), "vaulted");
        }
        other => panic!("unexpected mapping: {other:?}"),
    }
    let absent = map_response(
        &get_context(),
        ResponseBody::TranscriptionSecretGet { secret: None },
    );
    assert_eq!(
        absent,
        vec![LiveReply::TranscriptionSecret { secret: None }],
        "an honest no-key answer survives the mapping"
    );
    let stored = map_response(
        &set_context(),
        ResponseBody::TranscriptionSecretSet { present: true },
    );
    assert_eq!(
        stored,
        vec![LiveReply::TranscriptionSecretStored { present: true }]
    );
}

/// MUTATION CHECK: drop the `transcription` tag from the link's request
/// context. Expected failure: the error below maps to the generic
/// uncorrelated `Failed` (no command id) instead of the op-tagged reply.
#[test]
fn secret_errors_are_operation_tagged() {
    let error = ResponseBody::Error {
        code: "vault_unavailable".to_owned(),
        message: "no credential vault".to_owned(),
        retryable: false,
        data: None,
    };
    let get = map_response(&get_context(), error.clone());
    assert_eq!(
        get,
        vec![LiveReply::TranscriptionSecretFailed {
            op: TranscriptionOp::Get,
            message: "no credential vault".to_owned(),
        }]
    );
    let set = map_response(&set_context(), error);
    assert_eq!(
        set,
        vec![LiveReply::TranscriptionSecretFailed {
            op: TranscriptionOp::Set,
            message: "no credential vault".to_owned(),
        }]
    );
}

/// MUTATION CHECK: give either command a durable command id. Expected
/// failure: the identity pin below (reads + the deliberately
/// receipt-free set carry NONE) breaks — and a receipt could then carry
/// a secret.
#[test]
fn secret_commands_carry_no_durable_identity() {
    assert!(LiveCommand::TranscriptionSecretGet.command_id().is_none());
    assert!(
        LiveCommand::TranscriptionSecretSet {
            secret: SecretWire::new("k"),
            clear: false,
        }
        .command_id()
        .is_none()
    );
}

/// MUTATION CHECK: route `TranscriptionSecretRead` into the outbox (via
/// `enqueue`). Expected failure: the read resends on reconnect as if it
/// were a durable mutation.
#[test]
fn the_driver_routes_secret_requests_to_the_wire() {
    let mut driver = LiveDriver::new("test-instance".to_owned());
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    let commands = driver.handle_request(&mut model, AppRequest::TranscriptionSecretRead);
    assert_eq!(commands, vec![LiveCommand::TranscriptionSecretGet]);
    let commands = driver.handle_request(
        &mut model,
        AppRequest::TranscriptionSecretStore {
            secret: SecretWire::new("dg"),
            clear: false,
        },
    );
    assert_eq!(
        commands,
        vec![LiveCommand::TranscriptionSecretSet {
            secret: SecretWire::new("dg"),
            clear: false,
        }]
    );
    // TalkShell is runtime-owned: the driver must never invent a wire
    // command for it (live_pass hands it to the stt supervisor).
    let commands = driver.handle_request(
        &mut model,
        AppRequest::TalkShell(TalkShellCommand::LoadSetup),
    );
    assert!(commands.is_empty());
}

/// MUTATION CHECK: drop the `TranscriptionSecret` apply arm. Expected
/// failure: the daemon's answer never reaches the reducer and the
/// Deepgram start below stays wedged in `Starting` with no follow-up
/// request.
#[test]
fn apply_routes_the_secret_answer_into_the_reducer() {
    let mut driver = LiveDriver::new("test-instance".to_owned());
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&haider_protocol::ids::SessionId::new("s-link"));
    model.open_session(&haider_protocol::ids::SessionId::new("s-link"));
    assert_eq!(model.screen, Screen::Session);
    model.talk_config.engine = haider_stt::config::TranscriptionEngine::Deepgram;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    model.talk_toggle();
    assert_eq!(model.talk.secret_intent, Some(SecretIntent::Start));
    model.requests.clear();
    let commands = driver.apply(
        &mut model,
        LiveReply::TranscriptionSecret {
            secret: Some(SecretWire::new("dg-key")),
        },
    );
    assert!(
        commands.is_empty(),
        "no wire follow-up — the effect is local"
    );
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::TalkShell(TalkShellCommand::Start { .. })
        )),
        "the answered key starts the engine"
    );
    // And the failure arm settles an armed start.
    model.talk_toggle();
    let _ = model.requests.drain(..);
    model.talk.secret_intent = Some(SecretIntent::Start);
    let commands = driver.apply(
        &mut model,
        LiveReply::TranscriptionSecretFailed {
            op: TranscriptionOp::Get,
            message: "gone".to_owned(),
        },
    );
    assert!(commands.is_empty());
    assert_eq!(model.talk.phase, TalkPhase::Idle);
}

/// The wire builders come from ONE authority: the TUI link delegates to
/// the `haider-client` helpers, so both agree byte-for-byte.
///
/// MUTATION CHECK: re-hardcode the body in `link::request_body` with a
/// drifted field. Expected failure: this equality splits.
#[test]
fn the_link_and_the_client_helpers_agree() {
    let from_link = request_body(LiveCommand::TranscriptionSecretGet);
    let from_helper = haider_client::transcription::secret_get_request();
    assert_eq!(
        serde_json::to_value(&from_link).expect("link"),
        serde_json::to_value(&from_helper).expect("helper")
    );
    let from_link = request_body(LiveCommand::TranscriptionSecretSet {
        secret: SecretWire::new("same-key"),
        clear: false,
    });
    let from_helper =
        haider_client::transcription::secret_set_request(SecretWire::new("same-key"), false);
    assert_eq!(
        serde_json::to_value(&from_link).expect("link"),
        serde_json::to_value(&from_helper).expect("helper")
    );
    assert!(matches!(
        from_helper,
        RequestBody::TranscriptionSecretSet { .. }
    ));
}
