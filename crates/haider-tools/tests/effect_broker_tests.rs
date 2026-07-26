#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_tools::{
    EffectBroker, FsPatch, FsRead, JournalSink, PermissionPolicy, ResultBounds, ToolResult,
};
use std::fs;

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

#[derive(Debug, Default)]
struct RejectDispatchJournal {
    payloads: Vec<EventPayload>,
}

#[async_trait::async_trait]
impl JournalSink for RejectDispatchJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(
            payload,
            EventPayload::Effect(EffectPhase::Dispatched { .. })
        ) {
            return Err(haider_tools::ToolError::journal(
                "durable append unavailable",
            ));
        }
        self.payloads.push(payload);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct UnusedCas;

#[async_trait::async_trait]
impl haider_tools::CasSink for UnusedCas {
    async fn put(&mut self, _bytes: &[u8]) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        panic!("small result should not reach CAS")
    }
}

fn effect_phases(journal: &RecordingJournal) -> Vec<&EffectPhase> {
    journal
        .payloads
        .iter()
        .map(|payload| match payload {
            EventPayload::Effect(phase) => phase,
            other => panic!("journal received non-effect payload: {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn always_allow_is_bound_to_class_and_exact_argument_digest() {
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut policy = PermissionPolicy::default();
    policy.ask(EffectClass::FsRead);
    let original = FsRead::new("src/lib.rs");

    let first = broker
        .normalize(&original)
        .await
        .expect("normalize original");
    assert!(matches!(
        broker
            .authorize(&first, &policy)
            .await
            .expect("authorize original"),
        AuthorizationVerdict::Ask { .. }
    ));
    policy.always_allow(&first);

    let same = broker
        .normalize(&original)
        .await
        .expect("normalize same op");
    assert_eq!(same.args_digest, first.args_digest);
    assert_eq!(
        broker
            .authorize(&same, &policy)
            .await
            .expect("authorize same op"),
        AuthorizationVerdict::Allow
    );

    let mutated = broker
        .normalize(&FsRead::new("src/main.rs"))
        .await
        .expect("normalize mutated op");
    assert_ne!(mutated.args_digest, first.args_digest);
    assert!(matches!(
        broker
            .authorize(&mutated, &policy)
            .await
            .expect("authorize mutated op"),
        AuthorizationVerdict::Ask { .. }
    ));
}

#[tokio::test]
async fn allow_always_menu_resolution_creates_the_exact_digest_rule() {
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut policy = PermissionPolicy::default();
    let operation = FsRead::new("src/lib.rs");
    let intent = broker
        .normalize(&operation)
        .await
        .expect("normalize operation");
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&intent, &policy)
        .await
        .expect("operation asks")
    else {
        panic!("default policy must ask");
    };
    let opened = broker.permission_menu(&menu).expect("menu is available");
    assert!(opened.options.iter().any(|option| {
        option.key == "allow_always"
            && option.decision == Some(haider_protocol::menu::DecisionKind::AllowAlways)
    }));

    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: Some("allow_always".into()),
                option_index: 0,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("menu resolves");

    assert_eq!(policy.always_allow_rules().len(), 1);
    assert_eq!(policy.always_allow_rules()[0].class, EffectClass::FsRead);
    assert_eq!(
        policy.always_allow_rules()[0].args_digest,
        intent.args_digest
    );
    let retry = broker.normalize(&operation).await.expect("normalize retry");
    assert_eq!(
        broker
            .authorize(&retry, &policy)
            .await
            .expect("retry authorizes"),
        AuthorizationVerdict::Allow
    );
}

#[tokio::test]
async fn allow_once_resolution_grants_a_single_retry() {
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut policy = PermissionPolicy::default();
    let operation = FsRead::new("src/lib.rs");
    let intent = broker.normalize(&operation).await.expect("normalize");
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&intent, &policy)
        .await
        .expect("operation asks")
    else {
        panic!("default policy must ask");
    };

    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: Some("allow_once".into()),
                option_index: 0,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("menu resolves");
    assert!(policy.always_allow_rules().is_empty());

    let retry = broker.normalize(&operation).await.expect("normalize retry");
    assert_eq!(
        broker
            .authorize(&retry, &policy)
            .await
            .expect("retry authorizes"),
        AuthorizationVerdict::Allow
    );

    let third = broker.normalize(&operation).await.expect("normalize third");
    assert!(matches!(
        broker
            .authorize(&third, &policy)
            .await
            .expect("one-shot is consumed"),
        AuthorizationVerdict::Ask { .. }
    ));
}

#[tokio::test]
async fn reject_once_resolution_denies_a_single_retry() {
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut policy = PermissionPolicy::default();
    let operation = FsRead::new("src/lib.rs");
    let intent = broker.normalize(&operation).await.expect("normalize");
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&intent, &policy)
        .await
        .expect("operation asks")
    else {
        panic!("default policy must ask");
    };

    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: Some("reject_once".into()),
                option_index: 0,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("menu resolves");

    let retry = broker.normalize(&operation).await.expect("normalize retry");
    assert!(matches!(
        broker
            .authorize(&retry, &policy)
            .await
            .expect("retry is denied"),
        AuthorizationVerdict::Deny { .. }
    ));

    let third = broker.normalize(&operation).await.expect("normalize third");
    assert!(matches!(
        broker
            .authorize(&third, &policy)
            .await
            .expect("one-shot is consumed"),
        AuthorizationVerdict::Ask { .. }
    ));
}

