//! Broker-level workspace receipt tests.
//!
//! These drive a real `process_exec` and assert what the before/after receipt
//! pair concludes: no mutation, an exact non-wall bound, or a real fingerprint
//! change. Each of those is pre-empted when one receipt crosses the production
//! 500 ms budget on a loaded machine (a slow `git status` flips the strategy to
//! the walk fallback, or a wall-time `Unknown` assumes a mutation). They
//! therefore live in the crate's unit tests, where the `cfg(test)`-only
//! receipt-budget seam exists, and run with the bounded
//! [`TEST_RECEIPT_WALL_TIME`]. The production budget is pinned separately by
//! `production_receipt_limits_remain_pinned` and
//! `production_brokers_use_the_production_receipt_wall_time`.
#![allow(clippy::expect_used)]

use crate::workspace_receipt::TEST_RECEIPT_WALL_TIME;
use crate::{
    CasSink, EffectBroker, JournalSink, NoopCommandOutputSink, PermissionPolicy, ProcessBounds,
    ProcessExec, ToolError, ToolResult,
};
use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectPhase, WorkspaceMutation};
use haider_protocol::ids::{ArtifactRef, SessionId};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Default)]
struct SharedJournal {
    payloads: Arc<Mutex<Vec<EventPayload>>>,
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
struct HashingCas;

#[async_trait]
impl CasSink for HashingCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        Ok(ArtifactRef::new(format!("blake3:{}", blake3::hash(bytes))))
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        let bytes = std::fs::read(path)
            .map_err(|error| ToolError::cas(format!("read test CAS source: {error}")))?;
        self.put(&bytes).await
    }
}

fn production_broker(root: &Path) -> (EffectBroker, Arc<Mutex<Vec<EventPayload>>>) {
    let journal = SharedJournal::default();
    let observer = Arc::clone(&journal.payloads);
    let broker = EffectBroker::new_at(
        Box::new(journal),
        root,
        SessionId::new("process-receipt-session"),
        3,
        1_700_000_000_000,
    )
    .expect("create broker");
    (broker, observer)
}

fn broker_with_receipt_budget(
    root: &Path,
    wall_time: Duration,
) -> (EffectBroker, Arc<Mutex<Vec<EventPayload>>>) {
    let (mut broker, observer) = production_broker(root);
    broker.set_workspace_receipt_wall_time_for_tests(wall_time);
    (broker, observer)
}

fn broker(root: &Path) -> (EffectBroker, Arc<Mutex<Vec<EventPayload>>>) {
    broker_with_receipt_budget(root, TEST_RECEIPT_WALL_TIME)
}

fn process_policy() -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ProcessExec);
    policy
}

async fn run(broker: &mut EffectBroker, call_id: &str, command: &str) {
    run_exec(broker, &ProcessExec::new(call_id, command)).await;
}

async fn run_exec(broker: &mut EffectBroker, operation: &ProcessExec) {
    broker
        .process_exec(
            operation,
            &process_policy(),
            HashingCas,
            NoopCommandOutputSink,
            ProcessBounds::default(),
        )
        .await
        .expect("process starts")
        .wait()
        .await
        .expect("process completes");
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

fn workspace_mutations(observer: &Arc<Mutex<Vec<EventPayload>>>) -> Vec<Option<WorkspaceMutation>> {
    observer
        .lock()
        .expect("journal observer")
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(EffectPhase::Outcome {
                workspace_mutation, ..
            }) => Some(workspace_mutation.clone()),
            _ => None,
        })
        .collect()
}

/// A detected mutation must come from comparing two complete receipts, not
/// from an unknown receipt that merely assumes one. Without this, a test that
/// expects `Some` passes vacuously whenever a receipt hits a bound.
fn assert_compared_mutation(mutation: Option<&WorkspaceMutation>, context: &str) {
    let digest = &mutation
        .unwrap_or_else(|| panic!("{context}: mutation must be recorded"))
        .mutation_digest;
    assert!(
        digest.starts_with("blake3:") && !digest.contains("coverage=unknown"),
        "{context}: mutation must be a complete-receipt comparison: {digest}"
    );
}

#[tokio::test]
async fn workspace_mutation_fact_fires_only_when_process_changes_the_tree() {
    // LAW 1 mutation guard: deleting either comparison makes a pure command
    // advance revision state or lets a real process mutation disappear.
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("input.txt"), "stable").expect("seed input");
    initialize_git_workspace(workspace.path());
    let (mut broker, journal) = broker(workspace.path());

    run(&mut broker, "pure-read", "cat input.txt >/dev/null").await;
    run(&mut broker, "mutation", "printf changed > output.txt").await;

    let outcomes = workspace_mutations(&journal);
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes[0].is_none(), "pure read must not mutate");
    assert_compared_mutation(outcomes[1].as_ref(), "real process mutation");
    broker.close().await.expect("broker closes");
}

