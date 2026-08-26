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
#[path = "typed_agent_runtime_tests.rs"]
mod typed_agent_runtime_tests;