#[tokio::test]
async fn dispatch_cannot_follow_a_blocked_authorization() {
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut policy = PermissionPolicy::default();
    policy.deny(EffectClass::FsWrite, "workspace is read-only");
    let intent = broker
        .normalize(&FsPatch::new("file.txt", "before", "after"))
        .await
        .expect("normalize");
    assert!(matches!(
        broker.authorize(&intent, &policy).await.expect("authorize"),
        AuthorizationVerdict::Deny { .. }
    ));

    let error = broker
        .journal_dispatched(&intent.effect)
        .await
        .expect_err("dispatch after deny must be refused");
    assert!(matches!(error, haider_tools::ToolError::Lifecycle { .. }));
    assert!(
        !effect_phases(broker.journal())
            .iter()
            .any(|phase| matches!(phase, EffectPhase::Dispatched { .. }))
    );
}

#[tokio::test]
async fn deny_is_journaled_and_blocks_filesystem_apply() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("denied.txt");
    fs::write(&path, "before").expect("seed file");

    let mut policy = PermissionPolicy::default();
    policy.deny(EffectClass::FsWrite, "workspace is read-only");
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut ledger = haider_tools::ChangeLedger::new();
    let attribution = haider_tools::TurnAttribution::new(
        haider_protocol::ids::SessionId::new("session"),
        haider_protocol::ids::RunId::new("turn"),
    );

    let error = broker
        .fs_patch(
            &FsPatch::new(&path, "before", "after"),
            &policy,
            &attribution,
            &mut ledger,
        )
        .await
        .expect_err("deny must block patch");

    assert!(matches!(
        error,
        haider_tools::ToolError::PermissionDenied { .. }
    ));
    assert_eq!(fs::read_to_string(&path).expect("read file"), "before");
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
    let phases = effect_phases(broker.journal());
    assert!(matches!(phases[0], EffectPhase::Intent(_)));
    assert!(matches!(
        phases[1],
        EffectPhase::Authorized {
            verdict: AuthorizationVerdict::Deny { .. },
            ..
        }
    ));
    assert_eq!(phases.len(), 2);
}

#[tokio::test]
async fn failed_dispatched_append_blocks_filesystem_apply() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("journal-failure.txt");
    fs::write(&path, "before").expect("seed file");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsWrite);
    let mut broker = EffectBroker::new(RejectDispatchJournal::default());
    let mut ledger = haider_tools::ChangeLedger::new();
    let attribution = haider_tools::TurnAttribution::new(
        haider_protocol::ids::SessionId::new("session"),
        haider_protocol::ids::RunId::new("turn"),
    );

    let error = broker
        .fs_patch(
            &FsPatch::new(&path, "before", "after"),
            &policy,
            &attribution,
            &mut ledger,
        )
        .await
        .expect_err("dispatch append must gate apply");

    assert!(matches!(error, haider_tools::ToolError::Journal { .. }));
    assert_eq!(fs::read_to_string(&path).expect("read file"), "before");
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
    assert_eq!(broker.journal().payloads.len(), 2);
}

#[tokio::test]
async fn successful_dispatch_has_strict_four_phase_order() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("read.txt");
    fs::write(&path, "small result").expect("seed file");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsRead);
    let mut broker = EffectBroker::new(RecordingJournal::default());

    let result = broker
        .fs_read(
            &FsRead::new(&path),
            &policy,
            &mut UnusedCas,
            ResultBounds::default(),
        )
        .await
        .expect("read succeeds");
    assert_eq!(result.preview, "small result");

    let phases = effect_phases(broker.journal());
    assert!(matches!(phases[0], EffectPhase::Intent(_)));
    assert!(matches!(
        phases[1],
        EffectPhase::Authorized {
            verdict: AuthorizationVerdict::Allow,
            ..
        }
    ));
    assert!(matches!(phases[2], EffectPhase::Dispatched { .. }));
    assert!(matches!(
        phases[3],
        EffectPhase::Outcome {
            outcome: EffectOutcome::Ok,
            ..
        }
    ));
    assert_eq!(phases.len(), 4);
}

#[tokio::test]
async fn dispatched_effect_can_be_reconciled_as_unknown() {
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsRead);
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let intent = broker
        .normalize(&FsRead::new("lost-result.txt"))
        .await
        .expect("normalize");
    assert_eq!(
        broker.authorize(&intent, &policy).await.expect("authorize"),
        AuthorizationVerdict::Allow
    );
    broker
        .journal_dispatched(&intent.effect)
        .await
        .expect("journal dispatched");
    broker
        .journal_unknown(&intent.effect)
        .await
        .expect("reconcile unknown");

    assert!(matches!(
        effect_phases(broker.journal()).last(),
        Some(EffectPhase::Outcome {
            outcome: EffectOutcome::Unknown,
            ..
        })
    ));
}
