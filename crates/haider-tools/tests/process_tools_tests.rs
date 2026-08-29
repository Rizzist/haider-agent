#![cfg(unix)]
#![allow(clippy::expect_used)]

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationSource, AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase,
    WorkspaceMutation,
};
use haider_protocol::ids::{ArtifactRef, SessionId};
use haider_protocol::item::{ItemDelta, ToolStatus};
use haider_tools::{
    BuiltinResult, CasSink, CommandOutputSink, ComposerSubmission, EffectBroker, JournalSink,
    PROCESS_ADAPTER_INPUT_BYTES, PROCESS_OUTPUT_CHUNK_BYTES, PermissionPolicy, ProcessBounds,
    ProcessControl, ProcessExec, ProcessLifecycleEvent, ProcessOutputChunk, REDACTED_ENV_VALUE,
    ShellSession, ToolError, ToolResult, workspace_state_digest,
};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Debug, Default)]
struct SharedJournal {
    payloads: Arc<Mutex<Vec<EventPayload>>>,
}

struct SwapCwdJournal {
    target: std::path::PathBuf,
    anchored: std::path::PathBuf,
    replacement: std::path::PathBuf,
    swapped: bool,
}

#[async_trait]
impl JournalSink for SwapCwdJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if !self.swapped
            && matches!(
                payload,
                EventPayload::Effect(EffectPhase::Authorized {
                    verdict: AuthorizationVerdict::Allow,
                    ..
                })
            )
        {
            std::fs::rename(&self.target, &self.anchored).map_err(|error| ToolError::Runtime {
                message: format!("move authorized cwd: {error}"),
            })?;
            std::os::unix::fs::symlink(&self.replacement, &self.target).map_err(|error| {
                ToolError::Runtime {
                    message: format!("replace authorized cwd: {error}"),
                }
            })?;
            self.swapped = true;
        }
        Ok(())
    }
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

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        let bytes = std::fs::read(path)
            .map_err(|error| ToolError::cas(format!("read recording CAS source: {error}")))?;
        self.put(&bytes).await
    }
}

#[derive(Debug, Default)]
struct FailingCas;

#[async_trait]
impl CasSink for FailingCas {
    async fn put(&mut self, _bytes: &[u8]) -> ToolResult<ArtifactRef> {
        Err(ToolError::cas("injected CAS failure"))
    }

    async fn put_file(&mut self, _path: &Path) -> ToolResult<ArtifactRef> {
        Err(ToolError::cas("injected CAS file failure"))
    }
}

#[derive(Debug, Clone)]
struct GatedCas {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    fail: bool,
}

#[async_trait]
impl CasSink for GatedCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        Ok(ArtifactRef::new(format!("blake3:{}", blake3::hash(bytes))))
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail {
            return Err(ToolError::cas("gated CAS failure"));
        }
        let bytes = std::fs::read(path)
            .map_err(|error| ToolError::cas(format!("read gated CAS source: {error}")))?;
        self.put(&bytes).await
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

#[derive(Debug, Clone, Default)]
struct FailingOutput {
    attempted: Arc<Notify>,
}

