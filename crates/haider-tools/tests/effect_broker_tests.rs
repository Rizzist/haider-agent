#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::ids::SessionId;
use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_tools::{
    EffectBroker, FsEdit, FsRead, FsWrite, JournalSink, PermissionPolicy, ProcessExec,
    ResultBounds, ToolResult,
};
#[cfg(windows)]
use haider_tools::{FsPath, FsPathOperation};
use std::fs;
#[cfg(windows)]
use std::io::Read as _;
use std::path::Path;

// Each JournalSink double here is a distinct value moved exactly once into one
// broker. None shares an underlying journal with another boxed sink value.

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

    fn supports_checkpoint_batches(&self) -> bool {
        true
    }

    fn supports_checkpoint_artifacts(&self) -> bool {
        true
    }

    async fn put_checkpoint_artifact(
        &mut self,
        bytes: &[u8],
    ) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        Ok(haider_protocol::ids::ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(bytes).to_hex()
        )))
    }

    async fn append_checkpointed(
        &mut self,
        outcome: EventPayload,
        checkpoint: EventPayload,
    ) -> ToolResult<()> {
        self.payloads.extend([outcome, checkpoint]);
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

    async fn put_file(&mut self, _path: &Path) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        panic!("small result should not reach CAS")
    }
}

fn broker_at<J>(journal: J, workspace_root: &Path, generation: u64) -> EffectBroker
where
    J: JournalSink + 'static,
{
    EffectBroker::new_at(
        Box::new(journal),
        workspace_root,
        SessionId::new("session"),
        generation,
        1_700_000_000_000,
    )
    .expect("create broker")
}

fn source_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn effect_phases(broker: &EffectBroker) -> Vec<EffectPhase> {
    broker.journal_snapshot()
}

#[tokio::test]
async fn always_allow_is_bound_to_class_and_exact_argument_digest() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
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
        .normalize(&FsRead::new("src/broker.rs"))
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
async fn explicit_deny_is_higher_priority_than_session_grants() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let mut policy = PermissionPolicy::default();
    policy.ask(EffectClass::FsRead);

    policy
        .allow_for_session(EffectClass::FsRead)
        .expect("explicit read grant");
    let granted = broker
        .normalize(&FsRead::new("src/lib.rs"))
        .await
        .expect("normalize granted read");
    assert_eq!(
        broker
            .authorize(&granted, &policy)
            .await
            .expect("authorize granted read"),
        AuthorizationVerdict::Allow
    );

    policy.deny(EffectClass::FsRead, "explicit deny wins");
    let denied = broker
        .normalize(&FsRead::new("src/lib.rs"))
        .await
        .expect("normalize explicitly denied read");
    assert_eq!(
        broker
            .authorize(&denied, &policy)
            .await
            .expect("authorize explicitly denied read"),
        AuthorizationVerdict::Deny {
            reason: "explicit deny wins".into(),
        }
    );
}

#[tokio::test]
async fn autonomous_ask_promotion_allows_residual_classes_but_never_explicit_deny() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let mut policy = PermissionPolicy::default();
    policy.auto_allow_asks();

    let allowed = broker
        .normalize(&FsRead::new("src/lib.rs"))
        .await
        .expect("normalize residual Ask");
    assert_eq!(
        broker
            .authorize(&allowed, &policy)
            .await
            .expect("autonomous residual authorization"),
        AuthorizationVerdict::Allow
    );

    policy.deny(EffectClass::FsRead, "explicit read deny");
    let denied = broker
        .normalize(&FsRead::new("src/broker.rs"))
        .await
        .expect("normalize explicit deny");
    assert_eq!(
        broker
            .authorize(&denied, &policy)
            .await
            .expect("explicit denial remains a verdict"),
        AuthorizationVerdict::Deny {
            reason: "explicit read deny".into(),
        }
    );
}

