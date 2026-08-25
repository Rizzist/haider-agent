#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::ids::SessionId;
use haider_tools::{
    EffectBroker, JournalSink, MobileCancelToken, MobileError, MobileOperation, PermissionPolicy,
    ToolError, ToolResult, mobile_manifest, platform_mobile_backend,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct SharedJournal(Arc<Mutex<Vec<EventPayload>>>);

#[async_trait]
impl JournalSink for SharedJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.0.lock().expect("journal lock").push(payload);
        Ok(())
    }
}

fn operation() -> MobileOperation {
    MobileOperation::from_tool_args(serde_json::json!({
        "action": "sms_read",
        "folder": "inbox",
        "limit": 2
    }))
    .expect("sms_read operation")
}

fn new_broker() -> (
    tempfile::TempDir,
    EffectBroker,
    Arc<Mutex<Vec<EventPayload>>>,
) {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = SharedJournal::default();
    let observer = Arc::clone(&journal.0);
    let broker = EffectBroker::new_at(
        Box::new(journal),
        workspace.path(),
        SessionId::new("mobile-permission-session"),
        7,
        1_700_000_000_000,
    )
    .expect("mobile broker");
    (workspace, broker, observer)
}

#[test]
fn mobile_manifest_and_parser_pin_all_dynamic_effects() {
    let manifest = mobile_manifest();
    assert_eq!(manifest.name, "mobile");
    assert_eq!(
        manifest.effects,
        [
            EffectClass::ReadSms,
            EffectClass::MobileObserve,
            EffectClass::MobileControl
        ]
    );
    assert_eq!(
        manifest.input_schema["properties"]["action"]["enum"],
        serde_json::json!([
            "screenshot",
            "a11y_tree",
            "inspect",
            "tap",
            "long_press",
            "swipe",
            "type",
            "key",
            "open_app",
            "list_apps",
            "sms_read"
        ])
    );
    assert_eq!(operation().action().effect_class(), EffectClass::ReadSms);
    assert_eq!(
        MobileOperation::from_tool_args(serde_json::json!({"action": "screenshot"}))
            .expect("screenshot operation")
            .action()
            .effect_class(),
        EffectClass::MobileObserve
    );
    assert_eq!(
        MobileOperation::from_tool_args(serde_json::json!({
            "action": "tap",
            "x": 10,
            "y": 20
        }))
        .expect("tap operation")
        .action()
        .effect_class(),
        EffectClass::MobileControl
    );
    assert!(matches!(
        MobileOperation::from_tool_args(serde_json::json!({
            "action": "sms_read",
            "future": true
        })),
        Err(ToolError::InvalidArgument { .. })
    ));
    assert!(matches!(
        MobileOperation::from_tool_args(serde_json::json!({"action": "tap"})),
        Err(ToolError::InvalidArgument { .. })
    ));
}

#[tokio::test]
async fn mobile_authorize_and_dispatch_keep_the_durable_boundary_separate() {
    let (_workspace, mut broker, journal) = new_broker();
    let pending = broker
        .authorize_mobile(&operation(), &PermissionPolicy::default())
        .await
        .expect_err("ReadSms asks by default");
    assert!(matches!(pending, ToolError::AuthorizationRequired { .. }));
    let phases = journal
        .lock()
        .expect("journal lock")
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        phases.as_slice(),
        [
            EffectPhase::Intent(_),
            EffectPhase::Authorized {
                verdict: AuthorizationVerdict::Ask { .. },
                ..
            }
        ]
    ));
    assert!(
        !phases
            .iter()
            .any(|phase| matches!(phase, EffectPhase::Dispatched { .. }))
    );

    let (_workspace, mut broker, journal) = new_broker();
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ReadSms);
    let intent = broker
        .authorize_mobile(&operation(), &policy)
        .await
        .expect("ReadSms authorized");
    let token = MobileCancelToken::new();
    broker
        .dispatch_mobile(&intent, token.clone())
        .await
        .expect("ReadSms dispatched");
    broker
        .journal_mobile_outcome(&intent, EffectOutcome::Ok)
        .await
        .expect("ReadSms outcome");
    assert!(!token.is_cancelled());
    let phases = journal
        .lock()
        .expect("journal lock")
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        phases.as_slice(),
        [
            EffectPhase::Intent(_),
            EffectPhase::Authorized {
                verdict: AuthorizationVerdict::Allow,
                ..
            },
            EffectPhase::Dispatched { .. },
            EffectPhase::Outcome {
                outcome: EffectOutcome::Ok,
                ..
            }
        ]
    ));
}

#[tokio::test]
async fn mobile_cancellation_ownership_and_unavailable_stub_are_typed() {
    let (_workspace, mut broker, journal) = new_broker();
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ReadSms);
    let intent = broker
        .authorize_mobile(&operation(), &policy)
        .await
        .expect("ReadSms authorized");
    let token = MobileCancelToken::new();
    broker
        .dispatch_mobile(&intent, token.clone())
        .await
        .expect("ReadSms dispatched");
    broker.cancel_mobile_actions();
    assert!(token.is_cancelled());
    broker.cancel().await.expect("cancelled broker closes");
    assert!(journal.lock().expect("journal lock").iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Effect(EffectPhase::Outcome {
                effect,
                outcome: EffectOutcome::Cancelled,
                ..
            }) if effect == &intent.effect
        )
    }));

    let unavailable = platform_mobile_backend();
    let action = operation().action().clone();
    let error = unavailable
        .execute(&action, &MobileCancelToken::new())
        .await
        .expect_err("this lane has no production mobile backend");
    assert!(matches!(error, MobileError::Unavailable { .. }));
    assert_eq!(error.to_string(), "mobile backend unavailable");
}

#[tokio::test]
async fn ordinary_close_does_not_fabricate_mobile_cancellation() {
    let (_workspace, mut broker, journal) = new_broker();
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ReadSms);
    let intent = broker
        .authorize_mobile(&operation(), &policy)
        .await
        .expect("ReadSms authorized");
    let token = MobileCancelToken::new();
    broker
        .dispatch_mobile(&intent, token.clone())
        .await
        .expect("ReadSms dispatched");
    broker.close().await.expect("broker closes");
    assert!(!token.is_cancelled());
    assert!(journal.lock().expect("journal lock").iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Effect(EffectPhase::Outcome {
                effect,
                outcome: EffectOutcome::Unknown,
                ..
            }) if effect == &intent.effect
        )
    }));
}
