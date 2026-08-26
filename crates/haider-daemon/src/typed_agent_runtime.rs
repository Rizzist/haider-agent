//! Durable typed-agent CLI installation orchestration.
//!
//! The installer process is daemon-owned, while every lifecycle transition is
//! persisted through a compare-and-swap. A caller may disconnect at any point;
//! startup resumes every non-terminal job from the durable job/item rows.

use crate::typed_agent_installer::TypedAgentCliInstaller;
use async_trait::async_trait;
use haider_core::{SqliteStoreHandle, TypedAgentInstallCas, TypedAgentInstallItemCas};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::typed_agent::{
    TypedAgentInstallItem, TypedAgentInstallJob, TypedAgentInstallState,
};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[async_trait]
trait RequiredCliInstaller: Send + Sync {
    async fn install(
        &self,
        required: &haider_protocol::typed_agent::TypedAgentRequiredCli,
    ) -> Result<(), String>;
}

#[async_trait]
impl RequiredCliInstaller for TypedAgentCliInstaller {
    async fn install(
        &self,
        required: &haider_protocol::typed_agent::TypedAgentRequiredCli,
    ) -> Result<(), String> {
        TypedAgentCliInstaller::install(self, required)
            .await
            .map(|_| ())
            .map_err(|error| error.message)
    }
}

/// Resume all jobs which were non-terminal when the daemon last stopped.
pub(crate) async fn resume_pending_installs(store: SqliteStoreHandle) {
    resume_pending_installs_with(store, &TypedAgentCliInstaller::new()).await;
}

async fn resume_pending_installs_with(
    store: SqliteStoreHandle,
    installer: &impl RequiredCliInstaller,
) {
    // Migration 20 intentionally keeps registry rows intact. Re-register each
    // current record idempotently so pre-feature types receive their one
    // missing install job without a revision bump.
    let types = match store.loom_agent_types().await {
        Ok(types) => types,
        Err(error) => {
            tracing::warn!(%error, "cannot load typed-agent registry for install backfill");
            return;
        }
    };
    let current_types = types
        .iter()
        .map(|record| (record.id.clone(), (record.rev, record.digest())))
        .collect::<HashMap<_, _>>();
    for record in types {
        if let Err(error) = store.loom_register_agent_type_with_install(record).await {
            tracing::warn!(%error, "cannot backfill typed-agent required-CLI install job");
        }
    }
    let jobs = match store.typed_agent_install_jobs(None, None).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::warn!(%error, "cannot load durable typed-agent install jobs");
            return;
        }
    };
    for job in jobs.into_iter().filter(|job| !job.state.is_terminal()) {
        let current = current_types.get(&job.agent_type_id);
        if current.is_none_or(|(rev, digest)| {
            *rev != job.agent_type_rev || digest != &job.agent_type_digest
        }) {
            if let Err(error) = fail_superseded_job(&store, &job).await {
                tracing::warn!(job_id = %job.job_id, %error, "cannot retire stale typed-agent install job");
            }
            continue;
        }
        if let Err(error) = run_install_job_with(store.clone(), job.job_id.clone(), installer).await
        {
            tracing::warn!(job_id = %job.job_id, %error, "typed-agent install resume stopped");
        }
    }
}

/// Drive one durable job until it reaches success/failure or loses its CAS
/// race to another daemon-owned runner. Revision conflicts are harmless: the
/// winner owns the same immutable job and this runner exits.
pub(crate) async fn run_install_job(
    store: SqliteStoreHandle,
    job_id: String,
) -> Result<(), HaiderError> {
    let installer = TypedAgentCliInstaller::new();
    run_install_job_with(store, job_id, &installer).await
}