/// A first-committed one-shot rejection is explicit user policy for the exact
/// effect shape. Autonomous fallback must not turn that durable `RejectOnce`
/// into Allow during live retry or recovery.
#[tokio::test]
async fn autonomous_fallback_honors_exact_one_shot_user_rejection() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let mut policy = PermissionPolicy::default();
    let operation = FsRead::new("src/lib.rs");
    let asked = broker
        .normalize(&operation)
        .await
        .expect("normalize initial operation");
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&asked, &policy)
        .await
        .expect("initial operation asks")
    else {
        panic!("default policy must ask");
    };
    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: Some("deny".into()),
                option_index: 2,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("commit exact one-shot rejection");
    policy.auto_allow_asks();

    let retried = broker
        .normalize(&operation)
        .await
        .expect("normalize exact retry");
    assert_eq!(
        broker
            .authorize(&retried, &policy)
            .await
            .expect("authorize exact retry"),
        AuthorizationVerdict::Deny {
            reason: "rejected once by user".into(),
        }
    );
    assert!(!effect_phases(&broker).iter().any(|phase| {
        matches!(phase, EffectPhase::Dispatched { effect } if effect == &retried.effect)
    }));
}

#[tokio::test]
async fn authorization_rejects_any_substitute_for_the_journaled_intent() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let original = broker
        .normalize(&FsEdit::new("src/lib.rs", "mod broker;", "mod bypass;"))
        .await
        .expect("normalize write intent");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsRead);
    policy.deny(EffectClass::FsWrite, "writes denied");

    let mut changed_digest = original.clone();
    changed_digest.args_digest = "blake3:substituted".into();
    let digest_error = broker
        .authorize(&changed_digest, &policy)
        .await
        .expect_err("same id with another digest must require a new intent");
    assert!(matches!(
        digest_error,
        haider_tools::ToolError::Lifecycle { .. }
    ));

    let mut changed_class = original.clone();
    changed_class.class = EffectClass::FsRead;
    let class_error = broker
        .authorize(&changed_class, &policy)
        .await
        .expect_err("journaled write cannot be authorized as a read");
    assert!(matches!(
        class_error,
        haider_tools::ToolError::Lifecycle { .. }
    ));

    assert!(matches!(
        broker
            .authorize(&original, &policy)
            .await
            .expect("original intent remains authorizable"),
        AuthorizationVerdict::Deny { .. }
    ));
}

#[tokio::test]
async fn frozen_clock_restarts_use_generation_stamped_effect_and_menu_ids() {
    let mut first = broker_at(RecordingJournal::default(), source_root(), 41);
    let mut restarted = broker_at(RecordingJournal::default(), source_root(), 42);
    let operation = FsRead::new("src/lib.rs");
    let first_intent = first.normalize(&operation).await.expect("first intent");
    let restarted_intent = restarted
        .normalize(&operation)
        .await
        .expect("restarted intent");
    let policy = PermissionPolicy::default();
    let AuthorizationVerdict::Ask { menu: first_menu } = first
        .authorize(&first_intent, &policy)
        .await
        .expect("first menu")
    else {
        panic!("default policy asks");
    };
    let AuthorizationVerdict::Ask {
        menu: restarted_menu,
    } = restarted
        .authorize(&restarted_intent, &policy)
        .await
        .expect("restarted menu")
    else {
        panic!("default policy asks");
    };

    assert_ne!(first_intent.effect, restarted_intent.effect);
    assert_ne!(first_menu, restarted_menu);
    assert_eq!(
        first_intent.effect.as_str(),
        "effect-session-41-1700000000000-1"
    );
    assert_eq!(
        restarted_menu.as_str(),
        "permission-session-42-1700000000000-1"
    );
}

#[tokio::test]
async fn approve_for_session_menu_resolution_creates_a_class_rule() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
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
        option.key == "approve_for_session"
            && option.decision == Some(haider_protocol::menu::DecisionKind::AllowAlways)
    }));

    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: Some("approve_for_session".into()),
                option_index: 0,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("menu resolves");

    assert!(policy.always_allow_rules().is_empty());
    assert_eq!(
        policy.session_allowlist(),
        &[haider_tools::SessionGrant {
            class: EffectClass::FsRead,
            scope: haider_tools::SessionGrantScope::Class,
        }]
    );
    let retry = broker
        .normalize(&FsRead::new("src/broker.rs"))
        .await
        .expect("normalize another read in the approved class");
    assert_eq!(
        broker
            .authorize(&retry, &policy)
            .await
            .expect("class grant authorizes"),
        AuthorizationVerdict::Allow
    );
}

