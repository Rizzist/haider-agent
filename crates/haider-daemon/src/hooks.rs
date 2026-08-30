//! Daemon-owned hook discovery, trust, execution, and permission decisions.
//!
//! Ordinary and subscribe hooks consume only committed envelopes delivered by
//! the session actor's post-store seam. Decision hooks wait for the committed
//! `PermissionRequired` fact and answer the already-open menu through the same
//! durable CAS as every interactive surface.
//!
//! User-message hooks likewise classify only the canonical committed
//! `UserMessage` acceptance fact. TUI, RPC, headless, and voice submissions all
//! converge on that one acceptance transaction before this module observes
//! them, so one accepted message produces one surface-independent hook event.
//! Surface identity is intentionally absent: preserving that 1:1 fact/event
//! mapping gives every submission surface identical hook semantics.

#[path = "hooks_server.rs"]
mod hooks_server;
#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;

use crate::session_hub::SessionHub;
use haider_core::{
    HookTrustChange, HookTrustCommand, MenuResolutionCommand, MenuResolutionOutcome, StoreHandle,
};
use haider_protocol::effect::{AuthorizationVerdict, EffectIntent, EffectPhase};
use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::headless::HeadlessRunEventPayload;
use haider_protocol::hook::{
    HOOKS_CONFIG_SCHEMA, HookAttachmentMetadata, HookAttachmentSet, HookDecisionKind,
    HookEventPayload, HookFired, HookInput, HookNotice, HookOutput, HookRuntimeKind,
    HookSubscription, HookSubscriptionState,
};
use haider_protocol::ids::{EffectId, EventId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::{AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::{DeliveryMode, EventPayload};
use haider_rpc::{CommandId, HookSummaryWire, HookTrustStateWire};
use hooks_server::{HookServerRegistry, ServerReply};
#[cfg(unix)]
use rustix::fd::OwnedFd;
#[cfg(unix)]
use rustix::fs::{FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

#[cfg(unix)]
type DirectoryHandle = OwnedFd;
#[cfg(windows)]
type DirectoryHandle = haider_platform::WorkspaceDirectory;
#[cfg(unix)]
type DirectoryOpenError = rustix::io::Errno;
#[cfg(windows)]
type DirectoryOpenError = std::io::Error;

const HOOKS_FILE: &str = "hooks.json";
const MAX_HOOK_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_HOOK_ANCESTORS: usize = 256;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_SERVER_IDLE_TIMEOUT_MS: u64 = 60_000;
const INLINE_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_STREAM_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_USER_MESSAGE_TEXT_BYTES: usize = 32 * 1024;
const SUBSCRIBE_QUEUE: usize = 64;
const SUBSCRIBE_BACKOFF_MIN: Duration = Duration::from_millis(200);
const SUBSCRIBE_BACKOFF_MAX: Duration = Duration::from_secs(5);
const HOOK_SNAPSHOT_IDLE_DELAY: Duration = Duration::from_secs(1);
const HOOK_SNAPSHOT_BUSY_INTERVAL: Duration = Duration::from_secs(5);
const HOOK_CONTROL_MAX_REQUESTS: usize = 64;
const HOOK_CONTROL_MAX_BYTES: usize = 64 * 1024;
const HOOK_DRAIN_PAGE_MAX_REQUESTS: usize = 256;
const HOOK_DRAIN_PAGE_MAX_BYTES: usize = 16 * 1024 * 1024;
const HOOK_CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(unix)]
const ENV_ALLOWLIST: [&str; 5] = ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR"];
// `cmd.exe` and Windows system tools need these bootstrap variables even
// when a hook otherwise receives the same deliberately empty environment.
// None is a credential-bearing application variable.
#[cfg(windows)]
const ENV_ALLOWLIST: [&str; 7] = [
    "PATH",
    "SystemRoot",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
];

#[derive(Debug)]
#[must_use = "hook child cleanup failures must be handled"]
enum HookChildReapOutcome {
    Exited(std::process::ExitStatus),
    WaitFailed(std::io::Error),
    TimedOut(haider_platform::WaitTimeout),
}

async fn classify_hook_child_reap(
    wait: impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>>,
) -> HookChildReapOutcome {
    match haider_platform::bounded_wait("hook child reap", HOOK_CHILD_REAP_TIMEOUT, wait).await {
        haider_platform::BoundedWait::Completed(Ok(status)) => HookChildReapOutcome::Exited(status),
        haider_platform::BoundedWait::Completed(Err(error)) => {
            HookChildReapOutcome::WaitFailed(error)
        }
        haider_platform::BoundedWait::TimedOut(timeout) => HookChildReapOutcome::TimedOut(timeout),
    }
}

async fn reap_hook_child(child: &mut tokio::process::Child) -> HookChildReapOutcome {
    classify_hook_child_reap(child.wait()).await
}

fn report_hook_child_reap(context: &'static str, outcome: HookChildReapOutcome) {
    match outcome {
        HookChildReapOutcome::Exited(_) => {}
        HookChildReapOutcome::WaitFailed(error) => eprintln!(
            "haiderd: lifecycle event=hook_child_reap_failed context={context} error_kind={:?} raw_os_error={:?}",
            error.kind(),
            error.raw_os_error()
        ),
        HookChildReapOutcome::TimedOut(timeout) => eprintln!(
            "haiderd: lifecycle event=hook_child_reap_timeout context={context} operation={} timeout_ms={}",
            timeout.operation(),
            timeout.limit().as_millis()
        ),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HookTrustPolicy {
    TrustNone,
    #[default]
    PerDigest,
    TrustWorkspace,
}

impl HookTrustPolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TrustNone => "trust_none",
            Self::PerDigest => "per_digest",
            Self::TrustWorkspace => "trust_workspace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookKind {
    Exec,
    Subscribe,
    Server { idle_timeout_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookConfigKind {
    Exec,
    Subscribe,
}

impl HookKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Subscribe => "subscribe",
            Self::Server { .. } => "exec",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookMode {
    #[default]
    Spawn,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MatchEvent {
    SessionCreated,
    UserMessage,
    RunStarted,
    RunParked,
    RunFinished,
    SubagentSpawned,
    SubagentReported,
    CompactionCompleted,
    UpdateAvailable,
    AccountExpired,
}

impl MatchEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreated => "session_created",
            Self::UserMessage => "user_message",
            Self::RunStarted => "run_started",
            Self::RunParked => "run_parked",
            Self::RunFinished => "run_finished",
            Self::SubagentSpawned => "subagent_spawned",
            Self::SubagentReported => "subagent_reported",
            Self::CompactionCompleted => "compaction_completed",
            Self::UpdateAvailable => "update_available",
            Self::AccountExpired => "account_expired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct HookMatcher {
    #[serde(alias = "event_kind")]
    event: MatchEvent,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, alias = "run_outcome")]
    outcome: Option<String>,
    #[serde(default)]
    parked_kind: Option<String>,
    #[serde(default)]
    mode: Option<DeliveryMode>,
    #[serde(default)]
    has_attachments: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct HookEntry {
    matcher: HookMatcher,
    kind: HookConfigKind,
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    decision: bool,
    #[serde(default)]
    mode: HookMode,
    #[serde(default)]
    idle_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HookSource {
    Workspace,
    Profile,
}

#[derive(Debug, Clone)]
struct HookDefinition {
    name: String,
    matcher: HookMatcher,
    kind: HookKind,
    command: String,
    timeout: Duration,
    decision: bool,
    digest: String,
    source_path: PathBuf,
    source: HookSource,
    workspace_cwd: PathBuf,
}

impl HookDefinition {
    fn subscriber_key(&self) -> String {
        format!(
            "{}\0{}\0{}\0{}",
            self.workspace_cwd.display(),
            self.source_path.display(),
            self.name,
            self.digest
        )
    }
}

#[derive(Debug, Clone)]
struct Discovery {
    hooks: BTreeMap<String, HookDefinition>,
    notices: Vec<HookNotice>,
    policy: HookTrustPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveryStamp(Vec<(PathBuf, Option<(u64, u128)>)>);

#[derive(Debug, Clone)]
struct CachedDiscovery {
    stamp: DiscoveryStamp,
    discovery: Discovery,
}

#[derive(Clone)]
pub(crate) struct HookService {
    inner: Arc<HookServiceInner>,
}

#[derive(Clone)]
pub(crate) struct WeakHookService {
    inner: Weak<HookServiceInner>,
}

struct HookServiceInner {
    profile_root: PathBuf,
    store: haider_core::SqliteStoreHandle,
    hub: SessionHub,
    controls: mpsc::Sender<QueuedEngineControl>,
    control_bytes: Arc<Semaphore>,
    committed_wake: watch::Sender<Option<DurableHeadWake>>,
    dispatch_progress: Notify,
    shutdown: watch::Sender<bool>,
    servers: HookServerRegistry,
    pins: RwLock<HashSet<String>>,
    workspace_baselines: Mutex<HashMap<String, String>>,
    /// Hook identity → latest digest the daemon itself observed as trusted.
    /// This is the authority for revoked-by-edit classification; clients do
    /// not infer it by comparing snapshots.
    observed_trusted: Mutex<HashMap<String, String>>,
    /// Canonical workspace → mtime-keyed discovery result. Decision hooks
    /// deliberately bypass this cache again immediately before execution.
    discovery_cache: Mutex<HashMap<PathBuf, CachedDiscovery>>,
    next_event: AtomicU64,
    #[cfg(test)]
    snapshot_persist_count: AtomicU64,
    #[cfg(test)]
    discovery_stamp_count: AtomicU64,
}

impl HookService {
    #[cfg(test)]
    fn snapshot_persist_count(&self) -> u64 {
        self.inner.snapshot_persist_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn discovery_stamp_count(&self) -> u64 {
        self.inner.discovery_stamp_count.load(Ordering::Relaxed)
    }

    pub(crate) fn downgrade(&self) -> WeakHookService {
        WeakHookService {
            inner: Arc::downgrade(&self.inner),
        }
    }

    async fn send_control(&self, message: EngineMessage, weight: usize) -> Result<(), ()> {
        let charged = weight.clamp(1, HOOK_CONTROL_MAX_BYTES);
        let permits = u32::try_from(charged).map_err(|_| ())?;
        let permit = Arc::clone(&self.inner.control_bytes)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| ())?;
        self.inner
            .controls
            .send(QueuedEngineControl {
                message,
                _byte_permit: permit,
            })
            .await
            .map_err(|_| ())
    }

    pub(crate) fn observe_committed(&self, envelopes: &[RawEnvelope]) {
        if envelopes
            .iter()
            .all(|envelope| HookEventPayload::is_engine_fact(&envelope.payload))
        {
            return;
        }
        if let Some(last) = envelopes.last() {
            self.inner
                .committed_wake
                .send_replace(Some(DurableHeadWake {
                    session_id: last.session_id.clone(),
                    head_seq: last.seq,
                }));
        }
    }

    pub(crate) async fn session_deleted(&self, session_id: SessionId) {
        let weight = std::mem::size_of::<SessionId>().saturating_add(session_id.as_str().len());
        let _ = self
            .send_control(EngineMessage::SessionDeleted(session_id), weight)
            .await;
    }

    /// Drains the durable outbox through its current tail before session
    /// deletion publishes its admission tombstone. A second store check after
    /// the actor FIFO fence catches an append that won the intervening race.
    pub(crate) async fn drain_session_before_delete(
        &self,
        session_id: SessionId,
    ) -> Result<(), HaiderError> {
        let weight = std::mem::size_of::<SessionId>().saturating_add(session_id.as_str().len());
        let (completed, wait) = oneshot::channel();
        self.send_control(
            EngineMessage::DrainSession {
                session_id,
                completed,
            },
            weight,
        )
        .await
        .map_err(|()| {
            HaiderError::new(
                ErrorCode::Internal,
                "hook engine stopped before session deletion drain",
                true,
            )
        })?;
        wait.await.map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "hook engine stopped during session deletion drain",
                true,
            )
        })?
    }

    pub(crate) async fn list(
        &self,
        cwd: PathBuf,
    ) -> Result<(HookTrustPolicy, u64, Vec<HookSummaryWire>), String> {
        let workspace_cwd = cwd.clone();
        let discovery = discover_cached_async(self, cwd).await?;
        self.prepare_workspace_trust(&discovery).await;
        let current_servers = discovery
            .hooks
            .values()
            .filter(|definition| matches!(definition.kind, HookKind::Server { .. }))
            .map(HookDefinition::subscriber_key)
            .collect::<HashSet<_>>();
        let trusted_servers = discovery
            .hooks
            .values()
            .filter(|definition| {
                matches!(definition.kind, HookKind::Server { .. })
                    && self.is_trusted(definition, false, discovery.policy)
            })
            .map(HookDefinition::subscriber_key)
            .collect::<HashSet<_>>();
        self.inner.servers.reconcile_workspace(
            &workspace_cwd,
            &current_servers,
            &trusted_servers,
            None,
        );
        let revision = self
            .inner
            .store
            .hook_trust_changes()
            .await
            .map_err(|error| error.message)?
            .len() as u64;
        let mut hooks = Vec::with_capacity(discovery.hooks.len());
        for definition in discovery.hooks.values() {
            let trusted = self.is_trusted(definition, false, discovery.policy);
            let identity = workspace_identity(definition);
            let trust_state = if trusted {
                self.inner
                    .observed_trusted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(identity, definition.digest.clone());
                HookTrustStateWire::Trusted
            } else if discovery.policy != HookTrustPolicy::TrustNone
                && self
                    .inner
                    .observed_trusted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&identity)
                    .is_some_and(|digest| digest != &definition.digest)
            {
                HookTrustStateWire::RevokedByEdit
            } else {
                HookTrustStateWire::Untrusted
            };
            hooks.push(HookSummaryWire {
                name: definition.name.clone(),
                digest: definition.digest.clone(),
                source: definition.source_path.display().to_string(),
                kind: definition.kind.as_str().to_owned(),
                event: definition.matcher.event.as_str().to_owned(),
                trusted,
                trust_state: Some(trust_state),
                decision: definition.decision,
                timeout_ms: duration_ms(definition.timeout),
            });
        }
        Ok((discovery.policy, revision, hooks))
    }

    pub(crate) async fn apply_trust(
        &self,
        command_id: CommandId,
        digest: String,
        trusted: bool,
    ) -> Result<HookTrustChange, haider_protocol::error::HaiderError> {
        if !valid_digest(&digest) {
            return Err(haider_protocol::error::HaiderError::new(
                haider_protocol::error::ErrorCode::InvalidArgument,
                "hook trust digest must be 64 lowercase hexadecimal characters",
                false,
            ));
        }
        let request_json = serde_json::to_string(&json!({
            "digest": digest,
            "trusted": trusted,
        }))
        .map_err(|error| {
            haider_protocol::error::HaiderError::new(
                haider_protocol::error::ErrorCode::Internal,
                format!("cannot serialize hook trust request: {error}"),
                false,
            )
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let change = self
            .inner
            .store
            .apply_hook_trust_command(HookTrustCommand {
                command_id: command_id.0,
                request_digest,
                request_json,
                digest: digest.clone(),
                trusted,
                workspace: None,
            })
            .await?;
        {
            let mut pins = self
                .inner
                .pins
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if change.trusted {
                pins.insert(change.digest.clone());
            } else {
                pins.remove(&change.digest);
                self.inner.servers.kill_digest(&change.digest);
                self.inner
                    .observed_trusted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, digest| digest != &change.digest);
            }
        }
        let weight = std::mem::size_of::<HookTrustChange>()
            .saturating_add(change.digest.len())
            .saturating_add(change.workspace.as_deref().map_or(0, str::len));
        let _ = self
            .send_control(EngineMessage::TrustChanged(change.clone()), weight)
            .await;
        let revision = if change.revision == 0 {
            u64::try_from(self.inner.store.hook_trust_changes().await?.len()).unwrap_or(u64::MAX)
        } else {
            change.revision
        };
        self.journal_trust_change(&change.digest, change.trusted, revision)
            .await;
        Ok(change)
    }

    async fn journal_trust_change(&self, digest: &str, trusted: bool, revision: u64) {
        let payload = match (HookEventPayload::HookTrustChanged {
            digest: digest.to_owned(),
            trusted,
            revision,
        })
        .to_payload_value()
        {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook trust fact serialization failed");
                return;
            }
        };
        let session_ids = match self.inner.hub.session_ids().await {
            Ok(session_ids) => session_ids,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook trust journal session listing failed");
                return;
            }
        };
        for session_id in session_ids {
            let mut envelope = [RawEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("hook-trust-{revision}-{}", session_id.as_str())),
                seq: 0,
                session_id,
                branch_id: None,
                run_id: None,
                agent_id: None,
                device_id: self.inner.hub.device_id(),
                authority_epoch: 0,
                worker_generation: self.inner.hub.worker_generation(),
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 0,
                render: RenderTargets {
                    ui: false,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: payload.clone(),
            }];
            if let Err(error) = self.inner.hub.append(&mut envelope).await
                && error.code != ErrorCode::InvalidArgument
            {
                tracing::warn!(target: "haider.hooks", ?error, "hook trust fact append failed");
            }
        }
    }

    fn is_trusted(
        &self,
        definition: &HookDefinition,
        run_override: bool,
        policy: HookTrustPolicy,
    ) -> bool {
        if run_override {
            return true;
        }
        if policy == HookTrustPolicy::TrustNone {
            return false;
        }
        if self
            .inner
            .pins
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&definition.digest)
        {
            return true;
        }
        false
    }

    async fn prepare_workspace_trust(&self, discovery: &Discovery) {
        if discovery.policy != HookTrustPolicy::TrustWorkspace {
            return;
        }
        for definition in discovery
            .hooks
            .values()
            .filter(|definition| definition.source == HookSource::Workspace)
        {
            let identity = workspace_identity(definition);
            if self
                .inner
                .workspace_baselines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&identity)
            {
                continue;
            }
            let identity_digest = blake3::hash(identity.as_bytes()).to_hex().to_string();
            let request_json = match serde_json::to_string(&json!({
                "digest": &definition.digest,
                "trusted": true,
                "workspace": &identity,
            })) {
                Ok(request_json) => request_json,
                Err(error) => {
                    tracing::warn!(target: "haider.hooks", ?error, "workspace trust request serialization failed");
                    continue;
                }
            };
            let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
            let change = self
                .inner
                .store
                .apply_hook_trust_command(HookTrustCommand {
                    command_id: format!("hooks-workspace-{identity_digest}"),
                    request_digest,
                    request_json,
                    digest: definition.digest.clone(),
                    trusted: true,
                    workspace: Some(identity.clone()),
                })
                .await;
            match change {
                Ok(change) => {
                    self.inner
                        .workspace_baselines
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(identity, change.digest.clone());
                    self.inner
                        .pins
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(change.digest);
                }
                Err(error) => {
                    tracing::warn!(target: "haider.hooks", ?error, "workspace trust baseline failed");
                }
            }
        }
    }

    fn next_event_id(&self) -> EventId {
        let sequence = self.inner.next_event.fetch_add(1, Ordering::Relaxed) + 1;
        EventId::new(format!(
            "hook-{}-{sequence}",
            self.inner.store.worker_generation()
        ))
    }

    async fn journal(&self, cause: &RawEnvelope, payload: HookEventPayload) -> bool {
        let payload = match payload.to_payload_value() {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook fact serialization failed");
                return false;
            }
        };
        let mut envelope = [RawEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.next_event_id(),
            seq: 0,
            session_id: cause.session_id.clone(),
            branch_id: cause.branch_id.clone(),
            run_id: cause.run_id.clone(),
            agent_id: cause.agent_id.clone(),
            device_id: self.inner.hub.device_id(),
            authority_epoch: cause.authority_epoch,
            worker_generation: self.inner.hub.worker_generation(),
            causation_id: Some(cause.event_id.clone()),
            correlation_id: cause.correlation_id.clone(),
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload,
        }];
        if let Err(error) = self.inner.hub.append(&mut envelope).await {
            tracing::warn!(target: "haider.hooks", ?error, "hook fact append failed");
            return false;
        }
        true
    }

    async fn journal_lockdown_refusal(
        &self,
        cause: &RawEnvelope,
        provider: &str,
        reason: &str,
    ) -> bool {
        let payload = match serde_json::to_value(EventPayload::LockdownRefused(
            haider_protocol::lockdown::LockdownRefused {
                provider: provider.to_owned(),
                tool: "hooks".to_owned(),
                reason: reason.to_owned(),
                tools_allowed: crate::lockdown::allowed_tool_names(),
            },
        )) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "lockdown refusal serialization failed");
                return false;
            }
        };
        let mut envelope = [RawEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.next_event_id(),
            seq: 0,
            session_id: cause.session_id.clone(),
            branch_id: cause.branch_id.clone(),
            run_id: cause.run_id.clone(),
            agent_id: cause.agent_id.clone(),
            device_id: self.inner.hub.device_id(),
            authority_epoch: cause.authority_epoch,
            worker_generation: self.inner.hub.worker_generation(),
            causation_id: Some(cause.event_id.clone()),
            correlation_id: cause.correlation_id.clone(),
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload,
        }];
        if let Err(error) = self.inner.hub.append(&mut envelope).await {
            tracing::warn!(target: "haider.hooks", ?error, "lockdown refusal append failed");
            return false;
        }
        true
    }
}