async fn run_install_job_with(
    store: SqliteStoreHandle,
    job_id: String,
    installer: &impl RequiredCliInstaller,
) -> Result<(), HaiderError> {
    // At most four persisted transitions per required CLI plus the final job
    // transition. The bound protects against a future state-machine bug.
    for _ in 0..=129 {
        let Some(job) = store
            .typed_agent_install_jobs(Some(job_id.clone()), None)
            .await?
            .into_iter()
            .next()
        else {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("typed-agent install job `{job_id}` does not exist"),
                false,
            ));
        };
        if job.state.is_terminal() {
            return Ok(());
        }
        // This check runs while every production caller holds the hub's
        // installer-ownership mutex. A queued runner cannot install an old
        // contract after a newer registration wins the mutex first.
        let current = store.loom_agent_type(job.agent_type_id.clone()).await?;
        if current.as_ref().is_none_or(|record| {
            record.rev != job.agent_type_rev || record.digest() != job.agent_type_digest
        }) {
            fail_superseded_job(&store, &job).await?;
            return Ok(());
        }
        let mut items = store
            .typed_agent_install_items(Some(job_id.clone()), None)
            .await?;
        items.sort_by_key(|item| item.ordinal);

        let update = match job.state {
            TypedAgentInstallState::Queued => start_next_item(&job, &items)?,
            TypedAgentInstallState::Installing => {
                let current = current_item(&job, &items)?;
                match current.state {
                    TypedAgentInstallState::Queued => begin_item(&job, current),
                    TypedAgentInstallState::Installing => {
                        match installer.install(&current.required_cli).await {
                            Ok(_) => verify_item(&job, current),
                            Err(message) => fail_item(&job, current, message),
                        }
                    }
                    TypedAgentInstallState::Verifying => finish_item(&job, current, &items)?,
                    TypedAgentInstallState::Succeeded => {
                        return Err(corrupt_install(format!(
                            "job `{job_id}` points at an already completed CLI"
                        )));
                    }
                    TypedAgentInstallState::Failed => fail_job_from_item(&job, current),
                }
            }
            TypedAgentInstallState::Verifying => finish_job(&job, &items)?,
            TypedAgentInstallState::Succeeded | TypedAgentInstallState::Failed => return Ok(()),
        };
        if let Err(error) = store.typed_agent_install_compare_and_swap(update).await {
            if error.code == ErrorCode::RevisionConflict {
                return Ok(());
            }
            return Err(error);
        }
    }
    Err(corrupt_install(format!(
        "typed-agent install job `{job_id}` exceeded its transition bound"
    )))
}

async fn fail_superseded_job(
    store: &SqliteStoreHandle,
    job: &TypedAgentInstallJob,
) -> Result<(), HaiderError> {
    let mut next = job.clone();
    next.state = TypedAgentInstallState::Failed;
    next.error = Some("typed-agent contract was superseded before installation completed".into());
    stamp_job(&mut next, job.updated_at_ms);
    match store
        .typed_agent_install_compare_and_swap(TypedAgentInstallCas {
            expected_job: job.clone(),
            next_job: next,
            item: None,
        })
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if error.code == ErrorCode::RevisionConflict => Ok(()),
        Err(error) => Err(error),
    }
}

fn start_next_item(
    job: &TypedAgentInstallJob,
    items: &[TypedAgentInstallItem],
) -> Result<TypedAgentInstallCas, HaiderError> {
    let item = items
        .iter()
        .find(|item| item.state == TypedAgentInstallState::Queued)
        .ok_or_else(|| corrupt_install("queued install job has no queued CLI item"))?;
    let mut next_job = job.clone();
    next_job.state = TypedAgentInstallState::Installing;
    next_job.progress.current_cli = Some(item.required_cli.program.clone());
    stamp_job(&mut next_job, job.updated_at_ms);
    let mut next_item = item.clone();
    next_item.state = TypedAgentInstallState::Installing;
    stamp_item(&mut next_item, item.updated_at_ms);
    Ok(cas(job, next_job, Some((item, next_item))))
}

fn begin_item(job: &TypedAgentInstallJob, item: &TypedAgentInstallItem) -> TypedAgentInstallCas {
    let mut next_job = job.clone();
    stamp_job(&mut next_job, job.updated_at_ms);
    let mut next_item = item.clone();
    next_item.state = TypedAgentInstallState::Installing;
    stamp_item(&mut next_item, item.updated_at_ms);
    cas(job, next_job, Some((item, next_item)))
}

fn verify_item(job: &TypedAgentInstallJob, item: &TypedAgentInstallItem) -> TypedAgentInstallCas {
    let mut next_job = job.clone();
    stamp_job(&mut next_job, job.updated_at_ms);
    let mut next_item = item.clone();
    next_item.state = TypedAgentInstallState::Verifying;
    stamp_item(&mut next_item, item.updated_at_ms);
    cas(job, next_job, Some((item, next_item)))
}

