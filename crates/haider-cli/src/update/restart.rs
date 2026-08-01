//! Authenticated one-signal drain, retained restart, and exact health.

use super::UpdateError;
use super::transaction::{CommittedUpdate, TransactionPhase};
use haider_client::{
    ClientConfig, ConnectError, Connected, ConnectionState, ResolvedProfile, RpcClient, connect,
    required_live_features, signal_authenticated_peer, spawn_daemon_retained,
};
use haider_protocol::error::ErrorCode;
use haider_rpc::{LifecyclePhase, Welcome, WireFrame};
use haider_store::Store;
use std::collections::BTreeSet;
use std::process::Child;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};

const DRAIN_DEADLINE: Duration = Duration::from_secs(20);
const LOCK_DEADLINE: Duration = Duration::from_secs(20);
const HEALTH_DEADLINE: Duration = Duration::from_secs(30);
const POLL_BACKOFF: Duration = Duration::from_millis(50);

pub(crate) trait RestartHooks {
    /// Must synchronously observe both canonical paths before any other
    /// restart action is allowed.
    fn observe_committed_pair(&self, committed: &CommittedUpdate) -> Result<(), UpdateError>;

    /// Sends the one authenticated drain signal. Implementations must not
    /// retry; a failure retains the recovery assets.
    fn signal(&self, pid: u32) -> Result<(), UpdateError>;
}

struct SystemRestartHooks;

impl RestartHooks for SystemRestartHooks {
    fn observe_committed_pair(&self, committed: &CommittedUpdate) -> Result<(), UpdateError> {
        committed.verify_target_pair()
    }

    fn signal(&self, pid: u32) -> Result<(), UpdateError> {
        signal_authenticated_peer(pid)
            .map_err(|error| UpdateError::RestartTimeout(format!("SIGTERM failed: {error}")))
    }
}

pub(crate) struct Incumbent {
    client: RpcClient,
    events: mpsc::Receiver<WireFrame>,
    welcome: Welcome,
    pid: u32,
}

pub(crate) async fn detect_incumbent(
    profile: &ResolvedProfile,
) -> Result<Option<Incumbent>, UpdateError> {
    let mut config = ClientConfig {
        client_name: "haider-update".into(),
        client_instance_id: format!("update-{}", std::process::id()),
        ..ClientConfig::default()
    };
    config.ping_interval = Duration::from_secs(5);
    match connect(&profile.endpoint_path, config).await {
        Ok(connected) => incumbent_from_connected(profile, connected).map(Some),
        Err(ConnectError::NotFound(_) | ConnectError::Refused(_)) => Ok(None),
        Err(error) => Err(UpdateError::Io(format!(
            "cannot authenticate the current-profile daemon: {error}"
        ))),
    }
}

fn incumbent_from_connected(
    profile: &ResolvedProfile,
    connected: Connected,
) -> Result<Incumbent, UpdateError> {
    validate_ready_welcome(&connected.welcome, profile, &required_live_features())?;
    if connected.peer_credentials.uid != haider_client::effective_uid() {
        connected.client.close();
        return Err(UpdateError::Refused(
            "daemon socket peer is not owned by this user".into(),
        ));
    }
    let Some(pid) = connected.peer_credentials.pid.filter(|pid| *pid != 0) else {
        connected.client.close();
        return Err(UpdateError::Refused(
            "daemon socket did not expose an authenticated peer PID".into(),
        ));
    };
    let events = connected.client.take_events().ok_or_else(|| {
        UpdateError::Internal("update could not retain the daemon event stream".into())
    })?;
    Ok(Incumbent {
        client: connected.client,
        events,
        welcome: connected.welcome,
        pid,
    })
}