/// W4a2 command-shape sentinel.
///
/// MUTATION CHECK: change `SessionGrant::for_effect` to return `Class` for
/// `ProcessExec`, or ignore the command-shape digest in `matches`. The
/// different-command assertion must then fail. Verified by revert in W4a2.
#[tokio::test]
async fn process_session_grant_is_exact_command_shape_and_never_class_wide() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::create_dir(workspace.path().join("nested")).expect("nested cwd");
    let mut broker = broker_at(RecordingJournal::default(), workspace.path(), 1);
    let mut policy = PermissionPolicy::default();
    policy.ask(EffectClass::ProcessExec);
    let operation = ProcessExec::new("first-call", "printf exact");
    let intent = broker
        .normalize(&operation)
        .await
        .expect("normalize command");
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&intent, &policy)
        .await
        .expect("first command asks")
    else {
        panic!("process policy must ask");
    };
    let opened = broker.permission_menu(&menu).expect("permission menu");
    assert!(
        opened
            .body
            .iter()
            .any(|line| line.contains("\"printf exact\"")),
        "approval must show the exact escaped command"
    );
    assert!(
        opened
            .body
            .iter()
            .any(|line| line.contains("exact command shape"))
    );
    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: Some("approve_for_session".into()),
                option_index: 1,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("shape grant resolves");

    let same = broker
        .normalize(&ProcessExec::new("second-call", "printf exact"))
        .await
        .expect("normalize same shape");
    assert_eq!(same.args_digest, intent.args_digest);
    assert_eq!(
        broker
            .authorize(&same, &policy)
            .await
            .expect("same shape authorizes"),
        AuthorizationVerdict::Allow
    );

    let different = broker
        .normalize(&ProcessExec::new("different-call", "printf different"))
        .await
        .expect("normalize different command");
    assert!(matches!(
        broker
            .authorize(&different, &policy)
            .await
            .expect("different shape re-prompts"),
        AuthorizationVerdict::Ask { .. }
    ));

    let different_cwd = broker
        .normalize(
            &ProcessExec::new("different-cwd", "printf exact")
                .with_cwd(workspace.path().join("nested")),
        )
        .await
        .expect("normalize different cwd");
    assert!(matches!(
        broker
            .authorize(&different_cwd, &policy)
            .await
            .expect("different cwd re-prompts"),
        AuthorizationVerdict::Ask { .. }
    ));

    assert!(
        PermissionPolicy::default()
            .allow_for_session(EffectClass::ProcessExec)
            .is_err(),
        "the API must fail closed on class-wide shell grants"
    );
}