fn finish_item(
    job: &TypedAgentInstallJob,
    item: &TypedAgentInstallItem,
    items: &[TypedAgentInstallItem],
) -> Result<TypedAgentInstallCas, HaiderError> {
    let mut next_item = item.clone();
    next_item.state = TypedAgentInstallState::Succeeded;
    stamp_item(&mut next_item, item.updated_at_ms);

    let completed = job
        .progress
        .completed
        .checked_add(1)
        .ok_or_else(|| corrupt_install("typed-agent install completion count overflowed"))?;
    let mut next_job = job.clone();
    next_job.progress.completed = completed;
    if completed == job.progress.total {
        next_job.state = TypedAgentInstallState::Verifying;
    } else {
        let next = items
            .iter()
            .find(|candidate| candidate.state == TypedAgentInstallState::Queued)
            .ok_or_else(|| corrupt_install("install job has no next queued CLI item"))?;
        next_job.progress.current_cli = Some(next.required_cli.program.clone());
    }
    stamp_job(&mut next_job, job.updated_at_ms);
    Ok(cas(job, next_job, Some((item, next_item))))
}

fn finish_job(
    job: &TypedAgentInstallJob,
    items: &[TypedAgentInstallItem],
) -> Result<TypedAgentInstallCas, HaiderError> {
    if items.len() != usize::from(job.progress.total)
        || items
            .iter()
            .any(|item| item.state != TypedAgentInstallState::Succeeded)
    {
        return Err(corrupt_install(
            "verifying install job does not have all CLI items succeeded",
        ));
    }
    let mut next_job = job.clone();
    next_job.state = TypedAgentInstallState::Succeeded;
    next_job.progress.current_cli = None;
    stamp_job(&mut next_job, job.updated_at_ms);
    Ok(cas(job, next_job, None))
}

fn fail_item(
    job: &TypedAgentInstallJob,
    item: &TypedAgentInstallItem,
    message: String,
) -> TypedAgentInstallCas {
    let mut next_job = job.clone();
    next_job.state = TypedAgentInstallState::Failed;
    next_job.error = Some(message.clone());
    stamp_job(&mut next_job, job.updated_at_ms);
    let mut next_item = item.clone();
    next_item.state = TypedAgentInstallState::Failed;
    next_item.error = Some(message);
    stamp_item(&mut next_item, item.updated_at_ms);
    cas(job, next_job, Some((item, next_item)))
}

fn fail_job_from_item(
    job: &TypedAgentInstallJob,
    item: &TypedAgentInstallItem,
) -> TypedAgentInstallCas {
    let message = item
        .error
        .clone()
        .unwrap_or_else(|| "required CLI installation failed".into());
    let mut next_job = job.clone();
    next_job.state = TypedAgentInstallState::Failed;
    next_job.error = Some(message);
    stamp_job(&mut next_job, job.updated_at_ms);
    cas(job, next_job, None)
}

fn current_item<'a>(
    job: &TypedAgentInstallJob,
    items: &'a [TypedAgentInstallItem],
) -> Result<&'a TypedAgentInstallItem, HaiderError> {
    let current = job
        .progress
        .current_cli
        .as_deref()
        .ok_or_else(|| corrupt_install("installing job has no current CLI"))?;
    items
        .iter()
        .find(|item| item.required_cli.program == current)
        .ok_or_else(|| corrupt_install(format!("current CLI `{current}` has no durable item")))
}

fn cas(
    job: &TypedAgentInstallJob,
    next_job: TypedAgentInstallJob,
    item: Option<(&TypedAgentInstallItem, TypedAgentInstallItem)>,
) -> TypedAgentInstallCas {
    TypedAgentInstallCas {
        expected_job: job.clone(),
        next_job,
        item: item.map(|(expected, next)| TypedAgentInstallItemCas {
            expected: expected.clone(),
            next,
        }),
    }
}

fn stamp_job(job: &mut TypedAgentInstallJob, floor: u64) {
    job.updated_at_ms = unix_ms().max(floor);
}

fn stamp_item(item: &mut TypedAgentInstallItem, floor: u64) {
    item.updated_at_ms = unix_ms().max(floor);
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn corrupt_install(message: impl Into<String>) -> HaiderError {
    HaiderError::new(ErrorCode::StoreCorrupt, message.into(), false)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
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
                .typed_agent_install_watch(
                    job.job_id,
                    page.replay_through_cursor.saturating_add(1),
                )
                .await
                .expect("cursor-ahead rejection"),
            haider_core::TypedAgentInstallWatchResult::CursorAhead { .. }
        ));
    }
}