impl WeakHookService {
    pub(crate) fn upgrade(&self) -> Option<HookService> {
        self.inner.upgrade().map(|inner| HookService { inner })
    }
}

pub(crate) struct HookEngine {
    service: HookService,
    task: Option<JoinHandle<()>>,
}

impl HookEngine {
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    pub(crate) async fn start(
        profile_root: PathBuf,
        store: haider_core::SqliteStoreHandle,
        hub: SessionHub,
    ) -> Result<(HookService, Self), haider_protocol::error::HaiderError> {
        let hydration = HookStartupHydrator::prepare(&store).await?;
        let hydration = crate::runtime::finish_hook_hydration_for_test(&store, hydration).await?;
        Self::start_with_state(profile_root, store, hub, hydration.into_state()).await
    }

    pub(crate) async fn start_hydrated(
        profile_root: PathBuf,
        store: haider_core::SqliteStoreHandle,
        hub: SessionHub,
        hydration: HookStartupHydrator,
    ) -> Result<(HookService, Self), haider_protocol::error::HaiderError> {
        Self::start_with_state(profile_root, store, hub, hydration.into_state()).await
    }

    async fn start_with_state(
        profile_root: PathBuf,
        store: haider_core::SqliteStoreHandle,
        hub: SessionHub,
        state: EngineState,
    ) -> Result<(HookService, Self), haider_protocol::error::HaiderError> {
        let changes = store.hook_trust_changes().await?;
        let mut pins = HashSet::new();
        let mut workspace_baselines = HashMap::new();
        for change in changes {
            if let Some(workspace) = change.workspace.clone() {
                workspace_baselines.insert(workspace, change.digest.clone());
            }
            if change.trusted {
                pins.insert(change.digest);
            } else {
                pins.remove(&change.digest);
            }
        }
        let (controls, control_receiver) = mpsc::channel(HOOK_CONTROL_MAX_REQUESTS);
        let control_bytes = Arc::new(Semaphore::new(HOOK_CONTROL_MAX_BYTES));
        let (committed_wake, wake_receiver) = watch::channel(None);
        let (shutdown, _) = watch::channel(false);
        let observed_trusted = workspace_baselines.clone();
        let service = HookService {
            inner: Arc::new(HookServiceInner {
                profile_root,
                store,
                hub,
                controls,
                control_bytes,
                committed_wake,
                dispatch_progress: Notify::new(),
                shutdown,
                servers: HookServerRegistry::default(),
                pins: RwLock::new(pins),
                workspace_baselines: Mutex::new(workspace_baselines),
                observed_trusted: Mutex::new(observed_trusted),
                discovery_cache: Mutex::new(HashMap::new()),
                next_event: AtomicU64::new(0),
                #[cfg(test)]
                snapshot_persist_count: AtomicU64::new(0),
                #[cfg(test)]
                discovery_stamp_count: AtomicU64::new(0),
            }),
        };
        if let Err(error) = persist_service_snapshot(&service, &state).await {
            tracing::warn!(target: "haider.hooks", %error, "hook-engine snapshot persistence failed; journal rebuild remains authoritative");
        }
        let manager_service = service.clone();
        let task = tokio::spawn(run_engine(
            control_receiver,
            wake_receiver,
            manager_service,
            state,
        ));
        Ok((
            service.clone(),
            Self {
                service,
                task: Some(task),
            },
        ))
    }