#[tokio::test]
async fn non_repository_source_mutation_is_conservatively_detected_without_a_walk() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("source.rs"), "before").expect("seed source");
    let (mut broker, journal) = broker(workspace.path());

    run(&mut broker, "non-repo-mutation", "printf after > source.rs").await;

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

    // The entry cap, not the wall budget, bounds this receipt. The liveness
    // bound below is ten times the production receipt budget.
    let execution = tokio::time::timeout(
        Duration::from_secs(5),
        broker.process_exec(
            &ProcessExec::new("bounded-tree", "printf changed > source.rs"),
            &process_policy(),
            HashingCas,
            NoopCommandOutputSink,
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

    run(
        &mut broker,
        "ignored-only",
        "printf generated > ignored.log",
    )
    .await;

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

    run(
        &mut broker,
        "git-hidden-source",
        "printf 'after assumed' > assumed.rs; printf 'after skipped' > skipped.rs",
    )
    .await;

    let mutations = workspace_mutations(&journal);
    assert_eq!(mutations.len(), 1);
    assert_compared_mutation(
        mutations[0].as_ref(),
        "assume-unchanged and skip-worktree paths are content-hashed",
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

    run_exec(&mut broker, &operation).await;

    let mutations = workspace_mutations(&journal);
    assert_eq!(mutations.len(), 1);
    assert_compared_mutation(
        mutations[0].as_ref(),
        "anchored content coverage must detect a committed source mutation",
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

    run(
        &mut broker,
        "create-transient",
        "printf transient > transient.txt",
    )
    .await;
    run(&mut broker, "remove-transient", "rm transient.txt").await;

    let mutations = workspace_mutations(&journal);
    assert_eq!(mutations.len(), 2);
    assert_compared_mutation(mutations[0].as_ref(), "creation must register");
    assert_compared_mutation(
        mutations[1].as_ref(),
        "removal must compare with the prior after-receipt, not the turn-start receipt",
    );
    broker.close().await.expect("broker closes");
}

/// Seam guard: the broker's receipt budget must reach both the before and the
/// after receipt. A zero budget makes every receipt a wall-time `Unknown`, so
/// a pure command is conservatively reported; if the seam were ignored, the
/// production budget would compare two complete receipts and report nothing.
#[tokio::test]
async fn receipt_budget_seam_reaches_the_broker_receipts() {
    let workspace = tempfile::tempdir().expect("tempdir");
    fs::write(workspace.path().join("input.txt"), "stable").expect("seed input");
    initialize_git_workspace(workspace.path());
    let (mut broker, journal) = broker_with_receipt_budget(workspace.path(), Duration::ZERO);
    assert_eq!(broker.workspace_receipt_wall_time(), Duration::ZERO);

    run(&mut broker, "pure-read", "cat input.txt >/dev/null").await;

    let mutation = workspace_mutations(&journal)
        .into_iter()
        .next()
        .flatten()
        .expect("an exhausted receipt budget conservatively assumes mutation");
    assert!(
        mutation.mutation_digest.contains("reason=wall_time_limit"),
        "{}",
        mutation.mutation_digest
    );
    assert!(mutation.mutation_digest.contains("assumed_mutation=true"));
    broker.close().await.expect("broker closes");
}

/// The seam is opt-in per broker: every production constructor keeps the
/// pinned 500 ms receipt budget.
#[test]
fn production_brokers_use_the_production_receipt_wall_time() {
    let production = Duration::from_millis(500);
    let workspace = tempfile::tempdir().expect("tempdir");
    let (broker, _journal) = production_broker(workspace.path());
    assert_eq!(broker.workspace_receipt_wall_time(), production);
    let pending = EffectBroker::new_pending(
        Box::new(SharedJournal::default()),
        workspace.path().join("pending-leaf"),
        SessionId::new("process-receipt-pending"),
        3,
    )
    .expect("pending broker");
    assert_eq!(pending.workspace_receipt_wall_time(), production);
    let canonical = EffectBroker::new_canonical(
        Box::new(SharedJournal::default()),
        fs::canonicalize(workspace.path()).expect("canonical root"),
        SessionId::new("process-receipt-canonical"),
        3,
    )
    .expect("canonical broker");
    assert_eq!(canonical.workspace_receipt_wall_time(), production);
}
