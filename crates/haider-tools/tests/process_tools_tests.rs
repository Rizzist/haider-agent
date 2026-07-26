#![allow(clippy::expect_used)]

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationSource, AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase,
};
use haider_protocol::ids::{ArtifactRef, SessionId};
use haider_protocol::item::{ItemDelta, ToolStatus};
use haider_tools::{
    BuiltinResult, CasSink, CommandOutputSink, ComposerSubmission, EffectBroker, JournalSink,
    PermissionPolicy, ProcessBounds, ProcessControl, ProcessExec, ProcessOutputChunk, ShellSession,
    ToolError, ToolResult,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Default)]
struct SharedJournal {
    payloads: Arc<Mutex<Vec<EventPayload>>>,
}

impl SharedJournal {
    fn observer(&self) -> Arc<Mutex<Vec<EventPayload>>> {
        Arc::clone(&self.payloads)
    }
}

#[async_trait]
impl JournalSink for SharedJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads
            .lock()
            .map_err(|_| ToolError::journal("recording journal lock poisoned"))?
            .push(payload);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct RecordingCas {
    bytes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RecordingCas {
    fn observer(&self) -> Arc<Mutex<Vec<Vec<u8>>>> {
        Arc::clone(&self.bytes)
    }
}

#[async_trait]
impl CasSink for RecordingCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        self.bytes
            .lock()
            .map_err(|_| ToolError::cas("recording CAS lock poisoned"))?
            .push(bytes.to_vec());
        Ok(ArtifactRef::new(format!("blake3:{}", blake3::hash(bytes))))
    }
}

#[derive(Debug, Default)]
struct RecordingOutput {
    deltas: Arc<Mutex<Vec<ItemDelta>>>,
}

impl RecordingOutput {
    fn observer(&self) -> Arc<Mutex<Vec<ItemDelta>>> {
        Arc::clone(&self.deltas)
    }
}

#[async_trait]
impl CommandOutputSink for RecordingOutput {
    async fn emit(&self, _call_id: &str, delta: ItemDelta) -> ToolResult<()> {
        self.deltas
            .lock()
            .map_err(|_| ToolError::Runtime {
                message: "output observer lock poisoned".into(),
            })?
            .push(delta);
        Ok(())
    }
}

fn broker(root: &Path) -> (EffectBroker, Arc<Mutex<Vec<EventPayload>>>) {
    let journal = SharedJournal::default();
    let observer = journal.observer();
    let broker = EffectBroker::new_at(
        Box::new(journal),
        root,
        SessionId::new("process-session"),
        3,
        1_700_000_000_000,
    )
    .expect("create broker");
    (broker, observer)
}

fn process_policy() -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ProcessExec);
    policy
}

fn phases(observer: &Arc<Mutex<Vec<EventPayload>>>) -> Vec<EffectPhase> {
    observer
        .lock()
        .expect("journal observer")
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase.clone()),
            _ => None,
        })
        .collect()
}

fn output_bytes(deltas: &[ItemDelta]) -> Vec<u8> {
    deltas
        .iter()
        .flat_map(|delta| match delta {
            ItemDelta::CommandOutput { chunk_b64, .. } => {
                BASE64.decode(chunk_b64).expect("valid base64")
            }
            _ => panic!("process sink only receives command output"),
        })
        .collect()
}