#[async_trait]
impl CommandOutputSink for FailingOutput {
    async fn emit(&self, _call_id: &str, _delta: ItemDelta) -> ToolResult<()> {
        self.attempted.notify_one();
        Err(ToolError::Runtime {
            message: "injected output sink failure".into(),
        })
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

fn run_git(workspace: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("run git fixture command");
    assert!(status.success(), "git fixture command failed: {args:?}");
}

fn initialize_git_workspace(workspace: &Path) {
    run_git(workspace, &["init", "-q"]);
    run_git(workspace, &["add", "."]);
    run_git(
        workspace,
        &[
            "-c",
            "user.name=Haider Tests",
            "-c",
            "user.email=haider-tests@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    );
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

fn workspace_mutations(observer: &Arc<Mutex<Vec<EventPayload>>>) -> Vec<Option<WorkspaceMutation>> {
    phases(observer)
        .into_iter()
        .filter_map(|phase| match phase {
            EffectPhase::Outcome {
                workspace_mutation, ..
            } => Some(workspace_mutation),
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

/// Regression: the pre/post execution receipt must not read an arbitrarily
/// large workspace file in full before the command can start or complete.
/// The old full-content snapshot leaves this test before process spawn while
/// reading the sparse tebibyte fixture, exactly like a large `target/` tree.
#[test]
fn process_exec_runs_and_returns_output_with_a_huge_workspace_file() {
    let workspace = tempfile::tempdir().expect("tempdir");
    run_git(workspace.path(), &["init", "-q"]);
    let huge_path = workspace.path().join("huge-sparse.bin");
    let huge_file = fs::File::create(&huge_path).expect("create sparse workspace file");
    huge_file
        .set_len(1024 * 1024 * 1024 * 1024)
        .expect("size sparse workspace file");
    drop(huge_file);

    let (digest_sender, digest_receiver) = std::sync::mpsc::channel();
    let snapshot_root = workspace.path().to_path_buf();
    std::thread::spawn(move || {
        let _ = digest_sender.send(workspace_state_digest(&snapshot_root));
    });
    let digest = digest_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("workspace receipt must not read a huge file in full");
    assert!(digest.starts_with("blake3:"));
    assert!(
        digest.contains("reason=content_limit") || digest.contains("reason=wall_time_limit"),
        "large-file receipt must expose the first hard bound reached: {digest}"
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (mut broker, _journal) = broker(workspace.path());
        let output = RecordingOutput::default();
        let observer = output.observer();
        let execution = tokio::time::timeout(
            Duration::from_secs(5),
            broker.process_exec(
                &ProcessExec::new("large-workspace", "printf 'scan-complete\\n'"),
                &process_policy(),
                RecordingCas::default(),
                output,
                ProcessBounds::default(),
            ),
        )
        .await
        .expect("large workspace must not block process spawn")
        .expect("process starts");
        let result = tokio::time::timeout(Duration::from_secs(5), execution.wait())
            .await
            .expect("large workspace must not block process completion")
            .expect("process completes");

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            output_bytes(&observer.lock().expect("output observer")),
            b"scan-complete\n"
        );
        broker.close().await.expect("broker closes");
    });
}

#[test]
fn process_exec_repository_without_a_git_binary_still_detects_source_mutation() {
    const CHILD: &str = "HAIDER_PROCESS_RECEIPT_NO_GIT_CHILD";
    if std::env::var(CHILD).as_deref() != Ok("1") {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "process_exec_repository_without_a_git_binary_still_detects_source_mutation",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PATH", "/definitely/no/git/here")
            .status()
            .expect("run no-git child");
        assert!(status.success(), "no-git child must pass");
        return;
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    fs::write(workspace.path().join("source.rs"), "before").expect("seed source");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (mut broker, journal) = broker(workspace.path());
        broker
            .process_exec(
                &ProcessExec::new("no-git-source", "printf after > source.rs"),
                &process_policy(),
                RecordingCas::default(),
                RecordingOutput::default(),
                ProcessBounds::default(),
            )
            .await
            .expect("process starts without git")
            .wait()
            .await
            .expect("process completes without git");

        assert!(
            workspace_mutations(&journal)
                .into_iter()
                .next()
                .flatten()
                .is_some(),
            "repository walk fallback must detect the source mutation"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("source.rs")).expect("read source"),
            "after"
        );
        broker.close().await.expect("broker closes");
    });
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
    assert_eq!(
        result.transcript_digest,
        format!(
            "blake3:{}",
            blake3::hash(&[0xff, b'a', b'b', b'c']).to_hex()
        )
    );
    assert_eq!(
        result.inline_output.len(),
        1,
        "adapter tail retains exact bytes"
    );
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
async fn workspace_mutation_fact_fires_only_when_process_changes_the_tree() {
    // LAW 1 mutation guard: deleting either comparison makes a pure command
    // advance revision state or lets a real process mutation disappear.
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("input.txt"), "stable").expect("seed input");
    initialize_git_workspace(workspace.path());
    let (mut broker, journal) = broker(workspace.path());

    broker
        .process_exec(
            &ProcessExec::new("pure-read", "cat input.txt >/dev/null"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("pure process starts")
        .wait()
        .await
        .expect("pure process completes");
    broker
        .process_exec(
            &ProcessExec::new("mutation", "printf changed > output.txt"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("mutating process starts")
        .wait()
        .await
        .expect("mutating process completes");

    let outcomes = workspace_mutations(&journal);
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes[0].is_none(), "pure read must not mutate");
    assert!(
        outcomes[1].is_some(),
        "real process mutation must be recorded"
    );
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn non_repository_source_mutation_is_conservatively_detected_without_a_walk() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("source.rs"), "before").expect("seed source");
    let (mut broker, journal) = broker(workspace.path());

    broker
        .process_exec(
            &ProcessExec::new("non-repo-mutation", "printf after > source.rs"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("process starts")
        .wait()
        .await
        .expect("process completes");

    let mutation = workspace_mutations(&journal)
        .into_iter()
        .next()
        .flatten()
        .expect("unknown non-repository coverage assumes mutation");
    assert!(
        mutation
            .mutation_digest
            .contains("reason=not_enumerated_non_repository")
    );
    assert!(mutation.mutation_digest.contains("before_entries=0"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("source.rs")).expect("read mutation"),
        "after"
    );
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn entry_bound_is_reported_and_source_mutation_still_registers() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::create_dir(workspace.path().join(".git")).expect("broken repository marker");
    fs::create_dir(workspace.path().join("wide")).expect("wide directory");
    for index in 0..4_200 {
        fs::write(
            workspace.path().join("wide").join(format!("entry-{index}")),
            b"content",
        )
        .expect("wide entry");
    }
    let (mut broker, journal) = broker(workspace.path());

    let execution = tokio::time::timeout(
        Duration::from_secs(5),
        broker.process_exec(
            &ProcessExec::new("bounded-tree", "printf changed > source.rs"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        ),
    )
    .await
    .expect("entry-bounded receipt must not delay spawn")
    .expect("process starts");
    tokio::time::timeout(Duration::from_secs(5), execution.wait())
        .await
        .expect("entry-bounded receipt must not delay completion")
        .expect("process completes");

    let mutation = workspace_mutations(&journal)
        .into_iter()
        .next()
        .flatten()
        .expect("bounded unknown assumes mutation");
    assert!(mutation.mutation_digest.contains("reason=entry_limit"));
    assert!(mutation.mutation_digest.contains("before_entries=4097"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("source.rs")).expect("read mutation"),
        "changed"
    );
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn gitignored_only_change_is_deliberately_not_a_workspace_mutation() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join(".gitignore"), "ignored.log\n").expect("ignore control");
    fs::write(workspace.path().join("source.rs"), "stable").expect("tracked source");
    initialize_git_workspace(workspace.path());
    let (mut broker, journal) = broker(workspace.path());

    broker
        .process_exec(
            &ProcessExec::new("ignored-only", "printf generated > ignored.log"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("process starts")
        .wait()
        .await
        .expect("process completes");

    assert_eq!(workspace_mutations(&journal), vec![None]);
    assert_eq!(
        fs::read_to_string(workspace.path().join("ignored.log")).expect("ignored output"),
        "generated"
    );
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn git_index_visibility_flags_cannot_hide_real_source_mutations() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("assumed.rs"), "before assumed").expect("assumed source");
    fs::write(workspace.path().join("skipped.rs"), "before skipped").expect("skipped source");
    initialize_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["update-index", "--assume-unchanged", "assumed.rs"],
    );
    run_git(
        workspace.path(),
        &["update-index", "--skip-worktree", "skipped.rs"],
    );
    let (mut broker, journal) = broker(workspace.path());

    broker
        .process_exec(
            &ProcessExec::new(
                "git-hidden-source",
                "printf 'after assumed' > assumed.rs; printf 'after skipped' > skipped.rs",
            ),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("process starts")
        .wait()
        .await
        .expect("process completes");

    assert!(
        workspace_mutations(&journal)
            .into_iter()
            .next()
            .flatten()
            .is_some(),
        "assume-unchanged and skip-worktree paths are content-hashed"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("assumed.rs")).expect("assumed source"),
        "after assumed"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("skipped.rs")).expect("skipped source"),
        "after skipped"
    );
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn a_commit_that_leaves_git_status_clean_is_detected_by_content_receipt() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("source.rs"), "before").expect("tracked source");
    initialize_git_workspace(workspace.path());
    let (mut broker, journal) = broker(workspace.path());
    let operation = ProcessExec::new(
        "commit-source",
        "printf after > source.rs; git add source.rs; \
         git -c user.name='Haider Tests' -c user.email=haider-tests@example.invalid \
         -c commit.gpgsign=false commit -qm command-mutation",
    )
    .with_env_allowlist(vec!["PATH".into()]);

    broker
        .process_exec(
            &operation,
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("process starts")
        .wait()
        .await
        .expect("process completes");

    assert!(
        workspace_mutations(&journal)
            .into_iter()
            .next()
            .flatten()
            .is_some(),
        "anchored content coverage must detect a committed source mutation"
    );
    let clean = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace.path())
        .output()
        .expect("git status");
    assert!(clean.status.success());
    assert!(clean.stdout.is_empty(), "fixture must end clean");
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn prior_after_receipt_is_the_next_sequential_before_receipt() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("source.rs"), "stable").expect("tracked source");
    initialize_git_workspace(workspace.path());
    let (mut broker, journal) = broker(workspace.path());

    broker
        .process_exec(
            &ProcessExec::new("create-transient", "printf transient > transient.txt"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("create starts")
        .wait()
        .await
        .expect("create completes");
    broker
        .process_exec(
            &ProcessExec::new("remove-transient", "rm transient.txt"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("remove starts")
        .wait()
        .await
        .expect("remove completes");

    let mutations = workspace_mutations(&journal);
    assert_eq!(mutations.len(), 2);
    assert!(mutations[0].is_some(), "creation must register");
    assert!(
        mutations[1].is_some(),
        "removal must compare with the prior after-receipt, not the turn-start receipt"
    );
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn overlapping_process_receipts_are_conservatively_unknown_for_both_commands() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("source.rs"), "stable").expect("tracked source");
    initialize_git_workspace(workspace.path());
    let (mut broker, journal) = broker(workspace.path());

    let first = broker
        .process_exec(
            &ProcessExec::new("overlap-first", "sleep 0.2"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("first starts");
    let second = broker
        .process_exec(
            &ProcessExec::new("overlap-second", "printf stable >/dev/null"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect("second starts");
    second.wait().await.expect("second completes");
    first.wait().await.expect("first completes");

    let mutations = workspace_mutations(&journal);
    assert_eq!(mutations.len(), 2);
    assert!(mutations.iter().all(Option::is_some));
    assert!(mutations.iter().flatten().all(|mutation| {
        mutation
            .mutation_digest
            .contains("reason=concurrent_or_interleaved_mutation")
    }));
    broker.close().await.expect("broker closes");
}

#[tokio::test(start_paused = true)]
async fn output_flood_spills_while_streaming_and_completes_under_paused_time() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, _journal) = broker(workspace.path());
    let cas = RecordingCas::default();
    let cas_observer = cas.observer();
    let result = broker
        .process_exec(
            &ProcessExec::new("flood", "/usr/bin/head -c 1048576 /dev/zero"),
            &process_policy(),
            cas,
            RecordingOutput::default(),
            ProcessBounds {
                max_inline_bytes: 1024,
                max_output_bytes: 2 * 1024 * 1024,
                kill_grace: Duration::from_secs(1),
                ..ProcessBounds::default()
            },
        )
        .await
        .expect("flood starts")
        .wait()
        .await
        .expect("flood completes");
    assert_eq!(result.status, ToolStatus::Completed);
    assert_eq!(result.output_bytes, 1_048_576);
    assert_eq!(
        result.transcript_digest,
        format!("blake3:{}", blake3::hash(&vec![0_u8; 1_048_576]).to_hex())
    );
    assert!(!result.inline_output.is_empty());
    assert!(result.artifact.is_some());
    assert!(
        result.transcript_high_water_bytes
            <= 1024 + PROCESS_ADAPTER_INPUT_BYTES + PROCESS_OUTPUT_CHUNK_BYTES,
        "transcript payload high-water {} exceeded cap + one read chunk",
        result.transcript_high_water_bytes
    );
    {
        let artifacts = cas_observer.lock().expect("CAS observer");
        assert_eq!(artifacts.len(), 1);
        assert!(
            artifacts[0].len() > 1024,
            "spill file must grow beyond the in-memory cap"
        );
        let transcript: Vec<ProcessOutputChunk> =
            serde_json::from_slice(&artifacts[0]).expect("streamed transcript");
        assert_eq!(
            transcript
                .iter()
                .map(|chunk| BASE64.decode(&chunk.chunk_b64).expect("base64").len())
                .sum::<usize>(),
            1_048_576
        );
    }
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn hard_output_cap_terminates_the_process_group_and_reports_the_ledgered_limit() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let output = RecordingOutput::default();
    let output_observer = output.observer();
    let result = broker
        .process_exec(
            &ProcessExec::new("output-cap", "/usr/bin/head -c 1048576 /dev/zero"),
            &process_policy(),
            RecordingCas::default(),
            output,
            ProcessBounds {
                max_inline_bytes: 2048,
                max_output_bytes: 4096,
                wall_timeout: Duration::from_secs(10),
                kill_grace: Duration::from_millis(10),
            },
        )
        .await
        .expect("process starts")
        .wait()
        .await
        .expect("bounded process result");
    assert_eq!(result.status, ToolStatus::Failed);
    assert_eq!(
        result.limit_reached,
        Some(haider_tools::ProcessLimit::OutputCap)
    );
    assert_eq!(result.output_bytes, 4096);
    assert_eq!(
        result.transcript_digest,
        format!("blake3:{}", blake3::hash(&vec![0_u8; 4096]).to_hex())
    );
    assert_eq!(result.max_output_bytes, 4096);
    assert_eq!(
        output_bytes(&output_observer.lock().expect("output observer")).len(),
        4096,
        "streaming must stop exactly at the hard cap"
    );
    assert!(phases(&journal).iter().any(|phase| matches!(
        phase,
        EffectPhase::Intent(intent)
            if intent.summary.contains("timeout=10000ms")
                && intent.summary.contains("output_cap=4096 bytes")
    )));
    assert!(phases(&journal).iter().any(|phase| matches!(
        phase,
        EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { error },
            ..
        } if error.contains("OutputCap")
    )));
    broker.close().await.expect("broker closes");
}

#[tokio::test(start_paused = true)]
async fn wall_timeout_terminates_the_process_group_and_reports_the_ledgered_limit() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let wall_timeout = Duration::from_secs(3);
    let kill_grace = Duration::from_secs(1);
    let execution = broker
        .process_exec(
            &ProcessExec::new("wall-timeout", "trap '' TERM; while :; do :; done"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds {
                max_inline_bytes: 1024,
                max_output_bytes: 4096,
                wall_timeout,
                kill_grace,
            },
        )
        .await
        .expect("process starts");
    settle().await;
    tokio::time::advance(wall_timeout).await;
    settle().await;
    tokio::time::advance(kill_grace).await;
    let result = execution.wait().await.expect("bounded process result");
    assert_eq!(result.status, ToolStatus::Failed);
    assert_eq!(
        result.limit_reached,
        Some(haider_tools::ProcessLimit::WallTimeout)
    );
    assert_eq!(
        result.wall_timeout_ms,
        u64::try_from(wall_timeout.as_millis()).expect("test duration fits")
    );
    assert!(phases(&journal).iter().any(|phase| matches!(
        phase,
        EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { error },
            ..
        } if error.contains("WallTimeout")
    )));
    broker.close().await.expect("broker closes");
}

async fn cancellation_during_gated_cas_is_sticky(fail: bool) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let execution = broker
        .process_exec(
            &ProcessExec::new("cancel-cas", "/usr/bin/head -c 16384 /dev/zero"),
            &process_policy(),
            GatedCas {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                fail,
            },
            RecordingOutput::default(),
            ProcessBounds {
                max_inline_bytes: 1024,
                kill_grace: Duration::from_millis(1),
                ..ProcessBounds::default()
            },
        )
        .await
        .expect("process starts");
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("CAS ingestion starts");
    execution.cancel();
    release.notify_one();

    let result = execution
        .wait()
        .await
        .expect("cancellation masks either CAS completion arm");
    assert_eq!(result.status, ToolStatus::Cancelled);
    assert_eq!(result.artifact.is_some(), !fail);
    assert!(phases(&journal).iter().any(|phase| matches!(
        phase,
        EffectPhase::Outcome {
            outcome: EffectOutcome::Cancelled | EffectOutcome::CancelledEscalated { .. },
            ..
        }
    )));
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn cancellation_during_successful_cas_ingestion_is_sticky() {
    cancellation_during_gated_cas_is_sticky(false).await;
}

#[tokio::test]
async fn cancellation_during_failed_cas_ingestion_is_sticky() {
    cancellation_during_gated_cas_is_sticky(true).await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_output_sink_and_cas_failures() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, journal) = broker(workspace.path());
    let output = FailingOutput::default();
    let attempted = Arc::clone(&output.attempted);
    let grace = Duration::from_secs(2);
    let execution = broker
        .process_exec(
            &ProcessExec::new(
                "cancel-errors",
                "trap '' TERM; /usr/bin/head -c 16384 /dev/zero; while :; do :; done",
            ),
            &process_policy(),
            FailingCas,
            output,
            ProcessBounds {
                max_inline_bytes: 1024,
                kill_grace: grace,
                ..ProcessBounds::default()
            },
        )
        .await
        .expect("process starts");
    attempted.notified().await;
    execution.cancel();
    tokio::time::advance(grace).await;
    let result = execution
        .wait()
        .await
        .expect("cancellation context masks supervisor errors");
    assert_eq!(result.status, ToolStatus::Cancelled);
    assert!(
        result
            .escalation_note
            .as_deref()
            .is_some_and(|note| note.contains("supervisor error after cancellation"))
    );
    assert!(phases(&journal).iter().any(|phase| matches!(
        phase,
        EffectPhase::Outcome {
            outcome: EffectOutcome::CancelledEscalated { note },
            ..
        } if note.contains("injected")
    )));
    broker
        .close()
        .await
        .expect("non-leaked cancellation closes");
}

#[tokio::test]
async fn shell_exit_sweeps_background_members_of_its_process_group() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut broker, _journal) = broker(workspace.path());
    let result = broker
        .process_exec(
            &ProcessExec::new(
                "background",
                "/usr/bin/perl -e '$pid = fork; exit 0 if $pid; sleep 30'",
            ),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds {
                max_inline_bytes: 1024,
                kill_grace: Duration::from_secs(1),
                ..ProcessBounds::default()
            },
        )
        .await
        .expect("process starts")
        .wait()
        .await
        .expect("group sweep completes");
    assert_eq!(result.status, ToolStatus::Completed);
    let event_index = |expected| {
        result
            .lifecycle_events
            .iter()
            .position(|event| event == &expected)
            .unwrap_or_else(|| panic!("missing lifecycle event {expected:?}"))
    };
    assert!(
        event_index(ProcessLifecycleEvent::LeaderExitObserved)
            < event_index(ProcessLifecycleEvent::GroupSweepStarted)
    );
    assert!(
        event_index(ProcessLifecycleEvent::GroupSweepCompleted)
            < event_index(ProcessLifecycleEvent::LeaderReaped)
    );
    assert!(
        event_index(ProcessLifecycleEvent::LeaderReaped)
            < event_index(ProcessLifecycleEvent::RegistryRemoved)
    );
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
async fn cwd_identity_change_after_authorization_is_refused_before_spawn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let target = workspace.path().join("target");
    let anchored = workspace.path().join("authorized-original");
    std::fs::create_dir(&target).expect("target cwd");
    let journal = SwapCwdJournal {
        target: target.clone(),
        anchored: anchored.clone(),
        replacement: outside.path().to_path_buf(),
        swapped: false,
    };
    let mut broker = EffectBroker::new_at(
        Box::new(journal),
        workspace.path(),
        SessionId::new("cwd-race"),
        1,
        1_700_000_000_000,
    )
    .expect("broker");
    let error = broker
        .process_exec(
            &ProcessExec::new("cwd-race", "printf spawned > marker").with_cwd("target"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect_err("changed cwd identity is rejected");
    assert!(matches!(error, ToolError::PathChanged { .. }));
    assert!(!anchored.join("marker").exists());
    assert!(!outside.path().join("marker").exists());
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn same_inode_cwd_moved_outside_and_symlinked_back_is_refused_before_spawn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let target = workspace.path().join("target");
    let moved = outside.path().join("moved-authorized-cwd");
    std::fs::create_dir(&target).expect("target cwd");
    let journal = SwapCwdJournal {
        target: target.clone(),
        anchored: moved.clone(),
        replacement: moved.clone(),
        swapped: false,
    };
    let mut broker = EffectBroker::new_at(
        Box::new(journal),
        workspace.path(),
        SessionId::new("cwd-same-inode-race"),
        1,
        1_700_000_000_000,
    )
    .expect("broker");
    let error = broker
        .process_exec(
            &ProcessExec::new("cwd-same-inode-race", "printf spawned > marker").with_cwd("target"),
            &process_policy(),
            RecordingCas::default(),
            RecordingOutput::default(),
            ProcessBounds::default(),
        )
        .await
        .expect_err("same-inode cwd relocation is rejected");
    assert!(matches!(error, ToolError::PathChanged { .. }));
    assert!(!moved.join("marker").exists());
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
                ..ProcessBounds::default()
            },
        )
        .await
        .expect("process starts");
    while output_observer.lock().expect("output observer").is_empty() {
        tokio::task::yield_now().await;
    }
    execution.cancel();
    // Let the supervisor actually OBSERVE the cancel and arm its grace
    // deadline before the clock moves. One yield only reaches it if it
    // happens to be the next ready task; if the clock advanced first, the
    // deadline would be armed against the advanced `now` and the assertion
    // below would pass vacuously instead of proving the grace window.
    settle().await;
    tokio::time::advance(grace - Duration::from_millis(1)).await;
    settle().await;
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
    assert!(
        is_cancelled_outcome(&phases[3]),
        "supervised termination is journaled as a cancellation, got {:?}",
        phases[3]
    );
    broker.close().await.expect("broker closes");
}

/// W4a2 cancellation hand-off mutation sentinel.
///
/// MUTATION CHECK: remove the `send_replace(true)` in
/// `ProcessExecution::drop`. The bounded heartbeat command keeps writing
/// during the observation window and this test fails. Verified by revert in
/// W4a2.
#[tokio::test]
async fn dropping_process_execution_cancels_and_kills_the_child_group() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let heartbeat = workspace.path().join("heartbeat.log");
    let (mut broker, _journal) = broker(workspace.path());
    let output = RecordingOutput::default();
    let output_observer = output.observer();
    let execution = broker
        .process_exec(
            &ProcessExec::new(
                "drop-cancel",
                "printf started; i=0; while [ \"$i\" -lt 100 ]; do printf x >> heartbeat.log; i=$((i+1)); sleep 0.01; done",
            ),
            &process_policy(),
            RecordingCas::default(),
            output,
            ProcessBounds {
                kill_grace: Duration::from_millis(10),
                ..ProcessBounds::default()
            },
        )
        .await
        .expect("process starts");
    while output_observer.lock().expect("output observer").is_empty() {
        tokio::task::yield_now().await;
    }
    drop(execution);
    tokio::time::sleep(Duration::from_millis(80)).await;
    // Under gate load the kill can land before the child ever CREATES the
    // heartbeat file — absence is the strongest form of "not running"
    // (NotFound reads as size 0; a live child would create and grow it).
    let heartbeat_len =
        |path: &std::path::Path| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let stopped_size = heartbeat_len(&heartbeat);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        heartbeat_len(&heartbeat),
        stopped_size,
        "dropped execution left the child group running"
    );
    broker.close().await.expect("broker closes");
}

/// Yield enough times for a spawned supervisor to be scheduled and act on a
/// state change. `yield_now` hands the current-thread runtime ONE chance to
/// run other ready tasks; a supervisor that must wake on a watch, issue a
/// signal and arm a timer can legitimately need more than one, and under
/// load it competes with the output-reader tasks. Bounded, so a genuinely
/// stuck supervisor still fails the test rather than hanging.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// The legitimate terminal outcomes of a SUPERVISED cancellation.
///
/// These tests cancel a child that installs `trap '' TERM`, so it can only
/// die to SIGKILL after the grace window. That escalation path runs real
/// syscalls (`killpg` probe, `killpg(SIGKILL)`, reap) against a process
/// that is concurrently being torn down by the kernel. SOME mid-teardown
/// observations record a durable escalation note, which turns the outcome
/// into `CancelledEscalated` (process.rs:689-694); others are normalized
/// to plain completion with NO note — sweep-time ESRCH and the
/// zombie-leader EPERM case are treated as "sweep already complete"
/// (`signal_group_for_sweep`, process.rs:1284-1291). So the race decides
/// WHICH path runs, and thereby whether the outcome is `Cancelled` or
/// `CancelledEscalated`. BOTH mean "cancellation won"; the ordering is the
/// nondeterminism the round-2 review caught.
///
/// The set deliberately EXCLUDES `Ok` and `Failed`: if cancellation stops
/// working the process runs to completion or dies some other way, and this
/// predicate — plus the exact `ToolStatus::Cancelled` assertion each test
/// keeps — fails.
fn is_cancelled_outcome(phase: &EffectPhase) -> bool {
    matches!(
        phase,
        EffectPhase::Outcome {
            outcome: EffectOutcome::Cancelled | EffectOutcome::CancelledEscalated { .. },
            ..
        }
    )
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
                ..ProcessBounds::default()
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
    assert!(
        phases.iter().any(|phase| match phase {
            EffectPhase::Outcome { effect, .. } =>
                effect == &control.original_effect && is_cancelled_outcome(phase),
            _ => false,
        }),
        "the brokered kill cancels the ORIGINAL effect, got {phases:?}"
    );
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

#[test]
fn env_view_redacts_secret_names_and_preserves_non_secret_values() {
    const CHILD: &str = "HAIDER_ENV_VIEW_TEST_CHILD";
    const SECRET: &str = "HAIDER_SHELL_TEST_API_TOKEN";
    const VISIBLE: &str = "HAIDER_SHELL_TEST_REGION";
    const POSTGRES_SECRET: &str = "PGPASSWORD";
    const MYSQL_SECRET: &str = "MYSQL_PWD";
    const LOWERCASE_SECRET: &str = "github_token";
    if std::env::var(CHILD).as_deref() != Ok("1") {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "env_view_redacts_secret_names_and_preserves_non_secret_values",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(SECRET, "do-not-display")
            .env(VISIBLE, "eu-test-1")
            .env(POSTGRES_SECRET, "postgres-secret")
            .env(MYSQL_SECRET, "mysql-secret")
            .env(LOWERCASE_SECRET, "lowercase-secret")
            .env("PATH", "/visible/test/bin")
            .env("HOME", "/visible/test/home")
            .status()
            .expect("spawn isolated env-view test");
        assert!(status.success());
        return;
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    let mut shell = ShellSession::new(
        workspace.path(),
        vec![
            SECRET.into(),
            VISIBLE.into(),
            POSTGRES_SECRET.into(),
            MYSQL_SECRET.into(),
            LOWERCASE_SECRET.into(),
            "PATH".into(),
            "HOME".into(),
        ],
    )
    .expect("shell");
    let ComposerSubmission::Builtin(BuiltinResult::Environment { entries }) =
        shell.submit("!env-view").expect("env-view")
    else {
        panic!("env-view is a builtin");
    };
    let secret = entries
        .iter()
        .find(|entry| entry.name == SECRET)
        .expect("secret entry");
    let visible = entries
        .iter()
        .find(|entry| entry.name == VISIBLE)
        .expect("visible entry");
    assert_eq!(secret.value.as_deref(), Some(REDACTED_ENV_VALUE));
    for name in [POSTGRES_SECRET, MYSQL_SECRET, LOWERCASE_SECRET] {
        let value = entries
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.value.as_deref());
        assert_eq!(value, Some(REDACTED_ENV_VALUE), "{name} must be redacted");
    }
    assert_eq!(visible.value.as_deref(), Some("eu-test-1"));
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.name == "PATH")
            .and_then(|entry| entry.value.as_deref()),
        Some("/visible/test/bin")
    );
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.name == "HOME")
            .and_then(|entry| entry.value.as_deref()),
        Some("/visible/test/home")
    );
}