    pub(crate) async fn shutdown(mut self) {
        // `send_replace`, not `send`: `watch::Sender::send` does NOT store
        // the value when no receiver is currently subscribed, so the flip
        // could be lost exactly when no actor is alive to observe it — and
        // a late actor's start-check would then read a stale `false`.
        self.service.inner.shutdown.send_replace(true);
        self.service.inner.servers.shutdown().await;
        let (done, wait) = oneshot::channel();
        let _ = self
            .service
            .send_control(EngineMessage::Shutdown(done), 1)
            .await;
        let _ = wait.await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for HookEngine {
    fn drop(&mut self) {
        self.service.inner.shutdown.send_replace(true);
        self.service.inner.servers.abort_all();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

enum EngineMessage {
    DrainSession {
        session_id: SessionId,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    SessionDeleted(SessionId),
    TrustChanged(HookTrustChange),
    Shutdown(oneshot::Sender<()>),
}

struct QueuedEngineControl {
    message: EngineMessage,
    _byte_permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct DurableHeadWake {
    session_id: SessionId,
    head_seq: u64,
}

struct EngineState {
    sessions: HashMap<SessionId, DecisionState>,
    run_trust: HashSet<(SessionId, RunId)>,
    terminal_run_trust: HashSet<(SessionId, RunId)>,
    through_seq: HashMap<SessionId, u64>,
    through_digest: HashMap<SessionId, String>,
    notice_dedup: HashSet<String>,
    subscribers: HashMap<String, SubscriberHandle>,
}

type BatchDiscoveryCache =
    HashMap<SessionId, Option<(SessionMetadataV1, Result<Discovery, String>)>>;

#[derive(Default)]
struct HookDispatchFlights {
    active: HashSet<(SessionId, u64)>,
    completed: HashSet<(SessionId, u64)>,
    blocked: HashSet<(SessionId, u64)>,
}

type InflightHookDispatches = Arc<Mutex<HookDispatchFlights>>;

struct SubscriberHandle {
    sender: mpsc::Sender<SubscriberMessage>,
    definition_key: String,
    digest: String,
    workspace_cwd: PathBuf,
    run_scope: Option<(SessionId, RunId)>,
}

struct SubscriberMessage {
    input: Arc<[u8]>,
    delivered: oneshot::Sender<()>,
}

enum PendingServerResponse {
    Waiting(oneshot::Receiver<ServerReply>),
    Ready(ServerReply),
}

struct PendingServerFire {
    definition: HookDefinition,
    cause: RawEnvelope,
    decision: Option<DecisionContext>,
    response: PendingServerResponse,
}

impl PendingServerFire {
    async fn complete(self, service: &HookService) -> bool {
        let reply = match self.response {
            PendingServerResponse::Waiting(wait) => match wait.await {
                Ok(reply) => reply,
                Err(_) if *service.inner.shutdown.borrow() => return false,
                Err(_) => ServerReply::DefinitionChanged,
            },
            PendingServerResponse::Ready(reply) => reply,
        };
        match reply {
            ServerReply::DefinitionChanged => {
                let decision = self.decision.is_some();
                service
                    .journal(
                        &self.cause,
                        HookEventPayload::HookNotice(HookNotice {
                            hook: Some(self.definition.name),
                            digest: Some(self.definition.digest),
                            source: self.definition.source_path.display().to_string(),
                            reason: if decision {
                                "decision hook digest or trust changed before spawn".into()
                            } else {
                                "hook digest or trust changed before spawn".into()
                            },
                        }),
                    )
                    .await
            }
            ServerReply::Result(mut result) => {
                if result.cancelled {
                    return false;
                }
                let (kind, proposed_decision, menu_id, decision_applied) =
                    if let Some(decision) = self.decision {
                        let proposed = strict_decision(&result);
                        let applied = if let Some(proposed) = proposed {
                            resolve_decision(service, &self.definition, &decision, proposed)
                                .await
                                .unwrap_or(false)
                        } else {
                            false
                        };
                        result.proposed_decision = proposed;
                        (
                            HookRuntimeKind::Decision,
                            proposed,
                            Some(decision.permission.menu.id),
                            applied,
                        )
                    } else {
                        (HookRuntimeKind::Exec, None, None, false)
                    };
                service
                    .journal(
                        &self.cause,
                        HookEventPayload::HookFired(HookFired {
                            hook: self.definition.name,
                            digest: self.definition.digest,
                            kind,
                            observed_seq: self.cause.seq,
                            exit_code: result.exit_code,
                            timed_out: result.timed_out,
                            stdout: result.stdout,
                            stderr: result.stderr,
                            proposed_decision,
                            menu_id,
                            decision_applied,
                        }),
                    )
                    .await
            }
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct DecisionState {
    intents: HashMap<EffectId, EffectIntent>,
    bindings: HashMap<MenuId, EffectIntent>,
    menus: HashMap<MenuId, OpenPermission>,
}

#[derive(Clone, Serialize, Deserialize)]
struct OpenPermission {
    menu: Menu,
    opening: RawEnvelope,
}

#[derive(Clone)]
struct DecisionContext {
    intent: EffectIntent,
    permission: OpenPermission,
}

const HOOK_ENGINE_SNAPSHOT_VERSION: u32 = 2;
const HOOK_ENGINE_SNAPSHOT_FILE: &str = "hook-engine.snapshot.msgpack";
const HOOK_BOUNDARY_EVENT_ID_PREFIX: &str = "event-id:";

#[derive(Serialize, Deserialize)]
struct HookEngineSnapshot {
    version: u32,
    sessions: HashMap<SessionId, DecisionState>,
    run_trust: HashSet<(SessionId, RunId)>,
    #[serde(default)]
    terminal_run_trust: HashSet<(SessionId, RunId)>,
    #[serde(default)]
    terminal_run_trust_complete: bool,
    through_seq: HashMap<SessionId, u64>,
    through_digest: HashMap<SessionId, String>,
}

#[derive(Serialize)]
struct HookEngineSnapshotRef<'a> {
    version: u32,
    sessions: &'a HashMap<SessionId, DecisionState>,
    run_trust: &'a HashSet<(SessionId, RunId)>,
    terminal_run_trust: &'a HashSet<(SessionId, RunId)>,
    terminal_run_trust_complete: bool,
    through_seq: &'a HashMap<SessionId, u64>,
    through_digest: &'a HashMap<SessionId, String>,
}

#[derive(Serialize, Deserialize)]
struct HookEngineSnapshotFile {
    payload: Vec<u8>,
    digest: String,
}

/// Compact hook reducer state prepared before the shared startup scan.
/// Runtime feeds it the same decoded pages as turn recovery.
pub(crate) struct HookStartupHydrator {
    state: EngineState,
}

impl HookStartupHydrator {
    pub(crate) async fn prepare(
        store: &haider_core::SqliteStoreHandle,
    ) -> Result<Self, haider_protocol::error::HaiderError> {
        let snapshot = load_engine_snapshot(store).await;
        let (sessions, run_trust, terminal_run_trust, through_seq, through_digest) = snapshot
            .map_or_else(
                || {
                    (
                        HashMap::new(),
                        HashSet::new(),
                        HashSet::new(),
                        HashMap::new(),
                        HashMap::new(),
                    )
                },
                |snapshot| {
                    (
                        snapshot.sessions,
                        snapshot.run_trust,
                        snapshot.terminal_run_trust,
                        snapshot.through_seq,
                        snapshot.through_digest,
                    )
                },
            );
        let mut state = EngineState {
            sessions,
            run_trust,
            terminal_run_trust,
            through_seq,
            through_digest,
            notice_dedup: HashSet::new(),
            subscribers: HashMap::new(),
        };
        let session_ids = store.session_ids().await?;
        let current_sessions = session_ids.iter().cloned().collect::<HashSet<_>>();
        state
            .sessions
            .retain(|session_id, _| current_sessions.contains(session_id));
        state
            .through_seq
            .retain(|session_id, _| current_sessions.contains(session_id));
        state
            .through_digest
            .retain(|session_id, _| current_sessions.contains(session_id));
        state
            .run_trust
            .retain(|(session_id, _)| current_sessions.contains(session_id));
        state
            .terminal_run_trust
            .retain(|(session_id, _)| current_sessions.contains(session_id));
        for session_id in &session_ids {
            let has_decision_state = state.sessions.contains_key(session_id);
            let has_run_trust = state
                .run_trust
                .iter()
                .any(|(candidate, _)| candidate == session_id);
            let has_terminal_run_trust = state
                .terminal_run_trust
                .iter()
                .any(|(candidate, _)| candidate == session_id);
            let through_seq = state.through_seq.get(session_id).copied();
            let through_digest = state.through_digest.get(session_id);
            let cursor_coordinates_match = matches!(
                (through_seq, through_digest),
                (Some(_), Some(_)) | (None, None)
            );
            let retained_state_has_cursor =
                (!has_decision_state && !has_run_trust && !has_terminal_run_trust)
                    || through_seq.is_some();
            let structurally_valid = cursor_coordinates_match && retained_state_has_cursor;
            let boundary_valid = if structurally_valid {
                if let (Some(through_seq), Some(through_digest)) = (through_seq, through_digest) {
                    store
                        .read(session_id, through_seq.saturating_sub(1), 1)
                        .await?
                        .into_iter()
                        .find(|envelope| envelope.seq == through_seq)
                        .is_some_and(|envelope| hook_cursor_matches(&envelope, through_digest))
                } else {
                    true
                }
            } else {
                false
            };
            if !boundary_valid {
                clear_hook_session(&mut state, session_id);
            }
        }
        Ok(Self { state })
    }

    pub(crate) fn scan_start(&self, session_id: &SessionId) -> u64 {
        self.state.through_seq.get(session_id).copied().unwrap_or(0)
    }

    pub(crate) fn fold_page(&mut self, session_id: &SessionId, page: &[RawEnvelope]) {
        let mut since_seq = self.scan_start(session_id);
        for envelope in page {
            if envelope.seq <= since_seq {
                continue;
            }
            let payload = decode_committed_payload(&envelope.payload);
            advance_durable_cursor(&mut self.state, envelope);
            reduce_decoded_durable_state(&mut self.state, envelope, &payload);
            since_seq = envelope.seq;
        }
    }

    /// Advances the reducer checkpoint across a suffix whose payload kinds
    /// are irrelevant to hook state. The event id is an immutable journal
    /// coordinate, so this retains checkpoint validation without fetching or
    /// decoding the suffix merely to hash its final envelope.
    pub(crate) fn advance_through(
        &mut self,
        session_id: &SessionId,
        through_seq: u64,
        boundary_event_id: &EventId,
    ) {
        if through_seq > self.scan_start(session_id) {
            self.state
                .through_seq
                .insert(session_id.clone(), through_seq);
            self.state.through_digest.insert(
                session_id.clone(),
                hook_boundary_event_id_digest(boundary_event_id),
            );
        }
    }

    fn into_state(self) -> EngineState {
        self.state
    }
}

#[derive(Debug)]
enum HookSnapshotPersistError {
    Encode(rmp_serde::encode::Error),
    Io {
        operation: &'static str,
        source: std::io::Error,
    },
    Task(tokio::task::JoinError),
}

impl std::fmt::Display for HookSnapshotPersistError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode(error) => write!(formatter, "hook snapshot encode failed: {error}"),
            Self::Io { operation, source } => {
                write!(formatter, "hook snapshot {operation} failed: {source}")
            }
            Self::Task(error) => write!(formatter, "hook snapshot writer task failed: {error}"),
        }
    }
}

fn clear_hook_session(state: &mut EngineState, session_id: &SessionId) {
    state.sessions.remove(session_id);
    state
        .run_trust
        .retain(|(candidate, _)| candidate != session_id);
    state
        .terminal_run_trust
        .retain(|(candidate, _)| candidate != session_id);
    state.through_seq.remove(session_id);
    state.through_digest.remove(session_id);
}

async fn load_engine_snapshot(
    store: &haider_core::SqliteStoreHandle,
) -> Option<HookEngineSnapshot> {
    let bytes = tokio::fs::read(store.root().join(HOOK_ENGINE_SNAPSHOT_FILE))
        .await
        .ok()?;
    let file = rmp_serde::from_slice::<HookEngineSnapshotFile>(&bytes).ok()?;
    if file.digest != hook_snapshot_payload_digest(&file.payload) {
        return None;
    }
    let snapshot = rmp_serde::from_slice::<HookEngineSnapshot>(&file.payload).ok()?;
    (snapshot.version == HOOK_ENGINE_SNAPSHOT_VERSION && snapshot.terminal_run_trust_complete)
        .then_some(snapshot)
}

fn hook_snapshot_payload_digest(payload: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.hook-engine.snapshot-file.v1\0");
    hasher.update(
        &u64::try_from(payload.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.update(payload);
    hasher.finalize().to_hex().to_string()
}

fn encode_hook_snapshot_file(payload: Vec<u8>) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let digest = hook_snapshot_payload_digest(&payload);
    rmp_serde::to_vec_named(&HookEngineSnapshotFile { payload, digest })
}

async fn persist_engine_snapshot(
    store: &haider_core::SqliteStoreHandle,
    state: &EngineState,
) -> Result<(), HookSnapshotPersistError> {
    let payload = rmp_serde::to_vec_named(&HookEngineSnapshotRef {
        version: HOOK_ENGINE_SNAPSHOT_VERSION,
        sessions: &state.sessions,
        run_trust: &state.run_trust,
        terminal_run_trust: &state.terminal_run_trust,
        terminal_run_trust_complete: true,
        through_seq: &state.through_seq,
        through_digest: &state.through_digest,
    })
    .map_err(HookSnapshotPersistError::Encode)?;
    let bytes = encode_hook_snapshot_file(payload).map_err(HookSnapshotPersistError::Encode)?;
    let root = store.root().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let path = root.join(HOOK_ENGINE_SNAPSHOT_FILE);
        let temporary = root.join("hook-engine.snapshot.tmp");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| HookSnapshotPersistError::Io {
                operation: "temporary-file open",
                source,
            })?;
        file.write_all(&bytes)
            .map_err(|source| HookSnapshotPersistError::Io {
                operation: "write",
                source,
            })?;
        // Plain fsync hands the complete temporary file to the device before
        // rename. The one trailing directory Full below is the cache barrier
        // for both these bytes and the installed name, so a second earlier
        // whole-device flush would add latency without a stronger state.
        haider_platform::fs::sync_file(&file, haider_platform::SyncPolicy::Plain).map_err(
            |source| HookSnapshotPersistError::Io {
                operation: "file sync",
                source,
            },
        )?;
        drop(file);
        haider_platform::replace_file(&temporary, &path).map_err(|source| {
            HookSnapshotPersistError::Io {
                operation: "install",
                source,
            }
        })?;
        // The snapshot remains fully durable at publication: this barrier is
        // ordered after the file fsync and atomic rename.
        haider_platform::fs::sync_directory(&root, haider_platform::SyncPolicy::Full).map_err(
            |source| HookSnapshotPersistError::Io {
                operation: "directory sync",
                source,
            },
        )
    })
    .await
    .map_err(HookSnapshotPersistError::Task)?
}

async fn persist_service_snapshot(
    service: &HookService,
    state: &EngineState,
) -> Result<(), HookSnapshotPersistError> {
    #[cfg(test)]
    service
        .inner
        .snapshot_persist_count
        .fetch_add(1, Ordering::Relaxed);
    persist_engine_snapshot(&service.inner.store, state).await
}

fn reduce_decoded_durable_state(
    state: &mut EngineState,
    envelope: &RawEnvelope,
    payload: &DecodedCommittedPayload,
) -> Option<DecisionContext> {
    if let DecodedCommittedPayload::Hook(HookEventPayload::HookRunTrust { enabled }) = payload
        && let Some(run_id) = envelope.run_id.clone()
    {
        let key = (envelope.session_id.clone(), run_id);
        if *enabled {
            state.run_trust.insert(key.clone());
        } else {
            state.run_trust.remove(&key);
            state.terminal_run_trust.remove(&key);
        }
        return None;
    }
    if let DecodedCommittedPayload::Core(EventPayload::RunState(run_state)) = payload
        && run_state.is_terminal()
        && let Some(run_id) = envelope.run_id.clone()
    {
        let key = (envelope.session_id.clone(), run_id);
        if state.run_trust.contains(&key) {
            state.terminal_run_trust.insert(key);
        }
    }
    let DecodedCommittedPayload::Core(payload) = payload else {
        return None;
    };
    absorb_decision_fact(state, envelope, payload)
}

#[cfg(test)]
fn reduce_durable_state(
    state: &mut EngineState,
    envelope: &RawEnvelope,
) -> Option<DecisionContext> {
    let payload = decode_committed_payload(&envelope.payload);
    advance_durable_cursor(state, envelope);
    reduce_decoded_durable_state(state, envelope, &payload)
}

fn prune_terminal_run_trust(state: &mut EngineState) {
    for key in state.terminal_run_trust.drain() {
        state.run_trust.remove(&key);
    }
}

fn advance_durable_cursor(state: &mut EngineState, envelope: &RawEnvelope) {
    let through = state
        .through_seq
        .entry(envelope.session_id.clone())
        .or_default();
    if envelope.seq >= *through {
        *through = envelope.seq;
        state
            .through_digest
            .insert(envelope.session_id.clone(), hook_envelope_digest(envelope));
    }
}

fn hook_envelope_digest(envelope: &RawEnvelope) -> String {
    serde_json::to_vec(envelope)
        .map_or_else(
            |_| blake3::hash(b"haider-hook-snapshot-envelope-encode-error"),
            |bytes| blake3::hash(&bytes),
        )
        .to_hex()
        .to_string()
}

fn hook_boundary_event_id_digest(event_id: &EventId) -> String {
    format!("{HOOK_BOUNDARY_EVENT_ID_PREFIX}{}", event_id.as_str())
}

fn hook_cursor_matches(envelope: &RawEnvelope, digest: &str) -> bool {
    digest
        .strip_prefix(HOOK_BOUNDARY_EVENT_ID_PREFIX)
        .map_or_else(
            || digest == hook_envelope_digest(envelope),
            |event_id| event_id == envelope.event_id.as_str(),
        )
}

struct SnapshotSchedule {
    dirty: bool,
    last_commit: Option<Instant>,
    last_attempt: Instant,
}

impl SnapshotSchedule {
    fn new(now: Instant) -> Self {
        Self {
            dirty: false,
            last_commit: None,
            last_attempt: now,
        }
    }

    fn note_commit(&mut self, now: Instant) {
        if !self.dirty {
            // A fresh busy window starts at its first commit, not at an old persist.
            self.last_attempt = now;
        }
        self.dirty = true;
        self.last_commit = Some(now);
    }

    fn deadline(&self) -> Option<Instant> {
        if !self.dirty {
            return None;
        }
        let busy = checked_deadline(self.last_attempt, HOOK_SNAPSHOT_BUSY_INTERVAL);
        let Some(last_commit) = self.last_commit else {
            return Some(busy);
        };
        if self.last_attempt > last_commit {
            return Some(busy);
        }
        let idle = checked_deadline(last_commit, HOOK_SNAPSHOT_IDLE_DELAY);
        Some(idle.min(busy))
    }

    fn note_attempt(&mut self, now: Instant, succeeded: bool) {
        self.last_attempt = now;
        if succeeded {
            self.dirty = false;
        }
    }
}

fn checked_deadline(start: Instant, delay: Duration) -> Instant {
    start.checked_add(delay).unwrap_or(start)
}

async fn wait_for_snapshot_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn attempt_scheduled_snapshot(
    service: &HookService,
    state: &EngineState,
    schedule: &mut SnapshotSchedule,
) -> Result<(), HookSnapshotPersistError> {
    let result = persist_service_snapshot(service, state).await;
    schedule.note_attempt(Instant::now(), result.is_ok());
    result
}

async fn run_engine(
    mut controls: mpsc::Receiver<QueuedEngineControl>,
    mut committed_wake: watch::Receiver<Option<DurableHeadWake>>,
    service: HookService,
    mut state: EngineState,
) {
    let mut jobs = JoinSet::new();
    let inflight_dispatches = Arc::new(Mutex::new(HookDispatchFlights::default()));
    if !replay_pending_dispatches(&service, &mut state, &mut jobs, &inflight_dispatches).await {
        state.subscribers.clear();
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
        return;
    }
    prune_terminal_run_trust(&mut state);
    let mut snapshot_schedule = SnapshotSchedule::new(Instant::now());
    let mut blocked_run_acks = HashSet::new();
    loop {
        let snapshot_deadline = snapshot_schedule.deadline();
        tokio::select! {
            biased;
            changed = committed_wake.changed() => {
                if changed.is_err() {
                    continue;
                }
                let Some(wake) = committed_wake.borrow_and_update().clone() else {
                    continue;
                };
                tracing::trace!(
                    target: "haider.hooks",
                    session_id = %wake.session_id,
                    head_seq = wake.head_seq,
                    "draining durable hook outbox wake"
                );
                if matches!(
                    drain_hook_dispatch_page(
                        &service,
                        &mut state,
                        &mut jobs,
                        &mut snapshot_schedule,
                        &mut blocked_run_acks,
                        &inflight_dispatches,
                    ).await,
                    HookDrainPage::Progress
                ) && controls.is_empty()
                {
                    // An extra empty read is intentional: only the durable
                    // outbox can prove this page reached its current tail when
                    // count and byte ceilings may stop at different rows. A
                    // queued control gets one turn first and re-arms the wake.
                    service.inner.committed_wake.send_modify(|_| {});
                }
            }
            control = controls.recv() => {
                let Some(control) = control else {
                    if let Err(error) = attempt_scheduled_snapshot(
                        &service,
                        &state,
                        &mut snapshot_schedule,
                    ).await {
                        tracing::warn!(target: "haider.hooks", %error, "hook-engine snapshot persistence failed during drain; journal rebuild remains authoritative");
                    }
                    break;
                };
                match control.message {
                    EngineMessage::DrainSession {
                        session_id,
                        completed,
                    } => {
                        let result = drain_hook_dispatches_through_session(
                            &service,
                            &mut state,
                            &mut jobs,
                            &mut snapshot_schedule,
                            &mut blocked_run_acks,
                            &inflight_dispatches,
                            &session_id,
                        ).await;
                        let _ = completed.send(result);
                        service.inner.committed_wake.send_modify(|_| {});
                    }
                    EngineMessage::TrustChanged(change) => {
                        if !change.trusted {
                            service.inner.servers.kill_digest(&change.digest);
                            state.subscribers.retain(|_, handle| handle.digest != change.digest);
                        }
                        service.inner.committed_wake.send_modify(|_| {});
                    }
                    EngineMessage::SessionDeleted(session_id) => {
                        state.sessions.remove(&session_id);
                        state.through_seq.remove(&session_id);
                        state.through_digest.remove(&session_id);
                        state
                            .run_trust
                            .retain(|(candidate, _)| candidate != &session_id);
                        state
                            .terminal_run_trust
                            .retain(|(candidate, _)| candidate != &session_id);
                        blocked_run_acks.retain(|(candidate, _)| candidate != &session_id);
                        state.subscribers.retain(|_, handle| {
                            handle
                                .run_scope
                                .as_ref()
                                .is_none_or(|(candidate, _)| candidate != &session_id)
                        });
                        if let Err(error) = attempt_scheduled_snapshot(
                            &service,
                            &state,
                            &mut snapshot_schedule,
                        ).await
                        {
                            tracing::warn!(target: "haider.hooks", %error, "hook-engine snapshot persistence failed after session deletion");
                        }
                        service.inner.committed_wake.send_modify(|_| {});
                    }
                    EngineMessage::Shutdown(done) => {
                        while matches!(
                            drain_hook_dispatch_page(
                                &service,
                                &mut state,
                                &mut jobs,
                                &mut snapshot_schedule,
                                &mut blocked_run_acks,
                                &inflight_dispatches,
                            ).await,
                            HookDrainPage::Progress
                        ) {}
                        state.subscribers.clear();
                        jobs.abort_all();
                        while jobs.join_next().await.is_some() {}
                        if let Err(error) = attempt_scheduled_snapshot(
                            &service,
                            &state,
                            &mut snapshot_schedule,
                        ).await {
                            tracing::warn!(target: "haider.hooks", %error, "hook-engine snapshot persistence failed during shutdown; journal rebuild remains authoritative");
                        }
                        let _ = done.send(());
                        break;
                    }
                }
            }
            _ = wait_for_snapshot_deadline(snapshot_deadline) => {
                if let Err(error) = attempt_scheduled_snapshot(
                    &service,
                    &state,
                    &mut snapshot_schedule,
                ).await {
                    tracing::warn!(target: "haider.hooks", %error, "hook-engine idle snapshot persistence failed; journal rebuild remains authoritative");
                }
            }
            _ = jobs.join_next(), if !jobs.is_empty() => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HookDrainPage {
    Empty,
    Progress,
    Waiting,
    Blocked,
}

async fn drain_hook_dispatch_page(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
    snapshot_schedule: &mut SnapshotSchedule,
    blocked_run_acks: &mut HashSet<(SessionId, RunId)>,
    inflight_dispatches: &InflightHookDispatches,
) -> HookDrainPage {
    // A completed coordinate from an earlier page is safe to forget before a
    // fresh database read: its ACK committed before it entered this set. Any
    // job completing after this point remains visible through the whole page
    // and suppresses a stale decoded copy of the just-acknowledged row.
    inflight_dispatches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .completed
        .clear();
    let pending = match service
        .inner
        .store
        .pending_hook_dispatches_bounded(HOOK_DRAIN_PAGE_MAX_REQUESTS, HOOK_DRAIN_PAGE_MAX_BYTES)
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(target: "haider.hooks", ?error, "hook recovery outbox read failed");
            return HookDrainPage::Blocked;
        }
    };
    if pending.is_empty() {
        return HookDrainPage::Empty;
    }
    snapshot_schedule.note_commit(Instant::now());
    let mut acks = Vec::with_capacity(pending.len());
    let mut ordered_ack_scopes = HashSet::new();
    let mut terminal_trust_acks = HashSet::new();
    let mut terminal_snapshot = false;
    let mut aborted = false;
    let mut started_dispatch = false;
    let mut waiting_on_inflight = false;
    let mut blocked_inflight = false;
    let mut batch_discoveries = BatchDiscoveryCache::new();
    for envelope in pending {
        let coordinate = (envelope.session_id.clone(), envelope.seq);
        let (blocked, completed, active) = {
            let flight = inflight_dispatches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                flight.blocked.contains(&coordinate),
                flight.completed.contains(&coordinate),
                flight.active.contains(&coordinate),
            )
        };
        if blocked {
            blocked_inflight = true;
            continue;
        }
        if completed {
            continue;
        }
        if active {
            waiting_on_inflight = true;
            continue;
        }
        started_dispatch = true;
        let ordered_run_scope = envelope.run_id.as_ref().and_then(|run_id| {
            let scope = (envelope.session_id.clone(), run_id.clone());
            state.run_trust.contains(&scope).then_some(scope)
        });
        let ack_count = acks.len();
        let outcome = handle_and_complete(
            HookHandleContext {
                service,
                state,
                jobs,
                acks: &mut acks,
                batch_discoveries: &mut batch_discoveries,
                inflight_dispatches,
            },
            envelope,
            ordered_run_scope.is_none(),
        )
        .await;
        terminal_snapshot |= outcome.terminal_scope.is_some();
        if !outcome.completed {
            if let Some(scope) = ordered_run_scope {
                blocked_run_acks.insert(scope);
            }
            aborted = true;
            break;
        }
        if acks.len() > ack_count
            && let Some(scope) = &ordered_run_scope
        {
            ordered_ack_scopes.insert(scope.clone());
        }
        if acks.len() > ack_count
            && let Some(scope) = outcome.terminal_scope
            && state.terminal_run_trust.contains(&scope)
            && !blocked_run_acks.contains(&scope)
        {
            terminal_trust_acks.insert(scope);
        }
    }
    let acknowledgements_flushed = flush_hook_dispatch_acks(service, acks).await;
    if acknowledgements_flushed {
        for scope in terminal_trust_acks {
            state.terminal_run_trust.remove(&scope);
            state.run_trust.remove(&scope);
        }
    } else {
        blocked_run_acks.extend(ordered_ack_scopes);
    }
    let cadence_due = snapshot_schedule
        .deadline()
        .is_some_and(|deadline| deadline <= Instant::now());
    if (terminal_snapshot || cadence_due)
        && let Err(error) = attempt_scheduled_snapshot(service, state, snapshot_schedule).await
    {
        tracing::warn!(target: "haider.hooks", %error, "hook-engine scheduled snapshot persistence failed; journal rebuild remains authoritative");
    }
    if aborted || !acknowledgements_flushed || (!started_dispatch && blocked_inflight) {
        HookDrainPage::Blocked
    } else if !started_dispatch && waiting_on_inflight {
        HookDrainPage::Waiting
    } else {
        HookDrainPage::Progress
    }
}

async fn drain_hook_dispatches_through_session(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
    snapshot_schedule: &mut SnapshotSchedule,
    blocked_run_acks: &mut HashSet<(SessionId, RunId)>,
    inflight_dispatches: &InflightHookDispatches,
    session_id: &SessionId,
) -> Result<(), HaiderError> {
    loop {
        let progress = service.inner.dispatch_progress.notified();
        match service
            .inner
            .store
            .has_pending_hook_dispatches(session_id)
            .await
        {
            Ok(false) => return Ok(()),
            Err(error) => return Err(error),
            Ok(true) => {}
        }
        match drain_hook_dispatch_page(
            service,
            state,
            jobs,
            snapshot_schedule,
            blocked_run_acks,
            inflight_dispatches,
        )
        .await
        {
            HookDrainPage::Progress => {}
            HookDrainPage::Waiting => progress.await,
            HookDrainPage::Empty | HookDrainPage::Blocked => {
                if !service
                    .inner
                    .store
                    .has_pending_hook_dispatches(session_id)
                    .await?
                {
                    return Ok(());
                }
                return Err(HaiderError::new(
                    ErrorCode::Busy,
                    "hook dispatch remains pending; retry session deletion",
                    true,
                ));
            }
        }
    }
}

fn terminal_run_scope(
    envelope: &RawEnvelope,
    payload: &DecodedCommittedPayload,
) -> Option<(SessionId, RunId)> {
    let terminal = matches!(
        payload,
        DecodedCommittedPayload::Core(EventPayload::RunState(state)) if state.is_terminal()
    );
    if !terminal {
        return None;
    }
    Some((envelope.session_id.clone(), envelope.run_id.clone()?))
}

/// Acknowledges one drain cycle's handled hook-dispatch rows in a single
/// durable transaction. On failure the rows stay in the outbox and replay
/// at-least-once on the next engine start.
async fn flush_hook_dispatch_acks(service: &HookService, acks: Vec<(SessionId, u64)>) -> bool {
    if acks.is_empty() {
        return true;
    }
    if let Err(error) = service.inner.store.complete_hook_dispatches(acks).await {
        tracing::warn!(target: "haider.hooks", ?error, "hook recovery outbox acknowledgement failed");
        return false;
    }
    true
}

async fn replay_pending_dispatches(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
    inflight_dispatches: &InflightHookDispatches,
) -> bool {
    loop {
        let pending = match service
            .inner
            .store
            .pending_hook_dispatches_bounded(
                HOOK_DRAIN_PAGE_MAX_REQUESTS,
                HOOK_DRAIN_PAGE_MAX_BYTES,
            )
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook recovery outbox read failed");
                return false;
            }
        };
        if pending.is_empty() {
            return true;
        }
        // Per-page ack batch: one durable transaction per replay page. The
        // flush must land before the next `pending_hook_dispatches` read or
        // the same rows would be returned forever.
        let mut acks = Vec::with_capacity(pending.len());
        let mut batch_discoveries = BatchDiscoveryCache::new();
        for envelope in pending {
            if !handle_and_complete(
                HookHandleContext {
                    service,
                    state,
                    jobs,
                    acks: &mut acks,
                    batch_discoveries: &mut batch_discoveries,
                    inflight_dispatches,
                },
                envelope,
                false,
            )
            .await
            .completed
            {
                // Rows handled before the failure are still acknowledged so
                // a restart replays exactly the unhandled remainder.
                let _ = flush_hook_dispatch_acks(service, acks).await;
                return false;
            }
        }
        if !flush_hook_dispatch_acks(service, acks).await {
            return false;
        }
    }
}

struct HandleOutcome {
    terminal_scope: Option<(SessionId, RunId)>,
    completed: bool,
}

struct HookHandleContext<'a> {
    service: &'a HookService,
    state: &'a mut EngineState,
    jobs: &'a mut JoinSet<()>,
    acks: &'a mut Vec<(SessionId, u64)>,
    batch_discoveries: &'a mut BatchDiscoveryCache,
    inflight_dispatches: &'a InflightHookDispatches,
}