#[tokio::test]
async fn process_exec_streams_exact_bytes_freezes_overflow_and_journals_four_phases() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let cas = RecordingCas::default();
    let cas_observer = cas.observer();
    let output = RecordingOutput::default();
    let output_observer = output.observer();
    let execution = broker
        .process_exec(
            &ProcessExec::new("bytes", "printf '\\377abc'"),
            &process_policy(),
            cas,
            output,
            ProcessBounds {
                max_inline_bytes: 2,
                ..ProcessBounds::default()
            },
        )
        .await
        .expect("process starts");
    let result = execution.wait().await.expect("process completes");

    assert_eq!(result.status, ToolStatus::Completed);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.output_bytes, 4);
    assert!(result.inline_output.is_empty());
    assert!(result.artifact.is_some());
    assert_eq!(
        output_bytes(&output_observer.lock().expect("output observer")),
        [0xff, b'a', b'b', b'c']
    );

    {
        let artifacts = cas_observer.lock().expect("CAS observer");
        assert_eq!(artifacts.len(), 1);
        let frozen: Vec<ProcessOutputChunk> =
            serde_json::from_slice(&artifacts[0]).expect("frozen byte transcript");
        let frozen_bytes: Vec<_> = frozen
            .iter()
            .flat_map(|chunk| BASE64.decode(&chunk.chunk_b64).expect("base64"))
            .collect();
        assert_eq!(frozen_bytes, [0xff, b'a', b'b', b'c']);
    }

    let phases = phases(&journal);
    assert_eq!(phases.len(), 4);
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
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn process_digest_is_exactly_command_canonical_cwd_and_sorted_env_allowlist() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, _journal) = broker(workspace.path());
    let operation = ProcessExec::new("not-digested", "printf ok")
        .with_cwd(".")
        .with_env_allowlist(vec!["ZED".into(), "ALPHA".into(), "ZED".into()]);
    let intent = broker.normalize(&operation).await.expect("normalize");
    let canonical_cwd = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    let canonical = serde_json::to_vec(&serde_json::json!({
        "command": "printf ok",
        "cwd": canonical_cwd.to_str().expect("UTF-8 test path"),
        "env_allowlist": ["ALPHA", "ZED"],
    }))
    .expect("canonical JSON");
    assert_eq!(
        intent.args_digest,
        format!("blake3:{}", blake3::hash(&canonical))
    );

    let another_call = ProcessExec {
        call_id: "another-id".into(),
        ..operation
    };
    let another = broker
        .normalize(&another_call)
        .await
        .expect("normalize another call id");
    assert_eq!(
        another.args_digest, intent.args_digest,
        "call_id is correlation identity, not an execution argument"
    );
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn stdin_process_control_is_a_second_effect_bound_to_the_live_execution() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let output = RecordingOutput::default();
    let output_observer = output.observer();
    let execution = broker
        .process_exec(
            &ProcessExec::new("interactive", "read line; printf '%s' \"$line\""),
            &process_policy(),
            RecordingCas::default(),
            output,
            ProcessBounds::default(),
        )
        .await
        .expect("process starts");
    let original_effect = execution.effect().clone();
    let controlled = broker
        .process_control(
            &ProcessControl::stdin_write("interactive", b"hello\n".to_vec()),
            &process_policy(),
        )
        .await
        .expect("stdin control");
    assert_eq!(controlled.original_effect, original_effect);
    let result = execution.wait().await.expect("process completes");
    assert_eq!(result.status, ToolStatus::Completed);
    assert_eq!(
        output_bytes(&output_observer.lock().expect("output observer")),
        b"hello"
    );

    let phases = phases(&journal);
    assert_eq!(phases.len(), 8);
    let first_effect = match &phases[0] {
        EffectPhase::Intent(intent) => intent.effect.clone(),
        phase => panic!("expected first intent, got {phase:?}"),
    };
    let control_effect = match &phases[3] {
        EffectPhase::Intent(intent) => intent.effect.clone(),
        phase => panic!("expected control intent, got {phase:?}"),
    };
    assert_eq!(first_effect, original_effect);
    assert_ne!(control_effect, original_effect);
    assert!(matches!(phases[6], EffectPhase::Outcome { .. }));
    assert!(matches!(phases[7], EffectPhase::Outcome { .. }));
    broker.close().await.expect("broker closes");
}