/// Completes restart coordination after a verified pair commit.
///
/// MUTATION SAFETY: restart's first runtime read proves both canonical paths
/// are the target pair. The incumbent receives exactly one authenticated
/// SIGTERM. A drain timeout retains marker/backups and sends no second signal.
/// Health failure stops/reaps the retained child, restores both backups, and
/// starts the old sibling before returning [`UpdateError::Health`].
pub(crate) async fn restart_committed(
    committed: &mut CommittedUpdate,
    incumbent: Option<Incumbent>,
    profile: &ResolvedProfile,
) -> Result<(), UpdateError> {
    restart_committed_with_hooks(
        committed,
        incumbent,
        profile,
        &SystemRestartHooks,
        DRAIN_DEADLINE,
    )
    .await
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn restart_committed_for_test<H: RestartHooks>(
    committed: &mut CommittedUpdate,
    incumbent: Option<Incumbent>,
    profile: &ResolvedProfile,
    hooks: &H,
    drain_deadline: Duration,
) -> Result<(), UpdateError> {
    restart_committed_with_hooks(committed, incumbent, profile, hooks, drain_deadline).await
}

async fn restart_committed_with_hooks<H: RestartHooks>(
    committed: &mut CommittedUpdate,
    incumbent: Option<Incumbent>,
    profile: &ResolvedProfile,
    hooks: &H,
    drain_deadline: Duration,
) -> Result<(), UpdateError> {
    // RESTART SPY LAW: this is deliberately the first operation.
    hooks.observe_committed_pair(committed)?;
    let Some(mut incumbent) = incumbent else {
        committed.finalize()?;
        return Ok(());
    };

    committed.set_phase(TransactionPhase::DrainSignaled)?;
    if !matches!(incumbent.client.state(), ConnectionState::Connected) {
        return Err(UpdateError::RestartTimeout(
            "authenticated daemon connection closed before SIGTERM; no signal was sent and recovery assets remain"
                .into(),
        ));
    }
    hooks.signal(incumbent.pid)?;
    wait_for_matching_drain(&mut incumbent, drain_deadline).await?;
    wait_for_profile_lock(profile, LOCK_DEADLINE).await?;
    committed.set_phase(TransactionPhase::LockReleased)?;

    let old_instance = incumbent.welcome.instance_id.clone();
    let old_generation = incumbent.welcome.daemon_generation;
    let target = committed.target_version().to_owned();
    let old_version = committed.old_version().to_owned();
    let daemon_path = committed.layout().haiderd.clone();
    let mut child = match spawn_daemon_retained(profile, daemon_path) {
        Ok(child) => child,
        Err(error) => {
            return rollback_and_restart_old(
                committed,
                profile,
                &old_version,
                format!("cannot start updated daemon: {error}"),
            )
            .await;
        }
    };
    if let Err(error) = committed.set_phase(TransactionPhase::ChildSpawned) {
        stop_and_reap(&mut child);
        wait_for_profile_lock(profile, LOCK_DEADLINE).await?;
        return rollback_and_restart_old(
            committed,
            profile,
            &old_version,
            format!("cannot persist child-spawned phase: {error}"),
        )
        .await;
    }

    match wait_for_exact_health(
        profile,
        &target,
        &old_instance,
        old_generation,
        &mut child,
        HEALTH_DEADLINE,
    )
    .await
    {
        Ok(client) => {
            client.close();
            drop(child);
            committed.finalize()
        }
        Err(health_error) => {
            stop_and_reap(&mut child);
            wait_for_profile_lock(profile, LOCK_DEADLINE).await?;
            rollback_and_restart_old(committed, profile, &old_version, health_error).await
        }
    }
}

async fn rollback_and_restart_old(
    committed: &mut CommittedUpdate,
    profile: &ResolvedProfile,
    old_version: &str,
    health_error: String,
) -> Result<(), UpdateError> {
    committed.rollback()?;
    let mut old_child = spawn_daemon_retained(profile, committed.layout().haiderd.clone())
        .map_err(|error| {
            UpdateError::Health(format!(
                "updated daemon failed health ({health_error}); old daemon restart failed: {error}"
            ))
        })?;
    let restarted =
        wait_for_version_health(profile, old_version, &mut old_child, HEALTH_DEADLINE).await;
    match restarted {
        Ok(client) => {
            client.close();
            drop(old_child);
            Err(UpdateError::Health(format!(
                "updated daemon failed health and the old pair was restored: {health_error}"
            )))
        }
        Err(old_error) => {
            stop_and_reap(&mut old_child);
            Err(UpdateError::Health(format!(
                "updated daemon failed health ({health_error}); old pair restored but old daemon health failed ({old_error})"
            )))
        }
    }
}

async fn wait_for_matching_drain(
    incumbent: &mut Incumbent,
    drain_deadline: Duration,
) -> Result<(), UpdateError> {
    let expected_instance = incumbent.welcome.instance_id.clone();
    let expected_generation = incumbent.welcome.daemon_generation;
    let notice = tokio::time::timeout(drain_deadline, async {
        while let Some(frame) = incumbent.events.recv().await {
            if let WireFrame::ServerDraining {
                instance_id,
                daemon_generation,
                ..
            } = frame
            {
                return Some((instance_id, daemon_generation));
            }
        }
        None
    })
    .await
    .map_err(|_| {
        UpdateError::RestartTimeout(
            "daemon drain notice timed out; no second signal was sent and recovery assets remain"
                .into(),
        )
    })?
    .ok_or_else(|| {
        UpdateError::RestartTimeout(
            "daemon disconnected without a drain notice; recovery assets remain".into(),
        )
    })?;
    if notice != (expected_instance, expected_generation) {
        return Err(UpdateError::RestartTimeout(
            "daemon drain notice did not match the authenticated Welcome; recovery assets remain"
                .into(),
        ));
    }
    tokio::time::timeout(drain_deadline, incumbent.client.disconnected())
        .await
        .map_err(|_| {
            UpdateError::RestartTimeout(
                "daemon disconnect timed out; no second signal was sent and recovery assets remain"
                    .into(),
            )
        })?;
    Ok(())
}

async fn wait_for_profile_lock(
    profile: &ResolvedProfile,
    timeout: Duration,
) -> Result<(), UpdateError> {
    let deadline = Instant::now() + timeout;
    loop {
        match Store::acquire_profile(&profile.store_dir) {
            Ok(lease) => {
                drop(lease);
                return Ok(());
            }
            Err(error) if error.code == ErrorCode::StoreLocked => {}
            Err(error) => {
                return Err(UpdateError::Io(format!(
                    "cannot prove profile lock release: {}",
                    error.message
                )));
            }
        }
        if Instant::now() + POLL_BACKOFF > deadline {
            return Err(UpdateError::RestartTimeout(
                "profile lock release timed out; no second signal was sent and recovery assets remain"
                    .into(),
            ));
        }
        sleep(POLL_BACKOFF).await;
    }
}

async fn wait_for_exact_health(
    profile: &ResolvedProfile,
    target: &str,
    old_instance: &str,
    old_generation: u64,
    child: &mut Child,
    timeout: Duration,
) -> Result<RpcClient, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("updated daemon exited before health: {status}"));
        }
        match connect(&profile.endpoint_path, ClientConfig::default()).await {
            Ok(connected) => {
                if connected.welcome.lifecycle_phase != LifecyclePhase::Ready {
                    connected.client.close();
                    if Instant::now() + POLL_BACKOFF > deadline {
                        return Err("updated daemon did not reach Ready before the deadline".into());
                    }
                    sleep(POLL_BACKOFF).await;
                    continue;
                }
                let result = validate_new_health(
                    &connected,
                    profile,
                    target,
                    old_instance,
                    old_generation,
                    child.id(),
                );
                match result {
                    Ok(()) => return Ok(connected.client),
                    Err(error) => {
                        connected.client.close();
                        return Err(error);
                    }
                }
            }
            Err(error) if error.is_spawnable() => {}
            Err(error) => return Err(format!("updated daemon handshake failed: {error}")),
        }
        if Instant::now() + POLL_BACKOFF > deadline {
            return Err("updated daemon did not become healthy before the deadline".into());
        }
        sleep(POLL_BACKOFF).await;
    }
}