async fn handle_and_complete(
    context: HookHandleContext<'_>,
    envelope: RawEnvelope,
    defer_servers: bool,
) -> HandleOutcome {
    let HookHandleContext {
        service,
        state,
        jobs,
        acks,
        batch_discoveries,
        inflight_dispatches,
    } = context;
    let payload = decode_committed_payload(&envelope.payload);
    let terminal_scope = terminal_run_scope(&envelope, &payload);
    let committed = DecodedCommittedEnvelope {
        envelope: envelope.clone(),
        payload,
    };
    advance_durable_cursor(state, &envelope);
    let mut pending = Vec::new();
    let mut terminal_server_scope = None;
    if !handle_committed(
        service,
        state,
        jobs,
        committed,
        &mut pending,
        &mut terminal_server_scope,
        batch_discoveries,
    )
    .await
    {
        return HandleOutcome {
            terminal_scope,
            completed: false,
        };
    }
    if defer_servers && !pending.is_empty() {
        let service = service.clone();
        let coordinate = (envelope.session_id.clone(), envelope.seq);
        inflight_dispatches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .insert(coordinate.clone());
        let inflight_dispatches = Arc::clone(inflight_dispatches);
        jobs.spawn(async move {
            let handled = complete_server_fires(&service, pending).await;
            if let Some(scope) = &terminal_server_scope {
                service.inner.servers.kill_scope(scope);
            }
            let acknowledged = if handled {
                match service
                    .inner
                    .store
                    .complete_hook_dispatch(&envelope.session_id, envelope.seq)
                    .await
                {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(target: "haider.hooks", ?error, "hook recovery outbox acknowledgement failed");
                        false
                    }
                }
            } else {
                false
            };
            let mut flights = inflight_dispatches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            flights.active.remove(&coordinate);
            if acknowledged {
                flights.completed.insert(coordinate);
            } else {
                flights.blocked.insert(coordinate);
            }
            drop(flights);
            service.inner.dispatch_progress.notify_one();
            if acknowledged {
                service.inner.committed_wake.send_modify(|_| {});
            }
        });
        return HandleOutcome {
            terminal_scope,
            completed: true,
        };
    }
    if !complete_server_fires(service, pending).await {
        return HandleOutcome {
            terminal_scope,
            completed: false,
        };
    }
    if let Some(scope) = terminal_server_scope {
        service.inner.servers.kill_scope(&scope);
    }
    // Delete-after-handled is preserved: the caller's cycle flush runs
    // strictly after this handler returns, in one durable transaction.
    acks.push((envelope.session_id, envelope.seq));
    HandleOutcome {
        terminal_scope,
        completed: true,
    }
}

async fn complete_server_fires(service: &HookService, pending: Vec<PendingServerFire>) -> bool {
    for fire in pending {
        if !fire.complete(service).await {
            return false;
        }
    }
    true
}

