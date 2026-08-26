#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use haider_protocol::loom::LoomAgentType;
use haider_protocol::typed_agent::TypedAgentRequiredCli;
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
struct FakeInstaller {
    fail: HashSet<String>,
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl RequiredCliInstaller for FakeInstaller {
    async fn install(&self, required: &TypedAgentRequiredCli) -> Result<(), String> {
        self.calls
            .lock()
            .expect("fake installer calls")
            .push(required.program.clone());
        if self.fail.contains(&required.program) {
            Err(format!("{} install failed", required.program))
        } else {
            Ok(())
        }
    }
}

fn agent(id: &str, job: &str, cli: &str) -> LoomAgentType {
    agent_with_clis(id, job, &[cli])
}

fn agent_with_clis(id: &str, job: &str, clis: &[&str]) -> LoomAgentType {
    LoomAgentType {
        id: id.into(),
        name: id.into(),
        job: job.into(),
        in_type: "Input".into(),
        out_type: "Output".into(),
        clis: clis.iter().map(|cli| (*cli).to_owned()).collect(),
        apis: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: String::new(),
        glyph: String::new(),
        rev: 99,
    }
}

#[tokio::test]
async fn durable_runtime_resumes_installing_jobs_and_persists_terminal_progress() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let registration = store
        .loom_register_agent_type_with_install(agent("resume", "first", "rg"))
        .await
        .expect("register resumed type");
    let job = registration.install_job.expect("install job");
    let items = store
        .typed_agent_install_items(Some(job.job_id.clone()), None)
        .await
        .expect("items");
    store
        .typed_agent_install_compare_and_swap(
            start_next_item(&job, &items).expect("start durable item"),
        )
        .await
        .expect("persist installing state");
    drop(store);

    let restarted = SqliteStoreHandle::open(profile.path())
        .await
        .expect("restart store");
    let installer = FakeInstaller::default();
    resume_pending_installs_with(restarted.clone(), &installer).await;
    let snapshot = restarted
        .typed_agent_install_status(Some(job.job_id.clone()), None)
        .await
        .expect("status after resume");
    assert_eq!(snapshot.jobs[0].state, TypedAgentInstallState::Succeeded);
    assert_eq!(snapshot.jobs[0].progress.completed, 1);
    assert_eq!(snapshot.items[0].state, TypedAgentInstallState::Succeeded);
    assert_eq!(installer.calls.lock().expect("calls").as_slice(), ["rg"]);
}

#[tokio::test]
async fn startup_retires_stale_revision_and_runs_only_current_contract() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let stale = store
        .loom_register_agent_type_with_install(agent("changing", "old job", "rg"))
        .await
        .expect("old revision")
        .install_job
        .expect("old job");
    let current = store
        .loom_register_agent_type_with_install(agent("changing", "new job", "jq"))
        .await
        .expect("new revision")
        .install_job
        .expect("new job");

    let installer = FakeInstaller::default();
    resume_pending_installs_with(store.clone(), &installer).await;
    let stale_status = store
        .typed_agent_install_status(Some(stale.job_id), None)
        .await
        .expect("stale status");
    let current_status = store
        .typed_agent_install_status(Some(current.job_id), None)
        .await
        .expect("current status");
    assert_eq!(stale_status.jobs[0].state, TypedAgentInstallState::Failed);
    assert!(
        stale_status.jobs[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("superseded"))
    );
    assert_eq!(
        current_status.jobs[0].state,
        TypedAgentInstallState::Succeeded
    );
    assert_eq!(installer.calls.lock().expect("calls").as_slice(), ["jq"]);
}

#[tokio::test]
async fn installer_failure_is_durable_and_reconnectable() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let job = store
        .loom_register_agent_type_with_install(agent("failure", "fail", "ffmpeg"))
        .await
        .expect("register failure type")
        .install_job
        .expect("failure job");
    let installer = FakeInstaller {
        fail: HashSet::from(["ffmpeg".into()]),
        calls: Mutex::new(Vec::new()),
    };
    run_install_job_with(store.clone(), job.job_id.clone(), &installer)
        .await
        .expect("failure is terminal, not orchestration error");
    let snapshot = store
        .typed_agent_install_status(Some(job.job_id), None)
        .await
        .expect("failed status");
    assert_eq!(snapshot.jobs[0].state, TypedAgentInstallState::Failed);
    assert_eq!(snapshot.items[0].state, TypedAgentInstallState::Failed);
    assert_eq!(snapshot.jobs[0].error, snapshot.items[0].error);
}