#[tokio::test(start_paused = true)]
async fn cancellation_waits_for_grace_then_kills_the_group_and_is_not_a_failure() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let grace = Duration::from_secs(5);
    let output = RecordingOutput::default();
    let output_observer = output.observer();
    let execution = broker
        .process_exec(
            &ProcessExec::new(
                "stubborn",
                "trap '' TERM; printf ready; while :; do :; done",
            ),
            &process_policy(),
            RecordingCas::default(),
            output,
            ProcessBounds {
                max_inline_bytes: 1024,
                kill_grace: grace,
            },
        )
        .await
        .expect("process starts");
    while output_observer.lock().expect("output observer").is_empty() {
        tokio::task::yield_now().await;
    }
    execution.cancel();
    tokio::task::yield_now().await;
    tokio::time::advance(grace - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(
        phases(&journal)
            .iter()
            .all(|phase| !matches!(phase, EffectPhase::Outcome { .. })),
        "the original effect stays dispatched until supervised termination"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    let result = execution.wait().await.expect("cancelled result");
    assert_eq!(result.status, ToolStatus::Cancelled);

    let phases = phases(&journal);
    assert_eq!(phases.len(), 4);
    assert!(matches!(
        phases[3],
        EffectPhase::Outcome {
            outcome: EffectOutcome::Cancelled,
            ..
        }
    ));
    broker.close().await.expect("broker closes");
}

#[tokio::test(start_paused = true)]
async fn process_control_kill_is_brokered_and_cancels_the_original_as_an_outcome() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let grace = Duration::from_secs(3);
    let output = RecordingOutput::default();
    let output_observer = output.observer();
    let execution = broker
        .process_exec(
            &ProcessExec::new(
                "controlled-kill",
                "trap '' TERM; printf ready; while :; do :; done",
            ),
            &process_policy(),
            RecordingCas::default(),
            output,
            ProcessBounds {
                max_inline_bytes: 1024,
                kill_grace: grace,
            },
        )
        .await
        .expect("process starts");
    while output_observer.lock().expect("output observer").is_empty() {
        tokio::task::yield_now().await;
    }
    let control = broker
        .process_control(&ProcessControl::kill("controlled-kill"), &process_policy())
        .await
        .expect("kill control is brokered");
    assert_eq!(control.original_effect, execution.effect().clone());
    tokio::time::advance(grace).await;
    let result = execution.wait().await.expect("cancelled result");
    assert_eq!(result.status, ToolStatus::Cancelled);

    let phases = phases(&journal);
    assert_eq!(phases.len(), 8);
    assert_eq!(
        phases
            .iter()
            .filter(|phase| matches!(phase, EffectPhase::Intent(_)))
            .count(),
        2
    );
    assert_eq!(
        phases
            .iter()
            .filter(|phase| matches!(phase, EffectPhase::Authorized { .. }))
            .count(),
        2
    );
    assert_eq!(
        phases
            .iter()
            .filter(|phase| matches!(phase, EffectPhase::Dispatched { .. }))
            .count(),
        2
    );
    assert_eq!(
        phases
            .iter()
            .filter(|phase| matches!(phase, EffectPhase::Outcome { .. }))
            .count(),
        2
    );
    assert!(phases.iter().any(|phase| matches!(
        phase,
        EffectPhase::Outcome {
            effect,
            outcome: EffectOutcome::Cancelled,
        } if effect == &control.original_effect
    )));
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn shell_builtins_do_not_spawn_and_escaped_commands_are_user_preauthorized() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(workspace.path().join("nested")).expect("nested dir");
    let mut shell =
        ShellSession::new(workspace.path(), vec!["HAIDER_TEST_MISSING_ENV".into()]).expect("shell");
    let changed = shell.submit("!cd nested").expect("cd builtin");
    assert!(matches!(
        changed,
        ComposerSubmission::Builtin(BuiltinResult::ChangedDirectory { .. })
    ));
    assert_eq!(
        shell.cwd(),
        std::fs::canonicalize(workspace.path().join("nested")).expect("canonical nested")
    );
    let viewed = shell.submit("!env-view").expect("env builtin");
    let ComposerSubmission::Builtin(BuiltinResult::Environment { entries }) = viewed else {
        panic!("env-view must resolve without a process");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "HAIDER_TEST_MISSING_ENV");
    assert_eq!(entries[0].value, None);

    let ComposerSubmission::UserProcess(operation) =
        shell.submit("!printf user").expect("shell process")
    else {
        panic!("non-builtin escape must become a user process");
    };
    let (mut broker, journal) = broker(workspace.path());
    let result = broker
        .process_exec_user(
            &operation,
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("user process starts")
        .wait()
        .await
        .expect("user process completes");
    assert_eq!(result.status, ToolStatus::Completed);
    let phases = phases(&journal);
    assert_eq!(phases.len(), 4);
    assert!(matches!(
        phases[1],
        EffectPhase::Authorized {
            verdict: AuthorizationVerdict::PreAuthorized {
                source: AuthorizationSource::UserTyped,
            },
            ..
        }
    ));
    broker.close().await.expect("broker closes");
}