async fn handle_committed(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
    committed: DecodedCommittedEnvelope,
    pending_servers: &mut Vec<PendingServerFire>,
    terminal_server_scope: &mut Option<(SessionId, RunId)>,
    batch_discoveries: &mut BatchDiscoveryCache,
) -> bool {
    let DecodedCommittedEnvelope { envelope, payload } = committed;
    if HookEventPayload::is_engine_fact(&envelope.payload) {
        return true;
    }

    // A headless acceptance journals its fully resolved provider immediately
    // before the user-message fact in the same batch. Pin that actual model
    // provider first; session metadata can deliberately name another pair.
    if let Some(HeadlessRunEventPayload::HeadlessRunConfigured(spec)) =
        HeadlessRunEventPayload::from_payload_value(&envelope.payload)
    {
        let Some(run_id) = envelope.run_id.as_ref() else {
            tracing::warn!(target: "haider.hooks", "headless provider fact has no run id");
            return false;
        };
        let proposed = match service.inner.hub.provider_lockdown_policy(&spec.provider) {
            Ok(lockdown) => lockdown,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "headless provider trust lookup failed");
                return false;
            }
        };
        return match service.inner.hub.bind_lockdown_turn(
            &envelope.session_id,
            run_id,
            &spec.provider,
            proposed,
        ) {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "headless turn-ceiling bind failed");
                false
            }
        };
    }

    let decision = reduce_decoded_durable_state(state, &envelope, &payload);
    if let DecodedCommittedPayload::Hook(HookEventPayload::HookRunTrust { enabled }) = &payload {
        if !*enabled && let Some(run_id) = envelope.run_id.clone() {
            service
                .inner
                .servers
                .kill_scope(&(envelope.session_id.clone(), run_id));
        }
        return true;
    }
    let Some(facts) = classify_payload(&payload) else {
        return true;
    };
    let context = if let Some(context) = batch_discoveries.get(&envelope.session_id).cloned() {
        context
    } else {
        let metadata = match service
            .inner
            .store
            .session_metadata(&envelope.session_id)
            .await
        {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook metadata hydration failed");
                return false;
            }
        };
        let context = if let Some(metadata) = metadata {
            #[cfg(test)]
            service
                .inner
                .discovery_stamp_count
                .fetch_add(1, Ordering::Relaxed);
            let discovery = discover_cached_async(service, PathBuf::from(&metadata.cwd)).await;
            Some((metadata, discovery))
        } else {
            None
        };
        batch_discoveries.insert(envelope.session_id.clone(), context.clone());
        context
    };
    let Some((metadata, discovery)) = context else {
        return true;
    };
    let discovery = match discovery {
        Ok(discovery) => discovery,
        Err(reason) => {
            return journal_notice_once(
                service,
                state,
                &envelope,
                HookNotice {
                    hook: None,
                    digest: None,
                    source: metadata.cwd,
                    reason,
                },
            )
            .await;
        }
    };
    let terminal_scope = if facts.event == MatchEvent::RunFinished {
        envelope
            .run_id
            .clone()
            .map(|run_id| (envelope.session_id.clone(), run_id))
    } else {
        None
    };
    // Evaluate the complete matcher once before any hook input rendering or
    // process work. Discovery notices and live subscriber/server
    // reconciliation still run below: configuration removal and malformed
    // definitions are observable even when this event matches no hook.
    let bound = match envelope.run_id.as_ref() {
        Some(run_id) => match service
            .inner
            .hub
            .bound_lockdown_run(&envelope.session_id, run_id)
        {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook turn-ceiling lookup failed");
                return false;
            }
        },
        None => match service
            .inner
            .hub
            .bound_session_lockdown(&envelope.session_id)
        {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook session-ceiling lookup failed");
                return false;
            }
        },
    };
    let (provider, lockdown) = match bound {
        Some(bound) => bound,
        None => {
            let proposed = match service
                .inner
                .hub
                .provider_lockdown_policy(&metadata.provider)
            {
                Ok(lockdown) => lockdown,
                Err(error) => {
                    tracing::warn!(target: "haider.hooks", ?error, "hook provider trust lookup failed");
                    return false;
                }
            };
            let lockdown = match envelope.run_id.as_ref() {
                Some(run_id) => match service.inner.hub.bind_lockdown_turn(
                    &envelope.session_id,
                    run_id,
                    &metadata.provider,
                    proposed,
                ) {
                    Ok(lockdown) => lockdown,
                    Err(error) => {
                        tracing::warn!(target: "haider.hooks", ?error, "hook turn-ceiling bind failed");
                        return false;
                    }
                },
                None => proposed,
            };
            (metadata.provider.clone(), lockdown)
        }
    };
    let any_match = discovery
        .hooks
        .values()
        .any(|definition| definition.matcher.matches(&envelope, &provider, &facts));
    if lockdown {
        let workspace = Path::new(&metadata.cwd);
        state
            .subscribers
            .retain(|_, handle| handle.workspace_cwd != workspace);
        service.inner.servers.reconcile_workspace(
            workspace,
            &HashSet::new(),
            &HashSet::new(),
            Some(&HashSet::new()),
        );
        if let Some(terminal_scope) = terminal_scope {
            state
                .subscribers
                .retain(|_, handle| handle.run_scope.as_ref() != Some(&terminal_scope));
            service.inner.servers.kill_scope(&terminal_scope);
        }
        if !any_match {
            return true;
        }
        return journal_lockdown_refusal_once(
            service,
            state,
            &envelope,
            &provider,
            "automatic hooks cannot execute for a lockdown provider",
        )
        .await;
    }
    service.prepare_workspace_trust(&discovery).await;
    let current_servers = discovery
        .hooks
        .values()
        .filter(|definition| matches!(definition.kind, HookKind::Server { .. }))
        .map(HookDefinition::subscriber_key)
        .collect::<HashSet<_>>();
    let trusted_servers = discovery
        .hooks
        .values()
        .filter(|definition| {
            matches!(definition.kind, HookKind::Server { .. })
                && service.is_trusted(definition, false, discovery.policy)
        })
        .map(HookDefinition::subscriber_key)
        .collect::<HashSet<_>>();
    service.inner.servers.reconcile_workspace(
        Path::new(&metadata.cwd),
        &current_servers,
        &trusted_servers,
        Some(&state.run_trust),
    );
    for notice in discovery.notices {
        if !journal_notice_once(service, state, &envelope, notice).await {
            return false;
        }
    }

    let run_override = envelope.run_id.as_ref().is_some_and(|run_id| {
        state
            .run_trust
            .contains(&(envelope.session_id.clone(), run_id.clone()))
    });
    let current_subscribers = discovery
        .hooks
        .values()
        .filter(|definition| definition.kind == HookKind::Subscribe)
        .map(|definition| (definition.subscriber_key(), definition))
        .collect::<HashMap<_, _>>();
    state.subscribers.retain(|_, handle| {
        if handle.workspace_cwd != Path::new(&metadata.cwd) {
            return true;
        }
        let Some(current) = current_subscribers.get(&handle.definition_key) else {
            return false;
        };
        service.is_trusted(current, false, discovery.policy)
            || handle
                .run_scope
                .as_ref()
                .is_some_and(|scope| state.run_trust.contains(scope))
    });

    if !any_match {
        if let Some(terminal_scope) = terminal_scope {
            state
                .subscribers
                .retain(|_, handle| handle.run_scope.as_ref() != Some(&terminal_scope));
            service.inner.servers.kill_scope(&terminal_scope);
        }
        return true;
    }

    let mut prepared_input = None::<Result<Arc<[u8]>, String>>;
    for definition in discovery.hooks.into_values() {
        if !definition.matcher.matches(&envelope, &provider, &facts) {
            continue;
        }
        let profile_trusted = service.is_trusted(&definition, false, discovery.policy);
        if !profile_trusted && !run_override {
            if !journal_notice_once(
                service,
                state,
                &envelope,
                HookNotice {
                    hook: Some(definition.name.clone()),
                    digest: Some(definition.digest.clone()),
                    source: definition.source_path.display().to_string(),
                    reason: "hook is untrusted and was not executed".into(),
                },
            )
            .await
            {
                return false;
            }
            continue;
        }
        if definition.decision {
            let Some(decision) = decision.clone() else {
                continue;
            };
            if matches!(definition.kind, HookKind::Server { .. }) {
                let input = match serde_json::to_vec(&json!({
                    "schema": "haider.hook.decision.v1",
                    "envelope": &envelope,
                    "effect": &decision.intent,
                    "menu": &decision.permission.menu,
                })) {
                    Ok(input) => Arc::from(input),
                    Err(error) => {
                        tracing::warn!(target: "haider.hooks", ?error, "decision input serialization failed");
                        return false;
                    }
                };
                let run_scope = server_run_scope(profile_trusted, &envelope);
                pending_servers.push(queue_server_fire(
                    service,
                    definition,
                    envelope.clone(),
                    input,
                    run_scope,
                    Some(decision),
                ));
            } else if !fire_decision(
                service.clone(),
                definition,
                envelope.clone(),
                decision,
                run_override,
            )
            .await
            {
                return false;
            }
            continue;
        }
        if prepared_input.is_none() {
            prepared_input = Some(
                prepare_hook_input(&service.inner.store, &envelope)
                    .await
                    .map(Arc::from),
            );
        }
        let input = match prepared_input.as_ref() {
            Some(Ok(input)) => Arc::clone(input),
            Some(Err(reason)) => {
                if !journal_notice_once(
                    service,
                    state,
                    &envelope,
                    HookNotice {
                        hook: Some(definition.name.clone()),
                        digest: Some(definition.digest.clone()),
                        source: definition.source_path.display().to_string(),
                        reason: reason.clone(),
                    },
                )
                .await
                {
                    return false;
                }
                continue;
            }
            None => continue,
        };
        match definition.kind {
            HookKind::Exec => {
                if !fire_exec(
                    service.clone(),
                    definition,
                    envelope.clone(),
                    &input,
                    run_override,
                )
                .await
                {
                    return false;
                }
            }
            HookKind::Subscribe => {
                let run_scope = (!profile_trusted)
                    .then(|| {
                        envelope
                            .run_id
                            .clone()
                            .map(|run_id| (envelope.session_id.clone(), run_id))
                    })
                    .flatten();
                if !deliver_subscriber(
                    service,
                    state,
                    jobs,
                    definition,
                    envelope.clone(),
                    input,
                    run_scope,
                )
                .await
                {
                    return false;
                }
            }
            HookKind::Server { .. } => {
                let run_scope = server_run_scope(profile_trusted, &envelope);
                pending_servers.push(queue_server_fire(
                    service,
                    definition,
                    envelope.clone(),
                    input,
                    run_scope,
                    None,
                ));
            }
        }
    }
    if let Some(terminal_scope) = terminal_scope {
        state
            .subscribers
            .retain(|_, handle| handle.run_scope.as_ref() != Some(&terminal_scope));
        *terminal_server_scope = Some(terminal_scope);
    }
    true
}

fn server_run_scope(profile_trusted: bool, envelope: &RawEnvelope) -> Option<(SessionId, RunId)> {
    (!profile_trusted)
        .then(|| {
            envelope
                .run_id
                .clone()
                .map(|run_id| (envelope.session_id.clone(), run_id))
        })
        .flatten()
}

fn queue_server_fire(
    service: &HookService,
    definition: HookDefinition,
    cause: RawEnvelope,
    input: Arc<[u8]>,
    run_scope: Option<(SessionId, RunId)>,
    decision: Option<DecisionContext>,
) -> PendingServerFire {
    let response = match service.inner.servers.try_dispatch(
        service.clone(),
        definition.clone(),
        input,
        run_scope,
    ) {
        Ok(wait) => PendingServerResponse::Waiting(wait),
        Err(error) => PendingServerResponse::Ready(ServerReply::Result(error.process_result())),
    };
    PendingServerFire {
        definition,
        cause,
        decision,
        response,
    }
}

async fn deliver_subscriber(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
    definition: HookDefinition,
    envelope: RawEnvelope,
    input: Arc<[u8]>,
    run_scope: Option<(SessionId, RunId)>,
) -> bool {
    let definition_key = definition.subscriber_key();
    let key = match &run_scope {
        Some((session_id, run_id)) => {
            format!("{definition_key}\0run\0{session_id}\0{run_id}")
        }
        None => format!("{definition_key}\0profile"),
    };
    let mut message = SubscriberMessage {
        input,
        delivered: oneshot::channel().0,
    };
    loop {
        if !state.subscribers.contains_key(&key) {
            let (sender, receiver) = mpsc::channel(SUBSCRIBE_QUEUE);
            state.subscribers.insert(
                key.clone(),
                SubscriberHandle {
                    sender,
                    definition_key: definition_key.clone(),
                    digest: definition.digest.clone(),
                    workspace_cwd: definition.workspace_cwd.clone(),
                    run_scope: run_scope.clone(),
                },
            );
            let service = service.clone();
            let definition = definition.clone();
            let cause = envelope.clone();
            let run_override = run_scope.is_some();
            jobs.spawn(async move {
                run_subscriber(service, definition, cause, receiver, run_override).await;
            });
        }
        let Some(handle) = state.subscribers.get(&key) else {
            continue;
        };
        let (delivered, wait) = oneshot::channel();
        message.delivered = delivered;
        match handle.sender.send(message).await {
            Ok(()) => {
                let mut shutdown = service.inner.shutdown.subscribe();
                return tokio::select! {
                    delivered = wait => delivered.is_ok(),
                    _ = shutdown.changed() => false,
                };
            }
            Err(error) => {
                message = error.0;
                state.subscribers.remove(&key);
            }
        }
    }
}

async fn journal_notice_once(
    service: &HookService,
    state: &mut EngineState,
    cause: &RawEnvelope,
    notice: HookNotice,
) -> bool {
    let key = format!(
        "{}\0{}\0{}\0{}\0{}",
        cause.session_id,
        notice.source,
        notice.hook.as_deref().unwrap_or_default(),
        notice.digest.as_deref().unwrap_or_default(),
        notice.reason
    );
    if state.notice_dedup.contains(&key) {
        return true;
    }
    if service
        .journal(cause, HookEventPayload::HookNotice(notice))
        .await
    {
        state.notice_dedup.insert(key);
        true
    } else {
        false
    }
}

async fn journal_lockdown_refusal_once(
    service: &HookService,
    state: &mut EngineState,
    cause: &RawEnvelope,
    provider: &str,
    reason: &str,
) -> bool {
    let key = format!(
        "lockdown\0{}\0{}\0{}\0{}",
        cause.session_id, cause.event_id, provider, reason
    );
    if state.notice_dedup.contains(&key) {
        return true;
    }
    if service
        .journal_lockdown_refusal(cause, provider, reason)
        .await
    {
        state.notice_dedup.insert(key);
        true
    } else {
        false
    }
}

fn absorb_decision_fact(
    state: &mut EngineState,
    envelope: &RawEnvelope,
    payload: &EventPayload,
) -> Option<DecisionContext> {
    let session = state
        .sessions
        .entry(envelope.session_id.clone())
        .or_default();
    match payload {
        EventPayload::Effect(EffectPhase::Intent(intent)) => {
            session
                .intents
                .insert(intent.effect.clone(), intent.clone());
        }
        EventPayload::Effect(EffectPhase::Authorized {
            effect,
            verdict: AuthorizationVerdict::Ask { menu },
        }) => {
            if let Some(intent) = session.intents.get(effect).cloned() {
                session.bindings.insert(menu.clone(), intent);
            }
        }
        EventPayload::Effect(EffectPhase::Authorized { effect, .. })
        | EventPayload::Effect(EffectPhase::Outcome { effect, .. }) => {
            session.intents.remove(effect);
            session
                .bindings
                .retain(|_, intent| &intent.effect != effect);
        }
        EventPayload::MenuOpened(menu) if matches!(&menu.kind, MenuKind::Permission { .. }) => {
            session.menus.insert(
                menu.id.clone(),
                OpenPermission {
                    menu: menu.clone(),
                    opening: envelope.clone(),
                },
            );
        }
        EventPayload::RunState(RunState::PermissionRequired { menu }) => {
            let intent = session.bindings.get(menu)?.clone();
            let permission = session.menus.get(menu)?.clone();
            return Some(DecisionContext { intent, permission });
        }
        EventPayload::MenuAnswered(answer) => {
            let menu = &answer.menu;
            if let Some(intent) = session.bindings.remove(menu) {
                session.intents.remove(&intent.effect);
            }
            session.menus.remove(menu);
        }
        EventPayload::MenuClosed { menu, .. } => {
            if let Some(intent) = session.bindings.remove(menu) {
                session.intents.remove(&intent.effect);
            }
            session.menus.remove(menu);
        }
        _ => {}
    }
    None
}

