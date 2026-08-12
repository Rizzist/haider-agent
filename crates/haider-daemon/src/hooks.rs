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

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;

use crate::session_hub::SessionHub;
use haider_core::{
    HookTrustChange, HookTrustCommand, MenuResolutionCommand, MenuResolutionOutcome, StoreHandle,
};
use haider_protocol::effect::{AuthorizationVerdict, EffectIntent, EffectPhase};
use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::ErrorCode;
use haider_protocol::hook::{
    HOOKS_CONFIG_SCHEMA, HookAttachmentMetadata, HookAttachmentSet, HookDecisionKind,
    HookEventPayload, HookFired, HookInput, HookNotice, HookOutput, HookRuntimeKind,
    HookSubscription, HookSubscriptionState,
};
use haider_protocol::ids::{EffectId, EventId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::{AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind};
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::{DeliveryMode, EventPayload};
use haider_rpc::{CommandId, HookSummaryWire, HookTrustStateWire};
use rustix::fd::OwnedFd;
use rustix::fs::{FileType, Mode, OFlags};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

const HOOKS_FILE: &str = "hooks.json";
const MAX_HOOK_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_HOOK_ANCESTORS: usize = 256;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const INLINE_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_STREAM_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_USER_MESSAGE_TEXT_BYTES: usize = 32 * 1024;
const SUBSCRIBE_QUEUE: usize = 64;
const SUBSCRIBE_BACKOFF_MIN: Duration = Duration::from_millis(200);
const SUBSCRIBE_BACKOFF_MAX: Duration = Duration::from_secs(5);
const ENV_ALLOWLIST: [&str; 5] = ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR"];

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookKind {
    Exec,
    Subscribe,
}

impl HookKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Subscribe => "subscribe",
        }
    }
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
    kind: HookKind,
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    decision: bool,
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

#[derive(Debug)]
struct Discovery {
    hooks: BTreeMap<String, HookDefinition>,
    notices: Vec<HookNotice>,
    policy: HookTrustPolicy,
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
    committed: mpsc::UnboundedSender<EngineMessage>,
    shutdown: watch::Sender<bool>,
    pins: RwLock<HashSet<String>>,
    workspace_baselines: Mutex<HashMap<String, String>>,
    /// Hook identity → latest digest the daemon itself observed as trusted.
    /// This is the authority for revoked-by-edit classification; clients do
    /// not infer it by comparing snapshots.
    observed_trusted: Mutex<HashMap<String, String>>,
    next_event: AtomicU64,
}

impl HookService {
    pub(crate) fn downgrade(&self) -> WeakHookService {
        WeakHookService {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub(crate) fn observe_committed(&self, envelopes: &[RawEnvelope]) {
        if envelopes
            .iter()
            .all(|envelope| HookEventPayload::is_engine_fact(&envelope.payload))
        {
            return;
        }
        if self
            .inner
            .committed
            .send(EngineMessage::Committed(envelopes.to_vec()))
            .is_err()
        {
            tracing::warn!(
                target: "haider.hooks",
                "committed hook observer is closed; facts remain durable but were not dispatched"
            );
        }
    }

    pub(crate) async fn list(
        &self,
        cwd: PathBuf,
    ) -> Result<(HookTrustPolicy, u64, Vec<HookSummaryWire>), String> {
        let discovery = discover_async(cwd, self.inner.profile_root.clone()).await?;
        self.prepare_workspace_trust(&discovery).await;
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
                self.inner
                    .observed_trusted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, digest| digest != &change.digest);
            }
        }
        let _ = self
            .inner
            .committed
            .send(EngineMessage::TrustChanged(change.clone()));
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
    pub(crate) async fn start(
        profile_root: PathBuf,
        store: haider_core::SqliteStoreHandle,
        hub: SessionHub,
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
        let (sender, receiver) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let observed_trusted = workspace_baselines.clone();
        let service = HookService {
            inner: Arc::new(HookServiceInner {
                profile_root,
                store,
                hub,
                committed: sender,
                shutdown,
                pins: RwLock::new(pins),
                workspace_baselines: Mutex::new(workspace_baselines),
                observed_trusted: Mutex::new(observed_trusted),
                next_event: AtomicU64::new(0),
            }),
        };
        let state = hydrate_engine_state(&service.inner.store).await?;
        let manager_service = service.clone();
        let task = tokio::spawn(run_engine(receiver, manager_service, state));
        Ok((
            service.clone(),
            Self {
                service,
                task: Some(task),
            },
        ))
    }