async fn wait_for_version_health(
    profile: &ResolvedProfile,
    version: &str,
    child: &mut Child,
    timeout: Duration,
) -> Result<RpcClient, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("old daemon exited before health: {status}"));
        }
        match connect(&profile.endpoint_path, ClientConfig::default()).await {
            Ok(connected) => {
                if connected.welcome.lifecycle_phase != LifecyclePhase::Ready {
                    connected.client.close();
                    if Instant::now() + POLL_BACKOFF > deadline {
                        return Err("old daemon did not reach Ready before the deadline".into());
                    }
                    sleep(POLL_BACKOFF).await;
                    continue;
                }
                if let Err(error) =
                    validate_ready_welcome(&connected.welcome, profile, &required_live_features())
                {
                    connected.client.close();
                    return Err(error.to_string());
                }
                if connected.welcome.daemon_version != version
                    || connected.peer_credentials.pid != Some(child.id())
                {
                    connected.client.close();
                    return Err("old daemon Welcome version or peer PID did not match".into());
                }
                return Ok(connected.client);
            }
            Err(error) if error.is_spawnable() => {}
            Err(error) => return Err(format!("old daemon handshake failed: {error}")),
        }
        if Instant::now() + POLL_BACKOFF > deadline {
            return Err("old daemon did not become healthy before the deadline".into());
        }
        sleep(POLL_BACKOFF).await;
    }
}

fn validate_new_health(
    connected: &Connected,
    profile: &ResolvedProfile,
    target: &str,
    old_instance: &str,
    old_generation: u64,
    child_pid: u32,
) -> Result<(), String> {
    validate_ready_welcome(&connected.welcome, profile, &required_live_features())
        .map_err(|error| error.to_string())?;
    if connected.welcome.daemon_version != target {
        return Err(format!(
            "Welcome daemon_version `{}` did not exactly match target `{target}`",
            connected.welcome.daemon_version
        ));
    }
    if connected.welcome.instance_id == old_instance {
        return Err("updated daemon reused the old process instance identity".into());
    }
    if connected.welcome.daemon_generation <= old_generation {
        return Err("updated daemon generation did not increase".into());
    }
    if connected.peer_credentials.pid != Some(child_pid)
        || connected.peer_credentials.uid != haider_client::effective_uid()
    {
        return Err("updated daemon peer credentials did not match the retained child".into());
    }
    Ok(())
}

fn validate_ready_welcome(
    welcome: &Welcome,
    profile: &ResolvedProfile,
    required: &BTreeSet<String>,
) -> Result<(), UpdateError> {
    if welcome.lifecycle_phase != LifecyclePhase::Ready {
        return Err(UpdateError::Refused(
            "current-profile daemon is not Ready".into(),
        ));
    }
    if welcome.profile_id != profile.profile_id {
        return Err(UpdateError::Refused(
            "daemon Welcome profile identity does not exactly match".into(),
        ));
    }
    if !welcome.features.is_superset(required) {
        return Err(UpdateError::Refused(
            "daemon Welcome is missing required feature families".into(),
        ));
    }
    Ok(())
}

fn stop_and_reap(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}