#[derive(Debug, Clone)]
struct MatchFacts {
    event: MatchEvent,
    outcome: Option<&'static str>,
    parked_kind: Option<&'static str>,
    provider: Option<String>,
    mode: Option<DeliveryMode>,
    has_attachments: Option<bool>,
}

enum DecodedCommittedPayload {
    Core(EventPayload),
    Hook(HookEventPayload),
    Unknown,
}

struct DecodedCommittedEnvelope {
    envelope: RawEnvelope,
    payload: DecodedCommittedPayload,
}

fn decode_committed_payload(payload: &serde_json::Value) -> DecodedCommittedPayload {
    let kind = payload.get("type").and_then(serde_json::Value::as_str);
    if matches!(
        kind,
        Some(
            "hook_notice"
                | "hook_fired"
                | "hook_subscription"
                | "update_available"
                | "account_expired"
                | "hook_run_trust"
                | "hook_trust_changed"
        )
    ) {
        return HookEventPayload::from_payload_value(payload.clone()).map_or(
            DecodedCommittedPayload::Unknown,
            DecodedCommittedPayload::Hook,
        );
    }
    serde_json::from_value(payload.clone()).map_or(
        DecodedCommittedPayload::Unknown,
        DecodedCommittedPayload::Core,
    )
}

#[cfg(test)]
fn classify(envelope: &RawEnvelope) -> Option<MatchFacts> {
    classify_payload(&decode_committed_payload(&envelope.payload))
}

