#![allow(clippy::expect_used)]

//! W-B `web_fetch` broker laws (LW7): the fetch is a first-class effect —
//! URL-bearing intent, `Network { host }` authorization, dispatch, and an
//! honest terminal outcome — plus the permission shapes: Ask by default, the
//! empty-host Network family rule for auto-mode, and per-host menu grants.

use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::ids::SessionId;
use haider_tools::{
    EffectBroker, JournalSink, PermissionPolicy, ToolError, ToolResult, WebFetch,
    web_fetch_manifest,
};

#[derive(Debug, Default)]
struct RecordingJournal {
    payloads: Vec<EventPayload>,
}

#[async_trait::async_trait]
impl JournalSink for RecordingJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads.push(payload);
        Ok(())
    }
}

fn broker() -> EffectBroker {
    EffectBroker::new_at(
        Box::new(RecordingJournal::default()),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        SessionId::new("session"),
        7,
        1_700_000_000_000,
    )
    .expect("create broker")
}

fn phases(broker: &EffectBroker) -> Vec<EffectPhase> {
    broker.journal_snapshot()
}

/// LAW (LW7): an allowed `web_fetch` journals Intent (URL in the summary,
/// `Network { host }` as the class) → Authorized(Allow) → Dispatched, and
/// the caller-journaled terminal Outcome carries the honest result — Ok on
/// success, Failed WITH the URL-bearing message on refusal.
#[tokio::test]
async fn web_fetch_journals_intent_and_outcome_with_the_url() {
    let mut broker = broker();
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::Network {
        host: "example.com".into(),
    });
    let operation =
        WebFetch::new("https://example.com/docs/page?x=1", None).expect("valid operation");
    let intent = broker
        .begin_web_fetch(&operation, &policy)
        .await
        .expect("allowed fetch dispatches");
    assert_eq!(
        intent.class,
        EffectClass::Network {
            host: "example.com".into(),
        },
        "the permission key is the fetch host"
    );
    assert_eq!(intent.summary, "fetch https://example.com/docs/page?x=1");
    broker
        .journal_outcome(
            &intent,
            EffectOutcome::Failed {
                error: "web_fetch refuses non-public target 10.0.0.8 for https://example.com/docs/page?x=1".into(),
            },
        )
        .await
        .expect("outcome journals");

    let journal = phases(&broker);
    assert_eq!(journal.len(), 4, "four-phase effect law: {journal:?}");
    assert!(matches!(
        &journal[0],
        EffectPhase::Intent(intent)
            if intent.summary.contains("https://example.com/docs/page?x=1")
    ));
    assert!(matches!(
        &journal[1],
        EffectPhase::Authorized {
            verdict: AuthorizationVerdict::Allow,
            ..
        }
    ));
    assert!(matches!(&journal[2], EffectPhase::Dispatched { .. }));
    assert!(matches!(
        &journal[3],
        EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { error },
            ..
        } if error.contains("https://example.com/docs/page?x=1")
    ));
}

/// LAW (W-B permission shapes): with NO policy entry the fetch ASKS (a
/// permission menu, never a silent dispatch); the EMPTY-host `Network` rule
/// is the class-family wildcard that auto-mode uses; and a host-scoped rule
/// covers exactly its own host.
#[tokio::test]
async fn web_fetch_asks_by_default_and_the_empty_host_rule_is_the_family_wildcard() {
    // Default: Ask.
    let mut broker_ask = broker();
    let policy = PermissionPolicy::default();
    let operation = WebFetch::new("https://example.com/a", None).expect("valid operation");
    let error = broker_ask
        .begin_web_fetch(&operation, &policy)
        .await
        .expect_err("unlisted network class must ask");
    assert!(
        matches!(error, ToolError::AuthorizationRequired { .. }),
        "Ask surfaces as an authorization menu: {error:?}"
    );

    // Empty-host wildcard (the auto-mode upgrade shape) allows any host.
    let mut broker_auto = broker();
    let mut auto_policy = PermissionPolicy::default();
    auto_policy.allow(EffectClass::Network {
        host: String::new(),
    });
    for url in ["https://example.com/a", "https://other.example.org/b"] {
        let operation = WebFetch::new(url, None).expect("valid operation");
        broker_auto
            .begin_web_fetch(&operation, &auto_policy)
            .await
            .expect("family wildcard allows every host");
    }

    // A host-scoped rule covers only its own host.
    let mut broker_host = broker();
    let mut host_policy = PermissionPolicy::default();
    host_policy.allow(EffectClass::Network {
        host: "example.com".into(),
    });
    broker_host
        .begin_web_fetch(
            &WebFetch::new("https://example.com/ok", None).expect("valid"),
            &host_policy,
        )
        .await
        .expect("scoped host allowed");
    let error = broker_host
        .begin_web_fetch(
            &WebFetch::new("https://other.example.org/no", None).expect("valid"),
            &host_policy,
        )
        .await
        .expect_err("a different host still asks");
    assert!(matches!(error, ToolError::AuthorizationRequired { .. }));
}

/// The manifest is the honest registry shape: `web_fetch`, the Network
/// family effect, an object schema requiring `url`, and no extra properties.
#[test]
fn web_fetch_manifest_declares_the_network_family_and_url_schema() {
    let manifest = web_fetch_manifest();
    assert_eq!(manifest.name, "web_fetch");
    assert_eq!(
        manifest.effects,
        vec![EffectClass::Network {
            host: String::new(),
        }]
    );
    assert_eq!(
        manifest.input_schema["required"],
        serde_json::json!(["url"])
    );
    assert_eq!(
        manifest.input_schema["additionalProperties"],
        serde_json::json!(false)
    );

    // Argument validation is the permission floor: relative URLs, foreign
    // schemes, and userinfo never mint an intent.
    for hostile in [
        "notaurl",
        "ftp://example.com/x",
        "https://user:pw@example.com/x",
        "",
    ] {
        assert!(
            WebFetch::new(hostile, None).is_err(),
            "`{hostile}` must be refused at argument validation"
        );
    }
    let operation = WebFetch::new("https://Example.COM:8443/path", Some(2048)).expect("valid");
    assert_eq!(
        operation.max_bytes(),
        Some(2048),
        "the caller cap threads through"
    );
    assert_eq!(
        WebFetch::new("https://Example.COM:8443/path", None)
            .expect("valid")
            .url(),
        "https://Example.COM:8443/path"
    );
}