    pub(crate) async fn shutdown(mut self) {
        let _ = self.service.inner.shutdown.send(true);
        let (done, wait) = oneshot::channel();
        let _ = self
            .service
            .inner
            .committed
            .send(EngineMessage::Shutdown(done));
        let _ = wait.await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for HookEngine {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

enum EngineMessage {
    Committed(Vec<RawEnvelope>),
    TrustChanged(HookTrustChange),
    Shutdown(oneshot::Sender<()>),
}

struct EngineState {
    sessions: HashMap<SessionId, DecisionState>,
    run_trust: HashSet<(SessionId, RunId)>,
    notice_dedup: HashSet<String>,
    subscribers: HashMap<String, SubscriberHandle>,
}

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

#[derive(Default)]
struct DecisionState {
    intents: HashMap<EffectId, EffectIntent>,
    bindings: HashMap<MenuId, EffectIntent>,
    menus: HashMap<MenuId, OpenPermission>,
}

#[derive(Clone)]
struct OpenPermission {
    menu: Menu,
    opening: RawEnvelope,
}

#[derive(Clone)]
struct DecisionContext {
    intent: EffectIntent,
    permission: OpenPermission,
}

async fn hydrate_engine_state(
    store: &haider_core::SqliteStoreHandle,
) -> Result<EngineState, haider_protocol::error::HaiderError> {
    let mut state = EngineState {
        sessions: HashMap::new(),
        run_trust: HashSet::new(),
        notice_dedup: HashSet::new(),
        subscribers: HashMap::new(),
    };
    for session_id in store.session_ids().await? {
        let mut since_seq = 0;
        loop {
            let page = store.read(&session_id, since_seq, 1_024).await?;
            if page.is_empty() {
                break;
            }
            for envelope in &page {
                reduce_durable_state(&mut state, envelope);
            }
            since_seq = page.last().map_or(since_seq, |envelope| envelope.seq);
        }
    }
    Ok(state)
}

fn reduce_durable_state(
    state: &mut EngineState,
    envelope: &RawEnvelope,
) -> Option<DecisionContext> {
    if let Ok(HookEventPayload::HookRunTrust { enabled }) =
        HookEventPayload::from_payload_value(envelope.payload.clone())
        && let Some(run_id) = envelope.run_id.clone()
    {
        let key = (envelope.session_id.clone(), run_id);
        if enabled {
            state.run_trust.insert(key);
        } else {
            state.run_trust.remove(&key);
        }
        return None;
    }
    absorb_decision_fact(state, envelope)
}

async fn run_engine(
    mut messages: mpsc::UnboundedReceiver<EngineMessage>,
    service: HookService,
    mut state: EngineState,
) {
    let mut jobs = JoinSet::new();
    if !replay_pending_dispatches(&service, &mut state, &mut jobs).await {
        state.subscribers.clear();
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
        return;
    }
    loop {
        tokio::select! {
            biased;
            message = messages.recv() => match message {
                Some(EngineMessage::Committed(envelopes)) => {
                    for envelope in envelopes {
                        if !handle_and_complete(&service, &mut state, &mut jobs, envelope).await {
                            break;
                        }
                    }
                }
                Some(EngineMessage::TrustChanged(change)) => {
                    if !change.trusted {
                        state.subscribers.retain(|_, handle| handle.digest != change.digest);
                    }
                }
                Some(EngineMessage::Shutdown(done)) => {
                    state.subscribers.clear();
                    jobs.abort_all();
                    while jobs.join_next().await.is_some() {}
                    let _ = done.send(());
                    break;
                }
                None => break,
            },
            _ = jobs.join_next(), if !jobs.is_empty() => {}
        }
    }
}

async fn replay_pending_dispatches(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
) -> bool {
    loop {
        let pending = match service.inner.store.pending_hook_dispatches(1_024).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(target: "haider.hooks", ?error, "hook recovery outbox read failed");
                return false;
            }
        };
        if pending.is_empty() {
            return true;
        }
        for envelope in pending {
            if !handle_and_complete(service, state, jobs, envelope).await {
                return false;
            }
        }
    }
}

async fn handle_and_complete(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
    envelope: RawEnvelope,
) -> bool {
    if !handle_committed(service, state, jobs, envelope.clone()).await {
        return false;
    }
    if let Err(error) = service
        .inner
        .store
        .complete_hook_dispatch(&envelope.session_id, envelope.seq)
        .await
    {
        tracing::warn!(target: "haider.hooks", ?error, "hook recovery outbox acknowledgement failed");
        return false;
    }
    true
}

async fn handle_committed(
    service: &HookService,
    state: &mut EngineState,
    jobs: &mut JoinSet<()>,
    envelope: RawEnvelope,
) -> bool {
    if HookEventPayload::is_engine_fact(&envelope.payload) {
        return true;
    }

    let decision = reduce_durable_state(state, &envelope);
    if matches!(
        HookEventPayload::from_payload_value(envelope.payload.clone()),
        Ok(HookEventPayload::HookRunTrust { .. })
    ) {
        return true;
    }
    let Some(facts) = classify(&envelope) else {
        return true;
    };
    let metadata = match service
        .inner
        .store
        .session_metadata(&envelope.session_id)
        .await
    {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(target: "haider.hooks", ?error, "hook metadata hydration failed");
            return false;
        }
    };
    let discovery = match discover_async(
        PathBuf::from(&metadata.cwd),
        service.inner.profile_root.clone(),
    )
    .await
    {
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
    service.prepare_workspace_trust(&discovery).await;
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

    let terminal_scope = if facts.event == MatchEvent::RunFinished {
        envelope
            .run_id
            .clone()
            .map(|run_id| (envelope.session_id.clone(), run_id))
    } else {
        None
    };
    let mut prepared_input = None::<Result<Arc<[u8]>, String>>;
    for definition in discovery.hooks.into_values() {
        if !definition
            .matcher
            .matches(&envelope, &metadata.provider, &facts)
        {
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
            if !fire_decision(
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
        }
    }
    if let Some(terminal_scope) = terminal_scope {
        state
            .subscribers
            .retain(|_, handle| handle.run_scope.as_ref() != Some(&terminal_scope));
    }
    true
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

fn absorb_decision_fact(
    state: &mut EngineState,
    envelope: &RawEnvelope,
) -> Option<DecisionContext> {
    let session = state
        .sessions
        .entry(envelope.session_id.clone())
        .or_default();
    let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?;
    match payload {
        EventPayload::Effect(EffectPhase::Intent(intent)) => {
            session.intents.insert(intent.effect.clone(), intent);
        }
        EventPayload::Effect(EffectPhase::Authorized {
            effect,
            verdict: AuthorizationVerdict::Ask { menu },
        }) => {
            if let Some(intent) = session.intents.get(&effect).cloned() {
                session.bindings.insert(menu, intent);
            }
        }
        EventPayload::MenuOpened(menu) if matches!(menu.kind, MenuKind::Permission { .. }) => {
            session.menus.insert(
                menu.id.clone(),
                OpenPermission {
                    menu,
                    opening: envelope.clone(),
                },
            );
        }
        EventPayload::RunState(RunState::PermissionRequired { menu }) => {
            let intent = session.bindings.get(&menu)?.clone();
            let permission = session.menus.get(&menu)?.clone();
            return Some(DecisionContext { intent, permission });
        }
        EventPayload::MenuAnswered(answer) => {
            let menu = answer.menu;
            session.bindings.remove(&menu);
            session.menus.remove(&menu);
        }
        EventPayload::MenuClosed { menu, .. } => {
            session.bindings.remove(&menu);
            session.menus.remove(&menu);
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

fn classify(envelope: &RawEnvelope) -> Option<MatchFacts> {
    if let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
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
                mode: Some(mode),
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
        return Some(facts);
    }
    match HookEventPayload::from_payload_value(envelope.payload.clone()).ok()? {
        HookEventPayload::UpdateAvailable { .. } => Some(MatchFacts {
            event: MatchEvent::UpdateAvailable,
            outcome: None,
            parked_kind: None,
            provider: None,
            mode: None,
            has_attachments: None,
        }),
        HookEventPayload::AccountExpired { provider, .. } => Some(MatchFacts {
            event: MatchEvent::AccountExpired,
            outcome: None,
            parked_kind: None,
            provider: Some(provider),
            mode: None,
            has_attachments: None,
        }),
        _ => None,
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
    let Ok(discovery) = discover_async(
        definition.workspace_cwd.clone(),
        service.inner.profile_root.clone(),
    )
    .await
    else {
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
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(&definition.command)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.as_std_mut().process_group(0);
    // SAFETY: between fork and exec this closure invokes only fchdir(2) and
    // close(2), both async-signal-safe, without allocating or locking.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            rustix::process::fchdir(&cwd_fd).map_err(std::io::Error::from)?;
            for fd in 3..65_536i32 {
                rustix::io::close(fd);
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed_process_output(&format!("hook spawn failed: {error}")),
    };
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw);
    let process_group = ProcessGroupGuard { pid };
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let execution = async {
        if let Some(mut stdin) = stdin.take() {
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
        }
        let stdout = stdout.ok_or_else(|| std::io::Error::other("hook stdout unavailable"))?;
        let stderr = stderr.ok_or_else(|| std::io::Error::other("hook stderr unavailable"))?;
        let (stdout, stderr, status) = tokio::join!(
            read_limited(stdout, MAX_STREAM_OUTPUT_BYTES),
            read_limited(stderr, MAX_STREAM_OUTPUT_BYTES),
            child.wait(),
        );
        Ok::<_, std::io::Error>((stdout?, stderr?, status?))
    };
    let outcome = tokio::select! {
        outcome = tokio::time::timeout(definition.timeout, execution) => Some(outcome),
        _ = shutdown.changed() => None,
    };
    match outcome {
        None => {
            process_group.kill();
            let _ = child.start_kill();
            let _ = child.wait().await;
            cancelled_process_output()
        }
        Some(Ok(Ok((stdout, stderr, status)))) => HookProcessResult {
            exit_code: status.code(),
            timed_out: false,
            cancelled: false,
            stdout: make_output(store, stdout).await,
            stderr: make_output(store, stderr).await,
            proposed_decision: None,
        },
        Some(Ok(Err(error))) => failed_process_output(&format!("hook execution failed: {error}")),
        Some(Err(_)) => {
            process_group.kill();
            let _ = child.start_kill();
            let _ = child.wait().await;
            HookProcessResult {
                exit_code: None,
                timed_out: true,
                cancelled: false,
                stdout: empty_output(),
                stderr: empty_output(),
                proposed_decision: None,
            }
        }
    }
}

async fn read_limited(
    reader: impl AsyncRead + Unpin,
    cap: usize,
) -> std::io::Result<CapturedBytes> {
    let limit = u64::try_from(cap.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(cap.min(INLINE_OUTPUT_BYTES));
    reader.take(limit).read_to_end(&mut bytes).await?;
    let truncated = bytes.len() > cap;
    bytes.truncate(cap);
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
    pid: Option<rustix::process::Pid>,
}

impl ProcessGroupGuard {
    fn kill(&self) {
        if let Some(pid) = self.pid {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
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
        let Some(mut child) = spawn_subscriber(&definition) else {
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
        let process_group = ProcessGroupGuard {
            pid: child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(rustix::process::Pid::from_raw),
        };
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
                        let _ = child.wait().await;
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
                    let _ = child.wait().await;
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

fn spawn_subscriber(definition: &HookDefinition) -> Option<tokio::process::Child> {
    let cwd_fd = open_canonical_directory(&definition.workspace_cwd)?;
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(&definition.command)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.as_std_mut().process_group(0);
    // SAFETY: see `run_command`; only async-signal-safe syscalls run here.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            rustix::process::fchdir(&cwd_fd).map_err(std::io::Error::from)?;
            for fd in 3..65_536i32 {
                rustix::io::close(fd);
            }
            Ok(())
        });
    }
    command.spawn().ok()
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
    directory: &OwnedFd,
    display_directory: &Path,
    source: HookSource,
    workspace_cwd: &Path,
    reserved: &mut HashSet<String>,
    hooks: &mut BTreeMap<String, HookDefinition>,
    notices: &mut Vec<HookNotice>,
) -> Option<HookTrustPolicy> {
    let path = display_directory.join(HOOKS_FILE);
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
                &path,
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
                &path,
                format!("hook configuration metadata failed: {error}"),
            ));
            return None;
        }
    };
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        notices.push(notice(
            None,
            &path,
            "hook configuration is not a regular file",
        ));
        return None;
    }
    let mut bytes = Vec::with_capacity(MAX_HOOK_CONFIG_BYTES.min(16 * 1024));
    let limit = u64::try_from(MAX_HOOK_CONFIG_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
    if let Err(error) = fs::File::from(file).take(limit).read_to_end(&mut bytes) {
        notices.push(notice(
            None,
            &path,
            format!("hook configuration could not be read: {error}"),
        ));
        return None;
    }
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
        let invalid = if entry.command.trim().is_empty() {
            Some("hook command must not be empty")
        } else if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
            Some("hook timeout must be between 1ms and 300000ms")
        } else if entry.decision
            && (entry.kind != HookKind::Exec
                || entry.matcher.event != MatchEvent::RunParked
                || entry.matcher.parked_kind.as_deref() != Some("permission"))
        {
            Some("decision hooks must be exec hooks matching run_parked(permission)")
        } else {
            None
        };
        if let Some(reason) = invalid {
            notices.push(notice(Some(name), &path, reason));
            continue;
        }
        let digest = hook_digest(&bytes, &entry.command);
        hooks.insert(
            name.clone(),
            HookDefinition {
                name,
                matcher: entry.matcher,
                kind: entry.kind,
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

fn open_canonical_directory(path: &Path) -> Option<OwnedFd> {
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

fn open_directory_at(directory: &OwnedFd, path: &Path) -> Result<OwnedFd, rustix::io::Errno> {
    rustix::fs::openat(
        directory,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

fn same_directory(left: &OwnedFd, right: &OwnedFd) -> bool {
    let (Ok(left), Ok(right)) = (rustix::fs::fstat(left), rustix::fs::fstat(right)) else {
        return false;
    };
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}