fn classify_payload(payload: &DecodedCommittedPayload) -> Option<MatchFacts> {
    match payload {
        DecodedCommittedPayload::Core(payload) => {
            let facts = match payload {
                EventPayload::SessionState(SessionState::Created) => MatchFacts {
                    event: MatchEvent::SessionCreated,
                    outcome: None,
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::UserMessage {
                    attachments, mode, ..
                } => MatchFacts {
                    event: MatchEvent::UserMessage,
                    outcome: None,
                    parked_kind: None,
                    provider: None,
                    mode: Some(*mode),
                    has_attachments: Some(!attachments.is_empty()),
                },
                EventPayload::RunState(RunState::Thinking) => MatchFacts {
                    event: MatchEvent::RunStarted,
                    outcome: None,
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::RunState(RunState::PermissionRequired { .. }) => MatchFacts {
                    event: MatchEvent::RunParked,
                    outcome: None,
                    parked_kind: Some("permission"),
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::RunState(RunState::InputRequired { .. }) => MatchFacts {
                    event: MatchEvent::RunParked,
                    outcome: None,
                    parked_kind: Some("input"),
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::RunState(RunState::Done) => MatchFacts {
                    event: MatchEvent::RunFinished,
                    outcome: Some("done"),
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::RunState(RunState::Errored) => MatchFacts {
                    event: MatchEvent::RunFinished,
                    outcome: Some("errored"),
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::RunState(RunState::Cancelled) => MatchFacts {
                    event: MatchEvent::RunFinished,
                    outcome: Some("cancelled"),
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::AgentSpawned(_) => MatchFacts {
                    event: MatchEvent::SubagentSpawned,
                    outcome: None,
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::AgentReport(_) => MatchFacts {
                    event: MatchEvent::SubagentReported,
                    outcome: None,
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ContextCompaction { .. },
                    ..
                }) => MatchFacts {
                    event: MatchEvent::CompactionCompleted,
                    outcome: None,
                    parked_kind: None,
                    provider: None,
                    mode: None,
                    has_attachments: None,
                },
                _ => return None,
            };
            Some(facts)
        }
        DecodedCommittedPayload::Hook(HookEventPayload::UpdateAvailable { .. }) => {
            Some(MatchFacts {
                event: MatchEvent::UpdateAvailable,
                outcome: None,
                parked_kind: None,
                provider: None,
                mode: None,
                has_attachments: None,
            })
        }
        DecodedCommittedPayload::Hook(HookEventPayload::AccountExpired { provider, .. }) => {
            Some(MatchFacts {
                event: MatchEvent::AccountExpired,
                outcome: None,
                parked_kind: None,
                provider: Some(provider.clone()),
                mode: None,
                has_attachments: None,
            })
        }
        DecodedCommittedPayload::Hook(_) | DecodedCommittedPayload::Unknown => None,
    }
}

impl HookMatcher {
    fn matches(&self, envelope: &RawEnvelope, provider: &str, facts: &MatchFacts) -> bool {
        let provider = facts.provider.as_deref().unwrap_or(provider);
        self.event == facts.event
            && self
                .session
                .as_deref()
                .is_none_or(|session| session == envelope.session_id.as_str())
            && self
                .provider
                .as_deref()
                .is_none_or(|expected| expected == provider)
            && self
                .outcome
                .as_deref()
                .is_none_or(|expected| facts.outcome == Some(expected))
            && self
                .parked_kind
                .as_deref()
                .is_none_or(|expected| facts.parked_kind == Some(expected))
            && self
                .mode
                .is_none_or(|expected| facts.mode == Some(expected))
            && self
                .has_attachments
                .is_none_or(|expected| facts.has_attachments == Some(expected))
    }
}

async fn prepare_hook_input(
    store: &haider_core::SqliteStoreHandle,
    envelope: &RawEnvelope,
) -> Result<Vec<u8>, String> {
    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
        return serde_json::to_vec(envelope)
            .map_err(|error| format!("hook event JSON serialization failed: {error}"));
    };
    let EventPayload::UserMessage {
        text,
        attachments,
        mode,
    } = payload
    else {
        return serde_json::to_vec(envelope)
            .map_err(|error| format!("hook event JSON serialization failed: {error}"));
    };
    let run = envelope.run_id.clone().ok_or_else(|| {
        "user_message hook input was skipped: committed fact has no run id".to_owned()
    })?;
    let (text, truncated) = bounded_user_message_text(&text);
    let mut items = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let (artifact, mime) = match attachment {
            haider_protocol::tool::AttachmentBlock::Image { artifact, mime, .. } => {
                (artifact, mime)
            }
            haider_protocol::tool::AttachmentBlock::PastedText { artifact, .. } => {
                (artifact, "text/plain".to_owned())
            }
            haider_protocol::tool::AttachmentBlock::File { artifact, .. } => {
                (artifact, "text/plain".to_owned())
            }
            haider_protocol::tool::AttachmentBlock::Pdf { artifact, .. } => {
                (artifact, "application/pdf".to_owned())
            }
            haider_protocol::tool::AttachmentBlock::Skill { name, .. } => {
                return Err(format!(
                    "user_message hook input was skipped: skill attachment `{name}` has no artifact metadata"
                ));
            }
        };
        let bytes = store.get(&artifact).await.map_err(|error| {
            format!(
                "user_message hook input was skipped: attachment {artifact} metadata is unavailable: {error:?}"
            )
        })?;
        items.push(HookAttachmentMetadata {
            mime,
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            artifact,
        });
    }
    let count = u32::try_from(items.len()).unwrap_or(u32::MAX);
    serde_json::to_vec(&HookInput::UserMessage {
        session: envelope.session_id.clone(),
        run,
        branch: envelope.branch_id.clone(),
        mode,
        text,
        truncated,
        attachments: HookAttachmentSet { count, items },
    })
    .map_err(|error| format!("user_message hook input serialization failed: {error}"))
}

fn bounded_user_message_text(text: &str) -> (String, bool) {
    if text.len() <= MAX_USER_MESSAGE_TEXT_BYTES {
        return (text.to_owned(), false);
    }
    let mut end = MAX_USER_MESSAGE_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

async fn fire_exec(
    service: HookService,
    definition: HookDefinition,
    cause: RawEnvelope,
    input: &[u8],
    run_override: bool,
) -> bool {
    if !definition_current(&service, &definition, run_override).await {
        return service
            .journal(
                &cause,
                HookEventPayload::HookNotice(HookNotice {
                    hook: Some(definition.name),
                    digest: Some(definition.digest),
                    source: definition.source_path.display().to_string(),
                    reason: "hook digest or trust changed before spawn".into(),
                }),
            )
            .await;
    }
    let result = run_command(
        &definition,
        input,
        &service.inner.store,
        service.inner.shutdown.subscribe(),
    )
    .await;
    if result.cancelled {
        return false;
    }
    service
        .journal(
            &cause,
            HookEventPayload::HookFired(HookFired {
                hook: definition.name,
                digest: definition.digest,
                kind: HookRuntimeKind::Exec,
                observed_seq: cause.seq,
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                stdout: result.stdout,
                stderr: result.stderr,
                proposed_decision: None,
                menu_id: None,
                decision_applied: false,
            }),
        )
        .await
}

async fn fire_decision(
    service: HookService,
    definition: HookDefinition,
    cause: RawEnvelope,
    decision: DecisionContext,
    run_override: bool,
) -> bool {
    if !definition_current(&service, &definition, run_override).await {
        return service
            .journal(
                &cause,
                HookEventPayload::HookNotice(HookNotice {
                    hook: Some(definition.name),
                    digest: Some(definition.digest),
                    source: definition.source_path.display().to_string(),
                    reason: "decision hook digest or trust changed before spawn".into(),
                }),
            )
            .await;
    }
    let input = match serde_json::to_vec(&json!({
        "schema": "haider.hook.decision.v1",
        "envelope": &cause,
        "effect": &decision.intent,
        "menu": &decision.permission.menu,
    })) {
        Ok(input) => input,
        Err(error) => {
            tracing::warn!(target: "haider.hooks", ?error, "decision input serialization failed");
            return false;
        }
    };
    let mut result = run_command(
        &definition,
        &input,
        &service.inner.store,
        service.inner.shutdown.subscribe(),
    )
    .await;
    if result.cancelled {
        return false;
    }
    let proposed = strict_decision(&result);
    let applied = if let Some(proposed) = proposed {
        resolve_decision(&service, &definition, &decision, proposed)
            .await
            .unwrap_or(false)
    } else {
        false
    };
    result.proposed_decision = proposed;
    service
        .journal(
            &cause,
            HookEventPayload::HookFired(HookFired {
                hook: definition.name,
                digest: definition.digest,
                kind: HookRuntimeKind::Decision,
                observed_seq: cause.seq,
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                stdout: result.stdout,
                stderr: result.stderr,
                proposed_decision: result.proposed_decision,
                menu_id: Some(decision.permission.menu.id.clone()),
                decision_applied: applied,
            }),
        )
        .await
}

async fn resolve_decision(
    service: &HookService,
    definition: &HookDefinition,
    decision: &DecisionContext,
    proposed: HookDecisionKind,
) -> Result<bool, haider_protocol::error::HaiderError> {
    let wanted = match proposed {
        HookDecisionKind::Allow => DecisionKind::AllowOnce,
        HookDecisionKind::Deny => DecisionKind::RejectOnce,
    };
    let Some((index, option)) = decision
        .permission
        .menu
        .options
        .iter()
        .enumerate()
        .find(|(_, option)| option.decision == Some(wanted))
    else {
        return Ok(false);
    };
    let option_index = u32::try_from(index).map_err(|_| {
        haider_protocol::error::HaiderError::new(
            haider_protocol::error::ErrorCode::InvalidArgument,
            "permission menu option index does not fit u32",
            false,
        )
    })?;
    let mut hasher = blake3::Hasher::new();
    for part in [
        decision.permission.opening.session_id.as_str(),
        decision.permission.menu.id.as_str(),
        definition.name.as_str(),
        definition.digest.as_str(),
        match proposed {
            HookDecisionKind::Allow => "allow",
            HookDecisionKind::Deny => "deny",
        },
    ] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(&decision.permission.opening.seq.to_le_bytes());
    let command = MenuResolutionCommand {
        command_id: format!("hook-menu-{}", hasher.finalize().to_hex()),
        session_id: decision.permission.opening.session_id.clone(),
        request_seq: decision.permission.opening.seq,
        worker_generation: decision.permission.opening.worker_generation,
        allow_prior_generation: false,
        answer: MenuAnswer {
            menu: decision.permission.menu.id.clone(),
            option_key: Some(option.key.clone()),
            option_index,
            value: None,
            via: AnswerVia::Hook,
        },
        device_id: service.inner.hub.device_id(),
        input_is_secret_reference: false,
    };
    Ok(matches!(
        service.inner.hub.resolve_hook_menu(command).await?,
        MenuResolutionOutcome::Committed { .. } | MenuResolutionOutcome::IdempotentReplay { .. }
    ))
}

fn strict_decision(result: &HookProcessResult) -> Option<HookDecisionKind> {
    if result.timed_out || result.exit_code != Some(0) || result.stdout.truncated {
        return None;
    }
    match result.stdout.preview.trim() {
        "allow" => Some(HookDecisionKind::Allow),
        "deny" => Some(HookDecisionKind::Deny),
        _ => None,
    }
}

/// pub(crate) for the fire-time re-verification law tests.
async fn definition_current(
    service: &HookService,
    definition: &HookDefinition,
    run_override: bool,
) -> bool {
    let discovery = if definition.decision {
        // Security-relevant gates never accept cached discovery: the exact
        // current bytes and trust pin are revalidated at fire time.
        discover_async(
            definition.workspace_cwd.clone(),
            service.inner.profile_root.clone(),
        )
        .await
    } else {
        discover_cached_async(service, definition.workspace_cwd.clone()).await
    };
    let Ok(discovery) = discovery else {
        return false;
    };
    service.prepare_workspace_trust(&discovery).await;
    discovery
        .hooks
        .get(&definition.name)
        .is_some_and(|current| {
            current.digest == definition.digest
                && service.is_trusted(current, run_override, discovery.policy)
        })
}

struct HookProcessResult {
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    stdout: HookOutput,
    stderr: HookOutput,
    proposed_decision: Option<HookDecisionKind>,
}

async fn run_command(
    definition: &HookDefinition,
    input: &[u8],
    store: &haider_core::SqliteStoreHandle,
    mut shutdown: watch::Receiver<bool>,
) -> HookProcessResult {
    if *shutdown.borrow() {
        return cancelled_process_output();
    }
    let cwd_fd = open_canonical_directory(&definition.workspace_cwd);
    let Some(cwd_fd) = cwd_fd else {
        return failed_process_output("workspace cwd is no longer canonical");
    };
    #[cfg(unix)]
    let mut command = hook_command(&definition.command, std::env::var_os("SHELL"));
    #[cfg(windows)]
    let mut command = hook_command(&definition.command);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    haider_platform::configure_process_environment(&mut command);
    for name in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    haider_platform::configure_process_group(&mut command);
    #[cfg(unix)]
    configure_hook_cwd(&mut command, cwd_fd);
    #[cfg(windows)]
    configure_hook_cwd(&mut command, &cwd_fd);
    haider_platform::configure_background_process(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed_process_output(&format!("hook spawn failed: {error}")),
    };
    let Some(raw_pid) = child.id() else {
        let _ = child.start_kill();
        return failed_process_output("hook process did not expose a process id");
    };
    let pid = match haider_platform::register_process_group(raw_pid) {
        Ok(group) => Some(group),
        Err(error) => {
            let _ = child.start_kill();
            return failed_process_output(&format!(
                "hook process-group registration failed: {error}"
            ));
        }
    };
    let leader_pid = haider_platform::process_id(Some(raw_pid));
    let mut process_group = ProcessGroupGuard { pid };
    let mut stdin = child.stdin.take();
    let Some(stdout) = child.stdout.take() else {
        process_group.kill();
        let _ = child.start_kill();
        let reap = reap_hook_child(&mut child).await;
        report_hook_child_reap("spawn_stdout_unavailable", reap);
        return failed_process_output("hook stdout unavailable");
    };
    let Some(stderr) = child.stderr.take() else {
        process_group.kill();
        let _ = child.start_kill();
        let reap = reap_hook_child(&mut child).await;
        report_hook_child_reap("spawn_stderr_unavailable", reap);
        return failed_process_output("hook stderr unavailable");
    };
    let stdout_task = tokio::spawn(read_limited(stdout, MAX_STREAM_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(read_limited(stderr, MAX_STREAM_OUTPUT_BYTES));
    let leader = async {
        if let Some(mut stdin) = stdin.take() {
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
        }
        observe_hook_leader_exit(&mut child, leader_pid).await
    };
    let outcome = tokio::select! {
        outcome = tokio::time::timeout(definition.timeout, leader) => Some(outcome),
        _ = shutdown.changed() => None,
    };
    match outcome {
        None => {
            process_group.kill();
            let _ = child.start_kill();
            let reap = reap_hook_child(&mut child).await;
            report_hook_child_reap("shutdown", reap);
            drain_hook_capture(stdout_task, "shutdown_stdout").await;
            drain_hook_capture(stderr_task, "shutdown_stderr").await;
            cancelled_process_output()
        }
        Some(Ok(Ok(status))) => {
            // The leader is reaped, but a descendant can still own inherited
            // output handles. Terminate the Job before draining so natural
            // leader exit cannot park the hook until its wall timeout.
            process_group.kill();
            let status = match status {
                Some(status) => status,
                None => match reap_hook_child(&mut child).await {
                    HookChildReapOutcome::Exited(status) => status,
                    HookChildReapOutcome::WaitFailed(error) => {
                        drain_hook_capture(stdout_task, "reap_failure_stdout").await;
                        drain_hook_capture(stderr_task, "reap_failure_stderr").await;
                        return failed_process_output(&format!("hook leader reap failed: {error}"));
                    }
                    HookChildReapOutcome::TimedOut(timeout) => {
                        report_hook_child_reap(
                            "natural_exit",
                            HookChildReapOutcome::TimedOut(timeout),
                        );
                        drain_hook_capture(stdout_task, "reap_timeout_stdout").await;
                        drain_hook_capture(stderr_task, "reap_timeout_stderr").await;
                        return failed_process_output(&timeout.to_string());
                    }
                },
            };
            let stdout = await_hook_capture(stdout_task).await;
            let stderr = await_hook_capture(stderr_task).await;
            match (stdout, stderr) {
                (Ok(stdout), Ok(stderr)) => HookProcessResult {
                    exit_code: status.code(),
                    timed_out: false,
                    cancelled: false,
                    stdout: make_output(store, stdout).await,
                    stderr: make_output(store, stderr).await,
                    proposed_decision: None,
                },
                (Err(error), _) | (_, Err(error)) => {
                    failed_process_output(&format!("hook output capture failed: {error}"))
                }
            }
        }
        Some(Ok(Err(error))) => {
            process_group.kill();
            let _ = child.start_kill();
            let reap = reap_hook_child(&mut child).await;
            report_hook_child_reap("execution_error", reap);
            drain_hook_capture(stdout_task, "execution_error_stdout").await;
            drain_hook_capture(stderr_task, "execution_error_stderr").await;
            failed_process_output(&format!("hook execution failed: {error}"))
        }
        Some(Err(_)) => {
            process_group.kill();
            let _ = child.start_kill();
            let reap = reap_hook_child(&mut child).await;
            report_hook_child_reap("wall_timeout", reap);
            let stdout = optional_hook_capture(stdout_task, "wall_timeout_stdout").await;
            let stderr = optional_hook_capture(stderr_task, "wall_timeout_stderr").await;
            HookProcessResult {
                exit_code: None,
                timed_out: true,
                cancelled: false,
                stdout: match stdout {
                    Some(stdout) => make_output(store, stdout).await,
                    None => empty_output(),
                },
                stderr: match stderr {
                    Some(stderr) => make_output(store, stderr).await,
                    None => empty_output(),
                },
                proposed_decision: None,
            }
        }
    }
}

#[cfg(windows)]
async fn observe_hook_leader_exit(
    child: &mut tokio::process::Child,
    _pid: Option<haider_platform::ProcessId>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    child.wait().await.map(Some)
}

#[cfg(unix)]
async fn observe_hook_leader_exit(
    _child: &mut tokio::process::Child,
    pid: Option<haider_platform::ProcessId>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let pid = pid.ok_or_else(|| std::io::Error::other("hook leader PID is unavailable"))?;
    haider_platform::observe_process_leader_exit(pid).await?;
    Ok(None)
}

async fn await_hook_capture(
    task: tokio::task::JoinHandle<std::io::Result<CapturedBytes>>,
) -> std::io::Result<CapturedBytes> {
    match haider_platform::bounded_wait("hook output capture", HOOK_CHILD_REAP_TIMEOUT, task).await
    {
        haider_platform::BoundedWait::Completed(result) => result
            .map_err(|error| std::io::Error::other(format!("output reader stopped: {error}")))?,
        haider_platform::BoundedWait::TimedOut(timeout) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            timeout.to_string(),
        )),
    }
}

async fn drain_hook_capture(
    task: tokio::task::JoinHandle<std::io::Result<CapturedBytes>>,
    context: &'static str,
) {
    if let Err(error) = await_hook_capture(task).await {
        report_hook_capture_error(context, &error);
    }
}

async fn optional_hook_capture(
    task: tokio::task::JoinHandle<std::io::Result<CapturedBytes>>,
    context: &'static str,
) -> Option<CapturedBytes> {
    match await_hook_capture(task).await {
        Ok(capture) => Some(capture),
        Err(error) => {
            report_hook_capture_error(context, &error);
            None
        }
    }
}

fn report_hook_capture_error(context: &'static str, error: &std::io::Error) {
    eprintln!(
        "haiderd: lifecycle event=hook_output_drain_failed context={context} error_kind={:?} raw_os_error={:?}",
        error.kind(),
        error.raw_os_error()
    );
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    cap: usize,
) -> std::io::Result<CapturedBytes> {
    let mut bytes = Vec::with_capacity(cap.min(INLINE_OUTPUT_BYTES));
    let mut chunk = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let retained = cap.saturating_sub(bytes.len()).min(read);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
    Ok(CapturedBytes { bytes, truncated })
}

struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn make_output(
    store: &haider_core::SqliteStoreHandle,
    captured: CapturedBytes,
) -> HookOutput {
    let preview_end = captured.bytes.len().min(INLINE_OUTPUT_BYTES);
    let mut preview = String::from_utf8_lossy(&captured.bytes[..preview_end]).into_owned();
    let preview_expanded = preview.len() > INLINE_OUTPUT_BYTES;
    let artifact = if captured.bytes.len() > INLINE_OUTPUT_BYTES || preview_expanded {
        match store.put(captured.bytes.clone()).await {
            Ok(artifact) => Some(artifact),
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook output CAS spill failed");
                None
            }
        }
    } else {
        None
    };
    if preview_expanded {
        let mut end = INLINE_OUTPUT_BYTES;
        while !preview.is_char_boundary(end) {
            end -= 1;
        }
        preview.truncate(end);
    }
    HookOutput {
        preview,
        bytes: u64::try_from(captured.bytes.len()).unwrap_or(u64::MAX),
        truncated: captured.truncated
            || captured.bytes.len() > INLINE_OUTPUT_BYTES
            || preview_expanded,
        artifact,
    }
}

fn empty_output() -> HookOutput {
    HookOutput {
        preview: String::new(),
        bytes: 0,
        truncated: false,
        artifact: None,
    }
}

fn failed_process_output(message: &str) -> HookProcessResult {
    HookProcessResult {
        exit_code: None,
        timed_out: false,
        cancelled: false,
        stdout: empty_output(),
        stderr: HookOutput {
            preview: message.to_owned(),
            bytes: u64::try_from(message.len()).unwrap_or(u64::MAX),
            truncated: false,
            artifact: None,
        },
        proposed_decision: None,
    }
}

fn cancelled_process_output() -> HookProcessResult {
    HookProcessResult {
        exit_code: None,
        timed_out: false,
        cancelled: true,
        stdout: empty_output(),
        stderr: empty_output(),
        proposed_decision: None,
    }
}

struct ProcessGroupGuard {
    pid: Option<haider_platform::ProcessGroup>,
}

impl ProcessGroupGuard {
    fn kill(&mut self) {
        if let Some(pid) = self.pid.take() {
            let _ =
                haider_platform::signal_process_group(pid, haider_platform::ProcessSignal::Kill);
            haider_platform::release_process_group(pid);
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn run_subscriber(
    service: HookService,
    definition: HookDefinition,
    cause: RawEnvelope,
    mut events: mpsc::Receiver<SubscriberMessage>,
    run_override: bool,
) {
    let mut attempt = 0u32;
    let mut backoff = SUBSCRIBE_BACKOFF_MIN;
    let mut pending = None::<SubscriberMessage>;
    let mut shutdown = service.inner.shutdown.subscribe();
    loop {
        if *shutdown.borrow() || !definition_current(&service, &definition, run_override).await {
            break;
        }
        let Some((mut child, group)) = spawn_subscriber(&definition) else {
            service
                .journal(
                    &cause,
                    HookEventPayload::HookSubscription(HookSubscription {
                        hook: definition.name.clone(),
                        digest: definition.digest.clone(),
                        state: HookSubscriptionState::Exited,
                        restart_attempt: attempt,
                        exit_code: None,
                        backoff_ms: None,
                    }),
                )
                .await;
            attempt = attempt.saturating_add(1);
            service
                .journal(
                    &cause,
                    HookEventPayload::HookSubscription(HookSubscription {
                        hook: definition.name.clone(),
                        digest: definition.digest.clone(),
                        state: HookSubscriptionState::RestartScheduled,
                        restart_attempt: attempt,
                        exit_code: None,
                        backoff_ms: Some(duration_ms(backoff)),
                    }),
                )
                .await;
            if !subscriber_backoff(&mut events, &mut pending, &mut shutdown, backoff).await {
                return;
            }
            backoff = next_subscriber_backoff(backoff);
            continue;
        };
        let mut process_group = ProcessGroupGuard { pid: Some(group) };
        let mut stdin = child.stdin.take();
        service
            .journal(
                &cause,
                HookEventPayload::HookSubscription(HookSubscription {
                    hook: definition.name.clone(),
                    digest: definition.digest.clone(),
                    state: HookSubscriptionState::Started,
                    restart_attempt: attempt,
                    exit_code: None,
                    backoff_ms: None,
                }),
            )
            .await;
        let exit_code = loop {
            if let Some(message) = pending.take() {
                if write_jsonl(stdin.as_mut(), &message.input).await.is_ok() {
                    let _ = message.delivered.send(());
                } else {
                    pending = Some(message);
                    process_group.kill();
                    let _ = child.start_kill();
                }
            }
            tokio::select! {
                status = child.wait() => break status.ok().and_then(|status| status.code()),
                event = events.recv() => match event {
                    Some(message) => {
                        if write_jsonl(stdin.as_mut(), &message.input).await.is_ok() {
                            let _ = message.delivered.send(());
                        } else {
                            pending = Some(message);
                            process_group.kill();
                            let _ = child.start_kill();
                        }
                    }
                    None => {
                        process_group.kill();
                        let _ = child.start_kill();
                        let reap = reap_hook_child(&mut child).await;
                        report_hook_child_reap("subscription_input_closed", reap);
                        service.journal(
                            &cause,
                            HookEventPayload::HookSubscription(HookSubscription {
                                hook: definition.name.clone(),
                                digest: definition.digest.clone(),
                                state: HookSubscriptionState::Stopped,
                                restart_attempt: attempt,
                                exit_code: None,
                                backoff_ms: None,
                            }),
                        ).await;
                        return;
                    }
                },
                _ = shutdown.changed() => {
                    process_group.kill();
                    let _ = child.start_kill();
                    let reap = reap_hook_child(&mut child).await;
                    report_hook_child_reap("subscription_shutdown", reap);
                    return;
                },
            }
        };
        process_group.kill();
        service
            .journal(
                &cause,
                HookEventPayload::HookSubscription(HookSubscription {
                    hook: definition.name.clone(),
                    digest: definition.digest.clone(),
                    state: HookSubscriptionState::Exited,
                    restart_attempt: attempt,
                    exit_code,
                    backoff_ms: None,
                }),
            )
            .await;
        attempt = attempt.saturating_add(1);
        service
            .journal(
                &cause,
                HookEventPayload::HookSubscription(HookSubscription {
                    hook: definition.name.clone(),
                    digest: definition.digest.clone(),
                    state: HookSubscriptionState::RestartScheduled,
                    restart_attempt: attempt,
                    exit_code,
                    backoff_ms: Some(duration_ms(backoff)),
                }),
            )
            .await;
        if !subscriber_backoff(&mut events, &mut pending, &mut shutdown, backoff).await {
            return;
        }
        backoff = next_subscriber_backoff(backoff);
    }
}

fn next_subscriber_backoff(current: Duration) -> Duration {
    (current * 2).min(SUBSCRIBE_BACKOFF_MAX)
}

async fn subscriber_backoff(
    events: &mut mpsc::Receiver<SubscriberMessage>,
    pending: &mut Option<SubscriberMessage>,
    shutdown: &mut watch::Receiver<bool>,
    backoff: Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(backoff) => true,
        event = events.recv() => match event {
            Some(message) => {
                *pending = Some(message);
                true
            }
            None => false,
        },
        _ = shutdown.changed() => false,
    }
}

fn spawn_subscriber(
    definition: &HookDefinition,
) -> Option<(tokio::process::Child, haider_platform::ProcessGroup)> {
    let cwd_fd = open_canonical_directory(&definition.workspace_cwd)?;
    #[cfg(unix)]
    let mut command = hook_command(&definition.command, std::env::var_os("SHELL"));
    #[cfg(windows)]
    let mut command = hook_command(&definition.command);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    haider_platform::configure_process_environment(&mut command);
    for name in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    haider_platform::configure_process_group(&mut command);
    #[cfg(unix)]
    configure_hook_cwd(&mut command, cwd_fd);
    #[cfg(windows)]
    configure_hook_cwd(&mut command, &cwd_fd);
    haider_platform::configure_background_process(&mut command);
    let mut child = command.spawn().ok()?;
    let raw_pid = child.id()?;
    let group = match haider_platform::register_process_group(raw_pid) {
        Ok(group) => group,
        Err(_) => {
            let _ = child.start_kill();
            return None;
        }
    };
    Some((child, group))
}

async fn write_jsonl(
    stdin: Option<&mut tokio::process::ChildStdin>,
    input: &[u8],
) -> std::io::Result<()> {
    let stdin = stdin.ok_or_else(|| std::io::Error::other("subscriber stdin unavailable"))?;
    let mut bytes = input.to_vec();
    bytes.push(b'\n');
    stdin.write_all(&bytes).await
}

async fn discover_async(cwd: PathBuf, profile_root: PathBuf) -> Result<Discovery, String> {
    tokio::task::spawn_blocking(move || discover(&cwd, &profile_root))
        .await
        .map_err(|error| format!("hook discovery task stopped: {error}"))?
}

async fn discover_cached_async(service: &HookService, cwd: PathBuf) -> Result<Discovery, String> {
    let profile_root = service.inner.profile_root.clone();
    let stamp_cwd = cwd.clone();
    let stamp_profile = profile_root.clone();
    let before = tokio::task::spawn_blocking(move || discovery_stamp(&stamp_cwd, &stamp_profile))
        .await
        .map_err(|error| format!("hook discovery stamp task stopped: {error}"))??;
    if let Ok(cache) = service.inner.discovery_cache.lock()
        && let Some(cached) = cache.get(&cwd)
        && cached.stamp == before
    {
        return Ok(cached.discovery.clone());
    }
    let discovery = discover_async(cwd.clone(), profile_root.clone()).await?;
    let after_cwd = cwd.clone();
    let after = tokio::task::spawn_blocking(move || discovery_stamp(&after_cwd, &profile_root))
        .await
        .map_err(|error| format!("hook discovery stamp task stopped: {error}"))??;
    if before == after
        && let Ok(mut cache) = service.inner.discovery_cache.lock()
    {
        cache.insert(
            cwd,
            CachedDiscovery {
                stamp: after,
                discovery: discovery.clone(),
            },
        );
    }
    Ok(discovery)
}

fn discovery_stamp(cwd: &Path, profile_root: &Path) -> Result<DiscoveryStamp, String> {
    let canonical_cwd = fs::canonicalize(cwd)
        .map_err(|error| format!("workspace could not be canonicalized: {error}"))?;
    if canonical_cwd != cwd || !cwd.is_absolute() {
        return Err("workspace is not an absolute canonical path".into());
    }
    let mut paths = Vec::new();
    let mut directory = cwd.to_path_buf();
    for _ in 0..MAX_HOOK_ANCESTORS {
        paths.push(directory.clone());
        paths.push(directory.join(HOOKS_FILE));
        if !directory.pop() {
            break;
        }
    }
    paths.push(profile_root.to_path_buf());
    paths.push(profile_root.join(HOOKS_FILE));
    let mut stamped = Vec::with_capacity(paths.len());
    for path in paths {
        let metadata = fs::symlink_metadata(&path).ok();
        let value = metadata.map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos());
            (metadata.len(), modified)
        });
        stamped.push((path, value));
    }
    Ok(DiscoveryStamp(stamped))
}

fn discover(cwd: &Path, profile_root: &Path) -> Result<Discovery, String> {
    let canonical_cwd = fs::canonicalize(cwd)
        .map_err(|error| format!("workspace could not be canonicalized: {error}"))?;
    if canonical_cwd != cwd || !cwd.is_absolute() {
        return Err("workspace is not an absolute canonical path".into());
    }
    let mut directory = open_canonical_directory(cwd)
        .ok_or_else(|| "workspace is not a canonical symlink-free directory".to_owned())?;
    let mut display_directory = cwd.to_path_buf();
    let mut hooks = BTreeMap::new();
    let mut reserved = HashSet::new();
    let mut notices = Vec::new();

    for depth in 0..MAX_HOOK_ANCESTORS {
        read_document(
            &directory,
            &display_directory,
            HookSource::Workspace,
            cwd,
            &mut reserved,
            &mut hooks,
            &mut notices,
        );
        let parent = match open_directory_at(&directory, Path::new("..")) {
            Ok(parent) => parent,
            Err(error) => {
                notices.push(notice(
                    None,
                    &display_directory,
                    format!("parent directory could not be opened safely: {error}"),
                ));
                break;
            }
        };
        if same_directory(&directory, &parent) {
            break;
        }
        if !display_directory.pop() {
            break;
        }
        let Some(expected) = open_canonical_directory(&display_directory) else {
            notices.push(notice(
                None,
                &display_directory,
                "parent changed or contains a symlink; hook walk stopped",
            ));
            break;
        };
        if !same_directory(&parent, &expected) {
            notices.push(notice(
                None,
                &display_directory,
                "parent identity changed during hook walk",
            ));
            break;
        }
        directory = parent;
        if depth + 1 == MAX_HOOK_ANCESTORS {
            notices.push(notice(None, cwd, "bounded hook ancestor depth reached"));
        }
    }

    let mut policy = HookTrustPolicy::default();
    match open_canonical_directory(profile_root) {
        Some(profile) => {
            policy = read_document(
                &profile,
                profile_root,
                HookSource::Profile,
                cwd,
                &mut reserved,
                &mut hooks,
                &mut notices,
            )
            .unwrap_or_default();
        }
        None => notices.push(notice(
            None,
            profile_root,
            "profile hook directory is not canonical and symlink-free",
        )),
    }
    Ok(Discovery {
        hooks,
        notices,
        policy,
    })
}

#[allow(clippy::too_many_arguments)]
fn read_document(
    directory: &DirectoryHandle,
    display_directory: &Path,
    source: HookSource,
    workspace_cwd: &Path,
    reserved: &mut HashSet<String>,
    hooks: &mut BTreeMap<String, HookDefinition>,
    notices: &mut Vec<HookNotice>,
) -> Option<HookTrustPolicy> {
    let path = display_directory.join(HOOKS_FILE);
    let bytes = read_hook_bytes(directory, &path, notices)?;
    if bytes.len() > MAX_HOOK_CONFIG_BYTES {
        notices.push(notice(
            None,
            &path,
            "hook configuration exceeds the byte cap",
        ));
        return None;
    }
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            notices.push(notice(
                None,
                &path,
                format!("hook configuration is malformed JSON: {error}"),
            ));
            return None;
        }
    };
    let entries = match value.get("hooks").and_then(Value::as_object) {
        Some(entries) => entries.clone(),
        None => {
            notices.push(notice(
                None,
                &path,
                "hook configuration field `hooks` must be an object",
            ));
            return None;
        }
    };
    if value.get("schema").and_then(Value::as_str) != Some(HOOKS_CONFIG_SCHEMA) {
        for name in entries.keys() {
            reserved.insert(name.clone());
        }
        notices.push(notice(
            None,
            &path,
            format!("hook configuration schema must be {HOOKS_CONFIG_SCHEMA}"),
        ));
        return None;
    }
    let policy = if source == HookSource::Profile {
        match value.get("policy") {
            None => Some(HookTrustPolicy::default()),
            Some(policy) => match serde_json::from_value(policy.clone()) {
                Ok(policy) => Some(policy),
                Err(error) => {
                    notices.push(notice(
                        None,
                        &path,
                        format!("hook trust policy is malformed: {error}"),
                    ));
                    Some(HookTrustPolicy::default())
                }
            },
        }
    } else {
        None
    };
    for (name, raw) in entries {
        if !reserved.insert(name.clone()) {
            continue;
        }
        let entry: HookEntry = match serde_json::from_value(raw) {
            Ok(entry) => entry,
            Err(error) => {
                notices.push(notice(
                    Some(name),
                    &path,
                    format!("hook entry is malformed: {error}"),
                ));
                continue;
            }
        };
        let timeout_ms = entry.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let idle_timeout_ms = entry
            .idle_timeout_ms
            .unwrap_or(DEFAULT_SERVER_IDLE_TIMEOUT_MS);
        let invalid = if entry.command.trim().is_empty() {
            Some("hook command must not be empty")
        } else if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
            Some("hook timeout must be between 1ms and 300000ms")
        } else if entry.decision
            && (entry.kind != HookConfigKind::Exec
                || entry.matcher.event != MatchEvent::RunParked
                || entry.matcher.parked_kind.as_deref() != Some("permission"))
        {
            Some("decision hooks must be exec hooks matching run_parked(permission)")
        } else if entry.mode == HookMode::Server && entry.kind != HookConfigKind::Exec {
            Some("server mode is supported only for exec hooks")
        } else {
            None
        };
        if let Some(reason) = invalid {
            notices.push(notice(Some(name), &path, reason));
            continue;
        }
        let digest = hook_digest(&bytes, &entry.command);
        let kind = if entry.mode == HookMode::Server {
            HookKind::Server { idle_timeout_ms }
        } else {
            match entry.kind {
                HookConfigKind::Exec => HookKind::Exec,
                HookConfigKind::Subscribe => HookKind::Subscribe,
            }
        };
        hooks.insert(
            name.clone(),
            HookDefinition {
                name,
                matcher: entry.matcher,
                kind,
                command: entry.command,
                timeout: Duration::from_millis(timeout_ms),
                decision: entry.decision,
                digest,
                source_path: path.clone(),
                source: source.clone(),
                workspace_cwd: workspace_cwd.to_path_buf(),
            },
        );
    }
    policy
}

#[cfg(unix)]
fn read_hook_bytes(
    directory: &DirectoryHandle,
    path: &Path,
    notices: &mut Vec<HookNotice>,
) -> Option<Vec<u8>> {
    let file = match rustix::fs::openat(
        directory,
        HOOKS_FILE,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => file,
        Err(rustix::io::Errno::NOENT) => return None,
        Err(error) => {
            notices.push(notice(
                None,
                path,
                format!("hook configuration was skipped: {error}"),
            ));
            return None;
        }
    };
    let metadata = match rustix::fs::fstat(&file) {
        Ok(metadata) => metadata,
        Err(error) => {
            notices.push(notice(
                None,
                path,
                format!("hook configuration metadata failed: {error}"),
            ));
            return None;
        }
    };
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        notices.push(notice(
            None,
            path,
            "hook configuration is not a regular file",
        ));
        return None;
    }
    let mut bytes = Vec::with_capacity(MAX_HOOK_CONFIG_BYTES.min(16 * 1024));
    let limit = u64::try_from(MAX_HOOK_CONFIG_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
    if let Err(error) = fs::File::from(file).take(limit).read_to_end(&mut bytes) {
        notices.push(notice(
            None,
            path,
            format!("hook configuration could not be read: {error}"),
        ));
        return None;
    }
    Some(bytes)
}

#[cfg(windows)]
fn read_hook_bytes(
    directory: &DirectoryHandle,
    path: &Path,
    notices: &mut Vec<HookNotice>,
) -> Option<Vec<u8>> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let candidate = directory.path().join(HOOKS_FILE);
    let file = match fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&candidate)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            notices.push(notice(
                None,
                path,
                format!("hook configuration was skipped: {error}"),
            ));
            return None;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            notices.push(notice(
                None,
                path,
                format!("hook configuration metadata failed: {error}"),
            ));
            return None;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        notices.push(notice(
            None,
            path,
            "hook configuration is not a regular file",
        ));
        return None;
    }
    let mut bytes = Vec::with_capacity(MAX_HOOK_CONFIG_BYTES.min(16 * 1024));
    let limit = u64::try_from(MAX_HOOK_CONFIG_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
    if let Err(error) = file.take(limit).read_to_end(&mut bytes) {
        notices.push(notice(
            None,
            path,
            format!("hook configuration could not be read: {error}"),
        ));
        return None;
    }
    Some(bytes)
}

