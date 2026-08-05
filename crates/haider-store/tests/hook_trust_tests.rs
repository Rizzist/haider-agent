#![allow(clippy::expect_used)]

use haider_protocol::DeliveryMode;
use haider_protocol::hook::HookEventPayload;
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_store::{
    HookTrustCommand, SessionCreateCommand, Store, TurnAcceptCommand, TurnAcceptOutcome,
};

fn command(id: &str, digest: &str, trusted: bool) -> HookTrustCommand {
    let request_json = serde_json::json!({
        "digest": digest,
        "trusted": trusted,
    })
    .to_string();
    HookTrustCommand {
        command_id: id.into(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        digest: digest.into(),
        trusted,
        workspace: None,
    }
}

/// MUTATION CHECK: commit trust outside its receipt transaction or make the
/// command share the management revision. Expected RUNTIME failure: replay
/// diverges, the reduced order changes, or revision zero is advanced.
#[test]
fn trust_and_revoke_are_receipted_ordered_and_revision_independent() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let digest = "a".repeat(64);
    let trust = command("hooks-trust-a", &digest, true);
    let first = store
        .apply_hook_trust_command(&trust)
        .expect("trust commits");
    let replay = store
        .apply_hook_trust_command(&trust)
        .expect("trust replays");
    assert_eq!(first, replay);
    assert_eq!(store.management_revision().expect("revision"), 0);

    let revoke = command("hooks-revoke-a", &digest, false);
    store
        .apply_hook_trust_command(&revoke)
        .expect("revoke commits");
    assert_eq!(
        store.hook_trust_changes().expect("ordered changes"),
        [
            haider_store::HookTrustChange {
                digest: digest.clone(),
                trusted: true,
                workspace: None,
            },
            haider_store::HookTrustChange {
                digest,
                trusted: false,
                workspace: None,
            },
        ]
    );
    assert_eq!(store.management_revision().expect("final revision"), 0);
}

/// MUTATION CHECK: accept command-id reuse with different trust coordinates.
/// Expected RUNTIME failure: the changed request is silently replayed.
#[test]
fn hook_trust_receipt_rejects_changed_command_identity() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let original = command("hooks-collision", &"b".repeat(64), true);
    store
        .apply_hook_trust_command(&original)
        .expect("first command");
    let changed = command("hooks-collision", &"c".repeat(64), true);
    assert!(store.apply_hook_trust_command(&changed).is_err());
}

/// MUTATION CHECK: append run trust after acceptance or derive it from mutable
/// CLI state. Expected RUNTIME failure: the committed acceptance batch lacks
/// the correlated additive fact or an ordinary submission gains one.
#[test]
fn run_scoped_trust_is_atomic_with_turn_acceptance_and_omitted_by_default() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("hook-run-trust-session");
    store
        .create_session(&SessionCreateCommand {
            command_id: "create-hook-run-trust".into(),
            request_digest: "create-hook-run-trust-digest".into(),
            request_json: r#"{"session":"hook-run-trust"}"#.into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            system_prompt_version: "test-v1".into(),
            event_id: EventId::new("hook-run-trust-created"),
            device_id: DeviceId::new("hook-run-trust-device"),
        })
        .expect("create session");
    let accepted = store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "trusted",
            r#"{"attachments":[],"mode":"queue","text":"go","trust_hooks":true}"#,
        ))
        .expect("accept trusted run");
    let TurnAcceptOutcome::Committed { envelopes, .. } = accepted else {
        panic!("first acceptance commits");
    };
    let trust = envelopes
        .iter()
        .find_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
        .expect("atomic run trust fact");
    assert_eq!(trust, HookEventPayload::HookRunTrust { enabled: true });
    assert!(
        envelopes
            .iter()
            .all(|event| { event.run_id.as_ref() == Some(&RunId::new("hook-run-trusted")) })
    );

    let other_session = SessionId::new("ordinary-hook-run-session");
    store
        .create_session(&SessionCreateCommand {
            command_id: "create-ordinary-hook-run".into(),
            request_digest: "create-ordinary-hook-run-digest".into(),
            request_json: r#"{"session":"ordinary"}"#.into(),
            session_id: other_session.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            system_prompt_version: "test-v1".into(),
            event_id: EventId::new("ordinary-hook-run-created"),
            device_id: DeviceId::new("hook-run-trust-device"),
        })
        .expect("create ordinary session");
    let ordinary = store
        .accept_turn(&turn_command(
            &store,
            &other_session,
            "ordinary",
            r#"{"attachments":[],"mode":"queue","text":"go"}"#,
        ))
        .expect("accept ordinary run");
    let TurnAcceptOutcome::Committed { envelopes, .. } = ordinary else {
        panic!("ordinary acceptance commits");
    };
    assert!(
        envelopes
            .iter()
            .all(|event| { HookEventPayload::from_payload_value(event.payload.clone()).is_err() })
    );
}

fn turn_command(
    store: &Store,
    session_id: &SessionId,
    suffix: &str,
    request_json: &str,
) -> TurnAcceptCommand {
    TurnAcceptCommand {
        command_id: format!("hook-run-{suffix}"),
        request_digest: format!("hook-run-{suffix}-digest"),
        request_json: request_json.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(format!("hook-run-{suffix}")),
        agent_id: None,
        branch_id: None,
        text: "go".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("hook-run-{suffix}-queued")),
        user_event_id: EventId::new(format!("hook-run-{suffix}-user")),
        active_event_id: EventId::new(format!("hook-run-{suffix}-active")),
        device_id: DeviceId::new("hook-run-trust-device"),
    }
}