/// MUTATION CHECK: make retry return the existing failed row without
/// resetting every item/progress field, or stop recording CAS snapshots.
/// Expected runtime failure: the reset assertions fail, the second
/// installer cannot succeed, or replay lacks failure/requeue/success.
#[tokio::test]
async fn failed_install_retry_resets_reruns_and_replays_progress() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let registration = store
        .loom_register_agent_type_with_install(agent_with_clis(
            "retryable",
            "retry",
            &["rg", "ffmpeg", "jq"],
        ))
        .await
        .expect("register retryable type");
    let job = registration.install_job.expect("initial install job");
    assert_eq!(
        registration.install_job_id.as_deref(),
        Some(job.job_id.as_str())
    );
    let identical = store
        .loom_register_agent_type_with_install(agent_with_clis(
            "retryable",
            "retry",
            &["rg", "ffmpeg", "jq"],
        ))
        .await
        .expect("idempotent registration");
    assert!(identical.install_job.is_none());
    assert_eq!(
        identical.install_job_id.as_deref(),
        Some(job.job_id.as_str())
    );

    let failing = FakeInstaller {
        fail: HashSet::from(["ffmpeg".into()]),
        calls: Mutex::new(Vec::new()),
    };
    run_install_job_with(store.clone(), job.job_id.clone(), &failing)
        .await
        .expect("first runner records failure");
    assert_eq!(
        failing.calls.lock().expect("failing calls").as_slice(),
        ["rg", "ffmpeg"]
    );

    let requeued = store
        .typed_agent_install_retry(job.job_id.clone())
        .await
        .expect("retry transaction");
    assert!(matches!(
        requeued,
        haider_core::TypedAgentInstallRetryResult::Requeued(ref retry)
            if retry.state == TypedAgentInstallState::Queued
    ));
    let reset = store
        .typed_agent_install_status(Some(job.job_id.clone()), None)
        .await
        .expect("status after retry reset");
    assert_eq!(reset.jobs[0].progress.completed, 0);
    assert!(reset.jobs[0].progress.current_cli.is_none());
    assert!(reset.jobs[0].error.is_none());
    assert!(
        reset
            .items
            .iter()
            .all(|item| item.state == TypedAgentInstallState::Queued && item.error.is_none())
    );
    let succeeding = FakeInstaller::default();
    run_install_job_with(store.clone(), job.job_id.clone(), &succeeding)
        .await
        .expect("retry runner succeeds");
    assert_eq!(
        succeeding
            .calls
            .lock()
            .expect("succeeding calls")
            .as_slice(),
        ["rg", "ffmpeg", "jq"]
    );

    let watch = store
        .typed_agent_install_watch(job.job_id.clone(), 0)
        .await
        .expect("watch progress");
    let haider_core::TypedAgentInstallWatchResult::Watching(page) = watch else {
        panic!("known install job must produce a watch page");
    };
    assert_eq!(page.requested_after_cursor, 0);
    assert_eq!(page.next_cursor, page.replay_through_cursor);
    assert!(
        page.events
            .windows(2)
            .all(|pair| pair[0].cursor < pair[1].cursor)
    );
    let states = page
        .events
        .iter()
        .map(|event| event.job.state)
        .collect::<Vec<_>>();
    assert_eq!(states.first(), Some(&TypedAgentInstallState::Queued));
    assert!(states.contains(&TypedAgentInstallState::Installing));
    assert!(states.contains(&TypedAgentInstallState::Failed));
    assert!(
        states
            .iter()
            .filter(|state| **state == TypedAgentInstallState::Queued)
            .count()
            >= 2
    );
    assert_eq!(states.last(), Some(&TypedAgentInstallState::Succeeded));

    let not_retryable = store
        .typed_agent_install_retry(job.job_id.clone())
        .await
        .expect("terminal success rejection");
    assert!(matches!(
        not_retryable,
        haider_core::TypedAgentInstallRetryResult::StateNotRetryable {
            state: TypedAgentInstallState::Succeeded
        }
    ));
    assert!(matches!(
        store
            .typed_agent_install_retry("missing-install-job".into())
            .await
            .expect("unknown retry rejection"),
        haider_core::TypedAgentInstallRetryResult::JobNotFound
    ));
    let stale = store
        .loom_register_agent_type_with_install(agent("stale-retry", "old", "rg"))
        .await
        .expect("register stale retry type")
        .install_job
        .expect("stale retry job");
    run_install_job_with(
        store.clone(),
        stale.job_id.clone(),
        &FakeInstaller {
            fail: HashSet::from(["rg".into()]),
            calls: Mutex::new(Vec::new()),
        },
    )
    .await
    .expect("stale job failure");
    store
        .loom_register_agent_type_with_install(agent("stale-retry", "new", "jq"))
        .await
        .expect("supersede failed job");
    assert!(matches!(
        store
            .typed_agent_install_retry(stale.job_id)
            .await
            .expect("superseded retry rejection"),
        haider_core::TypedAgentInstallRetryResult::ContractNotCurrent
    ));
    assert!(matches!(
        store
            .typed_agent_install_watch(job.job_id, page.replay_through_cursor.saturating_add(1),)
            .await
            .expect("cursor-ahead rejection"),
        haider_core::TypedAgentInstallWatchResult::CursorAhead { .. }
    ));
}