fn hook_digest(bytes: &[u8], command: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(bytes);
    hasher.update(command.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn workspace_identity(definition: &HookDefinition) -> String {
    format!("{}\0{}", definition.source_path.display(), definition.name)
}

fn notice(hook: Option<String>, path: &Path, reason: impl Into<String>) -> HookNotice {
    HookNotice {
        hook,
        digest: None,
        source: path.display().to_string(),
        reason: reason.into(),
    }
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn hook_command(
    script: &str,
    configured_shell: Option<std::ffi::OsString>,
) -> tokio::process::Command {
    let shell = configured_shell
        .filter(|shell| trustworthy_posix_shell(std::path::Path::new(shell)))
        .unwrap_or_else(|| std::ffi::OsString::from("sh"));
    let mut command = tokio::process::Command::new(shell);
    command.arg("-c").arg(script);
    command
}

#[cfg(unix)]
fn trustworthy_posix_shell(shell: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    if !shell.is_absolute()
        || !matches!(
            shell.file_name().and_then(|name| name.to_str()),
            Some("sh" | "ash" | "bash" | "dash" | "ksh" | "mksh")
        )
    {
        return false;
    }
    std::fs::metadata(shell)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn hook_command(script: &str) -> tokio::process::Command {
    use std::os::windows::process::CommandExt as _;

    let mut command = tokio::process::Command::new(haider_platform::windows_command_interpreter());
    command.args(["/D", "/S", "/C"]);
    command.as_std_mut().raw_arg(format!("\"{script}\""));
    command
}

#[cfg(unix)]
fn configure_hook_cwd(command: &mut tokio::process::Command, directory: DirectoryHandle) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: between fork and exec this closure invokes only fchdir(2), an
    // async-signal-safe syscall, without allocating or locking.
    #[allow(unsafe_code)]
    unsafe {
        command
            .as_std_mut()
            .pre_exec(move || rustix::process::fchdir(&directory).map_err(std::io::Error::from));
    }
}

#[cfg(windows)]
fn configure_hook_cwd(command: &mut tokio::process::Command, directory: &DirectoryHandle) {
    command.current_dir(directory.path());
}

#[cfg(unix)]
fn open_canonical_directory(path: &Path) -> Option<DirectoryHandle> {
    if !path.is_absolute() || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return None;
    }
    let mut directory = rustix::fs::openat(
        rustix::fs::CWD,
        Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    for component in path.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        directory = open_directory_at(&directory, Path::new(component)).ok()?;
    }
    Some(directory)
}

#[cfg(unix)]
fn open_directory_at(
    directory: &DirectoryHandle,
    path: &Path,
) -> Result<DirectoryHandle, DirectoryOpenError> {
    rustix::fs::openat(
        directory,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

#[cfg(unix)]
fn same_directory(left: &DirectoryHandle, right: &DirectoryHandle) -> bool {
    let (Ok(left), Ok(right)) = (rustix::fs::fstat(left), rustix::fs::fstat(right)) else {
        return false;
    };
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(windows)]
fn open_canonical_directory(path: &Path) -> Option<DirectoryHandle> {
    if !path.is_absolute() || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return None;
    }
    haider_platform::open_workspace_directory(path).ok()
}

#[cfg(windows)]
fn open_directory_at(
    directory: &DirectoryHandle,
    path: &Path,
) -> Result<DirectoryHandle, DirectoryOpenError> {
    if path == Path::new("..") {
        let parent = directory.path().parent().unwrap_or(directory.path());
        return haider_platform::open_workspace_directory(parent);
    }
    let duplicate = haider_platform::duplicate_workspace_directory(directory)?;
    haider_platform::open_workspace_subdirectory(duplicate, path, false)
}

#[cfg(windows)]
fn same_directory(left: &DirectoryHandle, right: &DirectoryHandle) -> bool {
    matches!(
        (
            haider_platform::workspace_directory_identity(left),
            haider_platform::workspace_directory_identity(right),
        ),
        (Ok(left), Ok(right)) if left == right
    )
}