#[tokio::test]
async fn unknown_permission_key_fails_closed_and_keeps_menu_answerable() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let mut policy = PermissionPolicy::default();
    let intent = broker
        .normalize(&FsRead::new("src/lib.rs"))
        .await
        .expect("normalize");
    let AuthorizationVerdict::Ask { menu } = broker.authorize(&intent, &policy).await.expect("ask")
    else {
        panic!("default policy asks");
    };
    let error = broker
        .resolve_permission(
            &MenuAnswer {
                menu: menu.clone(),
                option_key: Some("reject_typo".into()),
                option_index: 0,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect_err("unknown key must not fall back to allow-once index");
    assert!(matches!(
        error,
        haider_tools::ToolError::InvalidMenuAnswer { .. }
    ));
    assert!(broker.permission_menu(&menu).is_some());

    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: Some("deny".into()),
                option_index: 0,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("same menu can be answered correctly");
}

#[tokio::test]
async fn out_of_range_permission_index_fails_closed_and_keeps_menu_answerable() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let mut policy = PermissionPolicy::default();
    let intent = broker
        .normalize(&FsRead::new("src/lib.rs"))
        .await
        .expect("normalize");
    let AuthorizationVerdict::Ask { menu } = broker.authorize(&intent, &policy).await.expect("ask")
    else {
        panic!("default policy asks");
    };
    let error = broker
        .resolve_permission(
            &MenuAnswer {
                menu: menu.clone(),
                option_key: None,
                option_index: u32::MAX,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect_err("invalid index must fail closed");
    assert!(matches!(
        error,
        haider_tools::ToolError::InvalidMenuAnswer { .. }
    ));
    assert!(broker.permission_menu(&menu).is_some());

    broker
        .resolve_permission(
            &MenuAnswer {
                menu,
                option_key: None,
                option_index: 2,
                value: None,
                via: AnswerVia::Rpc,
            },
            &mut policy,
        )
        .expect("same menu can be answered correctly");
}

#[tokio::test]
async fn allow_once_resolution_grants_a_single_retry() {
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
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
                option_key: Some("approve_once".into()),
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
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
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
                option_key: Some("deny".into()),
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
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let mut policy = PermissionPolicy::default();
    policy.deny(EffectClass::FsWrite, "workspace is read-only");
    let intent = broker
        .normalize(&FsWrite::new("file.txt", "after"))
        .await
        .expect("normalize");
    assert!(matches!(
        broker.authorize(&intent, &policy).await.expect("authorize"),
        AuthorizationVerdict::Deny { .. }
    ));

    let error = broker
        .journal_dispatched(&intent)
        .await
        .expect_err("dispatch after deny must be refused");
    assert!(matches!(error, haider_tools::ToolError::Lifecycle { .. }));
    assert!(
        !effect_phases(&broker)
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
    let mut broker = broker_at(RecordingJournal::default(), directory.path(), 1);
    let ledger = haider_tools::ChangeLedger::new();
    let attribution = haider_tools::TurnAttribution::new(
        haider_protocol::ids::SessionId::new("session"),
        haider_protocol::ids::RunId::new("turn"),
    );

    let error = broker
        .fs_edit(
            &FsEdit::new(&path, "before", "after"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect_err("deny must block edit");

    assert!(matches!(
        error,
        haider_tools::ToolError::PermissionDenied { .. }
    ));
    assert_eq!(fs::read_to_string(&path).expect("read file"), "before");
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
    let phases = effect_phases(&broker);
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
    let mut broker = broker_at(RejectDispatchJournal::default(), directory.path(), 1);
    let ledger = haider_tools::ChangeLedger::new();
    let attribution = haider_tools::TurnAttribution::new(
        haider_protocol::ids::SessionId::new("session"),
        haider_protocol::ids::RunId::new("turn"),
    );

    let error = broker
        .fs_edit(
            &FsEdit::new(&path, "before", "after"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect_err("dispatch append must gate apply");

    assert!(matches!(error, haider_tools::ToolError::Journal { .. }));
    assert_eq!(fs::read_to_string(&path).expect("read file"), "before");
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
    assert_eq!(broker.journal_snapshot().len(), 2);
}

/// Windows byte-range locks are mandatory even between handles in one process.
/// Publication must release every staged/validation handle before the broker
/// reports success, while subsequent mutations must re-read through the handle
/// that owns the source lock.
#[cfg(windows)]
#[tokio::test]
async fn windows_published_mutations_are_readable_while_broker_is_alive() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsWrite);
    let mut broker = broker_at(RecordingJournal::default(), directory.path(), 1);
    let ledger = haider_tools::ChangeLedger::new();
    let attribution = haider_tools::TurnAttribution::new(
        haider_protocol::ids::SessionId::new("session"),
        haider_protocol::ids::RunId::new("turn"),
    );
    let target = directory.path().join("published.txt");

    broker
        .fs_write(
            &FsWrite::new("published.txt", "before"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("publish Windows write");
    let mut independent = fs::File::open(&target).expect("independently open published write");
    let mut contents = String::new();
    independent
        .read_to_string(&mut contents)
        .expect("independently read published write");
    assert_eq!(contents, "before");
    drop(independent);

    broker
        .fs_edit(
            &FsEdit::new("published.txt", "before", "after"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("publish Windows edit through locked-source revalidation");
    let mut independent = fs::File::open(&target).expect("independently open published edit");
    contents.clear();
    independent
        .read_to_string(&mut contents)
        .expect("independently read published edit");
    assert_eq!(contents, "after");

    assert_eq!(
        ledger
            .changes_for(&attribution.session, &attribution.turn)
            .expect("published mutations are ledgered")
            .writes
            .len(),
        2
    );
    assert_eq!(broker.journal_snapshot().len(), 8);
}

/// Windows owns a distinct path-based atomic replacement backend. Keep one
/// integration law on that cfg so an approval can never regress back to a
/// successful tool result with no disk mutation.
#[cfg(windows)]
#[tokio::test]
async fn windows_write_edit_and_overwrite_apply_and_ledger() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsWrite);
    let mut broker = broker_at(RecordingJournal::default(), directory.path(), 1);
    let ledger = haider_tools::ChangeLedger::new();
    let attribution = haider_tools::TurnAttribution::new(
        haider_protocol::ids::SessionId::new("session"),
        haider_protocol::ids::RunId::new("turn"),
    );

    broker
        .fs_write(
            &FsWrite::new("nested/windows-mutation.txt", "before"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("create Windows file");
    let mutation_path = directory.path().join("nested/windows-mutation.txt");
    broker
        .fs_edit(
            &FsEdit::new("nested/windows-mutation.txt", "before", "after"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("edit Windows file");
    broker
        .fs_write(
            &FsWrite::new("nested/windows-mutation.txt", "final"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("overwrite Windows file");
    broker
        .fs_path(
            &FsPath::new(FsPathOperation::Copy, "nested/windows-mutation.txt")
                .with_destination("nested/windows-copy.txt"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("copy Windows file");
    broker
        .fs_path(
            &FsPath::new(FsPathOperation::Move, "nested/windows-copy.txt")
                .with_destination("nested/windows-moved.txt"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("move Windows file");
    broker
        .fs_path(
            &FsPath::new(FsPathOperation::Delete, "nested/windows-moved.txt"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("delete Windows file");

    assert_eq!(
        fs::read_to_string(&mutation_path).expect("read final Windows file"),
        "final"
    );
    assert!(!directory.path().join("nested/windows-copy.txt").exists());
    assert!(!directory.path().join("nested/windows-moved.txt").exists());
    let changes = ledger
        .changes_for(&attribution.session, &attribution.turn)
        .expect("Windows mutations are ledgered");
    assert_eq!(changes.writes.len(), 6);
}

/// Windows path ancestry is identity-based, not case-sensitive lexical text.
/// Otherwise staging `Dir` into `dir\child` recursively copies its own temp.
#[cfg(windows)]
#[tokio::test]
async fn windows_copy_rejects_case_aliased_destination_inside_source() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("Dir")).expect("source directory");
    fs::write(directory.path().join("Dir/source.txt"), "source").expect("source file");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsWrite);
    let mut broker = broker_at(RecordingJournal::default(), directory.path(), 1);
    let ledger = haider_tools::ChangeLedger::new();
    let attribution = haider_tools::TurnAttribution::new(
        haider_protocol::ids::SessionId::new("session"),
        haider_protocol::ids::RunId::new("turn"),
    );

    let error = broker
        .fs_path(
            &FsPath::new(FsPathOperation::Copy, "Dir").with_destination("dir/recursive-child"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect_err("case-aliased nested destination must be rejected");
    assert!(matches!(
        error,
        haider_tools::ToolError::InvalidArgument { .. }
    ));
    assert_eq!(
        fs::read_to_string(directory.path().join("Dir/source.txt")).expect("source survives"),
        "source"
    );
    assert!(!directory.path().join("Dir/recursive-child").exists());
}

/// PowerShell is resolved before `env_clear`, but Windows system commands also
/// require a small non-secret bootstrap environment after that clear.
#[cfg(windows)]
#[tokio::test]
async fn windows_process_exec_restores_the_system_bootstrap_environment() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ProcessExec);
    let mut broker = broker_at(RecordingJournal::default(), directory.path(), 1);
    let execution = broker
        .process_exec(
            &ProcessExec::new(
                "windows-bootstrap",
                "if ($env:SystemRoot) {[IO.File]::WriteAllText('bootstrap.txt','ok')} else {exit 9}",
            ),
            &policy,
            UnusedCas,
            haider_tools::NoopCommandOutputSink,
            haider_tools::ProcessBounds::default(),
        )
        .await
        .expect("spawn Windows command");
    let result = execution.wait().await.expect("supervise Windows command");

    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        fs::read_to_string(directory.path().join("bootstrap.txt"))
            .expect("read Windows bootstrap marker")
            .trim(),
        "ok"
    );
}

#[tokio::test]
async fn successful_dispatch_has_strict_four_phase_order() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("read.txt");
    fs::write(&path, "small result").expect("seed file");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsRead);
    let mut broker = broker_at(RecordingJournal::default(), directory.path(), 1);
    let bounds = ResultBounds::default();
    assert_eq!(haider_tools::TOOL_RESULT_INLINE_MAX_BYTES, 8 * 1024);
    assert_eq!(bounds.max_preview_bytes, 8 * 1024);

    let result = broker
        .fs_read(&FsRead::new(&path), &policy, &mut UnusedCas, bounds)
        .await
        .expect("read succeeds");
    assert_eq!(result.preview, "small result");

    let phases = effect_phases(&broker);
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
    let mut broker = broker_at(RecordingJournal::default(), source_root(), 1);
    let intent = broker
        .normalize(&FsRead::new("src/lib.rs"))
        .await
        .expect("normalize");
    assert_eq!(
        broker.authorize(&intent, &policy).await.expect("authorize"),
        AuthorizationVerdict::Allow
    );
    broker
        .journal_dispatched(&intent)
        .await
        .expect("journal dispatched");
    broker
        .journal_unknown(&intent)
        .await
        .expect("reconcile unknown");

    assert!(matches!(
        effect_phases(&broker).last(),
        Some(EffectPhase::Outcome {
            outcome: EffectOutcome::Unknown,
            ..
        })
    ));
}

// ── Orderly cancel vs crash window (dogfood bug 4) ──────────────────────
//
// The one deliberate exception to the header rule above: these two pins
// must read the journal AFTER close/cancel consumes the broker, so they
// share the payload vector behind an Arc.

#[derive(Debug, Clone, Default)]
struct SharedJournal {
    payloads: std::sync::Arc<std::sync::Mutex<Vec<EventPayload>>>,
}

#[async_trait::async_trait]
impl JournalSink for SharedJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(payload);
        Ok(())
    }

    fn supports_checkpoint_batches(&self) -> bool {
        true
    }

    fn supports_checkpoint_artifacts(&self) -> bool {
        true
    }

    async fn put_checkpoint_artifact(
        &mut self,
        bytes: &[u8],
    ) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        Ok(haider_protocol::ids::ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(bytes).to_hex()
        )))
    }

    async fn append_checkpointed(
        &mut self,
        outcome: EventPayload,
        checkpoint: EventPayload,
    ) -> ToolResult<()> {
        self.payloads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([outcome, checkpoint]);
        Ok(())
    }
}

fn residual_outcomes(journal: &SharedJournal) -> Vec<EffectOutcome> {
    journal
        .payloads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(EffectPhase::Outcome { outcome, .. }) => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

async fn dispatch_without_outcome(broker: &mut EffectBroker) {
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::FsRead);
    let intent = broker
        .normalize(&FsRead::new("src/lib.rs"))
        .await
        .expect("normalize");
    assert_eq!(
        broker.authorize(&intent, &policy).await.expect("authorize"),
        AuthorizationVerdict::Allow
    );
    broker
        .journal_dispatched(&intent)
        .await
        .expect("journal dispatched");
}

/// MUTATION CHECK (dogfood bug 4): make `EffectBroker::cancel` delegate to
/// the ordinary close fallback (Unknown). Expected runtime failure: an
/// orderly user cancellation records a crash window and the recovery card
/// reopens for a turn the user deliberately ended.
#[tokio::test]
async fn an_orderly_cancel_terminalizes_a_residual_dispatch_as_cancelled() {
    let journal = SharedJournal::default();
    let mut broker = broker_at(journal.clone(), source_root(), 1);
    dispatch_without_outcome(&mut broker).await;
    let _ = broker.cancel().await;
    let outcomes = residual_outcomes(&journal);
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, EffectOutcome::Cancelled)),
        "the abandoned dispatch closes as Cancelled: {outcomes:?}"
    );
    assert!(
        !outcomes
            .iter()
            .any(|outcome| matches!(outcome, EffectOutcome::Unknown)),
        "an orderly cancel never records a crash window: {outcomes:?}"
    );
}

/// The genuine-crash half stays intact: an ORDINARY close of the same
/// residual dispatch still reconciles it as the Unknown crash window.
#[tokio::test]
async fn an_ordinary_close_still_reconciles_a_residual_dispatch_as_unknown() {
    let journal = SharedJournal::default();
    let mut broker = broker_at(journal.clone(), source_root(), 1);
    dispatch_without_outcome(&mut broker).await;
    let _ = broker.close().await;
    let outcomes = residual_outcomes(&journal);
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, EffectOutcome::Unknown)),
        "a non-cancel close keeps the crash-window classification: {outcomes:?}"
    );
    assert!(
        !outcomes
            .iter()
            .any(|outcome| matches!(outcome, EffectOutcome::Cancelled)),
        "no Cancelled is fabricated outside an orderly cancel: {outcomes:?}"
    );
}
