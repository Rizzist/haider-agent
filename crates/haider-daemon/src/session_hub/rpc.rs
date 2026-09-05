//! CHARTER — the connection's request surface: transport in, semantics down.
//!
//! What lives here: [`HubConnection`]'s method handlers — capability and
//! control-attachment policy checks, argument validation, receipt-first
//! command orchestration (R2/R3/R5), workspace validation, and wire
//! error-code mapping. What may NOT live here: durable mutation (the store
//! owns every transaction; the session actor serializes it — actor.rs),
//! delivery pacing (replay.rs), and provider/tool work (`worker.rs`; a
//! request handler hands the manager a COMMITTED acceptance and returns).
//! Requests on one connection are handled inline by the connection task, so
//! nothing here may await provider work — the longest await is one store
//! transaction or one workspace `spawn_blocking`.

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;

#[cfg(test)]
#[path = "direct_ssh_tests.rs"]
mod direct_ssh_tests;

use super::*;
use crate::delegation::{DelegationHandle, MessageCoordinates};
use base64::Engine as _;
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::ChipState;
use haider_protocol::cache::{
    ProviderOperationEventPayload, ProviderRequestAttemptV1, ProviderRequestKind,
};
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::effect::{EffectClass, EffectIntent, EffectPhase};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawPayload, RenderTargets, SCHEMA_VERSION, write_payload_json,
};
use haider_protocol::ids::{AgentId, ItemId, RunId};
use haider_protocol::item::ItemEvent;
use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
use haider_protocol::permission::{PermissionEventPayload, SystemPermission};
use haider_protocol::state::RunState;
use haider_rpc::{ERROR_CODE_PERMISSION_DENIED, ERROR_CODE_RESTAGE_REQUIRED, ProviderSummaryWire};
use haider_tools::MessageSubagent;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{MissedTickBehavior, interval_at};
use zeroize::Zeroizing;

const MAX_ATTACHMENTS_PER_TURN: usize = 5;

pub(super) fn status_caching(
    idle_ttl_ms: Option<u64>,
    active_provider: Option<&str>,
    providers: &[ProviderSummaryWire],
) -> haider_rpc::DaemonCachingWire {
    use haider_rpc::{ProviderApiFamilyWire, ProviderCacheRegimeWire, SessionReuseWire};
    let cache_regimes_by_provider: BTreeMap<_, _> = providers
        .iter()
        .map(|provider| {
            let regime = match provider.api_family {
                ProviderApiFamilyWire::AnthropicMessages => {
                    Some(ProviderCacheRegimeWire::ExplicitBreakpoints)
                }
                ProviderApiFamilyWire::OpenAiResponses
                | ProviderApiFamilyWire::OpenAiChatCompletions => {
                    Some(ProviderCacheRegimeWire::AutomaticPrefix)
                }
                _ => None,
            };
            (provider.provider.clone(), regime)
        })
        .collect();
    haider_rpc::DaemonCachingWire {
        // The daemon always prepares cache-aware prompts and persists the
        // provider-view CAS when the adapter emits a view. These are support
        // declarations, not guarantees of a cacheable request or remote hit.
        prompt_cache: true,
        provider_view_cas: true,
        session_reuse: if idle_ttl_ms == Some(0) {
            SessionReuseWire::OneShot
        } else {
            SessionReuseWire::Resident
        },
        idle_ttl_ms,
        cache_regime: active_provider
            .and_then(|provider| cache_regimes_by_provider.get(provider).copied())
            .flatten(),
        cache_regimes_by_provider,
    }
}
const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES_PER_TURN: usize = 16 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES_PER_PDF_TURN: usize = 64 * 1024 * 1024;
const MAX_SURFACE_WATCHES_PER_CONNECTION: usize = 16;

#[derive(Clone)]
struct LoomProviderRequestAttemptRecorder {
    hub: SessionHub,
    session_id: SessionId,
    run_id: RunId,
    turn_ordinal: u64,
    ordinals: haider_provider::ProviderRequestOrdinal,
    trace: Option<haider_provider::TurnTraceContext>,
}

impl std::fmt::Debug for LoomProviderRequestAttemptRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoomProviderRequestAttemptRecorder")
            .field("session_id", &self.session_id)
            .field("run_id", &self.run_id)
            .field("turn_ordinal", &self.turn_ordinal)
            .finish_non_exhaustive()
    }
}

fn loom_provider_envelope(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    event_id: EventId,
    payload: EventPayload,
) -> Result<RawEnvelope, HaiderError> {
    let payload = RawPayload::from_event(payload).map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("Loom provider correlation could not serialize: {error}"),
            false,
        )
    })?;
    Ok(loom_provider_raw_envelope(
        hub, session_id, run_id, event_id, payload,
    ))
}

fn loom_provider_raw_envelope(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    event_id: EventId,
    payload: RawPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id,
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: hub.device_id(),
        authority_epoch: 0,
        worker_generation: hub.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

#[async_trait::async_trait]
impl haider_provider::ProviderRequestAttemptRecorder for LoomProviderRequestAttemptRecorder {
    async fn record_auxiliary_attempt(
        &self,
        request_kind: ProviderRequestKind,
    ) -> Result<ProviderRequestAttemptV1, haider_provider::ProviderError> {
        if request_kind == ProviderRequestKind::Primary {
            return Err(haider_provider::ProviderError::new(
                haider_provider::ProviderErrorKind::Internal,
                "Loom provider support cannot allocate a primary request",
            ));
        }
        let request_ordinal = self.ordinals.next()?;
        let attempt = ProviderRequestAttemptV1 {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            turn_ordinal: self.turn_ordinal,
            request_ordinal,
            request_kind,
        };
        if !attempt.coordinates_valid() {
            return Err(haider_provider::ProviderError::new(
                haider_provider::ProviderErrorKind::Internal,
                "Loom provider correlation coordinates are invalid or ambiguous",
            ));
        }
        let item = attempt.extension_item().map_err(|error| {
            haider_provider::ProviderError::new(
                haider_provider::ProviderErrorKind::Internal,
                format!("Loom provider request marker could not serialize: {error}"),
            )
        })?;
        let item_id = ItemId::new(format!(
            "loom-provider-request-{}-{request_ordinal}",
            self.run_id
        ));
        let mut envelopes = [
            loom_provider_envelope(
                &self.hub,
                &self.session_id,
                &self.run_id,
                EventId::new(format!("{item_id}-started")),
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
            ),
            loom_provider_envelope(
                &self.hub,
                &self.session_id,
                &self.run_id,
                EventId::new(format!("{item_id}-completed")),
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
            ),
        ]
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            haider_provider::ProviderError::new(
                haider_provider::ProviderErrorKind::Internal,
                format!("Loom provider request marker could not be built: {error}"),
            )
        })?;
        let started = self
            .trace
            .as_ref()
            .map(haider_provider::TurnTraceContext::now_us_from_accept);
        self.hub.append(&mut envelopes).await.map_err(|error| {
            haider_provider::ProviderError::new(
                haider_provider::ProviderErrorKind::Internal,
                format!("Loom provider request marker could not be journaled: {error}"),
            )
        })?;
        if let Some(trace) = self.trace.as_ref() {
            trace.register_request(&attempt);
            if let Some(started) = started {
                trace.emit(
                    "request_attempt_commit",
                    request_ordinal,
                    0,
                    started,
                    trace.now_us_from_accept(),
                );
            }
        }
        Ok(attempt)
    }
}

async fn begin_loom_provider_request(
    hub: &SessionHub,
    session_id: &SessionId,
    authoring_id: &str,
) -> Result<crate::loom_author::LoomProviderRequestContext, HaiderError> {
    // A projection-invisible provider operation reserves a durable,
    // session-monotonic turn ordinal without fabricating a conversation run,
    // hook terminal, usage timing, or nonterminal-run gate. The request marker
    // below still commits before provider I/O.
    let run_id = RunId::new(authoring_id);
    let mut reservation = [loom_provider_raw_envelope(
        hub,
        session_id,
        &run_id,
        EventId::new(format!("{authoring_id}-turn-reserved")),
        ProviderOperationEventPayload::ProviderOperationReserved {
            request_kind: ProviderRequestKind::Side,
        }
        .to_payload_value()
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("Loom provider operation reservation could not serialize: {error}"),
                false,
            )
        })?
        .into(),
    )];
    hub.append(&mut reservation).await?;
    let turn_ordinal = hub
        .turn_ordinal(session_id, &run_id)
        .await?
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                "Loom provider operation has no durable turn ordinal",
                false,
            )
        })?;
    let trace = haider_provider::turn_trace_enabled().then(|| {
        let trace = haider_provider::TurnTraceContext::new(
            session_id.clone(),
            run_id.clone(),
            turn_ordinal,
        );
        trace.emit("accept", 0, 0, 0, 0);
        trace
    });
    let recorder = Arc::new(LoomProviderRequestAttemptRecorder {
        hub: hub.clone(),
        session_id: session_id.clone(),
        run_id,
        turn_ordinal,
        ordinals: haider_provider::ProviderRequestOrdinal::new(0),
        trace: trace.clone(),
    });
    let attempt = haider_provider::ProviderRequestAttemptRecorder::record_auxiliary_attempt(
        recorder.as_ref(),
        ProviderRequestKind::Side,
    )
    .await
    .map_err(|error| {
        HaiderError::new(
            ErrorCode::ProviderError,
            format!("Loom provider request identity could not be committed: {error}"),
            false,
        )
    })?;
    Ok(crate::loom_author::LoomProviderRequestContext {
        attempt,
        auxiliary_recorder: recorder,
        turn_trace: trace,
    })
}

struct TurnSubmitInput {
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    branch_id: Option<haider_protocol::ids::BranchId>,
    text: String,
    attachments: Vec<haider_protocol::tool::AttachmentBlock>,
    mode: DeliveryMode,
    trust_hooks: bool,
    headless_spec: Option<haider_protocol::headless::HeadlessRunSpecV1>,
}

struct SessionSelectModelInput {
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    model: String,
    provider: Option<String>,
    confirm_new_epoch: bool,
}

enum SessionForkSelectorInput {
    Exact {
        node_id: haider_protocol::ids::NodeId,
        seq: u64,
    },
    Prompt {
        seq: u64,
    },
}

struct CheckpointDoorInput {
    command_id: CommandId,
    session_id: SessionId,
    branch_id: Option<haider_protocol::ids::BranchId>,
    worker_generation: u64,
    action: CheckpointDoorAction,
}

enum CheckpointDoorAction {
    Undo { target: String },
    Redo { target: String },
    RollbackTurn { run_id: haider_protocol::ids::RunId },
}

enum CheckpointDoorFailure {
    Response {
        code: &'static str,
        message: String,
        data: Option<ErrorData>,
    },
    Hub(SessionHubError),
}

impl CheckpointDoorFailure {
    fn not_found(message: impl Into<String>) -> Self {
        Self::Response {
            code: ERROR_CODE_NOT_FOUND,
            message: message.into(),
            data: None,
        }
    }
}

struct CheckpointEffectEnvelopeInput<'a> {
    session_id: SessionId,
    branch_id: Option<haider_protocol::ids::BranchId>,
    run_id: RunId,
    effect_id: haider_protocol::ids::EffectId,
    command_id: &'a str,
    mutation_digest: String,
    checkpoint: haider_protocol::checkpoint::CheckpointRecorded,
    worker_generation: u64,
    device_id: &'a DeviceId,
}

impl CheckpointDoorAction {
    fn method(&self) -> &'static str {
        match self {
            Self::Undo { .. } => "checkpoint.undo",
            Self::Redo { .. } => "checkpoint.redo",
            Self::RollbackTurn { .. } => "checkpoint.rollback_turn",
        }
    }

    fn origin(&self) -> haider_protocol::checkpoint::CheckpointOrigin {
        match self {
            Self::Undo { .. } => haider_protocol::checkpoint::CheckpointOrigin::Undo,
            Self::Redo { .. } => haider_protocol::checkpoint::CheckpointOrigin::Redo,
            Self::RollbackTurn { .. } => {
                haider_protocol::checkpoint::CheckpointOrigin::RollbackTurn
            }
        }
    }
}

fn checkpoint_action_json(action: &CheckpointDoorAction) -> serde_json::Value {
    match action {
        CheckpointDoorAction::Undo { target } => {
            serde_json::json!({ "kind": "undo", "target": target })
        }
        CheckpointDoorAction::Redo { target } => {
            serde_json::json!({ "kind": "redo", "target": target })
        }
        CheckpointDoorAction::RollbackTurn { run_id } => {
            serde_json::json!({ "kind": "rollback_turn", "run_id": run_id })
        }
    }
}

fn checkpoint_bytes_digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn checkpoint_capture_digest(captures: &[haider_tools::CheckpointCapturePath]) -> String {
    let mut hasher = blake3::Hasher::new();
    for capture in captures {
        for part in [
            capture.path.as_str(),
            capture.pre_digest.as_deref().unwrap_or("absent"),
            capture.post_digest.as_deref().unwrap_or("absent"),
        ] {
            let part_len = u64::try_from(part.len()).unwrap_or(u64::MAX);
            hasher.update(&part_len.to_be_bytes());
            hasher.update(part.as_bytes());
        }
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn checkpoint_effect_envelopes(
    input: CheckpointEffectEnvelopeInput<'_>,
) -> Result<Vec<RawEnvelope>, SessionHubError> {
    let CheckpointEffectEnvelopeInput {
        session_id,
        branch_id,
        run_id,
        effect_id,
        command_id,
        mutation_digest,
        checkpoint,
        worker_generation,
        device_id,
    } = input;
    let args_digest = checkpoint_capture_digest_from_record(&checkpoint);
    let payloads = vec![
        EventPayload::Effect(EffectPhase::Intent(EffectIntent {
            effect: effect_id.clone(),
            class: EffectClass::FsWrite,
            summary: format!("{} workspace checkpoint", checkpoint.origin.as_str()),
            args_digest,
            workspace_revision: None,
        })),
        EventPayload::Effect(EffectPhase::Authorized {
            effect: effect_id.clone(),
            verdict: haider_protocol::effect::AuthorizationVerdict::Allow,
        }),
        EventPayload::Effect(EffectPhase::Dispatched {
            effect: effect_id.clone(),
        }),
        EventPayload::Effect(EffectPhase::Outcome {
            effect: effect_id.clone(),
            outcome: haider_protocol::effect::EffectOutcome::Ok,
            freshness: None,
            workspace_mutation: Some(haider_protocol::effect::WorkspaceMutation {
                effect_id: effect_id.clone(),
                mutation_digest,
                workspace_revision: None,
                subject_digest: None,
            }),
        }),
        EventPayload::CheckpointRecorded(checkpoint),
    ];
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, payload)| {
            let ordinal = index + 1;
            Ok(haider_protocol::envelope::EventEnvelope {
                schema_version: haider_protocol::envelope::SCHEMA_VERSION,
                event_id: EventId::new(format!("checkpoint-command-{command_id}-{ordinal}")),
                seq: 0,
                session_id: session_id.clone(),
                branch_id: branch_id.clone(),
                run_id: Some(run_id.clone()),
                agent_id: None,
                device_id: device_id.clone(),
                authority_epoch: 0,
                worker_generation,
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 0,
                render: haider_protocol::envelope::RenderTargets {
                    ui: false,
                    durable: true,
                    prompt: haider_protocol::envelope::PromptRender::Omit,
                },
                payload: haider_protocol::envelope::RawPayload::from_event(payload).map_err(
                    |error| {
                        SessionHubError::Task(format!(
                            "cannot encode checkpoint effect envelope: {error}"
                        ))
                    },
                )?,
            })
        })
        .collect()
}

fn checkpoint_capture_digest_from_record(
    checkpoint: &haider_protocol::checkpoint::CheckpointRecorded,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for path in &checkpoint.paths {
        for part in [
            path.path.as_str(),
            path.pre_digest.as_deref().unwrap_or("absent"),
            path.post_digest.as_deref().unwrap_or("absent"),
        ] {
            let part_len = u64::try_from(part.len()).unwrap_or(u64::MAX);
            hasher.update(&part_len.to_be_bytes());
            hasher.update(part.as_bytes());
        }
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

async fn rollback_failed_checkpoint_command(
    plan: haider_tools::CheckpointRestorePlan,
) -> Result<(), SessionHubError> {
    match tokio::task::spawn_blocking(move || haider_tools::restore_checkpoint_plan(&plan)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(SessionHubError::Task(format!(
            "checkpoint command failed and workspace recovery also failed: {error}"
        ))),
        Err(error) => Err(SessionHubError::Task(format!(
            "checkpoint command recovery worker failed: {error}"
        ))),
    }
}

struct AccountLoginInput {
    command_id: CommandId,
    provider: String,
    alias: Option<String>,
    vault_reference: String,
    validation_model: Option<String>,
    replace_existing: bool,
}

enum InventoryRefreshError {
    Hub(SessionHubError),
    Provider(crate::accounts::ProviderModelsRefreshFailure),
}

/// Profile-vault alias holding the transcription secret (the Deepgram API
/// key). Daemon-internal: clients only ever speak
/// `transcription.secret_get`/`transcription.secret_set` — the alias never
/// crosses the wire. Public to the crate so integration tests can address
/// the same physical vault item the handler wrote.
pub(crate) const TRANSCRIPTION_SECRET_ALIAS: &str = "transcription.deepgram";
/// ADE key ceiling (`DEEPGRAM_MAX_API_KEY_LENGTH`).
const TRANSCRIPTION_SECRET_MAX_LEN: usize = 512;

const COMMAND_MENU_ORIGIN_PREFIX: &str = "command-door-v1:";
const COMMAND_MENU_ORIGIN_NAMESPACE: &str = "command-door-";

/// Private one-response sink used to reuse canonical operation handlers while
/// `command.invoke` wraps their exact receipt body in its own correlated
/// response. It is never registered for event delivery.
#[derive(Default)]
struct CommandResponseCapture {
    frame: Mutex<Option<WireFrame>>,
}

impl FrameSink for CommandResponseCapture {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        let mut slot = self.frame.lock().map_err(|_| FrameSendError)?;
        if slot.is_some() {
            return Err(FrameSendError);
        }
        *slot = Some(frame);
        Ok(())
    }
}

#[derive(Clone)]
enum ParkedCommandContinuation {
    Compact,
    Rename(String),
    Model(String),
    Provider(String),
    Effort(Option<String>),
    Fast(bool),
}

impl ParkedCommandContinuation {
    fn receipt_kind(&self) -> CommandReceiptKind {
        match self {
            Self::Compact => CommandReceiptKind::Compact,
            Self::Rename(_) => CommandReceiptKind::Rename,
            Self::Model(_) | Self::Provider(_) => CommandReceiptKind::Model,
            Self::Effort(_) => CommandReceiptKind::Effort,
            Self::Fast(_) => CommandReceiptKind::Fast,
        }
    }

    fn canonical_method(&self) -> &'static str {
        match self {
            Self::Compact => "session.compact",
            Self::Rename(_) => "session.rename",
            Self::Model(_) | Self::Provider(_) => "session.select_model",
            Self::Effort(_) => "session.select_effort",
            Self::Fast(_) => "session.select_fast",
        }
    }

    fn slash_name(&self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Rename(_) => "rename",
            Self::Model(_) => "model",
            Self::Provider(_) => "provider",
            Self::Effort(_) => "effort",
            Self::Fast(_) => "fast",
        }
    }
}

enum CommandMenuLookup {
    Ordinary,
    Continuation(ResolvedCommandMenu),
    Invalid(String),
}

struct ResolvedCommandMenu {
    action: ParkedCommandContinuation,
    opening_generation: u64,
    confirm_new_epoch: bool,
}

struct CommandMenuOrigin<'a> {
    command: &'a str,
    invocation_key: Option<&'a str>,
    encoded_continuation: Option<&'a str>,
}

#[derive(Clone, Copy)]
enum CommandReceiptKind {
    Compact,
    Rename,
    Model,
    Effort,
    Fast,
}

impl CommandReceiptKind {
    fn accepts(self, body: &ResponseBody) -> bool {
        match self {
            Self::Compact => matches!(
                body,
                ResponseBody::SessionCompact { .. } | ResponseBody::SessionCompactOnBranch { .. }
            ),
            Self::Rename => matches!(body, ResponseBody::SessionRename { .. }),
            Self::Model => matches!(body, ResponseBody::SessionSelectModel { .. }),
            Self::Effort => matches!(body, ResponseBody::SessionSelectEffort { .. }),
            Self::Fast => matches!(body, ResponseBody::SessionSelectFast { .. }),
        }
    }
}

struct StoredCommandMenu {
    opening: haider_protocol::envelope::RawEnvelope,
    menu: Menu,
    answer: Option<DurableMenuAnswer>,
    closed: bool,
}

enum StoredCommandMenuLookup {
    Missing,
    Found(StoredCommandMenu),
    CommandIdConflict,
}

fn command_menu_options(values: impl IntoIterator<Item = String>) -> Vec<MenuOption> {
    values
        .into_iter()
        .map(|value| MenuOption {
            key: value.clone(),
            label: value,
            detail: None,
            decision: None,
        })
        .filter(|option| !option.key.trim().is_empty())
        .fold(Vec::new(), |mut options, option| {
            if !options
                .iter()
                .any(|existing: &MenuOption| existing.key == option.key)
            {
                options.push(option);
            }
            options
        })
}

fn command_menu_invocation_key(session_id: &SessionId, command_id: &CommandId) -> String {
    blake3::hash(format!("{}\0{}", session_id.as_str(), command_id.as_str()).as_bytes())
        .to_hex()
        .to_string()
}

/// `Ok(None)` is an ordinary menu. A reserved but unknown command-door
/// version is an error, never an ordinary menu that may be consumed silently.
fn command_menu_origin_parts(origin: &str) -> Result<Option<CommandMenuOrigin<'_>>, ()> {
    if let Some(rest) = origin.strip_prefix(COMMAND_MENU_ORIGIN_PREFIX) {
        let mut parts = rest.splitn(3, ':');
        let command = parts.next().unwrap_or_default();
        let invocation_key = parts.next();
        let continuation = parts.next();
        return if command.is_empty() {
            Err(())
        } else {
            Ok(Some(CommandMenuOrigin {
                command,
                invocation_key,
                encoded_continuation: continuation,
            }))
        };
    }
    if origin.starts_with(COMMAND_MENU_ORIGIN_NAMESPACE) {
        return Err(());
    }
    Ok(None)
}

fn encode_cache_confirmation(continuation: &ParkedCommandContinuation) -> Option<String> {
    let encoded =
        |value: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes());
    match continuation {
        ParkedCommandContinuation::Model(value) => Some(format!("model.{}", encoded(value))),
        ParkedCommandContinuation::Provider(value) => Some(format!("provider.{}", encoded(value))),
        ParkedCommandContinuation::Effort(value) => Some(format!(
            "effort.{}",
            encoded(value.as_deref().unwrap_or("default"))
        )),
        ParkedCommandContinuation::Fast(enabled) => {
            Some(format!("fast.{}", if *enabled { "on" } else { "off" }))
        }
        ParkedCommandContinuation::Compact | ParkedCommandContinuation::Rename(_) => None,
    }
}

fn decode_cache_confirmation(encoded: &str) -> Result<ParkedCommandContinuation, String> {
    let (kind, value) = encoded
        .split_once('.')
        .ok_or_else(|| "cache confirmation continuation is malformed".to_owned())?;
    let decoded = || {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| "cache confirmation continuation is not valid base64".to_owned())
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| "cache confirmation continuation is not UTF-8".to_owned())
            })
    };
    match kind {
        "model" => decoded().map(ParkedCommandContinuation::Model),
        "provider" => decoded().map(ParkedCommandContinuation::Provider),
        "effort" => decoded().map(|effort| {
            ParkedCommandContinuation::Effort((effort != "default").then_some(effort))
        }),
        "fast" if value == "on" => Ok(ParkedCommandContinuation::Fast(true)),
        "fast" if value == "off" => Ok(ParkedCommandContinuation::Fast(false)),
        _ => Err("unknown cache confirmation continuation; no action was taken".into()),
    }
}

pub(crate) fn transcription_secret_alias() -> haider_protocol::ids::CredentialAlias {
    haider_protocol::ids::CredentialAlias::new(TRANSCRIPTION_SECRET_ALIAS)
}

struct AttachmentValidationFailure {
    code: &'static str,
    message: String,
    data: Option<ErrorData>,
}

struct HeadlessRunLookup {
    session_id: haider_protocol::ids::SessionId,
    run_id: haider_protocol::ids::RunId,
    state: RunState,
    state_seq: u64,
    head_seq: u64,
    budget_exhausted: Option<haider_protocol::headless::RunBudgetExhaustedV1>,
    spec: haider_protocol::headless::HeadlessRunSpecV1,
}

/// Compact direct-agent metrics from the same sealed journal head carried by
/// `SessionSummary`. The live child path publishes the identical shape into
/// the parent journal; this summary copy is the cold/reconnect and `/usage`
/// main-agent fallback. The same fold also returns the model active at that
/// head: its seed is the metadata model and each durable `model_selected`
/// fact replaces it in sequence order.
#[allow(dead_code)]
async fn session_agent_metrics_truth(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    through_seq: u64,
    initial_model: &str,
) -> Result<
    (
        Option<haider_protocol::agent::AgentMetricsSnapshot>,
        Option<String>,
    ),
    HaiderError,
> {
    let mut folder = crate::usage_report::SessionFolder::new(initial_model);
    let mut since_seq = 0;
    while since_seq < through_seq {
        let page = store.read(session_id, since_seq, REPLAY_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                return Ok((
                    folder.primary_agent_snapshot(session_id, through_seq),
                    folder.active_model().map(str::to_owned),
                ));
            }
            since_seq = envelope.seq;
            advanced = true;
            folder.push(&envelope);
        }
        if !advanced {
            break;
        }
    }
    Ok((
        folder.primary_agent_snapshot(session_id, through_seq),
        folder.active_model().map(str::to_owned),
    ))
}

struct FleetChildTruth {
    state: haider_rpc::FleetAgentStateWire,
    metrics: Option<haider_protocol::agent::AgentMetricsSnapshot>,
}

/// Reduces one child's exact durable run and direct metrics from the same
/// sealed journal head. Delegation bookkeeping is only the fallback for the
/// launch-crash window; it cannot distinguish failed from cancelled.
async fn fleet_child_truth(
    store: &dyn StoreHandle,
    record: &haider_core::DelegationRecord,
    through_seq: u64,
    initial_model: &str,
) -> Result<FleetChildTruth, HaiderError> {
    let mut folder = crate::usage_report::SessionFolder::new(initial_model);
    let mut latest_state = None;
    let mut since_seq = 0;
    while since_seq < through_seq {
        let page = store
            .read(&record.child_session_id, since_seq, REPLAY_PAGE_SIZE)
            .await?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                break;
            }
            since_seq = envelope.seq;
            advanced = true;
            if envelope.run_id.as_ref() == Some(&record.child_run_id)
                && let Ok(EventPayload::RunState(state)) = envelope.payload.decode_event()
            {
                latest_state = Some(state);
            }
            folder.push(&envelope);
        }
        if !advanced {
            break;
        }
    }
    let state = fleet_agent_state(record, latest_state.as_ref());
    let metrics = folder.agent_snapshot(
        &record.child_session_id,
        Some(&record.agent_id),
        through_seq,
    );
    Ok(FleetChildTruth { state, metrics })
}

fn fleet_agent_state(
    record: &haider_core::DelegationRecord,
    state: Option<&RunState>,
) -> haider_rpc::FleetAgentStateWire {
    use haider_rpc::FleetAgentStateWire as FleetState;
    match state {
        Some(RunState::Queued) => FleetState::Queued,
        Some(RunState::Done) => FleetState::Done,
        Some(RunState::Errored) => FleetState::Failed,
        Some(RunState::Cancelled) => FleetState::Cancelled,
        Some(state) if state.is_parked() => FleetState::Waiting,
        Some(_) => FleetState::Live,
        None => match record.state {
            haider_core::DelegationState::Spawned => FleetState::Queued,
            haider_core::DelegationState::Running => FleetState::Live,
            haider_core::DelegationState::Reported | haider_core::DelegationState::Collected => {
                if record.report.as_ref().is_some_and(|report| {
                    report.verified == haider_protocol::agent::ReportVerification::Red
                }) {
                    FleetState::Failed
                } else {
                    FleetState::Done
                }
            }
        },
    }
}

struct FleetFlatNode {
    record: haider_core::DelegationRecord,
    state: haider_rpc::FleetAgentStateWire,
    metrics: Option<haider_protocol::agent::AgentMetricsSnapshot>,
    direct_child_count: u32,
}

fn fleet_snapshot(
    session_id: SessionId,
    generated_at_ms: u64,
    nodes: Vec<FleetFlatNode>,
    truncated: bool,
) -> Result<haider_rpc::SessionFleetSnapshot, HaiderError> {
    let mut states = haider_rpc::FleetStateCountsWire::default();
    let mut max_depth = 0_u32;
    for node in &nodes {
        max_depth = max_depth.max(node.record.depth);
        let count = match node.state {
            haider_rpc::FleetAgentStateWire::Queued => &mut states.queued,
            haider_rpc::FleetAgentStateWire::Live => &mut states.live,
            haider_rpc::FleetAgentStateWire::Waiting => &mut states.waiting,
            haider_rpc::FleetAgentStateWire::Done => &mut states.done,
            haider_rpc::FleetAgentStateWire::Failed => &mut states.failed,
            haider_rpc::FleetAgentStateWire::Cancelled => &mut states.cancelled,
            _ => continue,
        };
        *count = count.saturating_add(1);
    }
    let (metrics, metrics_complete) = fleet_metrics_totals(&nodes, generated_at_ms);
    let rollup = haider_rpc::FleetRollupWire {
        node_count: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
        states,
        max_depth,
        metrics,
        metrics_complete,
        complete: !truncated,
    };

    let mut children_by_parent = HashMap::<SessionId, Vec<usize>>::new();
    let mut wire_nodes = nodes
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            children_by_parent
                .entry(node.record.parent_session_id.clone())
                .or_default()
                .push(index);
            let identity = fleet_node_identity(&node.record.manifest);
            Some(haider_rpc::FleetNodeWire {
                agent_id: node.record.agent_id,
                session_id: node.record.child_session_id,
                callsign: identity.callsign,
                model: identity.model,
                provider: identity.provider,
                task: node.record.task,
                depth: node.record.depth,
                parent_session_id: node.record.parent_session_id,
                parent_agent_id: node.record.parent_agent_id,
                state: node.state,
                metrics: node.metrics,
                folded_children: node.direct_child_count,
                children: Vec::new(),
            })
        })
        .collect::<Vec<_>>();

    fn take_tree(
        index: usize,
        nodes: &mut [Option<haider_rpc::FleetNodeWire>],
        children_by_parent: &HashMap<SessionId, Vec<usize>>,
    ) -> Result<haider_rpc::FleetNodeWire, HaiderError> {
        let mut node = nodes.get_mut(index).and_then(Option::take).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                "bounded fleet graph contains a duplicate node index",
                false,
            )
        })?;
        if let Some(children) = children_by_parent.get(&node.session_id) {
            let mut nested = Vec::with_capacity(children.len());
            for child in children {
                nested.push(take_tree(*child, nodes, children_by_parent)?);
            }
            let returned_children = u32::try_from(nested.len()).unwrap_or(u32::MAX);
            node.folded_children = node
                .folded_children
                .checked_sub(returned_children)
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "bounded fleet graph contains more children than durable relation truth",
                        false,
                    )
                })?;
            node.children = nested;
        }
        Ok(node)
    }

    let root_indices = children_by_parent
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    let mut roots = Vec::with_capacity(root_indices.len());
    for index in root_indices {
        roots.push(take_tree(index, &mut wire_nodes, &children_by_parent)?);
    }
    Ok(haider_rpc::SessionFleetSnapshot {
        session_id,
        generated_at_ms,
        node_limit: haider_rpc::FLEET_MAX_NODES,
        depth_limit: haider_rpc::FLEET_MAX_DEPTH,
        roots,
        rollup,
        truncated,
    })
}

fn fleet_metrics_totals(
    nodes: &[FleetFlatNode],
    generated_at_ms: u64,
) -> (haider_rpc::FleetMetricsTotalsWire, bool) {
    let metrics_complete = nodes.iter().all(|node| {
        node.metrics
            .as_ref()
            .is_some_and(|snapshot| snapshot.usage.is_some())
    });
    let mut totals = haider_rpc::FleetMetricsTotalsWire::default();
    for snapshot in nodes.iter().filter_map(|node| node.metrics.as_ref()) {
        totals.elapsed_ms = totals.elapsed_ms.saturating_add(
            snapshot
                .terminal_at_ms
                .unwrap_or(generated_at_ms)
                .saturating_sub(snapshot.started_at_ms),
        );
        totals.tool_attempts = totals.tool_attempts.saturating_add(snapshot.tool_attempts);
    }
    if nodes.is_empty() || !metrics_complete {
        return (totals, metrics_complete);
    }

    let mut usage = haider_protocol::agent::AgentUsageMetrics {
        all_lanes_priced: true,
        ..haider_protocol::agent::AgentUsageMetrics::default()
    };
    let mut metered_cost = 0_u64;
    let mut api_cost = 0_u64;
    let mut metered_priced = true;
    let mut api_priced = true;
    for item in nodes
        .iter()
        .filter_map(|node| node.metrics.as_ref())
        .filter_map(|snapshot| snapshot.usage.as_ref())
    {
        usage.logical_input_tokens = usage
            .logical_input_tokens
            .saturating_add(item.logical_input_tokens);
        usage.billed_output_tokens = usage
            .billed_output_tokens
            .saturating_add(item.billed_output_tokens);
        usage.additional_reasoning_tokens = usage
            .additional_reasoning_tokens
            .saturating_add(item.additional_reasoning_tokens);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(item.cache_read_tokens);
        usage.cache_write_tokens = usage
            .cache_write_tokens
            .saturating_add(item.cache_write_tokens);
        usage.has_metered_lanes |= item.has_metered_lanes;
        usage.has_oauth_lanes |= item.has_oauth_lanes;
        usage.all_lanes_priced &= item.all_lanes_priced;
        if item.has_metered_lanes {
            if let Some(cost) = item.metered_cost_microusd {
                metered_cost = metered_cost.saturating_add(cost);
            } else {
                metered_priced = false;
            }
        }
        if let Some(cost) = item.api_equivalent_cost_microusd {
            api_cost = api_cost.saturating_add(cost);
        } else {
            api_priced = false;
        }
    }
    usage.metered_cost_microusd =
        (usage.has_metered_lanes && metered_priced).then_some(metered_cost);
    usage.api_equivalent_cost_microusd = (usage.all_lanes_priced && api_priced).then_some(api_cost);
    usage.cache_hit_basis_points = nodes
        .iter()
        .filter_map(|node| node.metrics.as_ref())
        .filter_map(|snapshot| snapshot.usage.as_ref())
        .all(|item| item.cache_hit_basis_points.is_some())
        .then(|| {
            usage
                .cache_read_tokens
                .saturating_mul(10_000)
                .checked_div(usage.logical_input_tokens)
                .map_or(0, |cache_hit_basis_points| {
                    u32::try_from(cache_hit_basis_points)
                        .unwrap_or(10_000)
                        .min(10_000)
                })
        });
    totals.usage = Some(usage);
    (totals, metrics_complete)
}

fn fleet_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn provider_inventory_needs_refresh(summary: &ProviderSummaryWire, model: &str) -> bool {
    let misses_known_inventory =
        !summary.models.is_empty() && !summary.models.iter().any(|known| known == model);
    misses_known_inventory
        || summary
            .inventory_fetched_at_ms
            .is_some_and(|fetched_at_ms| {
                fleet_now_ms().saturating_sub(fetched_at_ms) >= haider_rpc::MODEL_INVENTORY_TTL_MS
            })
}

/// Projects one replayed truth into the summary's additive wire fields.
///
/// Zero-honesty law: `Some(0)` tokens are reported EXCLUSIVELY for truly
/// empty sessions (no committed user turn and no durable snapshot — zero
/// is then exact). A session with committed turns but no snapshot reports
/// `None`: unknown is never rendered as zero.
fn summary_footprint_fields(
    turns: u64,
    footprint: Option<&ContextFootprint>,
) -> (Option<u64>, Option<ContextFootprintTruth>) {
    match footprint {
        Some(footprint) => (Some(footprint.used_tokens), Some(footprint.truth)),
        None if turns == 0 => (Some(0), Some(ContextFootprintTruth::Exact)),
        None => (None, None),
    }
}

pub(super) fn filter_provider_summaries(
    providers: Vec<haider_rpc::ProviderSummaryWire>,
    provider: Option<&str>,
) -> Vec<haider_rpc::ProviderSummaryWire> {
    providers
        .into_iter()
        .filter(|summary| provider.is_none_or(|provider| summary.provider == provider))
        .collect()
}

fn lockdown_status_wire(
    mut status: crate::lockdown::LockdownStatus,
    policy: Option<crate::auto_hermetic::ProviderLockdownPolicy>,
    active: bool,
) -> haider_rpc::LockdownStatusWire {
    if let Some(policy) = policy
        && policy.is_lockdown()
    {
        status.tools_allowed = crate::auto_hermetic::tools_for(policy);
    }
    haider_rpc::LockdownStatusWire {
        provider: status.provider,
        activation: policy.and_then(|policy| policy.activation(active)),
        reason: policy
            .and_then(|policy| policy.reason(active))
            .map(str::to_owned),
        tools_allowed: status.tools_allowed,
        quota_used: status.quota_used,
        quota_limit: status.quota_limit,
    }
}

struct SessionLockdownEnvelope {
    provider: String,
    tools_allowed: Vec<String>,
}

impl std::fmt::Display for SessionLockdownEnvelope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.provider)
    }
}

fn lockdown_refusal_error_data(
    envelope: &SessionLockdownEnvelope,
    tool: &str,
    reason: &str,
) -> haider_rpc::ErrorData {
    haider_rpc::ErrorData::RefusedByLockdown {
        provider: envelope.provider.clone(),
        tool: tool.to_owned(),
        reason: reason.to_owned(),
        tools_allowed: envelope.tools_allowed.clone(),
    }
}

#[cfg(test)]
#[test]
fn auto_hermetic_refusal_data_never_advertises_an_egress_tool() {
    let envelope = SessionLockdownEnvelope {
        provider: "local".into(),
        tools_allowed: crate::auto_hermetic::tools_for(
            crate::auto_hermetic::ProviderLockdownPolicy::AutoHermetic,
        ),
    };
    let haider_rpc::ErrorData::RefusedByLockdown { tools_allowed, .. } =
        lockdown_refusal_error_data(&envelope, "web_fetch", "refused")
    else {
        panic!("lockdown refusal must retain its typed error data");
    };
    assert!(tools_allowed.iter().any(|tool| tool == "fs_read"));
    assert!(!tools_allowed.iter().any(|tool| tool == "web_fetch"));
    assert!(!tools_allowed.iter().any(|tool| tool == "peer_list"));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectSshSession<'a> {
    OutsideSession,
    Session(&'a SessionId),
    Ambiguous,
}

fn direct_ssh_session(sessions: &[SessionId]) -> DirectSshSession<'_> {
    match sessions {
        [] => DirectSshSession::OutsideSession,
        [session_id] => DirectSshSession::Session(session_id),
        _ => DirectSshSession::Ambiguous,
    }
}

#[derive(Debug, serde::Serialize)]
struct ObservedRun {
    state: RunState,
    seq: u64,
    branch_id: Option<BranchId>,
}

#[derive(serde::Serialize)]
struct ObserveProjection {
    event_limit: usize,
    event_kinds: VecDeque<String>,
    title: Option<String>,
    runs: HashMap<RunId, ObservedRun>,
    menus: BTreeMap<String, haider_rpc::ObserveMenuWire>,
    subagents: BTreeMap<String, haider_rpc::ObserveSubagentWire>,
    footprint: Option<ContextFootprint>,
    main_head_node_id: Option<haider_protocol::ids::NodeId>,
    main_head_seq: u64,
    branches: HashMap<haider_protocol::ids::BranchId, haider_protocol::branch::BranchDescriptor>,
    updated_at_ms: u64,
    graphs: haider_protocol::graph::GraphReductions,
}

/// Daemon-lifetime observe/roster fold. Missing sessions rebuild from the
/// journal oracle; once installed, the session actor extends the fold with
/// every committed envelope before waking roster consumers.
pub(super) struct ObserveDigestCache {
    state: Mutex<ObserveCacheState>,
    next_build: AtomicU64,
    building_admission: Arc<Semaphore>,
}

const MAX_OBSERVE_READY_ENTRIES: usize = 256;
const MAX_OBSERVE_READY_BYTES: usize = 32 * 1024 * 1024;
const MAX_OBSERVE_BUILDING_ENTRIES: usize = 8;
const MAX_OBSERVED_RUNS_PER_SESSION: usize = 16;

#[derive(Default)]
struct ObserveCacheState {
    sessions: HashMap<SessionId, Box<ObserveCacheEntry>>,
    ready_count: usize,
    ready_bytes: usize,
    touch_clock: u64,
}

impl Default for ObserveDigestCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(ObserveCacheState::default()),
            next_build: AtomicU64::new(1),
            building_admission: Arc::new(Semaphore::new(MAX_OBSERVE_BUILDING_ENTRIES)),
        }
    }
}

enum ObserveCacheEntry {
    Building {
        token: u64,
        pending: Vec<RawEnvelope>,
        completion: watch::Sender<Option<Arc<ObserveFoldSnapshot>>>,
        _permit: OwnedSemaphorePermit,
    },
    Ready {
        fold: ObserveFold,
        deep_bytes: usize,
        last_touched: u64,
    },
}

struct ObserveFold {
    head_seq: u64,
    projection: ObserveProjection,
    turns: u64,
    metrics: crate::usage_report::SessionFolder,
}

#[derive(Clone)]
struct ObserveFoldSnapshot {
    head_seq: u64,
    title: Option<String>,
    run_state: haider_rpc::ObserveRunStateWire,
    /// Identity of the run `run_state` describes; `None` when idle.
    run_id: Option<RunId>,
    active_branch_id: Option<BranchId>,
    branches: Vec<haider_protocol::branch::BranchDescriptor>,
    main_head_node_id: Option<haider_protocol::ids::NodeId>,
    main_head_seq: u64,
    footprint: Option<ContextFootprint>,
    pending_menus: Vec<haider_rpc::ObserveMenuWire>,
    subagents: Vec<haider_rpc::ObserveSubagentWire>,
    updated_at_ms: u64,
    event_kinds: Vec<String>,
    turns: u64,
    agent_metrics: Option<haider_protocol::agent::AgentMetricsSnapshot>,
    last_model: Option<String>,
    workflow: Option<haider_protocol::graph::GraphStatus>,
}

enum CacheStart {
    Ready(ObserveFoldSnapshot),
    BuildAndInstall(u64),
    WaitExisting(watch::Receiver<Option<Arc<ObserveFoldSnapshot>>>),
}

struct ObserveBuildGuard {
    cache: Arc<ObserveDigestCache>,
    session_id: SessionId,
    token: u64,
    armed: bool,
}

impl ObserveBuildGuard {
    fn new(cache: Arc<ObserveDigestCache>, session_id: SessionId, token: u64) -> Self {
        Self {
            cache,
            session_id,
            token,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ObserveBuildGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cache.abandon(&self.session_id, self.token);
        }
    }
}

impl ObserveFold {
    fn new(initial_model: &str) -> Self {
        Self {
            head_seq: 0,
            projection: ObserveProjection::new(100),
            turns: 0,
            metrics: crate::usage_report::SessionFolder::new(initial_model),
        }
    }

    fn apply(&mut self, envelope: RawEnvelope) {
        self.head_seq = self.head_seq.max(envelope.seq);
        if envelope.agent_id.is_none()
            && envelope.payload.decode_event().is_ok_and(|payload| {
                matches!(
                    payload,
                    EventPayload::UserMessage { .. } | EventPayload::PeerMessage(_)
                )
            })
        {
            self.turns = self.turns.saturating_add(1);
        }
        self.metrics.push(&envelope);
        self.projection.apply(envelope);
    }

    fn snapshot(&self, session_id: &SessionId) -> ObserveFoldSnapshot {
        let selected = select_observed_run(&self.projection.runs);
        let run_state = selected.map_or(haider_rpc::ObserveRunStateWire::Idle, |(_, run)| {
            observe_run_state(&run.state)
        });
        let run_id = selected.map(|(run_id, _)| run_id.clone());
        let active_branch_id = selected.and_then(|(_, run)| run.branch_id.clone());
        let mut branches = self
            .projection
            .branches
            .values()
            .cloned()
            .collect::<Vec<_>>();
        branches.sort_by_key(|branch| branch.created_seq);
        ObserveFoldSnapshot {
            head_seq: self.head_seq,
            title: self.projection.title.clone(),
            run_state,
            run_id,
            active_branch_id,
            branches,
            main_head_node_id: self.projection.main_head_node_id.clone(),
            main_head_seq: self.projection.main_head_seq,
            footprint: self.projection.footprint.clone(),
            pending_menus: self.projection.menus.values().cloned().collect(),
            subagents: self.projection.subagents.values().cloned().collect(),
            updated_at_ms: self.projection.updated_at_ms,
            event_kinds: self.projection.event_kinds.iter().cloned().collect(),
            turns: self.turns,
            agent_metrics: self
                .metrics
                .primary_agent_snapshot(session_id, self.head_seq),
            last_model: self.metrics.active_model().map(str::to_owned),
            workflow: self
                .projection
                .graphs
                .active()
                .and_then(|reduction| reduction.status.clone()),
        }
    }

    fn deep_owned_bytes(&self) -> usize {
        // Serialized bytes conservatively charge every recursively owned
        // string/value (including opaque graph internals) twice: once for its
        // used bytes and once for decoded-buffer headroom. Every collection's
        // actual allocation slab/node charge and every accessible spare String
        // capacity is added separately below. This intentionally overcounts;
        // the 32 MiB ceiling is a retained-heap ceiling, not a compact-wire
        // estimate.
        serialized_owned_charge(&self.projection)
            .saturating_add(std::mem::size_of::<Self>())
            .saturating_add(observe_projection_allocation_charge(&self.projection))
            .saturating_add(self.metrics.deep_owned_bytes())
    }
}

fn serialized_owned_charge<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |payload| payload.len().saturating_mul(2))
}

fn vec_slab<T>(values: &Vec<T>) -> usize {
    values.capacity().saturating_mul(std::mem::size_of::<T>())
}

fn hash_slab<K, V>(values: &HashMap<K, V>) -> usize {
    values.capacity().saturating_mul(
        std::mem::size_of::<(K, V)>()
            // Hashbrown-style control byte; one per bucket is a conservative
            // portable charge without depending on its private layout.
            .saturating_add(1),
    )
}

fn btree_node_charge<K, V>(values: &BTreeMap<K, V>) -> usize {
    // std's BTree nodes reserve eleven key/value slots even when a sparse
    // root owns one entry. Charge one complete node (plus generous edge and
    // metadata space) per occupied entry. A real tree has fewer nodes than
    // entries, so this covers fixed-capacity slack without relying on std's
    // private node layout.
    let conservative_node = 11_usize
        .saturating_mul(std::mem::size_of::<(K, V)>())
        .saturating_add(16_usize.saturating_mul(std::mem::size_of::<usize>()));
    values.len().saturating_mul(conservative_node)
}

fn string_spare(value: &String) -> usize {
    value.capacity().saturating_sub(value.len())
}

fn menu_allocation_charge(menu: &haider_rpc::ObserveMenuWire) -> usize {
    let mut total = string_spare(&menu.kind)
        .saturating_add(string_spare(&menu.title))
        .saturating_add(vec_slab(&menu.body))
        .saturating_add(vec_slab(&menu.options));
    for line in &menu.body {
        total = total.saturating_add(string_spare(line));
    }
    for option in &menu.options {
        total = total
            .saturating_add(string_spare(&option.key))
            .saturating_add(string_spare(&option.label))
            .saturating_add(option.detail.as_ref().map_or(0, string_spare))
            .saturating_add(option.decision.as_ref().map_or(0, string_spare));
    }
    if let Some(description) = &menu.permission_description {
        total = total.saturating_add(string_spare(description));
    }
    if let Some(presentation) = &menu.presentation {
        total = total
            .saturating_add(string_spare(&presentation.title))
            .saturating_add(string_spare(&presentation.detail))
            .saturating_add(
                presentation
                    .provider_request_id
                    .as_ref()
                    .map_or(0, string_spare),
            )
            .saturating_add(vec_slab(&presentation.allowed_actions));
    }
    total
}

fn subagent_allocation_charge(subagent: &haider_rpc::ObserveSubagentWire) -> usize {
    let mut total = subagent
        .agent_id
        .0
        .capacity()
        .saturating_sub(subagent.agent_id.0.len())
        .saturating_add(subagent.callsign.as_ref().map_or(0, string_spare))
        .saturating_add(string_spare(&subagent.task))
        .saturating_add(string_spare(&subagent.state))
        .saturating_add(subagent.provider.as_ref().map_or(0, string_spare));
    if let Some(lockdown) = &subagent.lockdown {
        total = total
            .saturating_add(lockdown.provider.as_ref().map_or(0, string_spare))
            .saturating_add(lockdown.reason.as_ref().map_or(0, string_spare))
            .saturating_add(vec_slab(&lockdown.tools_allowed));
        for tool in &lockdown.tools_allowed {
            total = total.saturating_add(string_spare(tool));
        }
    }
    total
}

fn graph_allocation_charge(graphs: &haider_protocol::graph::GraphReductions) -> usize {
    let mut total = hash_slab(&graphs.by_graph).saturating_add(hash_slab(&graphs.run_sets));
    for reduction in graphs.by_graph.values() {
        total = total
            .saturating_add(vec_slab(&reduction.evidence))
            .saturating_add(vec_slab(&reduction.finalization_deferrals))
            .saturating_add(vec_slab(&reduction.finalization_menus))
            .saturating_add(vec_slab(&reduction.template_nodes));
        if let Some(status) = &reduction.status {
            total = total
                .saturating_add(vec_slab(&status.ready_nodes))
                .saturating_add(vec_slab(&status.nodes))
                .saturating_add(vec_slab(&status.pending_menus));
            for node in &status.nodes {
                total = total.saturating_add(vec_slab(&node.evidence_slots));
            }
            if let Some(run_set) = &status.run_set {
                total = total.saturating_add(vec_slab(&run_set.children));
            }
        }
        for deferred in &reduction.finalization_deferrals {
            total = total.saturating_add(vec_slab(&deferred.unmet_nodes));
        }
        for node in &reduction.template_nodes {
            total = total
                .saturating_add(vec_slab(&node.depends_on))
                .saturating_add(vec_slab(&node.verify_slots));
        }
    }
    for run_set in graphs.run_sets.values() {
        total = total.saturating_add(vec_slab(&run_set.children));
    }
    total
}

fn observe_projection_allocation_charge(projection: &ObserveProjection) -> usize {
    let mut total = std::mem::size_of::<ObserveProjection>()
        .saturating_add(
            projection
                .event_kinds
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
        )
        .saturating_add(hash_slab(&projection.runs))
        .saturating_add(btree_node_charge(&projection.menus))
        .saturating_add(btree_node_charge(&projection.subagents))
        .saturating_add(hash_slab(&projection.branches))
        .saturating_add(projection.title.as_ref().map_or(0, string_spare))
        .saturating_add(graph_allocation_charge(&projection.graphs));
    for kind in &projection.event_kinds {
        total = total.saturating_add(string_spare(kind));
    }
    for (run_id, run) in &projection.runs {
        total = total
            .saturating_add(run_id.0.capacity().saturating_sub(run_id.0.len()))
            .saturating_add(run.branch_id.as_ref().map_or(0, |branch| {
                branch.0.capacity().saturating_sub(branch.0.len())
            }));
    }
    for (key, menu) in &projection.menus {
        total = total
            .saturating_add(string_spare(key))
            .saturating_add(menu_allocation_charge(menu));
    }
    for (key, subagent) in &projection.subagents {
        total = total
            .saturating_add(string_spare(key))
            .saturating_add(subagent_allocation_charge(subagent));
    }
    for (branch_id, branch) in &projection.branches {
        total = total
            .saturating_add(branch_id.0.capacity().saturating_sub(branch_id.0.len()))
            .saturating_add(string_spare(&branch.name));
    }
    total
}

fn ready_entry_deep_bytes(session_id: &SessionId, fold: &ObserveFold) -> usize {
    std::mem::size_of::<SessionId>()
        .saturating_add(session_id.as_str().len())
        .saturating_add(std::mem::size_of::<ObserveCacheEntry>())
        .saturating_add(fold.deep_owned_bytes())
}

impl ObserveFoldSnapshot {
    fn digest(
        &self,
        session_id: SessionId,
        worker_generation: u64,
        metadata: Option<haider_protocol::session::SessionMetadataV1>,
        event_limit: usize,
        include_summary: bool,
    ) -> haider_rpc::SessionObserveDigest {
        let title = self.title.clone().unwrap_or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| {
                    std::path::Path::new(&metadata.cwd)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(ToOwned::to_owned)
                })
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| session_id.as_str().to_owned())
        });
        let event_start = self.event_kinds.len().saturating_sub(event_limit);
        haider_rpc::SessionObserveDigest {
            session_id,
            head_seq: self.head_seq,
            worker_generation,
            metadata,
            title,
            run_state: self.run_state,
            run_id: self.run_id.clone(),
            active_branch_id: self.active_branch_id.clone(),
            branches: self.branches.clone(),
            main_head_node_id: self.main_head_node_id.clone(),
            main_head_seq: self.main_head_seq,
            latest_context_footprint: self.footprint.clone(),
            pending_menus: self.pending_menus.clone(),
            subagents: self.subagents.clone(),
            lockdown: None,
            updated_at_ms: self.updated_at_ms,
            last_event_kinds: self.event_kinds[event_start..].to_vec(),
            turn_count: include_summary.then_some(self.turns),
            agent_metrics: include_summary
                .then(|| self.agent_metrics.clone())
                .flatten(),
            needs_input: needs_input(self.run_state, &self.pending_menus),
            workflow: self.workflow.clone(),
        }
    }
}

impl ObserveDigestCache {
    async fn start(&self, session_id: &SessionId, sealed_head: u64) -> CacheStart {
        let mut permit = None;
        loop {
            let needs_admission = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.touch_clock = state.touch_clock.saturating_add(1);
                let touched = state.touch_clock;
                match state.sessions.get_mut(session_id).map(Box::as_mut) {
                    Some(ObserveCacheEntry::Ready {
                        fold, last_touched, ..
                    }) if fold.head_seq == sealed_head => {
                        *last_touched = touched;
                        return CacheStart::Ready(fold.snapshot(session_id));
                    }
                    Some(ObserveCacheEntry::Building { completion, .. }) => {
                        return CacheStart::WaitExisting(completion.subscribe());
                    }
                    Some(ObserveCacheEntry::Ready { .. }) | None if permit.is_some() => {
                        let Some(building_permit) = permit.take() else {
                            continue;
                        };
                        state.remove_ready(session_id);
                        let token = self.next_build.fetch_add(1, Ordering::Relaxed);
                        let (completion, _) = watch::channel(None);
                        state.sessions.insert(
                            session_id.clone(),
                            Box::new(ObserveCacheEntry::Building {
                                token,
                                pending: Vec::new(),
                                completion,
                                _permit: building_permit,
                            }),
                        );
                        return CacheStart::BuildAndInstall(token);
                    }
                    Some(ObserveCacheEntry::Ready { .. }) | None => true,
                }
            };
            if needs_admission {
                permit = Arc::clone(&self.building_admission)
                    .acquire_owned()
                    .await
                    .ok();
            }
        }
    }

    fn install(
        &self,
        session_id: SessionId,
        mut fold: ObserveFold,
        build_token: u64,
    ) -> ObserveFoldSnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(
            state.sessions.get(&session_id).map(Box::as_ref),
            Some(ObserveCacheEntry::Building { token, .. }) if *token == build_token
        ) {
            return fold.snapshot(&session_id);
        }
        let Some(entry) = state.sessions.remove(&session_id) else {
            return fold.snapshot(&session_id);
        };
        let ObserveCacheEntry::Building {
            pending,
            completion,
            ..
        } = *entry
        else {
            unreachable!("exact build token selected a non-building entry")
        };
        let mut contiguous = true;
        for envelope in pending {
            if envelope.seq > fold.head_seq {
                if envelope.seq != fold.head_seq.saturating_add(1) {
                    // A gap means a writer bypassed the live commit seam.
                    // Leave the cache absent so the next read replays the
                    // deterministic journal oracle instead of guessing.
                    contiguous = false;
                    break;
                }
                fold.apply(envelope);
            }
        }
        let snapshot = Arc::new(fold.snapshot(&session_id));
        completion.send_replace(Some(Arc::clone(&snapshot)));
        if contiguous {
            state.insert_ready(session_id, fold);
        }
        (*snapshot).clone()
    }

    fn abandon(&self, session_id: &SessionId, token: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            state.sessions.get(session_id).map(Box::as_ref),
            Some(ObserveCacheEntry::Building {
                token: current,
                ..
            }) if *current == token
        ) {
            state.sessions.remove(session_id);
        }
    }

    pub(super) fn observe_committed(&self, envelopes: &[RawEnvelope]) {
        let Some(first) = envelopes.first() else {
            return;
        };
        let session_id = &first.session_id;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.touch_clock = state.touch_clock.saturating_add(1);
        let touched = state.touch_clock;
        let Some(entry) = state.sessions.get_mut(session_id) else {
            return;
        };
        let mut invalidate = false;
        let mut ready_bytes = None;
        match entry.as_mut() {
            ObserveCacheEntry::Building { pending, .. } => pending.extend_from_slice(envelopes),
            ObserveCacheEntry::Ready {
                fold,
                deep_bytes,
                last_touched,
            } => {
                let old_bytes = *deep_bytes;
                for envelope in envelopes {
                    if envelope.seq <= fold.head_seq {
                        continue;
                    }
                    if envelope.seq != fold.head_seq.saturating_add(1) {
                        invalidate = true;
                        break;
                    }
                    fold.apply(envelope.clone());
                }
                if !invalidate {
                    *deep_bytes = ready_entry_deep_bytes(session_id, fold);
                    *last_touched = touched;
                    ready_bytes = Some((old_bytes, *deep_bytes));
                }
            }
        }
        if invalidate {
            state.remove_ready(session_id);
        } else if let Some((old_bytes, new_bytes)) = ready_bytes {
            state.ready_bytes = state
                .ready_bytes
                .saturating_sub(old_bytes)
                .saturating_add(new_bytes);
            state.enforce_ready_limits();
        }
    }

    pub(super) fn remove(&self, session_id: &SessionId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove_ready(session_id);
    }

    /// Evicts only the exact idle-head fold. A later append makes the head
    /// mismatch, so an old delayed release can never discard current state.
    pub(super) fn remove_ready_at_head(&self, session_id: &SessionId, head_seq: u64) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let released = match state.sessions.get(session_id).map(Box::as_ref) {
            Some(ObserveCacheEntry::Ready {
                fold, deep_bytes, ..
            }) if fold.head_seq == head_seq => *deep_bytes,
            _ => return 0,
        };
        state.remove_ready(session_id);
        released
    }

    #[cfg(test)]
    fn stats(&self) -> (usize, usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let building = state
            .sessions
            .values()
            .filter(|entry| matches!(entry.as_ref(), ObserveCacheEntry::Building { .. }))
            .count();
        (state.ready_count, building, state.ready_bytes)
    }

    pub(super) fn retention_stats(
        &self,
        session_id: &SessionId,
    ) -> (usize, usize, usize, usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let building = state
            .sessions
            .values()
            .filter(|entry| matches!(entry.as_ref(), ObserveCacheEntry::Building { .. }))
            .count();
        let (session_runs, session_bytes) = match state.sessions.get(session_id).map(Box::as_ref) {
            Some(ObserveCacheEntry::Ready {
                fold, deep_bytes, ..
            }) => (fold.projection.runs.len(), *deep_bytes),
            _ => (0, 0),
        };
        (
            state.ready_count,
            building,
            state.ready_bytes,
            session_runs,
            session_bytes,
        )
    }

    #[cfg(test)]
    fn contains(&self, session_id: &SessionId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sessions
            .contains_key(session_id)
    }
}

impl ObserveCacheState {
    fn remove_ready(&mut self, session_id: &SessionId) {
        if let Some(entry) = self.sessions.remove(session_id)
            && let ObserveCacheEntry::Ready { deep_bytes, .. } = *entry
        {
            self.ready_count = self.ready_count.saturating_sub(1);
            self.ready_bytes = self.ready_bytes.saturating_sub(deep_bytes);
        }
    }

    fn insert_ready(&mut self, session_id: SessionId, fold: ObserveFold) {
        self.remove_ready(&session_id);
        self.touch_clock = self.touch_clock.saturating_add(1);
        let deep_bytes = ready_entry_deep_bytes(&session_id, &fold);
        self.ready_count = self.ready_count.saturating_add(1);
        self.ready_bytes = self.ready_bytes.saturating_add(deep_bytes);
        self.sessions.insert(
            session_id,
            Box::new(ObserveCacheEntry::Ready {
                fold,
                deep_bytes,
                last_touched: self.touch_clock,
            }),
        );
        self.enforce_ready_limits();
    }

    fn enforce_ready_limits(&mut self) {
        self.enforce_ready_limits_with(MAX_OBSERVE_READY_ENTRIES, MAX_OBSERVE_READY_BYTES);
    }

    fn enforce_ready_limits_with(&mut self, max_entries: usize, max_bytes: usize) {
        while self.ready_count > max_entries || self.ready_bytes > max_bytes {
            let victim = self
                .sessions
                .iter()
                .filter_map(|(session_id, entry)| match entry.as_ref() {
                    ObserveCacheEntry::Ready { last_touched, .. } => {
                        Some((session_id.clone(), *last_touched))
                    }
                    ObserveCacheEntry::Building { .. } => None,
                })
                .min_by_key(|(_, touched)| *touched)
                .map(|(session_id, _)| session_id);
            let Some(victim) = victim else {
                break;
            };
            self.remove_ready(&victim);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod observe_cache_retention_tests {
    use super::*;
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{DeviceId, EventId};

    fn fold(session_id: &SessionId, seq: u64, title_bytes: usize) -> ObserveFold {
        let mut fold = ObserveFold::new("fake-model");
        fold.projection.title = Some("t".repeat(title_bytes));
        fold.apply(EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("observe-retain-{session_id}-{seq}")),
            seq,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("observe-retain-test"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: seq,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::json!({"type":"observe_retention_probe"}).into(),
        });
        fold
    }

    #[tokio::test]
    async fn observe_cache_ready_count_evicts_lru_at_256() {
        let cache = ObserveDigestCache::default();
        for index in 0..MAX_OBSERVE_READY_ENTRIES {
            let session_id = SessionId::new(format!("observe-ready-{index}"));
            cache
                .state
                .lock()
                .expect("cache state")
                .insert_ready(session_id.clone(), fold(&session_id, 1, 8));
        }
        let oldest = SessionId::new("observe-ready-0");
        assert!(matches!(
            cache.start(&oldest, 1).await,
            CacheStart::Ready(_)
        ));
        let newcomer = SessionId::new("observe-ready-new");
        cache
            .state
            .lock()
            .expect("cache state")
            .insert_ready(newcomer.clone(), fold(&newcomer, 1, 8));

        assert_eq!(cache.stats().0, MAX_OBSERVE_READY_ENTRIES);
        assert!(cache.contains(&oldest), "a touched Ready remains resident");
        assert!(cache.contains(&newcomer));
        assert!(!cache.contains(&SessionId::new("observe-ready-1")));
    }

    #[test]
    fn idle_release_removes_only_the_exact_observe_head() {
        let cache = ObserveDigestCache::default();
        let session_id = SessionId::new("observe-idle-release");
        cache
            .state
            .lock()
            .expect("cache state")
            .insert_ready(session_id.clone(), fold(&session_id, 7, 128));
        assert_eq!(cache.remove_ready_at_head(&session_id, 6), 0);
        assert!(cache.contains(&session_id), "stale idle head cannot evict");
        assert!(cache.remove_ready_at_head(&session_id, 7) > 0);
        assert!(!cache.contains(&session_id));
        assert_eq!(cache.stats(), (0, 0, 0));
    }

    #[tokio::test]
    async fn observe_cache_ready_byte_cap_evicts_only_ready() {
        let cache = ObserveDigestCache::default();
        let building = SessionId::new("observe-building-survivor");
        let CacheStart::BuildAndInstall(_token) = cache.start(&building, 1).await else {
            panic!("first miss owns the build");
        };
        let ready = SessionId::new("observe-byte-victim");
        let mut state = cache.state.lock().expect("cache state");
        state.insert_ready(ready.clone(), fold(&ready, 1, 128));
        state.enforce_ready_limits_with(usize::MAX, 0);
        drop(state);

        assert_eq!(cache.stats(), (0, 1, 0));
        assert!(
            cache.contains(&building),
            "Building is never a capacity victim"
        );
        assert!(!cache.contains(&ready));
    }

    #[test]
    fn observe_cache_shipped_32_mib_path_evicts_an_oversize_ready() {
        assert_eq!(MAX_OBSERVE_READY_BYTES, 32 * 1024 * 1024);
        let session_id = SessionId::new("observe-shipped-byte-cap");
        let mut state = ObserveCacheState::default();
        // Serialized owned strings are conservatively charged at 2x, so this
        // one Ready entry crosses the real shipped 32 MiB threshold.
        state.insert_ready(
            session_id.clone(),
            fold(&session_id, 1, MAX_OBSERVE_READY_BYTES / 2 + 1),
        );
        assert_eq!(state.ready_count, 0);
        assert_eq!(state.ready_bytes, 0);
        assert!(!state.sessions.contains_key(&session_id));
    }

    #[test]
    fn deep_owned_bytes_charges_sparse_btree_root_capacity() {
        let mut sparse = BTreeMap::new();
        sparse.insert(String::from("key"), String::from("value"));
        assert!(
            btree_node_charge(&sparse) >= 11 * std::mem::size_of::<(String, String)>(),
            "one sparse root is charged for all fixed-capacity key/value slots"
        );
    }

    #[tokio::test]
    async fn observe_cache_building_admission_waits_without_busy() {
        let cache = Arc::new(ObserveDigestCache::default());
        let mut owners = Vec::new();
        for index in 0..MAX_OBSERVE_BUILDING_ENTRIES {
            let session_id = SessionId::new(format!("observe-builder-{index}"));
            let CacheStart::BuildAndInstall(token) = cache.start(&session_id, 1).await else {
                panic!("bounded builder is admitted");
            };
            owners.push((session_id, token));
        }
        let waiting_session = SessionId::new("observe-builder-waiting");
        let waiting_cache = Arc::clone(&cache);
        let waiter = tokio::spawn(async move { waiting_cache.start(&waiting_session, 1).await });
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "the ninth builder waits for admission"
        );

        cache.abandon(&owners[0].0, owners[0].1);
        assert!(matches!(
            waiter.await.expect("admission waiter joins"),
            CacheStart::BuildAndInstall(_)
        ));
    }

    #[tokio::test]
    async fn concurrent_observers_of_evicted_session_share_one_rebuild() {
        let cache = Arc::new(ObserveDigestCache::default());
        let session_id = SessionId::new("observe-single-flight");
        let CacheStart::BuildAndInstall(token) = cache.start(&session_id, 1).await else {
            panic!("first observer owns rebuild");
        };
        let CacheStart::WaitExisting(mut completion) = cache.start(&session_id, 1).await else {
            panic!("second observer joins existing rebuild");
        };
        let owner = cache.install(session_id.clone(), fold(&session_id, 1, 32), token);
        completion.changed().await.expect("single-flight publishes");
        let waiter = completion
            .borrow_and_update()
            .as_ref()
            .map(|snapshot| snapshot.as_ref().clone())
            .expect("waiter receives built snapshot");
        assert_eq!(
            serde_json::to_vec(&owner.digest(session_id.clone(), 1, None, 100, true))
                .expect("owner digest"),
            serde_json::to_vec(&waiter.digest(session_id, 1, None, 100, true))
                .expect("waiter digest")
        );
    }

    #[tokio::test]
    async fn observe_cache_remove_blocks_inflight_build_reinstallation() {
        let cache = ObserveDigestCache::default();
        let session_id = SessionId::new("observe-delete-build-race");
        let CacheStart::BuildAndInstall(token) = cache.start(&session_id, 1).await else {
            panic!("build starts");
        };
        cache.remove(&session_id);
        let _ = cache.install(session_id.clone(), fold(&session_id, 1, 8), token);
        assert!(!cache.contains(&session_id));
        assert_eq!(cache.stats(), (0, 0, 0));
    }

    #[tokio::test]
    async fn ready_eviction_rebuilds_byte_identical_digest_from_journal() {
        let root = tempfile::tempdir().expect("temp store");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let session_id = SessionId::new("observe-byte-identical-rebuild");
        hub.create_internal_session(SessionCreateCommand {
            command_id: "observe-byte-identical-create".into(),
            request_digest: "observe-byte-identical-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
                .expect("canonical cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new("observe-byte-identical-created"),
            device_id: DeviceId::new("observe-byte-identical-device"),
        })
        .await
        .expect("session");
        let metadata = hub.session_metadata(&session_id).await.expect("metadata");
        let first = cached_observe_snapshot(&hub, &session_id, "fake-model")
            .await
            .expect("warm digest")
            .digest(
                session_id.clone(),
                store.worker_generation(),
                metadata.clone(),
                100,
                true,
            );
        hub.inner.observe_digests.remove(&session_id);
        let rebuilt = cached_observe_snapshot(&hub, &session_id, "fake-model")
            .await
            .expect("rebuilt digest")
            .digest(
                session_id.clone(),
                store.worker_generation(),
                metadata,
                100,
                true,
            );

        assert_eq!(
            serde_json::to_vec(&first).expect("first bytes"),
            serde_json::to_vec(&rebuilt).expect("rebuilt bytes")
        );
        hub.delete_session(session_id.clone())
            .await
            .expect("delete session");
        assert_eq!(hub.inner.observe_digests.stats(), (0, 0, 0));
        assert!(
            !hub.inner
                .session_actor_tasks
                .lock()
                .expect("actor tasks")
                .contains_key(&session_id),
            "completed deleted actor handles are released immediately"
        );
        hub.shutdown().await.expect("hub shutdown");
        store.close().await.expect("store close");
    }

    fn fitted_slope(samples: &[(u64, u64)]) -> f64 {
        let count = samples.len() as f64;
        let mean_x = samples.iter().map(|(x, _)| *x as f64).sum::<f64>() / count;
        let mean_y = samples.iter().map(|(_, y)| *y as f64).sum::<f64>() / count;
        let numerator = samples
            .iter()
            .map(|(x, y)| (*x as f64 - mean_x) * (*y as f64 - mean_y))
            .sum::<f64>();
        let denominator = samples
            .iter()
            .map(|(x, _)| (*x as f64 - mean_x).powi(2))
            .sum::<f64>();
        numerator / denominator
    }

    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    fn resident_bytes() -> Option<u64> {
        // SAFETY: `usage` is writable storage for the requested V0 layout;
        // `assume_init` is reached only after the kernel reports success.
        let usage = unsafe {
            let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
            (libc::proc_pid_rusage(
                std::process::id() as libc::c_int,
                libc::RUSAGE_INFO_V0,
                usage.as_mut_ptr().cast(),
            ) == 0)
                .then(|| usage.assume_init())
        }?;
        Some(usage.ri_resident_size)
    }

    #[cfg(target_os = "linux")]
    fn resident_bytes() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let rss_kib = status.lines().find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_ascii_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })?;
        Some(rss_kib.saturating_mul(1024))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn resident_bytes() -> Option<u64> {
        None
    }

    fn uptime_load() -> String {
        std::process::Command::new("uptime")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map_or_else(|| "unavailable".into(), |output| output.trim().to_owned())
    }

    fn retention_soak_error(context: &str, error: impl std::fmt::Display) -> HaiderError {
        HaiderError::new(ErrorCode::Internal, format!("{context}: {error}"), false)
    }

    fn verify_observe_soak_owned_retention(
        supervisors: usize,
        observe_ready: usize,
        observe_building: usize,
        observe_bytes: usize,
        actor_tasks: usize,
    ) -> Result<(), HaiderError> {
        if supervisors == 0
            && observe_ready == 0
            && observe_building == 0
            && observe_bytes == 0
            && actor_tasks == 0
        {
            return Ok(());
        }
        Err(HaiderError::new(
            ErrorCode::Internal,
            format!(
                "deleted session retained daemon-owned state: supervisors={supervisors} observe_ready={observe_ready} observe_building={observe_building} observe_bytes={observe_bytes} actor_tasks={actor_tasks}"
            ),
            false,
        ))
    }

    #[test]
    fn observe_soak_owned_retention_failure_is_typed() {
        let retained_owner_cases = [
            (1, 0, 0, 0, 0),
            (0, 1, 0, 0, 0),
            (0, 0, 1, 0, 0),
            (0, 0, 0, 1, 0),
            (0, 0, 0, 0, 1),
        ];
        for retained in retained_owner_cases {
            let error = verify_observe_soak_owned_retention(
                retained.0, retained.1, retained.2, retained.3, retained.4,
            )
            .expect_err("each retained owner must fail the soak");
            assert_eq!(error.code, ErrorCode::Internal);
            assert!(!error.retryable);
            assert!(error.message.contains("retained daemon-owned state"));
        }
    }

    #[tokio::test]
    async fn create_two_turn_observe_delete_soak_has_flat_retention_slopes()
    -> Result<(), HaiderError> {
        const CHILD_ENV: &str = "HAIDER_RETAIN_SOAK_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            // RSS is process-global. Run the measurement alone so unrelated
            // parallel daemon tests cannot be mistaken for retained session
            // heap, then preserve the child's complete diagnostic stream.
            let test_binary = std::env::current_exe().map_err(|error| {
                retention_soak_error("cannot locate the current daemon test binary", error)
            })?;
            let output = std::process::Command::new(test_binary)
            .env(CHILD_ENV, "1")
            .args([
                "--exact",
                "session_hub::rpc::observe_cache_retention_tests::create_two_turn_observe_delete_soak_has_flat_retention_slopes",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .map_err(|error| retention_soak_error("cannot launch isolated retention soak", error))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprint!("{stdout}{stderr}");
            if !output.status.success() {
                return Err(retention_soak_error(
                    "isolated retention soak child failed",
                    output.status,
                ));
            }
            return Ok(());
        }

        // Inspect the daemon-owned retention surfaces after every deletion.
        // Process RSS remains diagnostic: allocator arenas and SQLite page
        // caches are process-global high-water state and cannot identify an
        // owner, so their platform-dependent slope is not a correctness gate.
        // Eight unmeasured cycles pay one-time runtime initialization before
        // reporting the RSS samples.
        const WARMUP_CYCLES: u64 = 8;
        const CYCLES: u64 = 64;
        const SAMPLE_AT: [u64; 7] = [1, 2, 4, 8, 16, 32, 64];

        let root = tempfile::tempdir()
            .map_err(|error| retention_soak_error("cannot create soak store directory", error))?;
        let store = SqliteStoreHandle::open(root.path()).await?;
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default())
            .map_err(hub_error_as_store)?;
        let manager = crate::worker::WorkerManager::start(
            hub.clone(),
            crate::worker::WorkerDependencies::unconfigured_for_tests(),
            false,
        );
        let workers = manager.handle();
        hub.install_worker_manager(workers.clone())
            .map_err(hub_error_as_store)?;
        let cwd = std::env::current_dir()
            .map_err(|error| retention_soak_error("cannot read soak working directory", error))?;
        let cwd = std::fs::canonicalize(cwd)
            .map_err(|error| {
                retention_soak_error("cannot canonicalize soak working directory", error)
            })?
            .to_string_lossy()
            .into_owned();
        let mut started = std::time::Instant::now();
        let mut baseline_rss = None;
        let mut supervisor_samples = Vec::new();
        let mut observe_samples = Vec::new();
        let mut targeted_heap_samples = Vec::new();
        let mut rss_samples = Vec::new();

        for ordinal in 1..=WARMUP_CYCLES + CYCLES {
            let session_id = SessionId::new(format!("retain-soak-{ordinal}"));
            hub.create_internal_session(SessionCreateCommand {
                command_id: format!("retain-soak-create-{ordinal}"),
                request_digest: format!("retain-soak-create-digest-{ordinal}"),
                request_json: "{}".into(),
                session_id: session_id.clone(),
                cwd: cwd.clone(),
                provider: "fake".into(),
                model: "fake-model".into(),
                max_tokens: 4096,
                permission_overrides: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
                event_id: EventId::new(format!("retain-soak-created-{ordinal}")),
                device_id: DeviceId::new("retain-soak-device"),
            })
            .await?;

            for turn in 0..2_u64 {
                let run_id = RunId::new(format!("retain-soak-run-{ordinal}-{turn}"));
                let accepted = hub
                    .accept_turn(TurnAcceptCommand {
                        command_id: format!("retain-soak-turn-{ordinal}-{turn}"),
                        request_digest: format!("retain-soak-turn-digest-{ordinal}-{turn}"),
                        request_json: "{}".into(),
                        session_id: session_id.clone(),
                        worker_generation: store.worker_generation(),
                        branch_id: None,
                        run_id,
                        agent_id: None,
                        text: "soak".into(),
                        attachments: Vec::new(),
                        mode: DeliveryMode::Queue,
                        queued_event_id: EventId::new(format!(
                            "retain-soak-queued-{ordinal}-{turn}"
                        )),
                        user_event_id: EventId::new(format!("retain-soak-user-{ordinal}-{turn}")),
                        active_event_id: EventId::new(format!(
                            "retain-soak-active-{ordinal}-{turn}"
                        )),
                        device_id: DeviceId::new("retain-soak-device"),
                    })
                    .await
                    .map_err(hub_error_as_store)?;
                let accepted = match accepted {
                    TurnAcceptOutcome::Committed { accepted, .. }
                    | TurnAcceptOutcome::IdempotentReplay { accepted } => accepted,
                };
                workers.submit(accepted).await?;
                let mut settled = false;
                for _ in 0..1_000 {
                    if !hub.session_has_nonterminal_runs(&session_id).await? {
                        settled = true;
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                if !settled {
                    return Err(retention_soak_error(
                        "fake-provider turn did not reach durable quiescence",
                        format_args!("session={session_id} turn={turn}"),
                    ));
                }
            }

            let _ = cached_observe_snapshot(&hub, &session_id, "fake-model")
                .await
                .map_err(hub_error_as_store)?;
            hub.delete_session(session_id).await?;

            let (ready, building, bytes) = hub.inner.observe_digests.stats();
            let supervisors = workers.supervisor_count();
            let actor_tasks = lock(&hub.inner.session_actor_tasks)
                .map_err(hub_error_as_store)?
                .len();
            verify_observe_soak_owned_retention(supervisors, ready, building, bytes, actor_tasks)?;

            if ordinal == WARMUP_CYCLES {
                baseline_rss = resident_bytes();
                started = std::time::Instant::now();
                continue;
            }
            if ordinal < WARMUP_CYCLES {
                continue;
            }
            let cycle = ordinal - WARMUP_CYCLES;

            if SAMPLE_AT.contains(&cycle) {
                let (ready, building, bytes) = hub.inner.observe_digests.stats();
                let supervisors = workers.supervisor_count() as u64;
                let observe_entries = ready.saturating_add(building) as u64;
                let targeted_heap = bytes as u64;
                supervisor_samples.push((cycle, supervisors));
                observe_samples.push((cycle, observe_entries));
                targeted_heap_samples.push((cycle, targeted_heap));
                if let (Some(base), Some(current)) = (baseline_rss, resident_bytes()) {
                    rss_samples.push((cycle, current.saturating_sub(base)));
                }
                eprintln!(
                    "retain_soak n={cycle} uptime_ms={} load={:?} supervisors={supervisors} observe_ready={ready} observe_building={building} observe_bytes={bytes} rss_delta={:?}",
                    started.elapsed().as_millis(),
                    uptime_load(),
                    rss_samples.last().map(|(_, bytes)| bytes),
                );
            }
        }

        let supervisor_slope = fitted_slope(&supervisor_samples);
        let observe_slope = fitted_slope(&observe_samples);
        let targeted_heap_slope = fitted_slope(&targeted_heap_samples);
        let rss_slope = (rss_samples.len() == SAMPLE_AT.len()).then(|| fitted_slope(&rss_samples));
        eprintln!(
            "retain_soak slopes supervisors_per_session={supervisor_slope:.6} observe_entries_per_session={observe_slope:.6} targeted_heap_bytes_per_session={targeted_heap_slope:.3} rss_bytes_per_session={rss_slope:?}"
        );

        manager.shutdown().await?;
        let _ = hub.shutdown().await.map_err(hub_error_as_store)?;
        store.close().await?;
        Ok(())
    }
}

/// Deterministic cache-miss/cold-start oracle. It consumes a sealed journal
/// prefix in sequence order and is intentionally retained independently of
/// the incremental commit path so parity can be asserted directly.
async fn rebuild_observe_fold(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    through_seq: u64,
    initial_model: &str,
) -> Result<ObserveFold, HaiderError> {
    let mut fold = ObserveFold::new(initial_model);
    let mut cursor = 0;
    while cursor < through_seq {
        let page = store.read(session_id, cursor, REPLAY_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                break;
            }
            cursor = envelope.seq;
            advanced = true;
            fold.apply(envelope);
        }
        if !advanced {
            break;
        }
    }
    if fold.head_seq != through_seq {
        return Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!(
                "observe digest rebuild stopped at sequence {} before sealed head {through_seq}",
                fold.head_seq
            ),
            false,
        ));
    }
    Ok(fold)
}

async fn cached_observe_snapshot(
    hub: &SessionHub,
    session_id: &SessionId,
    initial_model: &str,
) -> Result<ObserveFoldSnapshot, SessionHubError> {
    loop {
        let sealed_head = hub.inner.store.latest_seq(session_id).await?;
        if sealed_head == 0 {
            // Deleted/missing sessions never enter the cache. An in-flight
            // pre-delete builder also cannot reinstall without its exact
            // Building token, so deletion cannot resurrect an entry.
            return Ok(ObserveFold::new(initial_model).snapshot(session_id));
        }
        match hub
            .inner
            .observe_digests
            .start(session_id, sealed_head)
            .await
        {
            CacheStart::Ready(snapshot) => return Ok(snapshot),
            CacheStart::BuildAndInstall(token) => {
                let mut guard = ObserveBuildGuard::new(
                    Arc::clone(&hub.inner.observe_digests),
                    session_id.clone(),
                    token,
                );
                let fold =
                    rebuild_observe_fold(&hub.inner.store, session_id, sealed_head, initial_model)
                        .await?;
                let snapshot = hub
                    .inner
                    .observe_digests
                    .install(session_id.clone(), fold, token);
                guard.disarm();
                if snapshot.head_seq == hub.inner.store.latest_seq(session_id).await? {
                    return Ok(snapshot);
                }
            }
            CacheStart::WaitExisting(mut completion) => loop {
                let snapshot = {
                    let current = completion.borrow_and_update();
                    let snapshot = current.as_ref().map(Arc::clone);
                    drop(current);
                    snapshot
                };
                if let Some(snapshot) = snapshot {
                    if snapshot.head_seq == hub.inner.store.latest_seq(session_id).await? {
                        return Ok((*snapshot).clone());
                    }
                    break;
                }
                if completion.changed().await.is_err() {
                    break;
                }
            },
        }
    }
}

const MAX_OBSERVE_EVENT_KINDS: usize = 100;
const MAX_OBSERVE_BATCH: usize = 64;

async fn session_observe_digest(
    hub: &SessionHub,
    session_id: SessionId,
    last_event_limit: u32,
    metadata_only: bool,
) -> Result<Option<haider_rpc::SessionObserveDigest>, SessionHubError> {
    let metadata = hub.inner.store.session_metadata(&session_id).await?;
    let active_provider = metadata.as_ref().map(|metadata| metadata.provider.clone());
    let initial_model = metadata
        .as_ref()
        .map_or("", |metadata| metadata.model.as_str());
    let snapshot = cached_observe_snapshot(hub, &session_id, initial_model).await?;
    if snapshot.head_seq == 0 {
        return Ok(None);
    }
    let event_limit = usize::try_from(last_event_limit)
        .unwrap_or(usize::MAX)
        .min(MAX_OBSERVE_EVENT_KINDS);
    let workflow = snapshot.workflow.clone();
    let mut digest = if metadata_only {
        let mut projection = ObserveProjection::new(event_limit);
        projection.title = snapshot.title;
        projection.finish(
            session_id,
            snapshot.head_seq,
            hub.inner.store.worker_generation(),
            metadata,
        )
    } else {
        snapshot.digest(
            session_id,
            hub.inner.store.worker_generation(),
            metadata,
            event_limit,
            true,
        )
    };
    digest.workflow = workflow;
    digest.lockdown = active_provider
        .as_deref()
        .map(|provider| observed_lockdown_status(hub, Some(&digest.session_id), provider))
        .transpose()?
        .flatten();
    for subagent in &mut digest.subagents {
        let active_child = hub.delegation(subagent.agent_id.clone()).await?;
        if let (Some(provider), Some(child)) = (subagent.provider.as_deref(), active_child) {
            subagent.lockdown =
                observed_lockdown_status(hub, Some(&child.child_session_id), provider)?;
            continue;
        }
        subagent.lockdown = match (
            subagent.provider.as_deref(),
            subagent.lockdown_bound,
            subagent.lockdown_auto_hermetic_bound,
        ) {
            (Some(provider), Some(true), auto_hermetic) => observed_lockdown_manager_status(
                provider,
                crate::auto_hermetic::ProviderLockdownPolicy::from_binding(
                    true,
                    auto_hermetic.unwrap_or(false),
                ),
            )?,
            (_, Some(false), _) => None,
            (Some(provider), None, _) => observed_lockdown_status(hub, None, provider)?,
            (None, _, _) => None,
        };
    }
    Ok(Some(digest))
}

fn observed_lockdown_status(
    hub: &SessionHub,
    session_id: Option<&SessionId>,
    provider: &str,
) -> Result<Option<haider_rpc::LockdownStatusWire>, SessionHubError> {
    let (provider, policy) = match session_id {
        Some(session_id) => {
            let active_binding = if crate::lockdown::global_if_initialized().is_some() {
                hub.bound_session_lockdown(session_id)?
            } else {
                None
            };
            active_binding.unwrap_or((
                provider.to_owned(),
                hub.provider_lockdown_policy_detail(provider)?,
            ))
        }
        None => (
            provider.to_owned(),
            hub.provider_lockdown_policy_detail(provider)?,
        ),
    };
    if !policy.is_lockdown() {
        return Ok(None);
    }
    observed_lockdown_manager_status(&provider, policy)
}

fn observed_lockdown_manager_status(
    provider: &str,
    policy: crate::auto_hermetic::ProviderLockdownPolicy,
) -> Result<Option<haider_rpc::LockdownStatusWire>, SessionHubError> {
    let Some(manager) = crate::lockdown::global_if_initialized() else {
        // The quota ledger is machine-user-global and may already contain
        // data, so a pre-start projection cannot truthfully invent usage.
        // Absence is the wire's backward-compatible "not projected" state.
        return Ok(None);
    };
    manager
        .status(Some(provider))
        .map(|status| lockdown_status_wire(status, Some(policy), true))
        .map(Some)
        .map_err(|error| SessionHubError::Task(error.to_string()))
}

impl ObserveProjection {
    fn new(event_limit: usize) -> Self {
        Self {
            event_limit,
            event_kinds: VecDeque::with_capacity(event_limit),
            title: None,
            runs: HashMap::new(),
            menus: BTreeMap::new(),
            subagents: BTreeMap::new(),
            footprint: None,
            main_head_node_id: None,
            main_head_seq: 0,
            branches: HashMap::new(),
            updated_at_ms: 0,
            graphs: haider_protocol::graph::GraphReductions::default(),
        }
    }

    fn apply(&mut self, envelope: haider_protocol::envelope::RawEnvelope) {
        self.graphs.apply_envelope(&envelope);
        self.updated_at_ms = self.updated_at_ms.max(envelope.committed_at_ms);
        if let Some(kind) = envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            && self.event_limit > 0
        {
            if self.event_kinds.len() == self.event_limit {
                self.event_kinds.pop_front();
            }
            self.event_kinds.push_back(kind.to_owned());
        }
        let seq = envelope.seq;
        let branch_id = envelope.branch_id;
        let run_id = envelope.run_id;
        if let Some(created) =
            haider_protocol::branch::BranchCreated::from_payload_value(&envelope.payload)
        {
            self.branches
                .insert(created.branch.branch_id.clone(), created.branch);
            return;
        }
        let Ok(payload) = envelope.payload.decode_event() else {
            return;
        };
        match payload {
            EventPayload::UserMessage { text, .. } if self.title.is_none() => {
                self.title = Some(observe_title(&text));
            }
            EventPayload::RunState(state) => {
                if let Some(run_id) = run_id {
                    self.runs.insert(
                        run_id,
                        ObservedRun {
                            state,
                            seq,
                            branch_id,
                        },
                    );
                    while self.runs.len() > MAX_OBSERVED_RUNS_PER_SESSION {
                        let Some(oldest_terminal) = self
                            .runs
                            .iter()
                            .filter(|(_, run)| run.state.is_terminal())
                            .min_by_key(|(_, run)| run.seq)
                            .map(|(run_id, _)| run_id.clone())
                        else {
                            break;
                        };
                        self.runs.remove(&oldest_terminal);
                    }
                }
            }
            EventPayload::MenuOpened(menu) => {
                let kind = observe_menu_kind(&menu.kind);
                // v0.0.937 unified input-required contract: every parked
                // menu is answerable from any surface, so the digest carries
                // display copy + options for EVERY kind EXCEPT Secret —
                // durable Secret menus are the one kind whose body/options
                // can carry vaulted material (the v0.0.935 leak class), so
                // they expose title + kind only and their answers travel as
                // secret references. All other bodies are daemon-authored
                // display copy by construction (effect summaries, recovery
                // evidence, update/trust prompts) and never vault material.
                let exposes_card = !matches!(menu.kind, MenuKind::Secret);
                let (permission_description, presentation) = match &menu.kind {
                    MenuKind::Permission { effect_summary } => (Some(effect_summary.clone()), None),
                    MenuKind::ErrorRecovery { presentation, .. } => {
                        (None, Some(presentation.clone()))
                    }
                    _ => (None, None),
                };
                let (body, options) = if exposes_card {
                    (
                        menu.body,
                        menu.options
                            .into_iter()
                            .map(|option| haider_rpc::ObserveMenuOptionWire {
                                key: option.key,
                                label: option.label,
                                detail: option.detail,
                                decision: option.decision.map(|decision| {
                                    match decision {
                                        haider_protocol::menu::DecisionKind::AllowOnce => {
                                            "allow_once"
                                        }
                                        haider_protocol::menu::DecisionKind::AllowAlways => {
                                            "allow_always"
                                        }
                                        haider_protocol::menu::DecisionKind::RejectOnce => {
                                            "reject_once"
                                        }
                                        haider_protocol::menu::DecisionKind::RejectAlways => {
                                            "reject_always"
                                        }
                                    }
                                    .to_owned()
                                }),
                            })
                            .collect(),
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
                self.menus.insert(
                    menu.id.as_str().to_owned(),
                    haider_rpc::ObserveMenuWire {
                        kind: kind.into(),
                        title: menu.title,
                        menu_id: Some(menu.id),
                        request_seq: Some(seq),
                        worker_generation: Some(envelope.worker_generation),
                        opened_at_ms: Some(envelope.committed_at_ms),
                        body,
                        options,
                        permission_description,
                        presentation,
                    },
                );
            }
            EventPayload::MenuAnswered(answer) => {
                self.menus.remove(answer.menu.as_str());
            }
            EventPayload::MenuClosed { menu, .. } => {
                self.menus.remove(menu.as_str());
            }
            EventPayload::AgentSpawned(manifest) => {
                let provider = manifest.provider().map(ToOwned::to_owned);
                let lockdown_bound = manifest
                    .coordinates
                    .as_ref()
                    .and_then(|coordinates| coordinates.get("lockdown"))
                    .and_then(serde_json::Value::as_bool);
                let lockdown_auto_hermetic_bound = manifest
                    .coordinates
                    .as_ref()
                    .and_then(|coordinates| coordinates.get("auto_hermetic"))
                    .and_then(serde_json::Value::as_bool);
                self.subagents.insert(
                    manifest.agent.as_str().to_owned(),
                    haider_rpc::ObserveSubagentWire {
                        agent_id: manifest.agent,
                        callsign: manifest.callsign,
                        task: manifest.task,
                        state: "thinking".into(),
                        provider,
                        lockdown_bound,
                        lockdown_auto_hermetic_bound,
                        lockdown: None,
                    },
                );
            }
            EventPayload::AgentChipState { agent, chip } => {
                let state = observe_chip_state(chip).to_owned();
                self.subagents
                    .entry(agent.as_str().to_owned())
                    .and_modify(|subagent| subagent.state.clone_from(&state))
                    .or_insert(haider_rpc::ObserveSubagentWire {
                        agent_id: agent,
                        callsign: None,
                        task: String::new(),
                        state,
                        provider: None,
                        lockdown_bound: None,
                        lockdown_auto_hermetic_bound: None,
                        lockdown: None,
                    });
            }
            EventPayload::AgentReport(report) => {
                if let Some(subagent) = self.subagents.get_mut(report.agent.as_str()) {
                    subagent.state = match report.verified {
                        haider_protocol::agent::ReportVerification::Red => "error",
                        _ => "done",
                    }
                    .into();
                }
            }
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                if let Some(footprint) = ContextFootprint::from_extension_item(&item) {
                    self.footprint = Some(footprint);
                }
            }
            EventPayload::NodeCommitted(node) => {
                if let Some(branch_id) = branch_id {
                    if let Some(branch) = self.branches.get_mut(&branch_id) {
                        branch.head_node_id = node.node;
                        branch.head_seq = seq;
                    }
                } else {
                    self.main_head_node_id = Some(node.node);
                    self.main_head_seq = seq;
                }
            }
            _ => {}
        }
    }

    fn finish(
        self,
        session_id: SessionId,
        head_seq: u64,
        worker_generation: u64,
        metadata: Option<haider_protocol::session::SessionMetadataV1>,
    ) -> haider_rpc::SessionObserveDigest {
        let selected = select_observed_run(&self.runs);
        let run_state = selected.map_or(haider_rpc::ObserveRunStateWire::Idle, |(_, run)| {
            observe_run_state(&run.state)
        });
        let run_id = selected.map(|(run_id, _)| run_id.clone());
        let active_branch_id = selected.and_then(|(_, run)| run.branch_id.clone());
        let title = self.title.unwrap_or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| {
                    std::path::Path::new(&metadata.cwd)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(ToOwned::to_owned)
                })
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| session_id.as_str().to_owned())
        });
        // Branch-created facts and branch node commits come from the same
        // sealed journal prefix as every other observation field. Avoid a
        // mutable registry read that could race ahead of `head_seq`.
        let mut branches = self.branches.into_values().collect::<Vec<_>>();
        branches.sort_by_key(|branch| branch.created_seq);
        let pending_menus: Vec<haider_rpc::ObserveMenuWire> = self.menus.into_values().collect();
        let needs_input = needs_input(run_state, &pending_menus);
        haider_rpc::SessionObserveDigest {
            session_id,
            head_seq,
            worker_generation,
            metadata,
            title,
            run_state,
            run_id,
            active_branch_id,
            branches,
            main_head_node_id: self.main_head_node_id,
            main_head_seq: self.main_head_seq,
            latest_context_footprint: self.footprint,
            pending_menus,
            subagents: self.subagents.into_values().collect(),
            lockdown: None,
            updated_at_ms: self.updated_at_ms,
            last_event_kinds: self.event_kinds.into_iter().collect(),
            // Roster-truth fields are stamped by the observe handler from the
            // same truth functions the session listing uses (None in
            // metadata-only responses).
            turn_count: None,
            agent_metrics: None,
            needs_input,
            workflow: self
                .graphs
                .active()
                .and_then(|reduction| reduction.status.clone()),
        }
    }
}

/// Pick the ONE run a session's observed `run_state` describes.
///
/// Returns the map KEY alongside the run (W-flow, owner 2026-08-22): the id
/// and the state must come from the same selection or a client cancelling
/// "what it sees" could name a different run than the one whose state it is
/// rendering. Discarding the key here was why no observation surface could
/// report a cancellable run id.
fn select_observed_run(runs: &HashMap<RunId, ObservedRun>) -> Option<(&RunId, &ObservedRun)> {
    let predicates: [fn(&RunState) -> bool; 5] = [
        |state| matches!(state, RunState::EffectOutcomeUnknown),
        |state| matches!(state, RunState::PermissionRequired { .. }),
        |state| matches!(state, RunState::InputRequired { .. }),
        |state| !state.is_terminal() && !matches!(state, RunState::Queued),
        |state| matches!(state, RunState::Queued),
    ];
    for predicate in predicates {
        if let Some(entry) = runs
            .iter()
            .filter(|(_, run)| predicate(&run.state))
            .max_by_key(|(_, run)| run.seq)
        {
            return Some(entry);
        }
    }
    runs.iter().max_by_key(|(_, run)| run.seq)
}

pub(crate) fn observe_run_state(state: &RunState) -> haider_rpc::ObserveRunStateWire {
    match state {
        RunState::PermissionRequired { .. } => haider_rpc::ObserveRunStateWire::ParkedPermission,
        RunState::InputRequired { .. } => haider_rpc::ObserveRunStateWire::ParkedInput,
        RunState::EffectOutcomeUnknown => haider_rpc::ObserveRunStateWire::EffectUnknown,
        RunState::Errored => haider_rpc::ObserveRunStateWire::Errored,
        RunState::Cancelled => haider_rpc::ObserveRunStateWire::Cancelled,
        RunState::Done => haider_rpc::ObserveRunStateWire::Idle,
        RunState::Waiting {
            reason: haider_protocol::state::WaitReason::NetworkUnavailable,
        } => haider_rpc::ObserveRunStateWire::WaitingForRoute,
        RunState::Queued
        | RunState::Thinking
        | RunState::Streaming
        | RunState::RunningTool
        | RunState::Waiting { .. }
        | RunState::Retrying { .. }
        | RunState::Compacting
        | RunState::Verifying { .. }
        | RunState::Concluding
        | RunState::Cancelling => haider_rpc::ObserveRunStateWire::Running,
    }
}

fn observe_menu_kind(kind: &MenuKind) -> &'static str {
    match kind {
        MenuKind::Permission { .. } => "permission",
        MenuKind::Recovery { .. } => "recovery",
        MenuKind::ErrorRecovery { .. } => "error_recovery",
        MenuKind::Exhausted => "exhausted",
        MenuKind::TrustHook => "trust_hook",
        MenuKind::Update => "update",
        MenuKind::Question => "question",
        MenuKind::Choice => "choice",
        MenuKind::Secret => "secret",
        MenuKind::File => "file",
        MenuKind::Conflict => "conflict",
        MenuKind::GraphHumanConfirm { .. } => "graph_human_confirm",
        MenuKind::GraphAbandonConfirm { .. } => "graph_abandon_confirm",
    }
}

fn observe_chip_state(state: ChipState) -> &'static str {
    match state {
        ChipState::Idle => "idle",
        ChipState::Thinking => "thinking",
        ChipState::Streaming => "streaming",
        ChipState::Tool => "tool",
        ChipState::Waiting => "waiting",
        ChipState::InputRequired => "input_required",
        ChipState::PermissionRequired => "permission_required",
        ChipState::Done => "done",
        ChipState::Error => "error",
        ChipState::Closed => "closed",
    }
}

fn observe_title(text: &str) -> String {
    let body = if text.starts_with('/') {
        text.split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        text.to_owned()
    };
    let joined = body
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ");
    let truncated = if joined.chars().count() > 46 {
        let cut = joined.chars().take(46).collect::<String>();
        format!("{}…", cut.trim_end())
    } else {
        joined
    };
    let mut chars = truncated.chars();
    chars.next().map_or_else(
        || "New session".to_owned(),
        |first| first.to_uppercase().collect::<String>() + chars.as_str(),
    )
}

// ─────────── connection RPC surface: list/read/attach/detach/menu ───────────

impl SessionHub {
    /// Executes a read-only check for one ambiguous effect, then reopens the
    /// standard card with the observation. The check intentionally lives
    /// outside the serialized session actor: filesystem/network inspection
    /// may block, while the actor's charter permits store awaits only.
    async fn probe_effect_outcome(
        &self,
        session_id: SessionId,
        effect: haider_protocol::ids::EffectId,
        menu_id: MenuId,
        answer: &RawEnvelope,
    ) -> Result<(), HaiderError> {
        let mut cursor = 0_u64;
        let mut intent = None::<EffectIntent>;
        loop {
            let page = self.inner.store.read(&session_id, cursor, 256).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if envelope.run_id != answer.run_id {
                    continue;
                }
                if let Ok(EventPayload::Effect(EffectPhase::Intent(candidate))) =
                    envelope.payload.decode_event()
                    && candidate.effect == effect
                {
                    intent = Some(candidate);
                }
            }
        }
        let intent = intent.ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!("recovery effect {effect} has no durable intent"),
                false,
            )
        })?;
        let metadata = self
            .inner
            .store
            .session_metadata(&session_id)
            .await?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "effect probe session metadata is unavailable",
                    false,
                )
            })?;
        let delegation_count = if intent.class == EffectClass::AgentSpawn {
            match answer.run_id.clone() {
                Some(run) => Some(
                    self.inner
                        .store
                        .delegations_for_parent_run(session_id.clone(), run)
                        .await?
                        .len(),
                ),
                None => Some(0),
            }
        } else {
            None
        };
        let observation = effect_probe_observation(&intent, &metadata.cwd, delegation_count).await;
        let probe_menu = effect_recovery_menu(
            MenuId::new(format!("{menu_id}-probe-{}", answer.seq)),
            effect,
            format!("{}; probe result: {observation}", intent.summary),
        );
        let payloads = [
            EventPayload::MenuOpened(probe_menu),
            EventPayload::RunState(RunState::EffectOutcomeUnknown),
        ];
        let mut envelopes = Vec::with_capacity(payloads.len());
        for (index, payload) in payloads.into_iter().enumerate() {
            envelopes.push(haider_protocol::envelope::EventEnvelope {
                schema_version: haider_protocol::envelope::SCHEMA_VERSION,
                event_id: EventId::new(format!("effect-probe-{}-{}", answer.event_id, index + 1)),
                seq: 0,
                session_id: session_id.clone(),
                branch_id: answer.branch_id.clone(),
                run_id: answer.run_id.clone(),
                agent_id: answer.agent_id.clone(),
                device_id: self.inner.device_id.clone(),
                authority_epoch: answer.authority_epoch,
                worker_generation: self.inner.store.worker_generation(),
                causation_id: Some(answer.event_id.clone()),
                correlation_id: answer.correlation_id.clone(),
                committed_at_ms: 0,
                render: haider_protocol::envelope::RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: haider_protocol::envelope::PromptRender::Pruned,
                },
                payload: haider_protocol::envelope::RawPayload::from_event(payload).map_err(
                    |error| {
                        HaiderError::new(
                            ErrorCode::Internal,
                            format!("effect probe payload could not serialize: {error}"),
                            false,
                        )
                    },
                )?,
            });
        }
        self.append(&mut envelopes).await?;
        Ok(())
    }

    /// Starts the real fresh turn selected by E6's `retry` handler. The
    /// original ambiguous effect is durably settled before this is called;
    /// the new turn receives an explicit probe-first instruction so it never
    /// blindly duplicates the prior mutation.
    async fn submit_effect_retry(
        &self,
        session_id: SessionId,
        effect: haider_protocol::ids::EffectId,
        menu_id: MenuId,
        resolution_seq: u64,
    ) -> Result<(), HaiderError> {
        let worker_generation = self.inner.store.worker_generation();
        let text = format!(
            "Retry unresolved effect {effect}. Probe the current state first; perform the operation only if it is still needed."
        );
        let command_id = format!("effect-retry-{menu_id}-{resolution_seq}");
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "effect": &effect,
            "menu": &menu_id,
            "resolution_seq": resolution_seq,
            "text": &text,
            "mode": DeliveryMode::Queue,
        }))
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("effect retry coordinates could not serialize: {error}"),
                false,
            )
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let command = TurnAcceptCommand {
            command_id,
            request_digest,
            request_json,
            session_id: session_id.clone(),
            worker_generation,
            run_id: RunId::new(random_id("effect-retry-run").map_err(hub_error_as_store)?),
            agent_id: None,
            branch_id: None,
            text,
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new(
                random_id("effect-retry-queued").map_err(hub_error_as_store)?,
            ),
            user_event_id: EventId::new(
                random_id("effect-retry-user").map_err(hub_error_as_store)?,
            ),
            active_event_id: EventId::new(
                random_id("effect-retry-active").map_err(hub_error_as_store)?,
            ),
            device_id: self.inner.device_id.clone(),
        };
        let accepted = match self
            .accept_turn(command)
            .await
            .map_err(hub_error_as_store)?
        {
            TurnAcceptOutcome::Committed { accepted, .. }
            | TurnAcceptOutcome::IdempotentReplay { accepted } => accepted,
        };
        self.worker_manager()
            .map_err(hub_error_as_store)?
            .submit(accepted)
            .await
    }
}

async fn effect_probe_observation(
    intent: &EffectIntent,
    cwd: &str,
    delegation_count: Option<usize>,
) -> String {
    match &intent.class {
        EffectClass::FsRead | EffectClass::FsWrite => {
            let path = ["read ", "list ", "edit ", "write ", "patch "]
                .into_iter()
                .find_map(|prefix| intent.summary.strip_prefix(prefix));
            let Some(path) = path else {
                return "inconclusive — the durable file intent has no probeable path".into();
            };
            let path = std::path::PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                std::path::Path::new(cwd).join(path)
            };
            match tokio::task::spawn_blocking(move || std::fs::read(&path)).await {
                Ok(Ok(bytes)) => format!(
                    "re-read succeeded ({} bytes, blake3:{})",
                    bytes.len(),
                    blake3::hash(&bytes).to_hex()
                ),
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    "re-read found the target absent".into()
                }
                Ok(Err(error)) => format!("re-read failed: {error}"),
                Err(error) => format!("re-read task failed: {error}"),
            }
        }
        EffectClass::Network { .. } => {
            let Some(url) = intent.summary.strip_prefix("fetch ") else {
                return "inconclusive — this network effect has no safe idempotent probe".into();
            };
            let execution =
                haider_provider::fetch_public_url_with_one_retry(url, Some(8 * 1024)).await;
            match execution.outcome {
                Ok(outcome) => format!(
                    "GET probe succeeded after {} attempt(s): {} ({})",
                    execution.attempts, outcome.final_url, outcome.content_type
                ),
                Err(error) => format!(
                    "GET probe failed after {} attempt(s): {}",
                    execution.attempts, error.message
                ),
            }
        }
        EffectClass::AgentSpawn => format!(
            "durable delegation probe found {} child record(s)",
            delegation_count.unwrap_or(0)
        ),
        // Every remaining class is unsafe to probe automatically, including
        // peer delivery and local/remote execution. Keep this exhaustive so a
        // new authority class cannot inherit probe semantics accidentally.
        EffectClass::ProcessExec
        | EffectClass::RemoteExecution
        | EffectClass::GitOp
        | EffectClass::PeerMessage
        | EffectClass::CredentialAccess
        | EffectClass::GuiAct
        | EffectClass::ScreenObserve
        | EffectClass::ScreenControl
        | EffectClass::MobileObserve
        | EffectClass::MobileControl
        | EffectClass::ReadSms => {
            "inconclusive — no safe automatic probe exists for this effect class".into()
        }
    }
}

fn ssh_timeout(timeout_s: Option<u32>) -> Result<Option<Duration>, crate::ssh::SshError> {
    match timeout_s {
        Some(seconds) if (1..=86_400).contains(&seconds) => {
            Ok(Some(Duration::from_secs(u64::from(seconds))))
        }
        Some(_) => Err(crate::ssh::SshError::SshProfileInvalid {
            field: "timeout_s",
            message: "must be between 1 and 86400".into(),
        }),
        None => Ok(None),
    }
}

impl HubConnection {
    /// Publishes the resident TUI's foreground session after applying the same
    /// exact worker-generation fence used by control mutations. This is an
    /// uncorrelated top-level signal: a stale publisher receives a non-fatal
    /// `ProtocolError`, while accepted state is fanned out to other clients.
    pub(crate) async fn resident_session_binding(
        &self,
        session_id: Option<SessionId>,
        worker_generation: u64,
        binding_token: Option<String>,
    ) -> Result<(), SessionHubError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if let Err(message) = authorize(&self.capabilities, Operation::Control) {
            return self.send(WireFrame::ProtocolError(ProtocolError {
                code: ERROR_CODE_CAPABILITY_DENIED.into(),
                message: message.into(),
                fatal: false,
                presentation: None,
                failed_write_ids: Vec::new(),
            }));
        }
        if worker_generation != self.hub.worker_generation() {
            return self.send(WireFrame::ProtocolError(ProtocolError {
                code: ERROR_CODE_STALE_GENERATION.into(),
                message: "resident session binding worker generation is stale".into(),
                fatal: false,
                presentation: None,
                failed_write_ids: Vec::new(),
            }));
        }
        if binding_token
            .as_deref()
            .is_some_and(|token| !haider_rpc::resident_binding_token_is_valid(token))
        {
            return self.send(WireFrame::ProtocolError(ProtocolError {
                code: ERROR_CODE_INVALID_ARGUMENT.into(),
                message: "resident binding token must be 1..=128 bytes of ASCII alphanumeric, '-', '_', '.', or ':'"
                    .into(),
                fatal: false,
                presentation: None,
                failed_write_ids: Vec::new(),
            }));
        }
        if let Some(session_id) = session_id.as_ref()
            && self.hub.inner.store.latest_seq(session_id).await? == 0
        {
            return self.send(WireFrame::ProtocolError(ProtocolError {
                code: ERROR_CODE_NOT_FOUND.into(),
                message: "resident session binding names an unknown session".into(),
                fatal: false,
                presentation: None,
                failed_write_ids: Vec::new(),
            }));
        }
        self.hub.publish_resident_binding(
            &self.connection_id,
            session_id,
            worker_generation,
            binding_token,
        )
    }

    pub(super) fn clear_resident_binding(&self) {
        self.hub.clear_resident_binding(&self.connection_id);
    }

    async fn artifact_put(
        &self,
        request_id: RequestId,
        data_base64: String,
    ) -> Result<(), SessionHubError> {
        let decoded_len = match standard_base64_decoded_len(&data_base64) {
            Ok(decoded_len) => decoded_len,
            Err(message) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    message,
                    false,
                    None,
                );
            }
        };
        if decoded_len > ARTIFACT_PUT_MAX_BYTES {
            let actual_bytes = u64::try_from(decoded_len).unwrap_or(u64::MAX);
            return self.respond_error(
                request_id,
                ERROR_CODE_ARTIFACT_TOO_LARGE,
                &format!(
                    "artifact.put decodes to {actual_bytes} bytes; the hard limit is {ARTIFACT_PUT_MAX_BYTES}"
                ),
                false,
                Some(ErrorData::ArtifactTooLarge {
                    actual_bytes,
                    max_bytes: ARTIFACT_PUT_MAX_BYTES as u64,
                }),
            );
        }
        let bytes = match base64::engine::general_purpose::STANDARD.decode(data_base64) {
            Ok(bytes) => bytes,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &format!("artifact.put data_base64 is invalid: {error}"),
                    false,
                    None,
                );
            }
        };
        self.artifact_put_bytes(request_id, Zeroizing::new(bytes))
            .await
    }

    async fn artifact_put_bytes(
        &self,
        request_id: RequestId,
        bytes: Zeroizing<Vec<u8>>,
    ) -> Result<(), SessionHubError> {
        if bytes.len() > ARTIFACT_PUT_MAX_BYTES {
            let actual_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            return self.respond_error(
                request_id,
                ERROR_CODE_ARTIFACT_TOO_LARGE,
                &format!(
                    "artifact.put decoded {actual_bytes} bytes; the hard limit is {ARTIFACT_PUT_MAX_BYTES}"
                ),
                false,
                Some(ErrorData::ArtifactTooLarge {
                    actual_bytes,
                    max_bytes: ARTIFACT_PUT_MAX_BYTES as u64,
                }),
            );
        }
        let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let artifact = match self.hub.inner.store.put_zeroizing(bytes).await {
            Ok(artifact) => artifact,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ArtifactPut {
                artifact,
                bytes: byte_count,
            },
        })
    }

    /// Completes a transport-decoded `artifact.put` while preserving the
    /// ordinary request's closed/draining/capability checks.
    pub(crate) async fn request_decoded_artifact_put(
        &self,
        request_id: RequestId,
        bytes: Zeroizing<Vec<u8>>,
    ) -> Result<(), SessionHubError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        if let Err(message) = authorize(&self.capabilities, Operation::Control) {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                message,
                false,
                None,
            );
        }
        self.artifact_put_bytes(request_id, bytes).await
    }

    pub(crate) fn binary_artifact_store(&self) -> Result<SqliteStoreHandle, ResponseBody> {
        if self.closed.load(Ordering::Acquire) || self.hub.inner.draining.load(Ordering::Acquire) {
            return Err(ResponseBody::Error {
                code: ERROR_CODE_DRAINING.into(),
                message: "connection closed or daemon draining".into(),
                retryable: true,
                data: None,
            });
        }
        if let Err(message) = authorize(&self.capabilities, Operation::Control) {
            return Err(ResponseBody::Error {
                code: ERROR_CODE_CAPABILITY_DENIED.into(),
                message: message.into(),
                retryable: false,
                data: None,
            });
        }
        Ok(self.hub.inner.store.clone())
    }

    /// Handles one request and enqueues its correlated response.
    pub async fn request(
        &self,
        request_id: RequestId,
        body: RequestBody,
    ) -> Result<(), SessionHubError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        match body {
            RequestBody::CommandList {
                query,
                in_session,
                slots,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::CommandList {
                        items: haider_rpc::command_catalog_items(&query, in_session, &slots),
                    },
                })
            }
            RequestBody::CommandInvoke {
                command_id,
                command,
                session_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.command_invoke(request_id, command_id, command, session_id)
                    .await
            }
            RequestBody::ArtifactPut { data_base64 } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.artifact_put(request_id, data_base64).await
            }
            RequestBody::SessionCreateWithPermissionOverrides {
                command_id,
                cwd,
                provider,
                model,
                max_tokens,
                permission_overrides,
                cache_policy,
                interaction_mode,
                ssh_scope,
                account_alias,
                resolve_provider,
                resolve_model,
                effort,
                fast,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_create(
                    request_id,
                    command_id,
                    cwd,
                    provider,
                    model,
                    max_tokens,
                    permission_overrides,
                    cache_policy.unwrap_or_default(),
                    interaction_mode,
                    ssh_scope,
                    account_alias,
                    resolve_provider,
                    resolve_model,
                    effort,
                    fast,
                )
                .await
            }
            RequestBody::SessionCreate {
                command_id,
                cwd,
                provider,
                model,
                max_tokens,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_create(
                    request_id,
                    command_id,
                    cwd,
                    provider,
                    model,
                    max_tokens,
                    None,
                    Default::default(),
                    haider_protocol::session::SessionInteractionModeV1::Interactive,
                    None,
                    None,
                    false,
                    false,
                    None,
                    None,
                )
                .await
            }
            RequestBody::SessionList {
                cursor,
                limit,
                order,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_list(request_id, cursor, limit, order).await
            }
            RequestBody::StatusSnapshot {} => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.status_snapshot(request_id).await
            }
            RequestBody::AccountListWatch {} => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_list_watch(request_id)
            }
            RequestBody::SessionListWatch {} => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_list_watch(request_id)
            }
            RequestBody::SessionSurfacePublish {
                session_id,
                input,
                status,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_surface_publish(request_id, session_id, input, status)
                    .await
            }
            RequestBody::SessionSurfaceWatch { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_surface_watch(request_id, session_id).await
            }
            RequestBody::SessionInputInject { session_id, op } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_input_inject(request_id, session_id, op)
            }
            RequestBody::SessionPipePath { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_pipe_path(request_id, session_id).await
            }
            RequestBody::SessionRead { session_id, range } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_read(request_id, session_id, range).await
            }
            RequestBody::SessionObserve {
                session_id,
                last_event_limit,
                metadata_only,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_observe(request_id, session_id, last_event_limit, metadata_only)
                    .await
            }
            RequestBody::SessionObserveBatch {
                session_ids,
                last_event_limit,
                metadata_only,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_observe_batch(request_id, session_ids, last_event_limit, metadata_only)
                    .await
            }
            RequestBody::SessionFleet { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_fleet(request_id, session_id).await
            }
            RequestBody::SessionDescendantsAttach {
                session_id,
                cursors,
                max_children,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_descendants_attach(request_id, session_id, cursors, max_children)
                    .await
            }
            RequestBody::GraphStatus { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_status(request_id, session_id).await
            }
            RequestBody::WorkflowInstance {
                workflow_id,
                template_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.workflow_instance(request_id, workflow_id, template_digest)
                    .await
            }
            RequestBody::LoomList { include_archived } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_list(request_id, include_archived).await
            }
            RequestBody::LoomRegisterAgentType {
                record,
                expected_rev,
                expected_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(expected_rev) = expected_rev else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "loom.register_agent_type requires expected_rev under loom_registry_cas_v1",
                        false,
                        None,
                    );
                };
                self.loom_register_agent_type(
                    request_id,
                    record,
                    haider_protocol::loom::LoomRevisionExpectation {
                        rev: expected_rev,
                        digest: expected_digest,
                    },
                )
                .await
            }
            RequestBody::LoomInstallStatus {
                job_id,
                agent_type_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_install_status(request_id, job_id, agent_type_id)
                    .await
            }
            RequestBody::LoomRegisterWorkflow {
                source,
                expected_rev,
                expected_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(expected_rev) = expected_rev else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "loom.register_workflow requires expected_rev under loom_registry_cas_v1",
                        false,
                        None,
                    );
                };
                self.loom_register_workflow(
                    request_id,
                    source,
                    haider_protocol::loom::LoomRevisionExpectation {
                        rev: expected_rev,
                        digest: expected_digest,
                    },
                )
                .await
            }
            RequestBody::GraphInspect {
                session_id,
                cursor,
                limit,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_inspect(request_id, session_id, cursor, limit)
                    .await
            }
            RequestBody::SessionDiagnostic {
                command_id,
                session_id,
                code,
                message,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "session diagnostic requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_diagnostic(request_id, command_id, session_id, code, message)
                    .await
            }
            RequestBody::HooksList { cwd } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.hooks_list(request_id, cwd).await
            }
            RequestBody::HooksTrust { command_id, digest } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.hooks_trust(request_id, command_id, digest, true).await
            }
            RequestBody::HooksRevoke { command_id, digest } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.hooks_trust(request_id, command_id, digest, false)
                    .await
            }
            RequestBody::SessionAttach {
                session_id,
                after_seq,
                mode,
                sealed_replay,
            } => {
                let operation = match mode {
                    AttachMode::View => Operation::View,
                    AttachMode::Control => Operation::Control,
                    // `Unknown` and any future mode: never guess an
                    // authorization level for a mode this daemon predates.
                    _ => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "unknown attachment mode",
                            false,
                            None,
                        );
                    }
                };
                if let Err(message) = authorize(&self.capabilities, operation) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_attach(request_id, session_id, after_seq, mode, sealed_replay)
                    .await
            }
            RequestBody::SessionDetach { attachment_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_detach(request_id, attachment_id).await
            }
            RequestBody::BranchCreate {
                command_id,
                session_id,
                worker_generation,
                source_branch_id,
                fork_node_id,
                fork_seq,
                name,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.branch_create(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    source_branch_id,
                    fork_node_id,
                    fork_seq,
                    name,
                )
                .await
            }
            RequestBody::SessionFork {
                command_id,
                session_id,
                worker_generation,
                source_branch_id,
                fork_node_id,
                fork_seq,
                prompt,
                name,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let selector = match (fork_node_id, fork_seq, prompt) {
                    (Some(node_id), Some(seq), None) => {
                        SessionForkSelectorInput::Exact { node_id, seq }
                    }
                    (None, None, Some(prompt)) => {
                        SessionForkSelectorInput::Prompt { seq: prompt.seq }
                    }
                    _ => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "session.fork requires exactly one complete selector",
                            false,
                            None,
                        );
                    }
                };
                self.session_fork(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    source_branch_id,
                    selector,
                    name,
                )
                .await
            }
            RequestBody::SessionMetafork {
                command_id,
                session_id,
                worker_generation,
                source_branch_id,
                fork_node_id,
                fork_seq,
                name,
                description,
                model_proposal,
                accepted_proposal_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_metafork(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    source_branch_id,
                    fork_node_id,
                    fork_seq,
                    name,
                    description,
                    model_proposal,
                    accepted_proposal_digest,
                )
                .await
            }
            RequestBody::AgentMessage {
                command_id,
                session_id,
                worker_generation,
                agent,
                text,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "agent messaging requires a control attachment to the parent session",
                        false,
                        None,
                    );
                }
                self.agent_message(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    agent,
                    text,
                )
                .await
            }
            RequestBody::AgentCancel {
                command_id,
                session_id,
                worker_generation,
                agent,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "agent cancellation requires a control attachment to the parent session",
                        false,
                        None,
                    );
                }
                self.agent_cancel(request_id, command_id, session_id, worker_generation, agent)
                    .await
            }
            RequestBody::TurnSubmitWithBranch {
                command_id,
                session_id,
                worker_generation,
                branch_id,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    TurnSubmitInput {
                        command_id,
                        session_id,
                        worker_generation,
                        branch_id,
                        text,
                        attachments,
                        mode,
                        trust_hooks: false,
                        headless_spec: None,
                    },
                )
                .await
            }
            RequestBody::TurnSubmit {
                command_id,
                session_id,
                worker_generation,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    TurnSubmitInput {
                        command_id,
                        session_id,
                        worker_generation,
                        branch_id: None,
                        text,
                        attachments,
                        mode,
                        trust_hooks: false,
                        headless_spec: None,
                    },
                )
                .await
            }
            RequestBody::TurnSubmitFromCli {
                command_id,
                session_id,
                worker_generation,
                branch_id,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    TurnSubmitInput {
                        command_id,
                        session_id,
                        worker_generation,
                        branch_id,
                        text,
                        attachments,
                        mode,
                        trust_hooks: false,
                        headless_spec: None,
                    },
                )
                .await
            }
            RequestBody::TurnSubmitWithHookTrust {
                command_id,
                session_id,
                worker_generation,
                branch_id,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    TurnSubmitInput {
                        command_id,
                        session_id,
                        worker_generation,
                        branch_id,
                        text,
                        attachments,
                        mode,
                        trust_hooks: true,
                        headless_spec: None,
                    },
                )
                .await
            }
            RequestBody::HeadlessRunStart {
                command_id,
                session_id,
                worker_generation,
                text,
                attachments,
                spec,
                trust_hooks,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "headless start requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    TurnSubmitInput {
                        command_id,
                        session_id,
                        worker_generation,
                        branch_id: None,
                        text,
                        attachments,
                        mode: DeliveryMode::Queue,
                        trust_hooks,
                        headless_spec: Some(spec),
                    },
                )
                .await
            }
            RequestBody::QueueList { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.queue_list(request_id, session_id).await
            }
            RequestBody::QueueRemove {
                session_id,
                id,
                revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "queue removal requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.queue_remove(request_id, session_id, id, revision)
                    .await
            }
            RequestBody::QueuePromoteSteer {
                session_id,
                id,
                revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "queue promotion requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.queue_promote_steer(request_id, session_id, id, revision)
                    .await
            }
            RequestBody::TurnCancel {
                command_id,
                session_id,
                worker_generation,
                run_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn cancellation requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_cancel(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    run_id,
                )
                .await
            }
            RequestBody::RunRetry {
                command_id,
                session_id,
                worker_generation,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "run retry requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.run_retry(request_id, command_id, session_id, worker_generation)
                    .await
            }
            RequestBody::SessionCompactOnBranch {
                command_id,
                session_id,
                worker_generation,
                branch_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "context compaction requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_compact(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    branch_id,
                )
                .await
            }
            RequestBody::SessionCompact {
                command_id,
                session_id,
                worker_generation,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "context compaction requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_compact(request_id, command_id, session_id, worker_generation, None)
                    .await
            }
            RequestBody::SessionProviderRebind {
                command_id,
                session_id,
                worker_generation,
                provider,
                base_url,
                account,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "provider rebind requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_provider_rebind(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    provider,
                    base_url,
                    account,
                )
                .await
            }
            RequestBody::SessionSelectModel {
                command_id,
                session_id,
                worker_generation,
                model,
                provider,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "model selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_select_model(
                    request_id,
                    SessionSelectModelInput {
                        command_id,
                        session_id,
                        worker_generation,
                        model,
                        provider,
                        confirm_new_epoch,
                    },
                )
                .await
            }
            RequestBody::SessionRename {
                command_id,
                session_id,
                worker_generation,
                title,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "session rename requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_rename(request_id, command_id, session_id, worker_generation, title)
                    .await
            }
            RequestBody::SessionWorkspaceSet {
                command_id,
                session_id,
                worker_generation,
                path,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "workspace selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_workspace_set(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    path,
                )
                .await
            }
            RequestBody::SessionSeen {
                command_id,
                session_id,
                worker_generation,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "session seen requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_seen(request_id, command_id, session_id, worker_generation)
                    .await
            }
            RequestBody::GraphPin {
                command_id,
                session_id,
                worker_generation,
                template,
                expected_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_pin(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    template,
                    expected_digest,
                )
                .await
            }
            RequestBody::GraphRunSetOpen {
                command_id,
                session_id,
                worker_generation,
                plan_item_id,
                plan_event_seq,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_run_set_open(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    plan_item_id,
                    plan_event_seq,
                )
                .await
            }
            RequestBody::GraphSwitch {
                command_id,
                session_id,
                worker_generation,
                old_graph_id,
                template,
                expected_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_switch(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    old_graph_id,
                    template,
                    expected_digest,
                )
                .await
            }
            RequestBody::GraphAbandon {
                command_id,
                session_id,
                worker_generation,
                why,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.graph_abandon(request_id, command_id, session_id, worker_generation, why)
                    .await
            }
            RequestBody::SessionSelectEffort {
                command_id,
                session_id,
                worker_generation,
                effort,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "effort selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_select_effort(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    effort,
                    confirm_new_epoch,
                )
                .await
            }
            RequestBody::SessionSelectAgentType {
                command_id,
                session_id,
                worker_generation,
                agent_type,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "agent-type selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_select_agent_type(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    agent_type,
                )
                .await
            }
            RequestBody::SessionSelectFast {
                command_id,
                session_id,
                worker_generation,
                enabled,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "fast-mode selection requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.session_select_fast(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    enabled,
                    confirm_new_epoch,
                )
                .await
            }
            RequestBody::ShellExecScoped {
                command_id,
                session_id,
                worker_generation,
                branch_id,
                agent_id,
                command,
                cwd,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "direct shell execution requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        "shell.exec",
                        "direct shell execution is outside the fixed lockdown envelope",
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: shell.exec, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "shell.exec",
                            "direct shell execution is outside the fixed lockdown envelope",
                        )),
                    );
                }
                self.shell_exec(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    branch_id,
                    agent_id,
                    command,
                    cwd,
                )
                .await
            }
            RequestBody::ShellExec {
                command_id,
                session_id,
                worker_generation,
                command,
                cwd,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "direct shell execution requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        "shell.exec",
                        "direct shell execution is outside the fixed lockdown envelope",
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: shell.exec, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "shell.exec",
                            "direct shell execution is outside the fixed lockdown envelope",
                        )),
                    );
                }
                self.shell_exec(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    None,
                    None,
                    command,
                    cwd,
                )
                .await
            }
            RequestBody::ToolsInventory { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.tools_inventory(request_id, session_id).await
            }
            RequestBody::VaultStage {
                stage_id,
                purpose,
                secret,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.vault_stage(request_id, stage_id, purpose, secret)
            }
            RequestBody::TranscriptionSecretGet => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.transcription_secret_get(request_id)
            }
            RequestBody::TranscriptionSecretSet { secret, clear } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.transcription_secret_set(request_id, secret, clear)
            }
            RequestBody::AccountLoginApi {
                command_id,
                provider,
                alias,
                vault_reference,
                validation_model,
                replace_existing,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_login(
                    request_id,
                    AccountLoginInput {
                        command_id,
                        provider,
                        alias,
                        vault_reference,
                        validation_model,
                        replace_existing,
                    },
                )
            }
            RequestBody::AccountOAuthStart {
                provider,
                desired_alias,
                attempt_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_start(request_id, provider, desired_alias, attempt_id)
            }
            RequestBody::AccountOAuthStatus {
                flow_id,
                attempt_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_status(request_id, flow_id, attempt_id)
            }
            RequestBody::AccountOAuthCancel {
                flow_id,
                attempt_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_cancel(request_id, flow_id, attempt_id)
            }
            RequestBody::AccountOAuthImportSources => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_import_sources(request_id)
            }
            RequestBody::AccountOAuthImport { command_id, source } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_oauth_import(request_id, command_id, source)
            }
            RequestBody::AccountDeviceCandidates => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_device_candidates(request_id)
            }
            RequestBody::AccountSourceList => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_source_list(request_id)
            }
            RequestBody::AccountSourceAdd {
                command_id,
                kind,
                root,
                label,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_source_add(request_id, command_id, kind, root, label)
            }
            RequestBody::AccountSourceRemove {
                command_id,
                source_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_source_remove(request_id, command_id, source_id)
            }
            RequestBody::AccountSourceScan => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_source_scan(request_id)
            }
            RequestBody::AccountImportDevice {
                command_id,
                candidate,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_import_device(request_id, command_id, candidate)
            }
            RequestBody::AccountAdd {
                command_id,
                provider,
                alias,
                auth_method,
                flow_id,
                attempt_id,
                oauth_reference,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_add_oauth(
                    request_id,
                    command_id,
                    provider,
                    alias,
                    auth_method,
                    flow_id,
                    attempt_id,
                    oauth_reference,
                )
            }
            RequestBody::AccountSetLabel { alias, label } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_set_label(request_id, alias, label)
            }
            RequestBody::AccountRefresh { alias } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_refresh(request_id, alias)
            }
            RequestBody::AccountSetActive {
                command_id,
                alias,
                confirm_new_epoch,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_set_active(request_id, command_id, alias, confirm_new_epoch)
                    .await
            }
            RequestBody::AccountRemove {
                command_id,
                alias,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_remove(request_id, command_id, alias, expected_revision)
            }
            RequestBody::AccountSetDefaultModel {
                command_id,
                provider,
                model,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_set_default_model(
                    request_id,
                    command_id,
                    provider,
                    model,
                    expected_revision,
                )
            }
            RequestBody::AccountList { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_list(request_id, provider)
            }
            RequestBody::ProviderList { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_list(request_id, provider)
            }
            RequestBody::ProviderModelsRefresh { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_models_refresh(request_id, provider)
            }
            RequestBody::ProviderModelsProbe {
                provider,
                origin,
                api_family,
                keyless,
                probe_vault_reference,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if provider.trim().is_empty() || origin.trim().is_empty() {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "model-probe provider and origin must not be empty",
                        false,
                        None,
                    );
                }
                // Probing may borrow an already-vaulted credential as well
                // as a staged one; both require the local secret surface.
                if !keyless && self.secret_surface_facade(&request_id)?.is_none() {
                    return Ok(());
                }
                let probe_secret = if let Some(reference) = probe_vault_reference {
                    let borrowed = {
                        let mut stages = lock(&self.stages)?;
                        stages.probe(&reference)
                    };
                    match borrowed {
                        Some((haider_rpc::StagePurpose::ApiKey, secret)) => Some(secret),
                        Some(_) => {
                            return self.respond_error(
                                request_id,
                                ERROR_CODE_INVALID_ARGUMENT,
                                "staged secret was not staged for api_key use",
                                false,
                                None,
                            );
                        }
                        None => return self.respond_error(
                            request_id,
                            ERROR_CODE_RESTAGE_REQUIRED,
                            "staged secret is no longer available; stage the key again and retry",
                            true,
                            None,
                        ),
                    }
                } else {
                    None
                };
                self.send_management_command(
                    request_id.clone(),
                    crate::accounts::AccountCommand::ProbeProviderModels(Box::new(
                        crate::accounts::ProviderModelsProbeJob {
                            provider,
                            origin,
                            api_family,
                            keyless,
                            probe_secret,
                            route: crate::accounts::LoginRoute {
                                request_id,
                                sink: Arc::clone(&self.sink),
                            },
                        },
                    )),
                )
            }
            RequestBody::ProviderConfigure {
                command_id,
                provider,
                api_family,
                origin,
                auth_requirement,
                enabled,
                models,
                default_model,
                response_open_timeout_ms,
                chunk_idle_timeout_ms,
                semantic_progress_timeout_ms,
                probe_vault_reference,
                trust,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let probe_secret = if let Some(vault_reference) = probe_vault_reference {
                    if self.secret_surface_facade(&request_id)?.is_none() {
                        return Ok(());
                    }
                    let borrowed = {
                        let mut stages = lock(&self.stages)?;
                        stages.probe(&vault_reference)
                    };
                    match borrowed {
                        Some((haider_rpc::StagePurpose::ApiKey, secret)) => Some(secret),
                        Some(_) => {
                            return self.respond_error(
                                request_id,
                                ERROR_CODE_INVALID_ARGUMENT,
                                "staged secret was not staged for api_key use",
                                false,
                                None,
                            );
                        }
                        None => {
                            return self.respond_error(
                                request_id,
                                ERROR_CODE_RESTAGE_REQUIRED,
                                "staged secret is no longer available; stage the key again and retry",
                                true,
                                None,
                            );
                        }
                    }
                } else {
                    None
                };
                self.provider_configure(
                    request_id,
                    command_id,
                    crate::provider_registry::ProviderConfigureInput {
                        provider,
                        api_family,
                        origin,
                        auth_requirement,
                        enabled,
                        models,
                        default_model,
                        response_open_timeout_ms,
                        chunk_idle_timeout_ms,
                        semantic_progress_timeout_ms,
                        trust,
                    },
                    probe_secret,
                    expected_revision,
                )
            }
            RequestBody::ProviderRemove {
                command_id,
                provider,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_remove(request_id, command_id, provider, expected_revision)
            }
            RequestBody::ProviderSetTrust {
                command_id,
                name: provider,
                trust,
                expected_revision,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.provider_set_trust(request_id, command_id, provider, trust, expected_revision)
            }
            RequestBody::LockdownStatus { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.lockdown_status(request_id, provider.as_deref())
            }
            RequestBody::LockdownSetQuota { command_id, bytes } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.lockdown_set_quota(request_id, command_id, bytes).await
            }
            RequestBody::UsageReport => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.usage_report(request_id).await
            }
            RequestBody::UsageHistoryDay { date } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.usage_history_day(request_id, date).await
            }
            RequestBody::UsageHistoryRange { through_date, days } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.usage_history_range(request_id, through_date, days)
                    .await
            }
            RequestBody::MonitorList { session_id } => {
                if authorize(&self.capabilities, Operation::View).is_err() {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorList {
                            receipt: crate::monitor::monitor_list_rejected(
                                session_id,
                                haider_rpc::MonitorControlRejectionWire::CapabilityDenied {
                                    required: Capability::View,
                                },
                            ),
                        },
                    });
                }
                self.monitor_list(request_id, session_id).await
            }
            RequestBody::MonitorRegister {
                command_id,
                session_id,
                worker_generation,
                source,
                filter,
                action,
                occurrence,
                lifetime,
            } => {
                if authorize(&self.capabilities, Operation::Control).is_err() {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorRegister {
                            receipt: crate::monitor::monitor_register_rejected(
                                command_id,
                                session_id,
                                self.hub.worker_generation(),
                                haider_rpc::MonitorControlRejectionWire::CapabilityDenied {
                                    required: Capability::Control,
                                },
                            ),
                        },
                    });
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorRegister {
                            receipt: crate::monitor::monitor_register_rejected(
                                command_id,
                                session_id,
                                self.hub.worker_generation(),
                                haider_rpc::MonitorControlRejectionWire::ControlAttachmentRequired,
                            ),
                        },
                    });
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    let reason = "monitor registration is outside the fixed lockdown envelope";
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        "monitor.register",
                        reason,
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: monitor.register, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "monitor.register",
                            reason,
                        )),
                    );
                }
                self.monitor_register(
                    request_id,
                    crate::monitor::MonitorClientRegistrationRequest {
                        command_id,
                        session_id,
                        worker_generation,
                        source,
                        filter,
                        action,
                        occurrence,
                        lifetime,
                    },
                )
                .await
            }
            RequestBody::MonitorRemove {
                command_id,
                session_id,
                worker_generation,
                monitor_id,
            } => {
                if authorize(&self.capabilities, Operation::Control).is_err() {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorRemove {
                            receipt: crate::monitor::monitor_remove_rejected(
                                command_id,
                                session_id,
                                self.hub.worker_generation(),
                                haider_rpc::MonitorControlRejectionWire::CapabilityDenied {
                                    required: Capability::Control,
                                },
                            ),
                        },
                    });
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorRemove {
                            receipt: crate::monitor::monitor_remove_rejected(
                                command_id,
                                session_id,
                                self.hub.worker_generation(),
                                haider_rpc::MonitorControlRejectionWire::ControlAttachmentRequired,
                            ),
                        },
                    });
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    let reason = "monitor mutation is outside the fixed lockdown envelope";
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        "monitor.remove",
                        reason,
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: monitor.remove, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "monitor.remove",
                            reason,
                        )),
                    );
                }
                self.monitor_remove(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    monitor_id,
                )
                .await
            }
            RequestBody::MonitorMutate {
                command_id,
                session_id,
                worker_generation,
                mutation,
            } => {
                if authorize(&self.capabilities, Operation::Control).is_err() {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorMutate {
                            receipt: crate::monitor::monitor_mutate_rejected(
                                command_id,
                                session_id,
                                self.hub.worker_generation(),
                                haider_rpc::MonitorControlRejectionWire::CapabilityDenied {
                                    required: Capability::Control,
                                },
                            ),
                        },
                    });
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorMutate {
                            receipt: crate::monitor::monitor_mutate_rejected(
                                command_id,
                                session_id,
                                self.hub.worker_generation(),
                                haider_rpc::MonitorControlRejectionWire::ControlAttachmentRequired,
                            ),
                        },
                    });
                }
                let operation = crate::monitor::monitor_mutation_name(&mutation);
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    let reason = "monitor mutation is outside the fixed lockdown envelope";
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        operation,
                        reason,
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: {operation}, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(&provider, operation, reason)),
                    );
                }
                self.monitor_mutate(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    mutation,
                )
                .await
            }
            RequestBody::MonitorWatch {
                session_id,
                after_cursor,
            } => {
                if authorize(&self.capabilities, Operation::View).is_err() {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::MonitorWatch {
                            receipt: crate::monitor::monitor_watch_rejected(
                                session_id,
                                haider_rpc::MonitorControlRejectionWire::CapabilityDenied {
                                    required: Capability::View,
                                },
                            ),
                        },
                    });
                }
                self.monitor_watch(request_id, session_id, after_cursor)
                    .await
            }
            RequestBody::ComputerPermissionOpenSettings {
                session_id,
                request_id: permission_request_id,
                permission,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "opening computer permission settings requires a control attachment",
                        false,
                        None,
                    );
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    let reason = "GUI permission settings are outside the fixed lockdown envelope";
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        &permission_request_id,
                        &provider,
                        "computer.permission_open_settings",
                        reason,
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: computer.permission_open_settings, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "computer.permission_open_settings",
                            reason,
                        )),
                    );
                }
                self.computer_permission_open_settings(
                    request_id,
                    session_id,
                    permission_request_id,
                    permission,
                )
                .await
            }
            RequestBody::LoomInstallRetry { job_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_install_retry(request_id, job_id).await
            }
            RequestBody::LoomInstallWatch {
                job_id,
                after_cursor,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_install_watch(request_id, job_id, after_cursor)
                    .await
            }
            RequestBody::HeadlessRunStatus { run_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.headless_run_status(request_id, run_id).await
            }
            RequestBody::HeadlessRunStop { command_id, run_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.headless_run_stop(request_id, command_id, run_id).await
            }
            RequestBody::WorkflowGraphState {
                session_id,
                graph_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.workflow_graph_state(request_id, session_id, graph_id)
                    .await
            }
            RequestBody::WorkflowGraphWatch {
                session_id,
                after_cursor,
                limit,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.workflow_graph_watch(request_id, session_id, after_cursor, limit)
                    .await
            }
            RequestBody::LoomAuthorDraft {
                session_id,
                kind,
                prose,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_author_draft(request_id, session_id, kind, prose)
                    .await
            }
            RequestBody::LoomAuthorRevise {
                authoring_id,
                expected_revision,
                kind,
                text,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_author_revise(request_id, authoring_id, expected_revision, kind, text)
                    .await
            }
            RequestBody::LoomAuthorConfirm {
                authoring_id,
                expected_revision,
                kind,
                text,
                expected_rev,
                expected_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(expected_rev) = expected_rev else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "loom.author.confirm requires expected_rev under loom_registry_cas_v1",
                        false,
                        None,
                    );
                };
                self.loom_author_confirm(
                    request_id,
                    authoring_id,
                    expected_revision,
                    kind,
                    text,
                    haider_protocol::loom::LoomRevisionExpectation {
                        rev: expected_rev,
                        digest: expected_digest,
                    },
                )
                .await
            }
            RequestBody::LoomInstallCancel { install_job_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_install_cancel(request_id, install_job_id).await
            }
            RequestBody::LoomArchive {
                kind,
                id,
                expected_rev,
                expected_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_archive(
                    request_id,
                    kind,
                    id,
                    true,
                    haider_protocol::loom::LoomRevisionExpectation {
                        rev: expected_rev,
                        digest: expected_digest,
                    },
                )
                .await
            }
            RequestBody::LoomUnarchive {
                kind,
                id,
                expected_rev,
                expected_digest,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_archive(
                    request_id,
                    kind,
                    id,
                    false,
                    haider_protocol::loom::LoomRevisionExpectation {
                        rev: expected_rev,
                        digest: expected_digest,
                    },
                )
                .await
            }
            RequestBody::LoomValidate { kind, text } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_validate(request_id, kind, text).await
            }
            RequestBody::LoomWatch { after_cursor } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.loom_watch(request_id, after_cursor).await
            }
            RequestBody::CheckpointList {
                session_id,
                branch_id,
                cursor,
                limit,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.checkpoint_list(request_id, session_id, branch_id, cursor, limit)
                    .await
            }
            RequestBody::CheckpointUndo {
                command_id,
                session_id,
                branch_id,
                worker_generation,
                target,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        "checkpoint.undo",
                        "checkpoint application is outside the fixed lockdown envelope",
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: checkpoint.undo, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "checkpoint.undo",
                            "checkpoint application is outside the fixed lockdown envelope",
                        )),
                    );
                }
                self.checkpoint_mutate(
                    request_id,
                    CheckpointDoorInput {
                        command_id,
                        session_id,
                        branch_id,
                        worker_generation,
                        action: CheckpointDoorAction::Undo { target },
                    },
                )
                .await
            }
            RequestBody::CheckpointRedo {
                command_id,
                session_id,
                branch_id,
                worker_generation,
                target,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        "checkpoint.redo",
                        "checkpoint application is outside the fixed lockdown envelope",
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: checkpoint.redo, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "checkpoint.redo",
                            "checkpoint application is outside the fixed lockdown envelope",
                        )),
                    );
                }
                self.checkpoint_mutate(
                    request_id,
                    CheckpointDoorInput {
                        command_id,
                        session_id,
                        branch_id,
                        worker_generation,
                        action: CheckpointDoorAction::Redo { target },
                    },
                )
                .await
            }
            RequestBody::CheckpointRollbackTurn {
                command_id,
                session_id,
                branch_id,
                worker_generation,
                run_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if let Some(provider) = self.session_lockdown_provider(&session_id).await? {
                    self.journal_session_lockdown_refusal(
                        &session_id,
                        command_id.as_str(),
                        &provider,
                        "checkpoint.rollback_turn",
                        "checkpoint application is outside the fixed lockdown envelope",
                    )
                    .await?;
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PERMISSION_DENIED,
                        &format!(
                            "RefusedByLockdown {{ tool: checkpoint.rollback_turn, reason: provider {provider} is in lockdown mode }}"
                        ),
                        false,
                        Some(lockdown_refusal_error_data(
                            &provider,
                            "checkpoint.rollback_turn",
                            "checkpoint application is outside the fixed lockdown envelope",
                        )),
                    );
                }
                self.checkpoint_mutate(
                    request_id,
                    CheckpointDoorInput {
                        command_id,
                        session_id,
                        branch_id,
                        worker_generation,
                        action: CheckpointDoorAction::RollbackTurn { run_id },
                    },
                )
                .await
            }
            RequestBody::PeerList {} => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.hub.enable_peer_events(&self.connection_id)?;
                match self.hub.peer_service()?.list().await {
                    Ok(agents) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::PeerList { agents },
                    }),
                    Err(error) => self.respond_peer_error(request_id, error),
                }
            }
            RequestBody::PeerSend {
                to,
                message,
                summary,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let sessions = self.hub.peer_control_sessions(&self.connection_id)?;
                let [session_id] = sessions.as_slice() else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PEER_INVALID,
                        "peer.send requires exactly one control-attached sender session",
                        false,
                        None,
                    );
                };
                self.hub.enable_peer_events(&self.connection_id)?;
                match self
                    .hub
                    .peer_service()?
                    .send(session_id, to, message, summary)
                    .await
                {
                    Ok(receipt) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::PeerSend { receipt },
                    }),
                    Err(error) => self.respond_peer_error(request_id, error),
                }
            }
            RequestBody::PeerName { name } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let sessions = self.hub.peer_control_sessions(&self.connection_id)?;
                let [session_id] = sessions.as_slice() else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_PEER_INVALID,
                        "peer.name requires exactly one control-attached session",
                        false,
                        None,
                    );
                };
                self.peer_name(request_id, session_id.clone(), name).await
            }
            RequestBody::SshList { session_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(ssh) = self.hub.ssh()? else {
                    return self.respond_error(
                        request_id,
                        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                        "SSH profile secret storage is unavailable",
                        false,
                        None,
                    );
                };
                let scope = match session_id {
                    Some(session_id) => {
                        if self.hub.session_metadata(&session_id).await?.is_none() {
                            return self.respond_error(
                                request_id,
                                ERROR_CODE_NOT_FOUND,
                                "session was not found",
                                false,
                                None,
                            );
                        }
                        self.hub.ssh_scope(&session_id)?
                    }
                    None => crate::ssh::SshScope::All,
                };
                match ssh.store.list() {
                    Ok(profiles) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::SshList {
                            profiles: profiles
                                .into_iter()
                                .map(|profile| {
                                    let in_scope = scope.allows(&profile.name);
                                    profile.public_with_scope(in_scope)
                                })
                                .collect(),
                        },
                    }),
                    Err(error) => self.respond_ssh_error(request_id, error),
                }
            }
            RequestBody::SshAdd { profile } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(ssh) = self.hub.ssh()? else {
                    return self.respond_error(
                        request_id,
                        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                        "SSH profile secret storage is unavailable",
                        false,
                        None,
                    );
                };
                let Some(auth) =
                    self.ssh_auth_from_wire(&request_id, &ssh.store, &profile.name, profile.auth)?
                else {
                    return Ok(());
                };
                let stored = crate::ssh::SshProfile {
                    name: profile.name,
                    description: profile.description,
                    ssh: crate::ssh::SshTarget {
                        host: profile.host,
                        port: profile.port,
                        user: profile.user,
                        auth,
                        default_cwd: profile.default_cwd,
                        host_key: None,
                    },
                    last_used_ms: None,
                };
                match ssh.store.add(stored.clone()) {
                    Ok(profile) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::SshAdd {
                            profile: profile.public(),
                        },
                    }),
                    Err(error) => {
                        ssh.store.discard_auth_secret(&stored.ssh.auth);
                        self.respond_ssh_error(request_id, error)
                    }
                }
            }
            RequestBody::SshUpdate { name, mut changes } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(ssh) = self.hub.ssh()? else {
                    return self.respond_error(
                        request_id,
                        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                        "SSH profile secret storage is unavailable",
                        false,
                        None,
                    );
                };
                let auth = match changes.auth.take() {
                    Some(auth) => {
                        let Some(auth) =
                            self.ssh_auth_from_wire(&request_id, &ssh.store, &name, auth)?
                        else {
                            return Ok(());
                        };
                        Some(auth)
                    }
                    None => None,
                };
                match ssh.store.update_non_secret(&name, changes, auth.clone()) {
                    Ok(profile) => {
                        ssh.runtime.forget(&name).await;
                        self.send(WireFrame::Response {
                            request_id,
                            body: ResponseBody::SshUpdate {
                                profile: profile.public(),
                            },
                        })
                    }
                    Err(error) => {
                        if let Some(auth) = &auth {
                            ssh.store.discard_auth_secret(auth);
                        }
                        self.respond_ssh_error(request_id, error)
                    }
                }
            }
            RequestBody::SshRemove { name } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(ssh) = self.hub.ssh()? else {
                    return self.respond_error(
                        request_id,
                        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                        "SSH profile secret storage is unavailable",
                        false,
                        None,
                    );
                };
                match ssh.store.remove(&name) {
                    Ok(()) => {
                        ssh.runtime.forget(&name).await;
                        self.send(WireFrame::Response {
                            request_id,
                            body: ResponseBody::SshRemove { removed: name },
                        })
                    }
                    Err(error) => self.respond_ssh_error(request_id, error),
                }
            }
            RequestBody::SshTest { name, timeout_s } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(ssh) = self.hub.ssh()? else {
                    return self.respond_error(
                        request_id,
                        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                        "SSH profile secret storage is unavailable",
                        false,
                        None,
                    );
                };
                let timeout = match ssh_timeout(timeout_s) {
                    Ok(timeout) => timeout,
                    Err(error) => return self.respond_ssh_error(request_id, error),
                };
                match ssh.runtime.test(&name, timeout).await {
                    Ok(host_key_pinned) => match ssh.store.get(&name) {
                        Ok(profile) => self.send(WireFrame::Response {
                            request_id,
                            body: ResponseBody::SshTest {
                                result: haider_rpc::SshTestResultWire {
                                    profile: profile.public(),
                                    connected: true,
                                    host_key_pinned,
                                },
                            },
                        }),
                        Err(error) => self.respond_ssh_error(request_id, error),
                    },
                    Err(error) => self.respond_ssh_error(request_id, error),
                }
            }
            RequestBody::SessionSetSshScope { session_id, scope } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if self.hub.session_metadata(&session_id).await?.is_none() {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_NOT_FOUND,
                        "session was not found",
                        false,
                        None,
                    );
                }
                match crate::ssh::SshScope::from_wire(scope) {
                    Ok(scope) => {
                        let public = scope.to_wire();
                        self.hub.set_ssh_scope(session_id.clone(), scope)?;
                        self.send(WireFrame::Response {
                            request_id,
                            body: ResponseBody::SessionSetSshScope {
                                session_id,
                                scope: public,
                            },
                        })
                    }
                    Err(error) => self.respond_ssh_error(request_id, error),
                }
            }
            RequestBody::SshShell {
                name,
                command,
                cwd,
                timeout_s,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let control_sessions = self.hub.peer_control_sessions(&self.connection_id)?;
                if matches!(
                    direct_ssh_session(&control_sessions),
                    DirectSshSession::Ambiguous
                ) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "ssh.shell requires at most one control-attached session",
                        false,
                        None,
                    );
                }
                if let DirectSshSession::Session(session_id) = direct_ssh_session(&control_sessions)
                {
                    if let Some(provider) = self.session_lockdown_provider(session_id).await? {
                        let reason = "remote SSH execution is outside the fixed lockdown envelope";
                        self.journal_session_lockdown_refusal(
                            session_id,
                            request_id.0.as_str(),
                            &provider,
                            "ssh_shell",
                            reason,
                        )
                        .await?;
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_PERMISSION_DENIED,
                            &format!(
                                "RefusedByLockdown {{ tool: ssh_shell, reason: provider {provider} is in lockdown mode }}"
                            ),
                            false,
                            Some(lockdown_refusal_error_data(&provider, "ssh_shell", reason)),
                        );
                    }
                    let scope = self.hub.ssh_scope(session_id)?;
                    if let Err(error) = crate::ssh::enforce_scope(&scope, session_id, &name) {
                        return self.respond_ssh_error(request_id, error);
                    }
                }
                let Some(ssh) = self.hub.ssh()? else {
                    return self.respond_error(
                        request_id,
                        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                        "SSH profile secret storage is unavailable",
                        false,
                        None,
                    );
                };
                let profile = match ssh.store.get(&name) {
                    Ok(profile) => profile,
                    Err(error) => return self.respond_ssh_error(request_id, error),
                };
                let timeout = match ssh_timeout(timeout_s) {
                    Ok(timeout) => timeout,
                    Err(error) => return self.respond_ssh_error(request_id, error),
                };
                let shell = self
                    .hub
                    .shell_registry()
                    .open(
                        haider_rpc::ShellKindWire::Ssh {
                            profile: name.clone(),
                        },
                        format!("ssh {name}"),
                        profile.ssh.host,
                    )
                    .map_err(|error| SessionHubError::Task(error.to_string()))?;
                shell
                    .running()
                    .map_err(|error| SessionHubError::Task(error.to_string()))?;
                let result = ssh
                    .runtime
                    .exec(crate::ssh::SshExecRequest {
                        profile: name,
                        command,
                        cwd,
                        timeout,
                        close: Some(shell.close_receiver()),
                        output: None,
                    })
                    .await;
                match result {
                    Ok(result) => {
                        shell
                            .add_output(result.stdout.len().saturating_add(result.stderr.len()))
                            .map_err(|error| SessionHubError::Task(error.to_string()))?;
                        shell
                            .exited(result.exit_code)
                            .map_err(|error| SessionHubError::Task(error.to_string()))?;
                        self.send(WireFrame::Response {
                            request_id,
                            body: ResponseBody::SshShell { result },
                        })
                    }
                    Err(error) => {
                        shell.exited(None).map_err(|registry_error| {
                            SessionHubError::Task(registry_error.to_string())
                        })?;
                        self.respond_ssh_error(request_id, error)
                    }
                }
            }
            RequestBody::SshShellOpen {
                name,
                session_id,
                term,
                size,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let Some(ssh) = self.hub.ssh()? else {
                    return self.respond_error(
                        request_id,
                        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                        "SSH profile secret storage is unavailable",
                        false,
                        None,
                    );
                };
                if let Some(session_id) = session_id.as_ref() {
                    if self.hub.session_metadata(session_id).await?.is_none() {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_NOT_FOUND,
                            "session was not found",
                            false,
                            None,
                        );
                    }
                    if !self
                        .hub
                        .holds_control_attachment(&self.connection_id, session_id)?
                    {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_CAPABILITY_DENIED,
                            "SSH terminal open requires a control attachment to this session",
                            false,
                            None,
                        );
                    }
                    if let Some(provider) = self.session_lockdown_provider(session_id).await? {
                        let reason = "remote SSH execution is outside the fixed lockdown envelope";
                        self.journal_session_lockdown_refusal(
                            session_id,
                            request_id.0.as_str(),
                            &provider,
                            "ssh_shell",
                            reason,
                        )
                        .await?;
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_PERMISSION_DENIED,
                            &format!(
                                "RefusedByLockdown {{ tool: ssh_shell, reason: provider {provider} is in lockdown mode }}"
                            ),
                            false,
                            Some(lockdown_refusal_error_data(&provider, "ssh_shell", reason)),
                        );
                    }
                    let scope = self.hub.ssh_scope(session_id)?;
                    if let Err(error) = crate::ssh::enforce_scope(&scope, session_id, &name) {
                        return self.respond_ssh_error(request_id, error);
                    }
                }
                let profile = match ssh.store.get(&name) {
                    Ok(profile) => profile,
                    Err(error) => return self.respond_ssh_error(request_id, error),
                };
                let (shell, controls) = self
                    .hub
                    .shell_registry()
                    .open_interactive(
                        haider_rpc::ShellKindWire::Ssh {
                            profile: name.clone(),
                        },
                        format!("ssh {name}"),
                        profile.ssh.host,
                        Some(self.connection_id.clone()),
                    )
                    .map_err(|error| SessionHubError::Task(error.to_string()))?;
                let (activate, activation) = tokio::sync::oneshot::channel();
                match ssh
                    .runtime
                    .start_pty(crate::ssh::SshPtyRequest {
                        profile: name,
                        term,
                        size,
                        shell,
                        controls,
                        activation: Some(activation),
                    })
                    .await
                {
                    Ok(shell) => {
                        self.send(WireFrame::Response {
                            request_id,
                            body: ResponseBody::SshShellOpen { shell },
                        })?;
                        let _ = activate.send(());
                        Ok(())
                    }
                    Err(error) => self.respond_ssh_error(request_id, error),
                }
            }
            RequestBody::SshShellInput { id, data_b64 } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let bytes = match base64::engine::general_purpose::STANDARD
                    .decode(data_b64.expose_secret())
                {
                    Ok(bytes)
                        if !bytes.is_empty()
                            && bytes.len() <= haider_rpc::SSH_PTY_INPUT_MAX_BYTES =>
                    {
                        zeroize::Zeroizing::new(bytes)
                    }
                    Ok(_) => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "SSH terminal input must decode to 1..=65536 bytes",
                            false,
                            None,
                        );
                    }
                    Err(_) => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "SSH terminal input is not padded standard base64",
                            false,
                            None,
                        );
                    }
                };
                match self.hub.shell_registry().control(
                    &id,
                    Some(&self.connection_id),
                    crate::shell_registry::ShellControl::Input(bytes),
                ) {
                    Ok(shell) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::SshShellInput { shell },
                    }),
                    Err(error) => self.respond_shell_control_error(request_id, error),
                }
            }
            RequestBody::SshShellResize { id, size } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if size.cols == 0 || size.rows == 0 {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "SSH terminal rows and columns must be greater than zero",
                        false,
                        None,
                    );
                }
                match self.hub.shell_registry().control(
                    &id,
                    Some(&self.connection_id),
                    crate::shell_registry::ShellControl::Resize(size),
                ) {
                    Ok(shell) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::SshShellResize { shell },
                    }),
                    Err(error) => self.respond_shell_control_error(request_id, error),
                }
            }
            RequestBody::SshShellEof { id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                match self.hub.shell_registry().control(
                    &id,
                    Some(&self.connection_id),
                    crate::shell_registry::ShellControl::Eof,
                ) {
                    Ok(shell) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::SshShellEof { shell },
                    }),
                    Err(error) => self.respond_shell_control_error(request_id, error),
                }
            }
            RequestBody::ShellList => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                let shells = self
                    .hub
                    .shell_registry()
                    .list()
                    .map_err(|error| SessionHubError::Task(error.to_string()))?;
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::ShellList { shells },
                })
            }
            RequestBody::ShellClose { id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                match self
                    .hub
                    .shell_registry()
                    .close_control(&id, Some(&self.connection_id))
                {
                    Ok(shell) => self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::ShellClose { shell },
                    }),
                    Err(crate::shell_registry::ShellRegistryError::NotFound(_)) => self
                        .respond_error(
                            request_id,
                            ERROR_CODE_NOT_FOUND,
                            &format!("shell `{id}` was not found"),
                            false,
                            None,
                        ),
                    Err(error) => self.respond_shell_control_error(request_id, error),
                }
            }
            // `Unknown` and any future method decode alike: a typed,
            // correlated rejection instead of a dropped request.
            _ => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unknown session method",
                false,
                None,
            ),
        }
    }

    fn respond_peer_error(
        &self,
        request_id: RequestId,
        error: crate::peer::PeerError,
    ) -> Result<(), SessionHubError> {
        match error {
            crate::peer::PeerError::Ambiguous { candidates } => self.respond_error(
                request_id,
                ERROR_CODE_PEER_AMBIGUOUS,
                "peer address is ambiguous; qualify it with an id prefix",
                false,
                Some(ErrorData::PeerAmbiguous { candidates }),
            ),
            crate::peer::PeerError::Invalid { message } => {
                self.respond_error(request_id, ERROR_CODE_PEER_INVALID, &message, false, None)
            }
            error => self.respond_error(
                request_id,
                ERROR_CODE_PEER_UNAVAILABLE,
                &error.to_string(),
                true,
                None,
            ),
        }
    }

    /// `peer.name` is the peer-shaped view of the existing durable session
    /// rename authority. The request has no caller-supplied command id, so
    /// this door mints one command/event coordinate and returns only after the
    /// peer publication has reconciled the committed title.
    async fn peer_name(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        name: String,
    ) -> Result<(), SessionHubError> {
        let Some(title) = normalize_session_title(Some(name)) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_PEER_INVALID,
                "peer.name requires a non-empty printable name",
                false,
                None,
            );
        };
        let Some(summary) = self
            .hub
            .peer_session_summaries()
            .await?
            .into_iter()
            .find(|summary| summary.session_id == session_id)
        else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "peer.name session is not live",
                false,
                None,
            );
        };
        let command_id = random_id("peer-name")?;
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": summary.worker_generation,
            "title": &title,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode peer-name coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let command = SessionRenameCommand {
            command_id: command_id.clone(),
            request_digest,
            request_json,
            session_id: session_id.clone(),
            worker_generation: summary.worker_generation,
            title: Some(title),
            only_if_untitled: false,
            event_id: EventId::new(format!("session-renamed-{command_id}")),
            device_id: self.hub.inner.device_id.clone(),
        };
        match self.hub.rename_session(command).await {
            Ok(
                SessionRenameOutcome::Committed { .. }
                | SessionRenameOutcome::IdempotentReplay { .. },
            ) => {}
            Ok(SessionRenameOutcome::Skipped) => {
                return Err(SessionHubError::Task(
                    "explicit peer name cannot be skipped".into(),
                ));
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        let agents = match self.hub.peer_service()?.list().await {
            Ok(agents) => agents,
            Err(error) => return self.respond_peer_error(request_id, error),
        };
        let Some(agent) = agents
            .into_iter()
            .find(|agent| agent.id == session_id.as_str())
        else {
            return self.respond_error(
                request_id,
                ERROR_CODE_PEER_UNAVAILABLE,
                "renamed peer publication is unavailable",
                true,
                None,
            );
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::PeerName { agent },
        })
    }

    async fn command_invoke(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        command: String,
        session_id: Option<SessionId>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "command.invoke needs a non-empty command id",
                false,
                None,
            );
        }
        let normalized = command.trim().strip_prefix('/').unwrap_or(command.trim());
        let mut parts = normalized.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").to_ascii_lowercase();
        let argument = parts.next().map_or("", str::trim);
        let Some(spec) = haider_rpc::command_spec(&name) else {
            return self.respond_command_outcome(
                request_id,
                haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                    command: name,
                    reason: Some("unknown command".into()),
                },
            );
        };
        // `/model` at the launcher selects a client-local default identity;
        // there is no session truth for this daemon to mutate.
        if name == "model" && session_id.is_none() {
            return self.respond_command_outcome(
                request_id,
                haider_rpc::CommandInvokeOutcomeWire::ClientOwned { command: name },
            );
        }
        match spec.ownership {
            haider_rpc::CommandOwnershipWire::ClientView => {
                return self.respond_command_outcome(
                    request_id,
                    haider_rpc::CommandInvokeOutcomeWire::ClientOwned {
                        command: spec.name.to_owned(),
                    },
                );
            }
            haider_rpc::CommandOwnershipWire::DaemonOperation => {}
            // `Unknown` and future ownership kinds are non-executable. The
            // fallback deliberately asserts no concrete daemon action.
            _ => {
                return self.respond_command_outcome(
                    request_id,
                    haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                        command: spec.name.to_owned(),
                        reason: Some("command ownership is unknown".into()),
                    },
                );
            }
        }

        let supported_session_command = matches!(
            name.as_str(),
            "compact" | "rename" | "model" | "provider" | "effort" | "fast"
        );
        if !supported_session_command {
            return self.respond_command_outcome(
                request_id,
                haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                    command: name,
                    reason: Some("daemon operation is not implemented by the command door".into()),
                },
            );
        }
        let Some(session_id) = session_id else {
            return self.respond_command_outcome(
                request_id,
                haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                    command: name,
                    reason: Some("command requires a session".into()),
                },
            );
        };
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "command.invoke requires a control attachment to this session",
                false,
                None,
            );
        }

        let continuation = match name.as_str() {
            "rename" if argument.is_empty() => {
                return self
                    .park_command_menu(request_id, &command_id, session_id, "rename")
                    .await;
            }
            "rename" => ParkedCommandContinuation::Rename(argument.to_owned()),
            "model" if argument.is_empty() => {
                return self
                    .park_command_menu(request_id, &command_id, session_id, "model")
                    .await;
            }
            "model" => ParkedCommandContinuation::Model(argument.to_owned()),
            "provider" if argument.is_empty() => {
                return self
                    .park_command_menu(request_id, &command_id, session_id, "provider")
                    .await;
            }
            "provider" => ParkedCommandContinuation::Provider(argument.to_owned()),
            "effort" if argument.is_empty() => {
                return self
                    .park_command_menu(request_id, &command_id, session_id, "effort")
                    .await;
            }
            "effort" if argument.eq_ignore_ascii_case("default") => {
                ParkedCommandContinuation::Effort(None)
            }
            "effort" => ParkedCommandContinuation::Effort(Some(argument.to_owned())),
            "fast" if argument.is_empty() => {
                return self
                    .park_command_menu(request_id, &command_id, session_id, "fast")
                    .await;
            }
            "fast" => {
                let enabled = match argument.to_ascii_lowercase().as_str() {
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    _ => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "fast accepts only on or off",
                            false,
                            None,
                        );
                    }
                };
                ParkedCommandContinuation::Fast(enabled)
            }
            "compact" if argument.is_empty() => ParkedCommandContinuation::Compact,
            "compact" => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "compact takes no arguments",
                    false,
                    None,
                );
            }
            _ => {
                return self.respond_command_outcome(
                    request_id,
                    haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                        command: name,
                        reason: Some("command form is unsupported".into()),
                    },
                );
            }
        };
        let expected = continuation.receipt_kind();
        let operation_generation = match self
            .hub
            .command_receipt_worker_generation(&command_id, continuation.canonical_method())
            .await
        {
            Ok(Some(generation)) => generation,
            Ok(None) => self.hub.worker_generation(),
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        let body = self
            .execute_command_continuation(
                request_id.clone(),
                command_id.clone(),
                session_id.clone(),
                continuation.clone(),
                operation_generation,
                false,
            )
            .await?;
        if let ResponseBody::Error { code, message, .. } = &body
            && code == haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED
        {
            let stored = self
                .ensure_cache_confirmation_menu(
                    &session_id,
                    &command_id,
                    spec.name,
                    &continuation,
                    message.clone(),
                )
                .await?;
            return self
                .respond_existing_command_menu(request_id, session_id, spec.name, stored)
                .await;
        }
        self.respond_command_body(request_id, body, expected)
    }

    fn respond_command_outcome(
        &self,
        request_id: RequestId,
        outcome: haider_rpc::CommandInvokeOutcomeWire,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::CommandInvoke { outcome },
        })
    }

    fn respond_command_body(
        &self,
        request_id: RequestId,
        body: ResponseBody,
        expected: CommandReceiptKind,
    ) -> Result<(), SessionHubError> {
        if matches!(body, ResponseBody::Error { .. }) {
            return self.send(WireFrame::Response { request_id, body });
        }
        if !expected.accepts(&body) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "command handler returned an unexpected response; no success was asserted",
                false,
                None,
            );
        }
        self.respond_command_outcome(
            request_id,
            haider_rpc::CommandInvokeOutcomeWire::Receipt {
                receipt: Box::new(body),
            },
        )
    }

    fn command_capture_connection(&self) -> (HubConnection, Arc<CommandResponseCapture>) {
        let capture = Arc::new(CommandResponseCapture::default());
        let sink: Arc<dyn FrameSink> = capture.clone();
        (
            HubConnection {
                hub: self.hub.clone(),
                connection_id: self.connection_id.clone(),
                capabilities: self.capabilities.clone(),
                sink,
                transport: self.transport,
                runtime_paths: None,
                daemon_idle_ttl_ms: self.daemon_idle_ttl_ms,
                daemon_warm: self.daemon_warm,
                daemon_readiness: self.daemon_readiness.clone(),
                stages: Mutex::new(crate::accounts::StagedSecrets::default()),
                roster_watch: Mutex::new(None),
                accounts_watch: Mutex::new(None),
                surface_watch: Mutex::new(None),
                monitor_watch: Mutex::new(None),
                loom_registry_watch: Mutex::new(None),
                loom_registry_watch_serial: tokio::sync::Mutex::new(()),
                metafork_reviews: Arc::clone(&self.metafork_reviews),
                loom_author_sessions: Arc::clone(&self.loom_author_sessions),
                identity_lease: Arc::clone(&self.identity_lease),
                closed: AtomicBool::new(false),
            },
            capture,
        )
    }

    fn take_command_response(
        capture: &CommandResponseCapture,
        request_id: &RequestId,
    ) -> Result<ResponseBody, SessionHubError> {
        let frame = capture
            .frame
            .lock()
            .map_err(|_| SessionHubError::Task("command response capture poisoned".into()))?
            .take()
            .ok_or_else(|| SessionHubError::Task("command handler produced no response".into()))?;
        match frame {
            WireFrame::Response {
                request_id: captured,
                body,
            } if captured == *request_id => Ok(body),
            _ => Err(SessionHubError::Task(
                "command handler produced a non-correlated response".into(),
            )),
        }
    }

    async fn execute_command_continuation(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        continuation: ParkedCommandContinuation,
        generation: u64,
        confirm_new_epoch: bool,
    ) -> Result<ResponseBody, SessionHubError> {
        let (connection, capture) = self.command_capture_connection();
        match continuation {
            ParkedCommandContinuation::Compact => {
                connection
                    .session_compact(request_id.clone(), command_id, session_id, generation, None)
                    .await?;
            }
            ParkedCommandContinuation::Rename(title) => {
                connection
                    .session_rename(
                        request_id.clone(),
                        command_id,
                        session_id,
                        generation,
                        Some(title),
                    )
                    .await?;
            }
            ParkedCommandContinuation::Model(model) => {
                connection
                    .session_select_model(
                        request_id.clone(),
                        SessionSelectModelInput {
                            command_id,
                            session_id,
                            worker_generation: generation,
                            model,
                            provider: None,
                            confirm_new_epoch,
                        },
                    )
                    .await?;
            }
            ParkedCommandContinuation::Provider(provider) => {
                let model = match self.provider_default_model(&session_id, &provider).await {
                    Ok(Some(model)) => model,
                    Ok(None) => {
                        return Ok(ResponseBody::Error {
                            code: ERROR_CODE_INVALID_ARGUMENT.into(),
                            message: format!("provider {provider} has no daemon-known model"),
                            retryable: false,
                            data: None,
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            provider,
                            %error,
                            "could not determine provider model inventory for command"
                        );
                        return Ok(ResponseBody::Error {
                            code: ERROR_CODE_PROVIDER_MODELS_UNKNOWN.into(),
                            message: format!(
                                "could not determine the daemon-known model set for provider {provider}"
                            ),
                            retryable: true,
                            data: None,
                        });
                    }
                };
                connection
                    .session_select_model(
                        request_id.clone(),
                        SessionSelectModelInput {
                            command_id,
                            session_id,
                            worker_generation: generation,
                            model,
                            provider: Some(provider),
                            confirm_new_epoch,
                        },
                    )
                    .await?;
            }
            ParkedCommandContinuation::Effort(effort) => {
                connection
                    .session_select_effort(
                        request_id.clone(),
                        command_id,
                        session_id,
                        generation,
                        effort,
                        confirm_new_epoch,
                    )
                    .await?;
            }
            ParkedCommandContinuation::Fast(enabled) => {
                connection
                    .session_select_fast(
                        request_id.clone(),
                        command_id,
                        session_id,
                        generation,
                        enabled,
                        confirm_new_epoch,
                    )
                    .await?;
            }
        }
        Self::take_command_response(&capture, &request_id)
    }

    async fn provider_default_model(
        &self,
        session_id: &SessionId,
        provider: &str,
    ) -> Result<Option<String>, SessionHubError> {
        let facade = self.hub.accounts()?;
        if let Some(facade) = facade.as_ref() {
            let view = facade.management.read().ok_or_else(|| {
                SessionHubError::Task("account management snapshot is poisoned".into())
            })?;
            if let Some(summary) = view
                .providers
                .iter()
                .find(|summary| summary.provider == provider)
            {
                return Ok(summary
                    .default_model
                    .clone()
                    .or_else(|| summary.models.first().cloned()));
            }
        }
        let metadata_model = self
            .hub
            .session_metadata(session_id)
            .await?
            .filter(|metadata| metadata.provider == provider)
            .map(|metadata| metadata.model);
        if metadata_model.is_some() || facade.is_some() {
            return Ok(metadata_model);
        }
        Err(SessionHubError::Task(
            "account provider inventory is unavailable".into(),
        ))
    }

    async fn stored_command_menu(
        &self,
        session_id: &SessionId,
        command: &str,
        invocation_key: &str,
    ) -> Result<StoredCommandMenuLookup, SessionHubError> {
        let mut after_seq = 0;
        let mut found = None::<StoredCommandMenu>;
        loop {
            let page = self
                .hub
                .inner
                .store
                .read(session_id, after_seq, REPLAY_PAGE_SIZE)
                .await?;
            if page.is_empty() {
                break;
            }
            for envelope in &page {
                let Ok(payload) = envelope.payload.decode_event() else {
                    continue;
                };
                match payload {
                    EventPayload::MenuOpened(menu) => {
                        let Ok(Some(CommandMenuOrigin {
                            command: stored_command,
                            invocation_key: Some(stored_key),
                            ..
                        })) = command_menu_origin_parts(&menu.origin)
                        else {
                            continue;
                        };
                        if stored_key != invocation_key {
                            continue;
                        }
                        if stored_command != command {
                            return Ok(StoredCommandMenuLookup::CommandIdConflict);
                        }
                        found.get_or_insert(StoredCommandMenu {
                            opening: envelope.clone(),
                            menu,
                            answer: None,
                            closed: false,
                        });
                    }
                    EventPayload::MenuAnswered(answer) => {
                        if let Some(stored) = found.as_mut()
                            && answer.menu == stored.menu.id
                        {
                            stored.answer = Some(answer);
                        }
                    }
                    EventPayload::MenuClosed { menu, .. } => {
                        if let Some(stored) = found.as_mut()
                            && menu == stored.menu.id
                        {
                            stored.closed = true;
                        }
                    }
                    _ => {}
                }
            }
            let page_len = page.len();
            after_seq = page.last().map_or(after_seq, |envelope| envelope.seq);
            if page_len < REPLAY_PAGE_SIZE {
                break;
            }
        }
        Ok(found.map_or(
            StoredCommandMenuLookup::Missing,
            StoredCommandMenuLookup::Found,
        ))
    }

    fn command_needs_input(
        opening: &haider_protocol::envelope::RawEnvelope,
        menu: &Menu,
    ) -> haider_rpc::NeedsInputWire {
        haider_rpc::NeedsInputWire {
            kind: match menu.kind {
                MenuKind::Question => haider_rpc::NeedsInputKindWire::Question,
                MenuKind::Choice => haider_rpc::NeedsInputKindWire::Choice,
                _ => haider_rpc::NeedsInputKindWire::Unknown,
            },
            title: menu.title.clone(),
            safe_body: menu.body.clone(),
            menu_id: Some(menu.id.clone()),
            request_seq: Some(opening.seq),
            worker_generation: Some(opening.worker_generation),
            since_ms: Some(opening.committed_at_ms),
            options: menu
                .options
                .iter()
                .cloned()
                .map(|option| haider_rpc::ObserveMenuOptionWire {
                    key: option.key,
                    label: option.label,
                    detail: option.detail,
                    decision: None,
                })
                .collect(),
            secret_answer: false,
        }
    }

    fn respond_stored_command_menu(
        &self,
        request_id: RequestId,
        stored: &StoredCommandMenu,
    ) -> Result<(), SessionHubError> {
        self.respond_command_outcome(
            request_id,
            haider_rpc::CommandInvokeOutcomeWire::Parked {
                needs_input: Self::command_needs_input(&stored.opening, &stored.menu),
            },
        )
    }

    async fn respond_existing_command_menu(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        command: &str,
        stored: StoredCommandMenu,
    ) -> Result<(), SessionHubError> {
        if let Some(answer) = stored.answer.as_ref() {
            return match self
                .command_menu_lookup(&session_id, stored.opening.seq, &stored.menu.id, answer)
                .await?
            {
                CommandMenuLookup::Continuation(resolved) => {
                    let (body, expected) = self
                        .execute_parked_command(
                            session_id,
                            &stored.menu.id,
                            resolved.action,
                            resolved.opening_generation,
                            resolved.confirm_new_epoch,
                        )
                        .await?;
                    self.respond_command_body(request_id, body, expected)
                }
                CommandMenuLookup::Invalid(message) => self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                ),
                CommandMenuLookup::Ordinary => self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "stored command menu lost its typed origin; no action was taken",
                    false,
                    None,
                ),
            };
        }
        if stored.closed {
            return self.respond_command_outcome(
                request_id,
                haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                    command: command.to_owned(),
                    reason: Some("the parked command menu was closed without an answer".into()),
                },
            );
        }
        self.respond_stored_command_menu(request_id, &stored)
    }

    async fn ensure_cache_confirmation_menu(
        &self,
        session_id: &SessionId,
        invocation_id: &CommandId,
        command: &'static str,
        continuation: &ParkedCommandContinuation,
        message: String,
    ) -> Result<StoredCommandMenu, SessionHubError> {
        let invocation_key = command_menu_invocation_key(session_id, invocation_id);
        match self
            .stored_command_menu(session_id, command, &invocation_key)
            .await?
        {
            StoredCommandMenuLookup::Found(stored) => return Ok(stored),
            StoredCommandMenuLookup::CommandIdConflict => {
                return Err(SessionHubError::Task(
                    "command id was already used for a different parked command".into(),
                ));
            }
            StoredCommandMenuLookup::Missing => {}
        }
        let encoded = encode_cache_confirmation(continuation).ok_or_else(|| {
            SessionHubError::Task("command cannot require cache confirmation".into())
        })?;
        let latest_seq = self.hub.inner.store.latest_seq(session_id).await?;
        let mut head = self
            .hub
            .inner
            .store
            .read(session_id, latest_seq.saturating_sub(1), 1)
            .await?;
        let head = head.pop().ok_or_else(|| {
            SessionHubError::Task("cache confirmation could not resolve the session head".into())
        })?;
        let menu = Menu {
            id: MenuId::new(format!("command-cache-menu-{invocation_key}")),
            kind: MenuKind::Choice,
            title: "Confirm cache epoch".into(),
            body: vec![message],
            options: vec![MenuOption {
                key: "confirm".into(),
                label: "Confirm change".into(),
                detail: Some("Start a new cache epoch and apply the command".into()),
                decision: None,
            }],
            blocking: false,
            scope: MenuScope::Session,
            origin: format!("{COMMAND_MENU_ORIGIN_PREFIX}{command}:{invocation_key}:{encoded}"),
            ttl_ms: None,
            timeout_option: None,
        };
        let mut envelopes = [haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: EventId::new(format!("command-cache-menu-opened-{invocation_key}")),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: self.hub.inner.device_id.clone(),
            authority_epoch: head.authority_epoch,
            worker_generation: self.hub.worker_generation(),
            causation_id: Some(head.event_id),
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: haider_protocol::envelope::RawPayload::from_event(EventPayload::MenuOpened(
                menu.clone(),
            ))
            .map_err(|error| {
                SessionHubError::Task(format!("cannot encode command cache confirmation: {error}"))
            })?,
        }];
        if let Err(error) = self.hub.append(&mut envelopes).await {
            if let StoredCommandMenuLookup::Found(stored) = self
                .stored_command_menu(session_id, command, &invocation_key)
                .await?
            {
                return Ok(stored);
            }
            return Err(error.into());
        }
        Ok(StoredCommandMenu {
            opening: envelopes[0].clone(),
            menu,
            answer: None,
            closed: false,
        })
    }

    async fn park_command_menu(
        &self,
        request_id: RequestId,
        command_id: &CommandId,
        session_id: SessionId,
        command: &'static str,
    ) -> Result<(), SessionHubError> {
        let invocation_key = command_menu_invocation_key(&session_id, command_id);
        match self
            .stored_command_menu(&session_id, command, &invocation_key)
            .await?
        {
            StoredCommandMenuLookup::Found(stored) => {
                return self
                    .respond_existing_command_menu(request_id, session_id, command, stored)
                    .await;
            }
            StoredCommandMenuLookup::CommandIdConflict => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "command id was already used for a different parked command",
                    false,
                    None,
                );
            }
            StoredCommandMenuLookup::Missing => {}
        }
        let latest_seq = self.hub.inner.store.latest_seq(&session_id).await?;
        if latest_seq == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "command menu requires a live session",
                false,
                None,
            );
        }
        let mut head = self
            .hub
            .inner
            .store
            .read(&session_id, latest_seq.saturating_sub(1), 1)
            .await?;
        let Some(head) = head.pop() else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "command menu could not resolve the session head",
                false,
                None,
            );
        };
        let metadata = self.hub.session_metadata(&session_id).await?;
        let summaries = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .map(|view| view.providers)
            .unwrap_or_default();
        let (kind, title, body, options) = match command {
            "rename" => (
                MenuKind::Question,
                "Rename session".to_owned(),
                vec!["Enter the new session name.".to_owned()],
                Vec::new(),
            ),
            "model" => {
                let Some(metadata) = metadata.as_ref() else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_NOT_FOUND,
                        "model menu requires typed session metadata",
                        false,
                        None,
                    );
                };
                let mut models = summaries
                    .iter()
                    .find(|summary| summary.provider == metadata.provider)
                    .map(|summary| summary.models.clone())
                    .unwrap_or_default();
                if !models.iter().any(|model| model == &metadata.model) {
                    models.insert(0, metadata.model.clone());
                }
                (
                    MenuKind::Choice,
                    "Choose model".to_owned(),
                    vec![format!("Provider: {}", metadata.provider)],
                    command_menu_options(models),
                )
            }
            "provider" => {
                let mut providers: BTreeSet<String> = summaries
                    .iter()
                    .filter(|summary| {
                        summary.enabled
                            && (summary.default_model.is_some() || !summary.models.is_empty())
                    })
                    .map(|summary| summary.provider.clone())
                    .collect();
                if let Some(metadata) = metadata.as_ref() {
                    providers.insert(metadata.provider.clone());
                }
                (
                    MenuKind::Choice,
                    "Choose provider".to_owned(),
                    Vec::new(),
                    command_menu_options(providers),
                )
            }
            "effort" => {
                let Some(metadata) = metadata.as_ref() else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_NOT_FOUND,
                        "effort menu requires typed session metadata",
                        false,
                        None,
                    );
                };
                let mut efforts = vec!["default".to_owned()];
                if let Some(detail) = summaries
                    .iter()
                    .find(|summary| summary.provider == metadata.provider)
                    .and_then(|summary| {
                        summary
                            .model_details
                            .iter()
                            .find(|detail| detail.name == metadata.model)
                    })
                {
                    efforts.extend(detail.supported_efforts.clone());
                }
                if let Some(current) = metadata.effort.as_ref()
                    && !efforts.iter().any(|effort| effort == current)
                {
                    efforts.push(current.clone());
                }
                (
                    MenuKind::Choice,
                    "Choose reasoning effort".to_owned(),
                    vec![format!("Model: {}", metadata.model)],
                    command_menu_options(efforts),
                )
            }
            "fast" => {
                let current = metadata.as_ref().map(|metadata| metadata.fast);
                (
                    MenuKind::Choice,
                    "Choose fast mode".to_owned(),
                    current.map_or_else(Vec::new, |enabled| {
                        vec![format!("Currently: {}", if enabled { "on" } else { "off" })]
                    }),
                    command_menu_options(["on".to_owned(), "off".to_owned()]),
                )
            }
            _ => {
                return self.respond_command_outcome(
                    request_id,
                    haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                        command: command.to_owned(),
                        reason: Some("command has no safe menu producer".into()),
                    },
                );
            }
        };
        if matches!(kind, MenuKind::Choice) && options.is_empty() {
            return self.respond_command_outcome(
                request_id,
                haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                    command: command.to_owned(),
                    reason: Some("daemon has no choices for this command".into()),
                },
            );
        }
        let menu_id = MenuId::new(format!("command-menu-{invocation_key}"));
        let menu = Menu {
            id: menu_id.clone(),
            kind: kind.clone(),
            title: title.clone(),
            body: body.clone(),
            options: options.clone(),
            blocking: false,
            scope: MenuScope::Session,
            origin: format!("{COMMAND_MENU_ORIGIN_PREFIX}{command}:{invocation_key}"),
            ttl_ms: None,
            timeout_option: None,
        };
        let mut envelopes = [haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: EventId::new(format!("command-menu-opened-{invocation_key}")),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: self.hub.inner.device_id.clone(),
            authority_epoch: head.authority_epoch,
            worker_generation: self.hub.worker_generation(),
            causation_id: Some(head.event_id),
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: haider_protocol::envelope::RawPayload::from_event(EventPayload::MenuOpened(
                menu,
            ))
            .map_err(|error| {
                SessionHubError::Task(format!("cannot encode command menu: {error}"))
            })?,
        }];
        if let Err(error) = self.hub.append(&mut envelopes).await {
            // Two control connections can race the preflight read. The
            // deterministic opening identity makes one append win; the loser
            // replays that exact card instead of minting a second menu.
            if let StoredCommandMenuLookup::Found(stored) = self
                .stored_command_menu(&session_id, command, &invocation_key)
                .await?
            {
                return self.respond_stored_command_menu(request_id, &stored);
            }
            return self.respond_turn_error(request_id, error);
        }
        let opened = &envelopes[0];
        let EventPayload::MenuOpened(menu) = opened.payload.decode_event().map_err(|error| {
            SessionHubError::Task(format!("cannot decode committed command menu: {error}"))
        })?
        else {
            return Err(SessionHubError::Task(
                "committed command menu changed payload kind".into(),
            ));
        };
        let needs_input = Self::command_needs_input(opened, &menu);
        self.respond_command_outcome(
            request_id,
            haider_rpc::CommandInvokeOutcomeWire::Parked { needs_input },
        )
    }

    /// The transport + vault gate shared by `vault.stage` and
    /// `account.login_api` (R7/R10): Control alone must not expose raw-secret
    /// staging to a remote transport, and a vaultless platform answers the
    /// stable `vault_unsupported` BEFORE staging/validation.
    fn secret_surface_facade(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<crate::accounts::AccountsFacade>, SessionHubError> {
        if self.transport != crate::accounts::ConnectionTransport::LocalSameUid {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_CAPABILITY_DENIED,
                "secret staging is only served on authenticated same-UID local connections",
                false,
                None,
            )?;
            return Ok(None);
        }
        let facade = self.hub.accounts()?;
        match facade {
            Some(facade) if facade.vault_supported => Ok(Some(facade)),
            _ => {
                self.respond_error(
                    request_id.clone(),
                    haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                    "this platform has no supported secret vault (W3c supports macOS Keychain)",
                    false,
                    None,
                )?;
                Ok(None)
            }
        }
    }

    fn device_surface_facade(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<crate::accounts::AccountsFacade>, SessionHubError> {
        if self.transport != crate::accounts::ConnectionTransport::LocalSameUid {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_CAPABILITY_DENIED,
                "device credential discovery is only served on authenticated same-UID local connections",
                false,
                None,
            )?;
            return Ok(None);
        }
        let Some(facade) = self.hub.accounts()? else {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_DRAINING,
                "account actor is unavailable",
                true,
                None,
            )?;
            return Ok(None);
        };
        Ok(Some(facade))
    }

    fn oauth_import_source_facade(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<crate::accounts::AccountsFacade>, SessionHubError> {
        if self.transport != crate::accounts::ConnectionTransport::LocalSameUid {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_CAPABILITY_DENIED,
                "OAuth import-source discovery is only served on authenticated same-UID local connections",
                false,
                None,
            )?;
            return Ok(None);
        }
        let Some(facade) = self.hub.accounts()? else {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_DRAINING,
                "account actor is unavailable",
                true,
                None,
            )?;
            return Ok(None);
        };
        Ok(Some(facade))
    }

    /// `vault.stage`: connection-scoped, non-durable, inline (no I/O). The
    /// secret enters zeroizing storage here and the wire frame drops
    /// (zeroized) with this call.
    fn vault_stage(
        &self,
        request_id: RequestId,
        stage_id: String,
        purpose: haider_rpc::StagePurpose,
        secret: haider_rpc::SecretWire,
    ) -> Result<(), SessionHubError> {
        if self.secret_surface_facade(&request_id)?.is_none() {
            return Ok(());
        }
        if stage_id.trim().is_empty() || secret.is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "stage id and secret must not be empty",
                false,
                None,
            );
        }
        if matches!(purpose, haider_rpc::StagePurpose::Unknown) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unknown stage purpose",
                false,
                None,
            );
        }
        let staged = {
            let mut stages = lock(&self.stages)?;
            stages.stage(&stage_id, purpose, secret.expose_secret().as_bytes())
        };
        match staged {
            Ok((vault_reference, expires_at_ms)) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::VaultStage {
                    stage_id,
                    vault_reference,
                    expires_at_ms,
                },
            }),
            Err(crate::accounts::StageError::Mismatch) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "stage id was already used with different secret bytes",
                false,
                None,
            ),
            Err(crate::accounts::StageError::Mint(message)) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!("cannot mint stage reference: {message}"),
                true,
                None,
            ),
        }
    }

    /// `transcription.secret_get` (T1): answers the vaulted Deepgram key on
    /// the same-UID local UDS surface only. Inline like `vault.stage`: one
    /// bounded ≤512-byte vault file read, comparable to one store
    /// transaction. A missing entry is an honest `secret: None`, never an
    /// error — "no key yet" is a first-class setup state.
    fn transcription_secret_get(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let Some(vault) = facade.vault else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault",
                false,
                None,
            );
        };
        let alias = transcription_secret_alias();
        match vault.resolve(&alias) {
            Ok(secret) => {
                let Ok(value) = std::str::from_utf8(secret.expose_secret()) else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "stored transcription secret is not valid UTF-8; set it again",
                        false,
                        None,
                    );
                };
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::TranscriptionSecretGet {
                        secret: Some(haider_rpc::SecretWire::new(value)),
                    },
                })
            }
            Err(error) if error.code == haider_protocol::error::ErrorCode::CredentialMissing => {
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::TranscriptionSecretGet { secret: None },
                })
            }
            // The vault's own message carries an alias at most, never
            // secret bytes (haider-accounts redaction law).
            Err(error) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!("could not read transcription secret: {}", error.message),
                error.retryable,
                None,
            ),
        }
    }

    /// `transcription.secret_set` (T1): stores or clears the Deepgram key
    /// in the profile vault. ADE key hygiene is enforced BEFORE any vault
    /// write: non-empty, ≤512 chars, no control bytes — and no refusal ever
    /// echoes key material.
    fn transcription_secret_set(
        &self,
        request_id: RequestId,
        secret: haider_rpc::SecretWire,
        clear: bool,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let Some(vault) = facade.vault else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault",
                false,
                None,
            );
        };
        let alias = transcription_secret_alias();
        if clear {
            if !secret.is_empty() {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "clear:true must not carry a secret",
                    false,
                    None,
                );
            }
            return match vault.delete(&alias) {
                Ok(()) => self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::TranscriptionSecretSet { present: false },
                }),
                Err(error) => self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &format!("could not clear transcription secret: {}", error.message),
                    error.retryable,
                    None,
                ),
            };
        }
        let value = secret.expose_secret().trim();
        if value.is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "transcription secret must not be empty",
                false,
                None,
            );
        }
        if value.len() > TRANSCRIPTION_SECRET_MAX_LEN || value.chars().any(char::is_control) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "transcription secret is not a valid API key",
                false,
                None,
            );
        }
        match vault.put(&alias, value.as_bytes()) {
            Ok(()) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::TranscriptionSecretSet { present: true },
            }),
            Err(error) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!("could not store transcription secret: {}", error.message),
                error.retryable,
                None,
            ),
        }
    }

    /// `account.login_api`: claims the stage and HANDS OFF to the account
    /// actor (R7: the connection task never awaits validation/Keychain work
    /// inline). The correlated response arrives from the actor through this
    /// connection's sink; disconnect drops only that route, never the
    /// durable command.
    fn account_login(
        &self,
        request_id: RequestId,
        input: AccountLoginInput,
    ) -> Result<(), SessionHubError> {
        let AccountLoginInput {
            command_id,
            provider,
            alias,
            vault_reference,
            validation_model,
            replace_existing,
        } = input;
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || vault_reference.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "login command id, provider, and vault reference must not be empty",
                false,
                None,
            );
        }
        let claimed = {
            let mut stages = lock(&self.stages)?;
            stages.claim(&vault_reference)
        };
        let secret = match claimed {
            Some((haider_rpc::StagePurpose::ApiKey, secret)) => Some(secret),
            Some(_) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "staged secret was not staged for api_key use",
                    false,
                    None,
                );
            }
            // Unknown/expired reference: the actor may still hold the
            // pending command's secret (retry-after-retryable), else it
            // answers restage_required.
            None => None,
        };
        let Some(login) = facade.login else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault (W3c supports macOS Keychain)",
                false,
                None,
            );
        };
        let job = crate::accounts::LoginJob {
            command_id: command_id.0,
            provider,
            display_alias: alias.filter(|value| !value.trim().is_empty()),
            validation_model: validation_model.filter(|value| !value.trim().is_empty()),
            replace_existing,
            secret,
            route: crate::accounts::LoginRoute {
                request_id: request_id.clone(),
                sink: Arc::clone(&self.sink),
            },
        };
        match login.try_send(crate::accounts::AccountCommand::Login(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                // Honest recovery: the single-use stage was already claimed
                // and dropped with this rejected job, so the retry needs a
                // fresh stage (the restage protocol covers it).
                "account actor is busy; stage the key again and retry",
                true,
                None,
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            ),
        }
    }

    fn account_oauth_start(
        &self,
        request_id: RequestId,
        provider: String,
        desired_alias: String,
        attempt_id: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let Some(oauth) = facade.oauth else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_UNAVAILABLE,
                "OAuth coordinator is unavailable",
                false,
                None,
            );
        };
        if provider.trim().is_empty()
            || desired_alias.trim().is_empty()
            || attempt_id.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "OAuth provider, alias, and attempt id must not be empty",
                false,
                None,
            );
        }
        let availability = oauth.availability(&provider, true);
        if !availability.available {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountOAuthStart {
                    availability,
                    flow_id: None,
                    authorization_url: None,
                    provider_origin: None,
                    loopback_port: None,
                    expires_at_ms: None,
                    user_code: None,
                },
            });
        }
        let route = crate::oauth::OAuthRoute {
            request_id: request_id.clone(),
            sink: Arc::clone(&self.sink),
        };
        match oauth.try_start(
            &self.connection_id,
            provider,
            desired_alias,
            attempt_id,
            route,
        ) {
            Ok(()) => Ok(()),
            Err(crate::oauth::StartAdmissionError::Busy) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                "OAuth coordinator is busy",
                true,
                None,
            ),
            Err(crate::oauth::StartAdmissionError::Closed) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "OAuth coordinator is shut down",
                true,
                None,
            ),
        }
    }

    fn account_oauth_import(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        source: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty()
            || crate::oauth::oauth_import_source_spec(&source).is_err()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.oauth_import requires a command id and a source published by account.oauth_import_sources",
                false,
                None,
            );
        }
        let Some(commands) = facade.login else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            );
        };
        let job = crate::accounts::OAuthImportJob {
            command_id: command_id.0,
            source,
            route: crate::accounts::LoginRoute {
                request_id: request_id.clone(),
                sink: Arc::clone(&self.sink),
            },
        };
        match commands.try_send(crate::accounts::AccountCommand::ImportOAuth(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                "account actor is busy; retry with the same command id",
                true,
                None,
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            ),
        }
    }

    fn account_oauth_import_sources(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(_facade) = self.oauth_import_source_facade(&request_id)? else {
            return Ok(());
        };
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::OAuthImportSources {
                completed: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            },
        )
    }

    fn account_device_candidates(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(facade) = self.device_surface_facade(&request_id)? else {
            return Ok(());
        };
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::DeviceCandidates {
                discovery_disabled: facade.discovery_disabled,
                completed: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            },
        )
    }

    fn account_source_list(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(_facade) = self.device_surface_facade(&request_id)? else {
            return Ok(());
        };
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SourceList {
                completed: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            },
        )
    }

    fn account_source_add(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        kind: String,
        root: String,
        label: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(_facade) = self.device_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty() || kind.trim().is_empty() || root.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.source_add requires command id, kind, and root",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SourceAdd(Box::new(crate::accounts::SourceAddJob {
                kind,
                root,
                label,
                route: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            })),
        )
    }

    fn account_source_remove(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        source_id: String,
    ) -> Result<(), SessionHubError> {
        let Some(_facade) = self.device_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty() || source_id.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.source_remove requires command id and source id",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SourceRemove(Box::new(
                crate::accounts::SourceRemoveJob {
                    source_id,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn account_source_scan(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(_facade) = self.device_surface_facade(&request_id)? else {
            return Ok(());
        };
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SourceScan {
                completed: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            },
        )
    }

    fn account_import_device(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        candidate: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty() || !valid_device_candidate_id(&candidate) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.import_device requires a command id and valid opaque candidate id",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::ImportDevice(Box::new(
                crate::accounts::DeviceImportJob {
                    command_id: command_id.0,
                    candidate,
                    discovery_disabled: facade.discovery_disabled,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn account_oauth_status(
        &self,
        request_id: RequestId,
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let status = facade
            .oauth
            .and_then(|oauth| oauth.status(&self.connection_id, &flow_id, &attempt_id));
        match status {
            Some(status) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountOAuthStatus { flow_id, status },
            }),
            None => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_FLOW_NOT_FOUND,
                "OAuth flow is unavailable for this connection and attempt",
                true,
                None,
            ),
        }
    }

    fn account_oauth_cancel(
        &self,
        request_id: RequestId,
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        let status = facade
            .oauth
            .and_then(|oauth| oauth.cancel(&self.connection_id, &flow_id, &attempt_id));
        match status {
            Some(status) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountOAuthCancel { flow_id, status },
            }),
            None => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_FLOW_NOT_FOUND,
                "OAuth flow is unavailable for this connection and attempt",
                true,
                None,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn account_add_oauth(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        alias: String,
        auth_method: haider_rpc::AccountAddMethod,
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
        oauth_reference: haider_rpc::OAuthReadyRefWire,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if !matches!(auth_method, haider_rpc::AccountAddMethod::OAuth)
            || command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || alias.trim().is_empty()
            || attempt_id.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.add requires OAuth method and complete coordinates",
                false,
                None,
            );
        }
        let Some(oauth) = facade.oauth else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_OAUTH_UNAVAILABLE,
                "OAuth coordinator is unavailable",
                false,
                None,
            );
        };
        let claim = oauth.claim_ready(
            &self.connection_id,
            &flow_id,
            &attempt_id,
            &provider,
            &alias,
            &oauth_reference,
        );
        let Some(login) = facade.login else {
            if let Some(claim) = claim {
                oauth.restore_ready(claim);
            }
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault",
                false,
                None,
            );
        };
        let job = crate::accounts::OAuthAddJob {
            command_id: command_id.0,
            provider,
            display_alias: alias,
            claim,
            route: crate::accounts::LoginRoute {
                request_id: request_id.clone(),
                sink: Arc::clone(&self.sink),
            },
        };
        match login.try_send(crate::accounts::AccountCommand::AddOAuth(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(command)) => {
                if let crate::accounts::AccountCommand::AddOAuth(job) = command
                    && let Some(claim) = job.claim
                {
                    oauth.restore_ready(claim);
                }
                self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_BUSY,
                    "account actor is busy; retry with the same OAuth reference",
                    true,
                    None,
                )
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            ),
        }
    }

    fn send_management_command(
        &self,
        request_id: RequestId,
        command: crate::accounts::AccountCommand,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.hub.accounts()? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account/provider actor is unavailable",
                true,
                None,
            );
        };
        let Some(commands) = facade.login else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account/provider actor is unavailable",
                true,
                None,
            );
        };
        match commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                "account/provider actor mailbox is full",
                true,
                None,
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account/provider actor is shut down",
                true,
                None,
            ),
        }
    }

    async fn account_set_active(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        alias: String,
        confirm_new_epoch: bool,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || alias.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "set-active command id and alias must not be empty",
                false,
                None,
            );
        }
        let descriptor = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .and_then(|view| {
                view.descriptors
                    .into_iter()
                    .find(|descriptor| descriptor.alias.as_str() == alias)
            });
        if let Some(descriptor) = descriptor {
            let target_auth_scope = match descriptor.auth_method {
                haider_protocol::credential::AuthMethod::ApiKey => "api_key",
                haider_protocol::credential::AuthMethod::OAuth => "oauth_subscription",
            };
            let mut warnings = Vec::new();
            for session_id in self.hub.inner.store.session_ids().await? {
                let Some(metadata) = self.hub.inner.store.session_metadata(&session_id).await?
                else {
                    continue;
                };
                if metadata.provider != descriptor.provider {
                    continue;
                }
                let scope = crate::cache_policy::latest_main_cache_scope(
                    &self.hub.inner.store,
                    &session_id,
                )
                .await?;
                let Some(scope) = scope else {
                    continue;
                };
                let mut changed_fields = Vec::new();
                if scope.account_scope.as_ref().map(|value| value.as_str())
                    != Some(descriptor.alias.as_str())
                {
                    changed_fields.push("account".to_owned());
                }
                if scope.auth_scope != target_auth_scope {
                    changed_fields.push("auth".to_owned());
                }
                if let Some(warning) = crate::cache_policy::assess_cache_change(
                    &metadata,
                    Some(&scope),
                    &metadata.provider,
                    &metadata.model,
                    Some(target_auth_scope),
                    changed_fields,
                    false,
                ) {
                    warnings.push(warning);
                }
            }
            if let Some(warning) = crate::cache_policy::combine_cache_change_warnings(warnings)
                && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
            {
                return self.respond_cache_confirmation_required(request_id, &warning);
            }
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SetActive(Box::new(crate::accounts::SetActiveJob {
                command_id: command_id.0,
                alias,
                route: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            })),
        )
    }

    /// `account.set_label` — cosmetic rename. No command id and no receipt:
    /// the mutation is idempotent by value and carries no credential
    /// authority, so replaying it produces the same descriptor. Everything
    /// that changes what a turn spends stays receipted.
    fn account_set_label(
        &self,
        request_id: RequestId,
        alias: String,
        label: Option<String>,
    ) -> Result<(), SessionHubError> {
        if alias.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.set_label requires an alias",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SetLabel(Box::new(crate::accounts::SetLabelJob {
                alias,
                label,
                route: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            })),
        )
    }

    fn account_refresh(&self, request_id: RequestId, alias: String) -> Result<(), SessionHubError> {
        if alias.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account.refresh requires an alias",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::RefreshIdentity(Box::new(
                crate::accounts::RefreshIdentityJob {
                    alias,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn account_remove(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        alias: String,
        expected_revision: Option<u64>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || alias.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "remove command id and alias must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::Remove(Box::new(crate::accounts::RemoveAccountJob {
                command_id: command_id.0,
                alias,
                expected_revision,
                route: crate::accounts::LoginRoute {
                    request_id,
                    sink: Arc::clone(&self.sink),
                },
            })),
        )
    }

    fn account_set_default_model(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        model: String,
        expected_revision: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || model.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "default-model command id, provider, and model must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SetDefaultModel(Box::new(
                crate::accounts::SetDefaultModelJob {
                    command_id: command_id.0,
                    provider,
                    model,
                    expected_revision,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn provider_configure(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        input: crate::provider_registry::ProviderConfigureInput,
        probe_secret: Option<zeroize::Zeroizing<Vec<u8>>>,
        expected_revision: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || input.provider.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "provider-configure command id and provider must not be empty",
                false,
                None,
            );
        }
        if input
            .api_family
            .is_some_and(|family| matches!(family, haider_rpc::ProviderApiFamilyWire::Unknown))
            || input.auth_requirement.is_some_and(|requirement| {
                matches!(
                    requirement,
                    haider_rpc::ProviderAuthRequirementWire::Unknown
                )
            })
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "provider configuration contains an unknown identity field",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::ConfigureProvider(Box::new(
                crate::accounts::ProviderConfigureJob {
                    command_id: command_id.0,
                    input,
                    probe_secret,
                    expected_revision,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn provider_remove(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        expected_revision: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || provider.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "provider-remove command id and provider must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::RemoveProvider(Box::new(
                crate::accounts::ProviderRemoveJob {
                    command_id: command_id.0,
                    provider,
                    expected_revision,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn provider_set_trust(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        trust: haider_rpc::ProviderTrustWire,
        expected_revision: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || matches!(trust, haider_rpc::ProviderTrustWire::Unknown)
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "provider-trust command id/provider must not be empty and trust must be full or lockdown",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::SetProviderTrust(Box::new(
                crate::accounts::ProviderSetTrustJob {
                    command_id: command_id.0,
                    provider,
                    trust,
                    expected_revision,
                    route: crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                },
            )),
        )
    }

    fn lockdown_status(
        &self,
        request_id: RequestId,
        provider: Option<&str>,
    ) -> Result<(), SessionHubError> {
        let policy = match provider
            .map(|provider| self.hub.provider_lockdown_policy_detail(provider))
            .transpose()
        {
            Ok(policy) => policy,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_PROVIDER_ERROR,
                    &error.to_string(),
                    false,
                    None,
                );
            }
        };
        match crate::lockdown::global().and_then(|manager| manager.status(provider)) {
            Ok(status) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::LockdownStatus {
                    status: lockdown_status_wire(status, policy, false),
                },
            }),
            Err(error) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_PROVIDER_ERROR,
                &error.to_string(),
                false,
                None,
            ),
        }
    }

    async fn session_lockdown_provider(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionLockdownEnvelope>, SessionHubError> {
        let Some(metadata) = self.hub.session_metadata(session_id).await? else {
            return Ok(None);
        };
        let (provider, policy) = self.hub.bound_session_lockdown(session_id)?.unwrap_or((
            metadata.provider.clone(),
            self.hub
                .provider_lockdown_policy_detail(&metadata.provider)?,
        ));
        if !policy.is_lockdown() {
            return Ok(None);
        }
        Ok(Some(SessionLockdownEnvelope {
            provider,
            tools_allowed: crate::auto_hermetic::tools_for(policy),
        }))
    }

    async fn journal_session_lockdown_refusal(
        &self,
        session_id: &SessionId,
        command_id: &str,
        envelope: &SessionLockdownEnvelope,
        tool: &str,
        reason: &str,
    ) -> Result<(), SessionHubError> {
        let payload = haider_protocol::envelope::RawPayload::from_event(
            EventPayload::LockdownRefused(haider_protocol::lockdown::LockdownRefused {
                provider: envelope.provider.clone(),
                tool: tool.to_owned(),
                reason: reason.to_owned(),
                tools_allowed: envelope.tools_allowed.clone(),
            }),
        )
        .map_err(|error| SessionHubError::Task(error.to_string()))?;
        let event_id = EventId::new(format!("lockdown-refusal-{command_id}-{tool}"));
        let mut cursor = 0_u64;
        loop {
            let page =
                haider_core::StoreHandle::read(&self.hub.inner.store, session_id, cursor, 512)
                    .await?;
            if page.is_empty() {
                break;
            }
            if page
                .iter()
                .any(|envelope| envelope.event_id == event_id && envelope.payload == payload)
            {
                return Ok(());
            }
            let Some(next) = page.last().map(|envelope| envelope.seq) else {
                break;
            };
            if next <= cursor {
                return Err(SessionHubError::Task(
                    "lockdown refusal journal scan did not advance".to_owned(),
                ));
            }
            cursor = next;
        }
        let mut envelope = haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id,
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("lockdown-permission-broker"),
            authority_epoch: 0,
            worker_generation: self.hub.inner.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload,
        };
        haider_core::StoreHandle::append(
            &self.hub.inner.store,
            std::slice::from_mut(&mut envelope),
        )
        .await?;
        Ok(())
    }

    async fn lockdown_set_quota(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        bytes: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "lockdown quota command id must not be empty",
                false,
                None,
            );
        }
        match crate::lockdown::global()
            .and_then(|manager| manager.set_quota_command(command_id.as_str(), bytes))
        {
            Ok(status) => {
                self.journal_lockdown_quota(command_id.as_str(), &status)
                    .await?;
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::LockdownSetQuota {
                        status: lockdown_status_wire(status, None, false),
                    },
                })
            }
            Err(crate::lockdown::LockdownError::LockdownQuotaExceeded { used, limit }) => self
                .respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_LOCKDOWN_QUOTA_EXCEEDED,
                    &format!("LockdownQuotaExceeded {{ used: {used}, limit: {limit} }}"),
                    false,
                    Some(haider_rpc::ErrorData::LockdownQuotaExceeded { used, limit }),
                ),
            Err(crate::lockdown::LockdownError::QuotaCommandConflict { command_id }) => self
                .respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &format!(
                        "lockdown quota command `{command_id}` was already used with different bytes"
                    ),
                    false,
                    None,
                ),
            Err(error) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_PROVIDER_ERROR,
                &error.to_string(),
                false,
                None,
            ),
        }
    }

    async fn journal_lockdown_quota(
        &self,
        command_id: &str,
        status: &crate::lockdown::LockdownStatus,
    ) -> Result<(), SessionHubError> {
        let providers = self
            .hub
            .accounts()?
            .and_then(|accounts| accounts.management.read())
            .map(|view| {
                view.providers
                    .into_iter()
                    .filter(|provider| {
                        !matches!(provider.trust, haider_rpc::ProviderTrustWire::Full)
                    })
                    .map(|provider| provider.provider)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let payload = haider_protocol::envelope::RawPayload::from_event(
            EventPayload::LockdownQuota(haider_protocol::lockdown::LockdownQuota {
                provider: None,
                used: status.quota_used,
                limit: status.quota_limit,
            }),
        )
        .map_err(|error| SessionHubError::Task(error.to_string()))?;
        let event_id = EventId::new(format!("lockdown-quota-{command_id}"));
        for session_id in self.hub.inner.store.session_ids().await? {
            let Some(metadata) = self.hub.inner.store.session_metadata(&session_id).await? else {
                continue;
            };
            let lockdown = self.hub.bound_session_lockdown(&session_id)?.map_or_else(
                || providers.contains(&metadata.provider),
                |(_, policy)| policy.is_lockdown(),
            );
            if !lockdown {
                continue;
            }
            let mut cursor = 0_u64;
            let mut exists = false;
            loop {
                let page =
                    haider_core::StoreHandle::read(&self.hub.inner.store, &session_id, cursor, 512)
                        .await?;
                if page.is_empty() {
                    break;
                }
                exists = page
                    .iter()
                    .any(|envelope| envelope.event_id == event_id && envelope.payload == payload);
                if exists {
                    break;
                }
                let Some(next) = page.last().map(|envelope| envelope.seq) else {
                    break;
                };
                if next <= cursor {
                    return Err(SessionHubError::Task(
                        "lockdown quota journal scan did not advance".to_owned(),
                    ));
                }
                cursor = next;
            }
            if exists {
                continue;
            }
            let mut envelope = haider_protocol::envelope::EventEnvelope {
                schema_version: haider_protocol::envelope::SCHEMA_VERSION,
                event_id: event_id.clone(),
                seq: 0,
                session_id,
                branch_id: None,
                run_id: None,
                agent_id: None,
                device_id: DeviceId::new("lockdown-management"),
                authority_epoch: 0,
                worker_generation: self.hub.inner.store.worker_generation(),
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 0,
                render: haider_protocol::envelope::RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: haider_protocol::envelope::PromptRender::Omit,
                },
                payload: payload.clone(),
            };
            haider_core::StoreHandle::append(
                &self.hub.inner.store,
                std::slice::from_mut(&mut envelope),
            )
            .await?;
        }
        Ok(())
    }

    fn provider_models_refresh(
        &self,
        request_id: RequestId,
        provider: String,
    ) -> Result<(), SessionHubError> {
        if provider.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "model-refresh provider must not be empty",
                false,
                None,
            );
        }
        self.send_management_command(
            request_id.clone(),
            crate::accounts::AccountCommand::RefreshProviderModels {
                provider,
                completed: crate::accounts::ProviderModelsRefreshCompletion::Wire(
                    crate::accounts::LoginRoute {
                        request_id,
                        sink: Arc::clone(&self.sink),
                    },
                ),
            },
        )
    }

    async fn refresh_inventory_if_needed(
        &self,
        summaries: &mut Vec<ProviderSummaryWire>,
        provider: &str,
        model: &str,
    ) -> Result<(), InventoryRefreshError> {
        let cached_serves_model = summaries
            .iter()
            .find(|summary| summary.provider == provider)
            .is_some_and(|summary| summary.models.iter().any(|known| known == model));
        let advisory_inventory = summaries
            .iter()
            .find(|summary| summary.provider == provider)
            .is_some_and(|summary| {
                matches!(
                    summary.inventory_authority,
                    haider_rpc::ModelInventoryAuthorityWire::Advisory
                )
            });
        let needs_refresh = summaries
            .iter()
            .find(|summary| summary.provider == provider)
            .is_some_and(|summary| provider_inventory_needs_refresh(summary, model));
        if !needs_refresh {
            return Ok(());
        }
        let facade = self
            .hub
            .accounts()
            .map_err(InventoryRefreshError::Hub)?
            .ok_or_else(|| {
                InventoryRefreshError::Provider(crate::accounts::ProviderModelsRefreshFailure {
                    code: haider_rpc::ERROR_CODE_PROVIDER_MODELS_UNKNOWN.to_owned(),
                    message: "provider model refresh is unavailable".to_owned(),
                    retryable: true,
                    data: None,
                })
            })?;
        let refreshed = match facade.refresh_provider_models(provider.to_owned()).await {
            Ok(refreshed) => refreshed,
            // A custom compatible catalog is advisory. Refresh-on-miss still
            // probes exactly once, but discovery failure cannot veto a
            // caller-configured passthrough id; the chat request remains the
            // endpoint's authority and selection below is typed Unlisted.
            Err(_) if advisory_inventory => return Ok(()),
            // TTL is a freshness policy, not an availability outage: a
            // transient refresh failure may keep serving a cached requested
            // model. A cache MISS still surfaces the typed discovery failure.
            Err(error) if cached_serves_model && error.retryable => return Ok(()),
            // Seeded/manual inventories have no live catalog. The attempted
            // refresh satisfies refresh-on-miss; validation below then emits
            // ModelUnknown against that unchanged known inventory.
            Err(error)
                if matches!(
                    &error.data,
                    Some(ErrorData::ProviderModelsUnavailable { .. })
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(InventoryRefreshError::Provider(error)),
        };
        if let Some(summary) = summaries
            .iter_mut()
            .find(|summary| summary.provider == provider)
        {
            *summary = refreshed;
        } else {
            summaries.push(refreshed);
        }
        Ok(())
    }

    /// `account.list`: inline snapshot read (short command; the actor is the
    /// only writer, so a queued login never head-of-line-blocks listing).
    fn account_list(
        &self,
        request_id: RequestId,
        provider: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.hub.accounts()? else {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountList {
                    descriptors: Vec::new(),
                    sources: Vec::new(),
                    revision: None,
                    provider_active: Vec::new(),
                    provider_defaults: Vec::new(),
                    availability: Some(haider_rpc::SnapshotAvailabilityWire::Unavailable {
                        reason: "account subsystem is not configured".into(),
                    }),
                },
            });
        };
        let Some(view) = facade.management.read() else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "management snapshot is unavailable",
                true,
                None,
            );
        };
        let descriptors = view
            .descriptors
            .iter()
            .filter(|descriptor| {
                provider
                    .as_deref()
                    .is_none_or(|provider| descriptor.provider == provider)
            })
            .cloned()
            .collect::<Vec<_>>();
        let provider_active = view
            .descriptors
            .iter()
            .filter(|descriptor| {
                descriptor.active
                    && provider
                        .as_deref()
                        .is_none_or(|provider| descriptor.provider == provider)
            })
            .map(|descriptor| haider_rpc::ProviderActiveWire {
                provider: descriptor.provider.clone(),
                alias: descriptor.alias.clone(),
            })
            .collect();
        let provider_defaults = view
            .providers
            .iter()
            .filter(|summary| {
                provider
                    .as_deref()
                    .is_none_or(|provider| summary.provider == provider)
            })
            .filter_map(|summary| {
                summary
                    .default_model
                    .as_ref()
                    .map(|model| haider_rpc::ProviderDefaultWire {
                        provider: summary.provider.clone(),
                        model: model.clone(),
                    })
            })
            .collect();
        let sources = facade
            .sources
            .lock()
            .map(|sources| {
                sources
                    .iter()
                    .filter(|source| {
                        provider.as_deref().is_none_or(|provider| {
                            source.account_alias.as_ref().is_some_and(|alias| {
                                view.descriptors.iter().any(|descriptor| {
                                    descriptor.alias == *alias && descriptor.provider == provider
                                })
                            })
                        })
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::AccountList {
                descriptors,
                sources,
                revision: Some(view.revision),
                provider_active,
                provider_defaults,
                availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
            },
        })
    }

    /// `provider.list`: a short, cached management-snapshot read. Endpoint
    /// probing and provider validation are never performed on the connection
    /// task.
    fn provider_list(
        &self,
        request_id: RequestId,
        provider: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.hub.accounts()? else {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::ProviderList {
                    providers: Vec::new(),
                    revision: 0,
                    availability: Some(haider_rpc::SnapshotAvailabilityWire::Unavailable {
                        reason: "provider subsystem is not configured".into(),
                    }),
                },
            });
        };
        let Some(view) = facade.management.read() else {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "management snapshot is unavailable",
                true,
                None,
            );
        };
        let providers = filter_provider_summaries(view.providers, provider.as_deref());
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ProviderList {
                providers,
                revision: view.revision,
                availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
            },
        })
    }

    async fn checkpoint_list(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        branch_id: Option<haider_protocol::ids::BranchId>,
        cursor: Option<haider_protocol::checkpoint::CheckpointCursor>,
        limit: u16,
    ) -> Result<(), SessionHubError> {
        if self.hub.session_metadata(&session_id).await?.is_none() {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "checkpoint session does not exist",
                false,
                None,
            );
        }
        let page = match self
            .hub
            .inner
            .store
            .list_checkpoints(session_id, branch_id, cursor, limit)
            .await
        {
            Ok(page) => page,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::CheckpointList { page },
        })
    }

    async fn checkpoint_mutate(
        &self,
        request_id: RequestId,
        input: CheckpointDoorInput,
    ) -> Result<(), SessionHubError> {
        let CheckpointDoorInput {
            command_id,
            session_id,
            branch_id,
            worker_generation,
            action,
        } = input;
        let method = action.method();
        let action_coordinate_is_empty = match &action {
            CheckpointDoorAction::Undo { target } | CheckpointDoorAction::Redo { target } => {
                target.trim().is_empty()
            }
            CheckpointDoorAction::RollbackTurn { run_id } => run_id.as_str().trim().is_empty(),
        };
        if command_id.as_str().trim().is_empty() || action_coordinate_is_empty {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "checkpoint command id and target/run coordinates must not be empty",
                false,
                None,
            );
        }
        let _checkpoint_serial = self.hub.lock_checkpoint_mutation(&session_id).await;
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "branch_id": &branch_id,
            "worker_generation": worker_generation,
            "action": checkpoint_action_json(&action),
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode checkpoint command: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .checkpoint_command_receipt(&command_id, method, &request_digest, &request_json)
            .await
        {
            Ok(Some(receipt)) => {
                return self.respond_checkpoint_receipt(request_id, method, receipt);
            }
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "checkpoint mutation requires a control attachment to this session",
                false,
                None,
            );
        }
        if worker_generation != self.hub.inner.store.worker_generation() {
            return self.respond_error(
                request_id,
                ERROR_CODE_STALE_GENERATION,
                "checkpoint command worker generation is stale",
                false,
                None,
            );
        }
        // Hold the same serial used by turn admission from the idle check
        // through workspace publication and journal commit. Otherwise a new
        // tool mutation could race between checkpoint freshness verification
        // and the durable receipt.
        let _turn_admission_serial = self.hub.lock_workflow_selection(&session_id).await;
        match self.hub.session_has_nonterminal_runs(&session_id).await {
            Ok(false) => {}
            Ok(true) => {
                return self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_BUSY,
                    "checkpoint mutation requires an idle session",
                    true,
                    None,
                );
            }
            Err(error) => return self.respond_turn_error(request_id, error),
        }
        let Some(metadata) = self.hub.session_metadata(&session_id).await? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "checkpoint session does not exist",
                false,
                None,
            );
        };
        let source_checkpoints = match self
            .resolve_checkpoint_action(&session_id, branch_id.as_ref(), &action)
            .await
        {
            Ok(checkpoints) => checkpoints,
            Err(CheckpointDoorFailure::Response {
                code,
                message,
                data,
            }) => return self.respond_error(request_id, code, &message, false, data),
            Err(CheckpointDoorFailure::Hub(error)) => return Err(error),
        };
        let restored_checkpoint_ids = source_checkpoints
            .iter()
            .map(|checkpoint| checkpoint.checkpoint_id.clone())
            .collect::<Vec<_>>();
        let plan = match self
            .checkpoint_restore_plan(&metadata.cwd, &source_checkpoints)
            .await
        {
            Ok(plan) => plan,
            Err(CheckpointDoorFailure::Response {
                code,
                message,
                data,
            }) => return self.respond_error(request_id, code, &message, false, data),
            Err(CheckpointDoorFailure::Hub(error)) => return Err(error),
        };
        let plan_for_worker = plan.clone();
        let captures = match tokio::task::spawn_blocking(move || {
            haider_tools::restore_checkpoint_plan(&plan_for_worker)
        })
        .await
        {
            Ok(Ok(captures)) => captures,
            Ok(Err(haider_tools::CheckpointRestoreError::Conflict(conflict))) => {
                let data = if conflict.conflicts.len() == 1
                    && !matches!(action, CheckpointDoorAction::RollbackTurn { .. })
                {
                    Some(ErrorData::CheckpointConflict {
                        conflict: conflict.conflicts[0].clone(),
                    })
                } else {
                    Some(ErrorData::CheckpointRollbackConflict { conflict })
                };
                return self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_CHECKPOINT_CONFLICT,
                    "checkpoint freshness guard refused foreign workspace edits",
                    false,
                    data,
                );
            }
            Ok(Err(haider_tools::CheckpointRestoreError::Tool(error))) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.to_string(),
                    false,
                    None,
                );
            }
            Err(error) => {
                return Err(SessionHubError::Task(format!(
                    "checkpoint restore worker failed: {error}"
                )));
            }
        };
        let mutation_digest = checkpoint_capture_digest(&captures);
        let recovery_plan = haider_tools::CheckpointRestorePlan {
            workspace_root: PathBuf::from(&metadata.cwd),
            targets: captures
                .iter()
                .map(|capture| haider_tools::CheckpointRestoreTarget {
                    path: capture.path.clone(),
                    expected_digest: capture.post_digest.clone(),
                    restore_bytes: capture.pre_bytes.clone(),
                })
                .collect(),
        };
        let command_run_id = RunId::new(format!("checkpoint-command:{}", command_id.as_str()));
        let effect_id = haider_protocol::ids::EffectId::new(format!(
            "checkpoint-effect:{}",
            command_id.as_str()
        ));
        let checkpoint_kind = source_checkpoints.first().map_or(
            haider_protocol::checkpoint::CheckpointKind::Write,
            |checkpoint| checkpoint.kind,
        );
        let capture = haider_tools::CheckpointCapture {
            kind: checkpoint_kind,
            paths: captures,
            post_digest: mutation_digest.clone(),
        };
        let mut cas = self.hub.inner.store.clone();
        let checkpoint = match haider_tools::freeze_checkpoint(
            &mut cas,
            haider_tools::FreezeCheckpointInput {
                session_id: session_id.clone(),
                branch_id: branch_id.clone(),
                run_id: command_run_id.clone(),
                effect_id: effect_id.clone(),
                call_id: command_id.as_str().to_owned(),
                origin: action.origin(),
                source_checkpoint_id: (source_checkpoints.len() == 1)
                    .then(|| source_checkpoints[0].checkpoint_id.clone()),
            },
            capture,
        )
        .await
        {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                rollback_failed_checkpoint_command(recovery_plan.clone()).await?;
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.to_string(),
                    false,
                    None,
                );
            }
        };
        let envelopes = match checkpoint_effect_envelopes(CheckpointEffectEnvelopeInput {
            session_id: session_id.clone(),
            branch_id: branch_id.clone(),
            run_id: command_run_id,
            effect_id,
            command_id: command_id.as_str(),
            mutation_digest,
            checkpoint,
            worker_generation,
            device_id: &self.hub.inner.device_id,
        }) {
            Ok(envelopes) => envelopes,
            Err(error) => {
                rollback_failed_checkpoint_command(recovery_plan.clone()).await?;
                return Err(error);
            }
        };
        let outcome = self
            .hub
            .commit_checkpoint(CheckpointCommitCommand {
                command_id: command_id.0.clone(),
                method: method.to_owned(),
                request_digest: request_digest.clone(),
                request_json: request_json.clone(),
                session_id,
                worker_generation,
                restored_checkpoint_ids,
                envelopes,
            })
            .await;
        let receipt = match outcome {
            Ok(CheckpointCommitOutcome::Committed { receipt, .. })
            | Ok(CheckpointCommitOutcome::IdempotentReplay { receipt }) => receipt,
            Err(CheckpointCommitFailure::DefinitelyUncommitted(SessionHubError::Store(error))) => {
                rollback_failed_checkpoint_command(recovery_plan).await?;
                return self.respond_turn_error(request_id, error);
            }
            Err(CheckpointCommitFailure::DefinitelyUncommitted(error)) => {
                rollback_failed_checkpoint_command(recovery_plan).await?;
                return Err(error);
            }
            Err(CheckpointCommitFailure::Ambiguous(error)) => {
                match self
                    .hub
                    .checkpoint_command_receipt(&command_id, method, &request_digest, &request_json)
                    .await
                {
                    Ok(Some(receipt)) => receipt,
                    Ok(None) => {
                        rollback_failed_checkpoint_command(recovery_plan).await?;
                        return Err(error);
                    }
                    Err(reconcile_error) => return Err(reconcile_error),
                }
            }
        };
        self.respond_checkpoint_receipt(request_id, method, receipt)
    }

    async fn resolve_checkpoint_action(
        &self,
        session_id: &SessionId,
        branch_id: Option<&haider_protocol::ids::BranchId>,
        action: &CheckpointDoorAction,
    ) -> Result<Vec<haider_protocol::checkpoint::CheckpointRecorded>, CheckpointDoorFailure> {
        if let CheckpointDoorAction::RollbackTurn { run_id } = action {
            let checkpoints = self
                .hub
                .inner
                .store
                .checkpoints_for_run(session_id.clone(), branch_id.cloned(), run_id.clone())
                .await
                .map_err(|error| CheckpointDoorFailure::Hub(error.into()))?;
            if checkpoints.is_empty() {
                return Err(CheckpointDoorFailure::not_found(
                    "the requested turn has no checkpoints on this branch",
                ));
            }
            return Ok(checkpoints);
        }
        let target = match action {
            CheckpointDoorAction::Undo { target } | CheckpointDoorAction::Redo { target } => target,
            CheckpointDoorAction::RollbackTurn { .. } => {
                return Err(CheckpointDoorFailure::Hub(SessionHubError::Task(
                    "rollback action escaped its run lookup".into(),
                )));
            }
        };
        let selected = if target == "last" {
            match action {
                CheckpointDoorAction::Undo { .. } => self
                    .hub
                    .inner
                    .store
                    .list_checkpoints(session_id.clone(), branch_id.cloned(), None, 1)
                    .await
                    .map_err(|error| CheckpointDoorFailure::Hub(error.into()))?
                    .checkpoints
                    .into_iter()
                    .next(),
                CheckpointDoorAction::Redo { .. } => {
                    self.find_redo_checkpoint(session_id, branch_id, None)
                        .await?
                }
                CheckpointDoorAction::RollbackTurn { .. } => None,
            }
        } else {
            let requested_id = haider_protocol::ids::CheckpointId::new(target);
            let checkpoint = self
                .hub
                .inner
                .store
                .checkpoint(session_id.clone(), requested_id.clone())
                .await
                .map_err(|error| CheckpointDoorFailure::Hub(error.into()))?;
            if let Some(checkpoint) = checkpoint.as_ref()
                && checkpoint.branch_id.as_ref() != branch_id
            {
                return Err(CheckpointDoorFailure::Response {
                    code: haider_rpc::ERROR_CODE_CHECKPOINT_BRANCH_MISMATCH,
                    message: "checkpoint belongs to another branch".into(),
                    data: Some(ErrorData::CheckpointBranchMismatch {
                        checkpoint_id: checkpoint.checkpoint_id.clone(),
                        checkpoint_branch_id: checkpoint.branch_id.clone(),
                        requested_branch_id: branch_id.cloned(),
                    }),
                });
            }
            match (action, checkpoint) {
                (CheckpointDoorAction::Redo { .. }, Some(checkpoint))
                    if !matches!(
                        checkpoint.origin,
                        haider_protocol::checkpoint::CheckpointOrigin::Undo
                            | haider_protocol::checkpoint::CheckpointOrigin::RollbackTurn
                    ) =>
                {
                    if checkpoint.origin == haider_protocol::checkpoint::CheckpointOrigin::Redo {
                        return Err(CheckpointDoorFailure::Response {
                            code: ERROR_CODE_INVALID_ARGUMENT,
                            message: "a redo checkpoint is not itself redoable; undo it instead"
                                .into(),
                            data: None,
                        });
                    }
                    self.find_redo_checkpoint(
                        session_id,
                        branch_id,
                        Some(&checkpoint.checkpoint_id),
                    )
                    .await?
                }
                (_, checkpoint) => checkpoint,
            }
        };
        selected.map(|checkpoint| vec![checkpoint]).ok_or_else(|| {
            CheckpointDoorFailure::not_found("no matching checkpoint exists on this branch")
        })
    }

    async fn find_redo_checkpoint(
        &self,
        session_id: &SessionId,
        branch_id: Option<&haider_protocol::ids::BranchId>,
        source_checkpoint_id: Option<&haider_protocol::ids::CheckpointId>,
    ) -> Result<Option<haider_protocol::checkpoint::CheckpointRecorded>, CheckpointDoorFailure>
    {
        let mut cursor = None;
        loop {
            let page = self
                .hub
                .inner
                .store
                .list_checkpoints(
                    session_id.clone(),
                    branch_id.cloned(),
                    cursor,
                    haider_protocol::checkpoint::CHECKPOINT_LIST_MAX_PAGE,
                )
                .await
                .map_err(|error| CheckpointDoorFailure::Hub(error.into()))?;
            let next_cursor = page.next_cursor;
            if let Some(checkpoint) = page.checkpoints.into_iter().find(|checkpoint| {
                matches!(
                    checkpoint.origin,
                    haider_protocol::checkpoint::CheckpointOrigin::Undo
                        | haider_protocol::checkpoint::CheckpointOrigin::RollbackTurn
                ) && source_checkpoint_id
                    .is_none_or(|source| checkpoint.source_checkpoint_id.as_ref() == Some(source))
            }) {
                return Ok(Some(checkpoint));
            }
            let Some(next_cursor) = next_cursor else {
                return Ok(None);
            };
            cursor = Some(next_cursor);
        }
    }

    async fn checkpoint_restore_plan(
        &self,
        workspace_root: &str,
        checkpoints: &[haider_protocol::checkpoint::CheckpointRecorded],
    ) -> Result<haider_tools::CheckpointRestorePlan, CheckpointDoorFailure> {
        let mut targets = BTreeMap::<String, haider_tools::CheckpointRestoreTarget>::new();
        for checkpoint in checkpoints {
            for path in &checkpoint.paths {
                if let Some(reason) = &path.truncated_reason {
                    return Err(CheckpointDoorFailure::Response {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "checkpoint {} cannot restore {}: {reason}",
                            checkpoint.checkpoint_id, path.path
                        ),
                        data: None,
                    });
                }
                let restore_bytes = match &path.pre_artifact {
                    Some(artifact) => Some(
                        self.hub
                            .inner
                            .store
                            .get(artifact)
                            .await
                            .map_err(|error| CheckpointDoorFailure::Hub(error.into()))?,
                    ),
                    None => None,
                };
                if restore_bytes.as_deref().map(checkpoint_bytes_digest) != path.pre_digest {
                    return Err(CheckpointDoorFailure::Response {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "checkpoint {} pre-image digest is inconsistent",
                            checkpoint.checkpoint_id
                        ),
                        data: None,
                    });
                }
                match targets.get_mut(&path.path) {
                    Some(target) => {
                        if target.restore_bytes.as_deref().map(checkpoint_bytes_digest)
                            != path.post_digest
                        {
                            return Err(CheckpointDoorFailure::Response {
                                code: haider_rpc::ERROR_CODE_CHECKPOINT_CONFLICT,
                                message: "turn checkpoint chain is not contiguous".into(),
                                data: Some(ErrorData::CheckpointConflict {
                                    conflict: haider_protocol::checkpoint::CheckpointConflict {
                                        path: path.path.clone(),
                                        expected_digest: path.post_digest.clone(),
                                        current_digest: target
                                            .restore_bytes
                                            .as_deref()
                                            .map(checkpoint_bytes_digest),
                                    },
                                }),
                            });
                        }
                        target.restore_bytes = restore_bytes;
                    }
                    None => {
                        targets.insert(
                            path.path.clone(),
                            haider_tools::CheckpointRestoreTarget {
                                path: path.path.clone(),
                                expected_digest: path.post_digest.clone(),
                                restore_bytes,
                            },
                        );
                    }
                }
            }
        }
        Ok(haider_tools::CheckpointRestorePlan {
            workspace_root: PathBuf::from(workspace_root),
            targets: targets.into_values().collect(),
        })
    }

    fn respond_checkpoint_receipt(
        &self,
        request_id: RequestId,
        method: &str,
        receipt: haider_protocol::checkpoint::CheckpointMutationReceipt,
    ) -> Result<(), SessionHubError> {
        let body = match method {
            "checkpoint.undo" => ResponseBody::CheckpointUndo { receipt },
            "checkpoint.redo" => ResponseBody::CheckpointRedo { receipt },
            "checkpoint.rollback_turn" => ResponseBody::CheckpointRollbackTurn { receipt },
            _ => {
                return Err(SessionHubError::Task(
                    "unsupported checkpoint response method".into(),
                ));
            }
        };
        self.send(WireFrame::Response { request_id, body })
    }

    #[allow(clippy::too_many_arguments)]
    async fn branch_create(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        source_branch_id: Option<haider_protocol::ids::BranchId>,
        fork_node_id: haider_protocol::ids::NodeId,
        fork_seq: u64,
        name: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty()
            || fork_node_id.as_str().is_empty()
            || fork_seq == 0
            || name.as_ref().is_some_and(|name| name.trim().is_empty())
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "branch command, fork node/sequence, and optional name must be valid",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "source_branch_id": &source_branch_id,
            "fork_node_id": &fork_node_id,
            "fork_seq": fork_seq,
            "name": &name,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode branch-create coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes attachment, generation, and current-lineage
        // validation so a lost response remains recoverable after restart.
        match self
            .hub
            .branch_create_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(created)) => return self.respond_branch_created(request_id, created),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "branch creation requires a control attachment to this session",
                false,
                None,
            );
        }

        let command = BranchCreateCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            branch_id: haider_protocol::ids::BranchId::new(random_id("branch")?),
            source_branch_id,
            fork_node_id,
            fork_seq,
            name,
            event_id: EventId::new(random_id("branch-created")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let created = match self.hub.create_branch(command).await {
            Ok(BranchCreateOutcome::Committed { created, .. })
            | Ok(BranchCreateOutcome::IdempotentReplay { created }) => created,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_branch_created(request_id, created)
    }

    fn respond_branch_created(
        &self,
        request_id: RequestId,
        created: CreatedBranch,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::BranchCreate {
                session_id: created.session_id,
                branch_id: created.branch_id,
                source_branch_id: created.source_branch_id,
                fork_node_id: created.fork_node_id,
                fork_seq: created.fork_seq,
                created_seq: created.created_seq,
                worker_generation: created.worker_generation,
                name: created.name,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn session_fork(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        source_session_id: SessionId,
        worker_generation: u64,
        source_branch_id: Option<haider_protocol::ids::BranchId>,
        selector: SessionForkSelectorInput,
        name: Option<String>,
    ) -> Result<(), SessionHubError> {
        let name = normalize_session_title(name);
        let selector_is_valid = match &selector {
            SessionForkSelectorInput::Exact { node_id, seq } => {
                !node_id.as_str().is_empty() && *seq > 0
            }
            SessionForkSelectorInput::Prompt { seq } => *seq > 0,
        };
        if command_id.as_str().is_empty() || !selector_is_valid {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-fork command and fork coordinate must be valid",
                false,
                None,
            );
        }
        let request_value = match &selector {
            SessionForkSelectorInput::Exact { node_id, seq } => serde_json::json!({
                "session_id": &source_session_id,
                "worker_generation": worker_generation,
                "source_branch_id": &source_branch_id,
                "fork_node_id": node_id,
                "fork_seq": seq,
                "name": &name,
            }),
            SessionForkSelectorInput::Prompt { seq } => serde_json::json!({
                "session_id": &source_session_id,
                "worker_generation": worker_generation,
                "source_branch_id": &source_branch_id,
                "prompt": { "seq": seq },
                "name": &name,
            }),
        };
        let request_json = serde_json::to_string(&request_value).map_err(|error| {
            SessionHubError::Task(format!("cannot encode session-fork coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .session_fork_receipt(&command_id, &request_digest, &request_json, false)
            .await
        {
            Ok(Some(created)) => match self.hub.publish_received_fork(&created).await {
                Ok(()) => return self.respond_session_fork_created(request_id, created),
                Err(SessionHubError::Store(error)) => {
                    return self.respond_session_fork_error(request_id, error);
                }
                Err(error) => return Err(error),
            },
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_session_fork_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &source_session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "session fork requires a control attachment to the source session",
                false,
                None,
            );
        }
        let session_id = SessionId::new(random_id("session")?);
        let audit_event_id = EventId::new(random_id("session-forked")?);
        let outcome = match selector {
            SessionForkSelectorInput::Exact { node_id, seq } => {
                self.hub
                    .fork_session(SessionForkCommand {
                        command_id: command_id.0,
                        request_digest,
                        request_json,
                        source_session_id,
                        session_id,
                        worker_generation,
                        source_branch_id,
                        fork_node_id: node_id,
                        fork_seq: seq,
                        name,
                        metafork: None,
                        audit_event_id,
                        device_id: self.hub.inner.device_id.clone(),
                    })
                    .await
            }
            SessionForkSelectorInput::Prompt { seq } => {
                self.hub
                    .fork_session_from_prompt(SessionPromptForkCommand {
                        command_id: command_id.0,
                        request_digest,
                        request_json,
                        source_session_id,
                        session_id,
                        worker_generation,
                        source_branch_id,
                        prompt_seq: seq,
                        name,
                        audit_event_id,
                        device_id: self.hub.inner.device_id.clone(),
                    })
                    .await
            }
        };
        let created = match outcome {
            Ok(SessionForkOutcome::Committed { created, .. })
            | Ok(SessionForkOutcome::IdempotentReplay { created }) => created,
            Err(SessionHubError::Store(error)) => {
                return self.respond_session_fork_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_session_fork_created(request_id, created)
    }

    fn respond_session_fork_created(
        &self,
        request_id: RequestId,
        created: CreatedSessionFork,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionFork {
                session_id: created.session_id,
                source_session_id: created.source_session_id,
                source_branch_id: created.source_branch_id,
                fork_node_id: created.fork_node_id,
                fork_seq: created.fork_seq,
                created_seq: created.created_seq,
                worker_generation: created.worker_generation,
                metadata: created.metadata,
                forked_from: created.forked_from,
                draft: created.draft,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn session_metafork(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        source_session_id: SessionId,
        worker_generation: u64,
        source_branch_id: Option<haider_protocol::ids::BranchId>,
        fork_node_id: haider_protocol::ids::NodeId,
        fork_seq: u64,
        name: Option<String>,
        description: String,
        model_proposal: haider_protocol::session_fork::SessionMetaforkProposal,
        accepted_proposal_digest: Option<String>,
    ) -> Result<(), SessionHubError> {
        let name = normalize_session_title(name);
        if let Err(message) = validate_metafork_review_shape(
            &command_id,
            &fork_node_id,
            fork_seq,
            &description,
            &model_proposal,
        ) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &message,
                false,
                None,
            );
        }
        let Some(accepted_proposal_digest) = accepted_proposal_digest else {
            if !self
                .hub
                .holds_control_attachment(&self.connection_id, &source_session_id)?
            {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CAPABILITY_DENIED,
                    "session metafork review requires a control attachment to the source session",
                    false,
                    None,
                );
            }
            let canonical = match self
                .canonical_metafork_proposal(
                    &source_session_id,
                    worker_generation,
                    source_branch_id.as_ref(),
                    &fork_node_id,
                    fork_seq,
                    &model_proposal,
                )
                .await
            {
                Ok(proposal) => proposal,
                Err(SessionHubError::Store(error)) => {
                    return self.respond_session_fork_error(request_id, error);
                }
                Err(error) => return Err(error),
            };
            let review_manifest = haider_protocol::session_fork::SessionMetaforkReviewManifest {
                command_id: command_id.0.clone(),
                source_session_id: source_session_id.clone(),
                worker_generation,
                source_branch_id: source_branch_id.clone(),
                fork_node_id: fork_node_id.clone(),
                fork_seq,
                name: name.clone(),
                description: description.clone(),
                model_proposal: canonical.clone(),
            };
            let proposal_digest = review_manifest.digest().map_err(|error| {
                SessionHubError::Task(format!("cannot digest metafork review manifest: {error}"))
            })?;
            let gate_command_id = command_id.0.clone();
            let gate_digest = proposal_digest.clone();
            {
                let mut reviews = lock(&self.metafork_reviews)?;
                if !reviews.contains_key(command_id.as_str()) && reviews.len() >= 64 {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_OVERLOADED,
                        "this connection already has 64 metafork reviews awaiting acceptance",
                        true,
                        None,
                    );
                }
                // Reserve capacity without making acceptance possible. The real
                // digest is installed only after the review response is served.
                reviews.insert(gate_command_id.clone(), String::new());
            }
            // Proposal phase is intentionally write-free: no receipt, source
            // journal fact, child row, or durable review token is created. A
            // connection-local gate proves this exact review was served, and
            // the daemon replaces model previews with source-row truth.
            let delivery = self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::SessionMetafork {
                    committed: false,
                    source_session_id,
                    session_id: None,
                    source_branch_id,
                    fork_node_id,
                    fork_seq,
                    description,
                    model_proposal: canonical,
                    review_manifest: Some(review_manifest),
                    proposal_digest,
                    created_seq: None,
                    worker_generation: None,
                    metadata: None,
                    omission_count: None,
                },
            });
            {
                let mut reviews = lock(&self.metafork_reviews)?;
                if delivery.is_ok() {
                    reviews.insert(gate_command_id, gate_digest);
                } else if reviews
                    .get(&gate_command_id)
                    .is_some_and(|digest| digest.is_empty())
                {
                    reviews.remove(&gate_command_id);
                }
            }
            return delivery;
        };
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &source_session_id,
            "worker_generation": worker_generation,
            "source_branch_id": &source_branch_id,
            "fork_node_id": &fork_node_id,
            "fork_seq": fork_seq,
            "name": &name,
            "description": &description,
            "model_proposal": &model_proposal,
            "accepted_proposal_digest": &accepted_proposal_digest,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode session-metafork coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .session_fork_receipt(&command_id, &request_digest, &request_json, true)
            .await
        {
            Ok(Some(created)) => match self.hub.publish_received_fork(&created).await {
                Ok(()) => return self.respond_session_metafork_created(request_id, created),
                Err(SessionHubError::Store(error)) => {
                    return self.respond_session_fork_error(request_id, error);
                }
                Err(error) => return Err(error),
            },
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_session_fork_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &source_session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "session metafork requires a control attachment to the source session",
                false,
                None,
            );
        }
        let canonical = match self
            .canonical_metafork_proposal(
                &source_session_id,
                worker_generation,
                source_branch_id.as_ref(),
                &fork_node_id,
                fork_seq,
                &model_proposal,
            )
            .await
        {
            Ok(proposal) => proposal,
            Err(SessionHubError::Store(error)) => {
                return self.respond_session_fork_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        if canonical != model_proposal {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "metafork acceptance must echo the exact source-derived proposal shown for review",
                false,
                None,
            );
        }
        let review_digest = haider_protocol::session_fork::SessionMetaforkReviewManifest {
            command_id: command_id.0.clone(),
            source_session_id: source_session_id.clone(),
            worker_generation,
            source_branch_id: source_branch_id.clone(),
            fork_node_id: fork_node_id.clone(),
            fork_seq,
            name: name.clone(),
            description: description.clone(),
            model_proposal: model_proposal.clone(),
        }
        .digest()
        .map_err(|error| {
            SessionHubError::Task(format!("cannot digest metafork review manifest: {error}"))
        })?;
        if accepted_proposal_digest != review_digest
            || lock(&self.metafork_reviews)?
                .get(command_id.as_str())
                .is_none_or(|reviewed| reviewed != &review_digest)
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "metafork acceptance requires the exact review served on this connection",
                false,
                None,
            );
        }
        let review_command_id = command_id.0.clone();
        let command = SessionForkCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            source_session_id,
            session_id: SessionId::new(random_id("session")?),
            worker_generation,
            source_branch_id,
            fork_node_id,
            fork_seq,
            name,
            metafork: Some(SessionMetaforkCommit {
                description,
                model_proposal,
                accepted_proposal_digest,
            }),
            audit_event_id: EventId::new(random_id("session-metaforked")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let created = match self.hub.fork_session(command).await {
            Ok(SessionForkOutcome::Committed { created, .. })
            | Ok(SessionForkOutcome::IdempotentReplay { created }) => created,
            Err(SessionHubError::Store(error)) => {
                return self.respond_session_fork_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        lock(&self.metafork_reviews)?.remove(&review_command_id);
        self.respond_session_metafork_created(request_id, created)
    }

    pub(super) async fn canonical_metafork_proposal(
        &self,
        source_session_id: &SessionId,
        worker_generation: u64,
        source_branch_id: Option<&haider_protocol::ids::BranchId>,
        fork_node_id: &haider_protocol::ids::NodeId,
        fork_seq: u64,
        proposal: &haider_protocol::session_fork::SessionMetaforkProposal,
    ) -> Result<haider_protocol::session_fork::SessionMetaforkProposal, SessionHubError> {
        self.hub
            .inner
            .store
            .validate_session_fork_source(
                worker_generation,
                source_session_id.clone(),
                source_branch_id.cloned(),
                fork_node_id.clone(),
                fork_seq,
            )
            .await?;
        let lineage = self
            .hub
            .inner
            .store
            .branch_lineage(source_session_id, source_branch_id)
            .await?;
        let source_owner_agent = self
            .hub
            .inner
            .store
            .delegation_for_child_session(source_session_id.clone())
            .await?
            .map(|delegation| delegation.agent_id);
        let mut scopes = std::collections::HashMap::new();
        let mut ceiling = u64::MAX;
        for descriptor in lineage.iter().rev() {
            scopes.insert(Some(descriptor.branch_id.clone()), ceiling);
            ceiling = ceiling.min(descriptor.fork_seq);
        }
        scopes.insert(None, ceiling);

        let mut admitted = Vec::new();
        let mut cursor = 0;
        while cursor < fork_seq {
            let page = self
                .hub
                .inner
                .store
                .read(source_session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if envelope.seq > fork_seq {
                    break;
                }
                if scopes
                    .get(&envelope.branch_id)
                    .is_some_and(|through| envelope.seq <= *through)
                {
                    admitted.push(envelope);
                }
            }
        }

        let mut canonical = proposal.clone();
        let mut reviewed_event_count = 0_usize;
        for removal in &mut canonical.removals {
            let expected_span = removal
                .through_seq
                .checked_sub(removal.from_seq)
                .and_then(|span| span.checked_add(1))
                .ok_or_else(|| {
                    SessionHubError::Store(HaiderError::new(
                        ErrorCode::InvalidArgument,
                        "metafork proposal range is not bounded",
                        false,
                    ))
                })?;
            let admitted_span = admitted
                .iter()
                .filter(|envelope| {
                    envelope.seq >= removal.from_seq && envelope.seq <= removal.through_seq
                })
                .count();
            if u64::try_from(admitted_span).ok() != Some(expected_span) {
                // Reject rather than clamp: the exact proposal is hashed and
                // shown for human authorization, so silently changing its
                // coordinates would make acceptance authorize different text.
                return Err(SessionHubError::Store(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "metafork proposal range must be wholly contained in the copied source lineage",
                    false,
                )));
            }
            let matches = admitted
                .iter()
                .filter(|envelope| {
                    envelope.seq >= removal.from_seq
                        && envelope.seq <= removal.through_seq
                        && (envelope.agent_id.is_none()
                            || envelope.agent_id.as_ref() == source_owner_agent.as_ref())
                        && envelope.render.prompt != haider_protocol::envelope::PromptRender::Omit
                })
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(SessionHubError::Store(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "metafork proposal range has no prompt-visible event in the copied source lineage",
                    false,
                )));
            }
            reviewed_event_count = reviewed_event_count.saturating_add(matches.len());
            if reviewed_event_count > 512 {
                return Err(SessionHubError::Store(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "metafork review selects more than 512 prompt-visible events; split the proposal",
                    false,
                )));
            }
            let mut reviewed_events = Vec::with_capacity(matches.len());
            for envelope in matches {
                let kind = envelope
                    .payload
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                struct PreviewWriter {
                    bytes: Vec<u8>,
                    total: usize,
                }
                impl std::io::Write for PreviewWriter {
                    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                        self.total = self.total.saturating_add(bytes.len());
                        let remaining = 384_usize.saturating_sub(self.bytes.len());
                        self.bytes
                            .extend_from_slice(&bytes[..remaining.min(bytes.len())]);
                        Ok(bytes.len())
                    }

                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                let mut preview = PreviewWriter {
                    bytes: Vec::with_capacity(384),
                    total: 0,
                };
                write_payload_json(&mut preview, &envelope.payload).map_err(|error| {
                    SessionHubError::Task(format!("cannot render metafork review preview: {error}"))
                })?;
                while std::str::from_utf8(&preview.bytes).is_err() {
                    preview.bytes.pop();
                }
                let excerpt_truncated = preview.bytes.len() < preview.total;
                let excerpt = String::from_utf8(preview.bytes).unwrap_or_default();
                reviewed_events.push(haider_protocol::session_fork::SessionMetaforkReviewEvent {
                    source_seq: envelope.seq,
                    source_event_id: envelope.event_id.clone(),
                    payload_kind: kind.to_owned(),
                    excerpt_truncated,
                    excerpt,
                });
            }
            removal.preview = Some(format!(
                "{} prompt-visible event(s); see reviewed_events",
                reviewed_events.len()
            ));
            removal.reviewed_events = reviewed_events;
        }
        Ok(canonical)
    }

    fn respond_session_metafork_created(
        &self,
        request_id: RequestId,
        created: CreatedSessionFork,
    ) -> Result<(), SessionHubError> {
        let description = created.description.ok_or_else(|| {
            SessionHubError::Task("metafork receipt is missing its description".into())
        })?;
        let model_proposal = created.model_proposal.ok_or_else(|| {
            SessionHubError::Task("metafork receipt is missing its model proposal".into())
        })?;
        let proposal_digest = created.proposal_digest.ok_or_else(|| {
            SessionHubError::Task("metafork receipt is missing its proposal digest".into())
        })?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionMetafork {
                committed: true,
                source_session_id: created.source_session_id,
                session_id: Some(created.session_id),
                source_branch_id: created.source_branch_id,
                fork_node_id: created.fork_node_id,
                fork_seq: created.fork_seq,
                description,
                model_proposal,
                review_manifest: None,
                proposal_digest,
                created_seq: Some(created.created_seq),
                worker_generation: Some(created.worker_generation),
                metadata: Some(created.metadata),
                omission_count: Some(created.omission_count),
            },
        })
    }

    /// `session.select_model` — receipted live-session model selection.
    ///
    /// Sessions are provider-agnostic: this is exactly as ceremonial as
    /// picking a model. Resolution/validation ride the ONE authority in
    /// `crate::model_select`; the store owns durability; the next logical
    /// turn re-reads the committed metadata (R6 re-resolution), so commit
    /// here IS next-turn pickup.
    async fn session_select_model(
        &self,
        request_id: RequestId,
        input: SessionSelectModelInput,
    ) -> Result<(), SessionHubError> {
        let SessionSelectModelInput {
            command_id,
            session_id,
            worker_generation,
            model,
            provider,
            confirm_new_epoch,
        } = input;
        if command_id.as_str().trim().is_empty() || model.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "model selection needs a command id and a model",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "model": &model,
            "provider": &provider,
            "confirm_new_epoch": confirm_new_epoch,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode model-selection coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes validation so a lost response remains
        // recoverable even after registry or inventory changes.
        match self
            .hub
            .session_select_model_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_model_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let Some(current) = (match self.hub.session_metadata(&session_id).await {
            Ok(metadata) => metadata,
            Err(error) => return self.respond_turn_error(request_id, error),
        }) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "model selection requires a live session with typed metadata",
                false,
                None,
            );
        };
        let (mut summaries, descriptors) = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .map_or_else(
                || (Vec::new(), Vec::new()),
                |view| (view.providers, view.descriptors),
            );
        let selected_provider = provider
            .as_deref()
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .unwrap_or(&current.provider);
        match self
            .refresh_inventory_if_needed(&mut summaries, selected_provider, model.trim())
            .await
        {
            Ok(()) => {}
            Err(InventoryRefreshError::Hub(error)) => return Err(error),
            Err(InventoryRefreshError::Provider(error)) => {
                return self.respond_error(
                    request_id,
                    &error.code,
                    &error.message,
                    error.retryable,
                    error.data,
                );
            }
        }
        let authority = crate::model_select::ModelSelectionAuthority::new(
            self.hub.creatable_providers()?,
            summaries,
        );
        let validated = match authority.validate_selection_with_status(
            &current.provider,
            provider.as_deref(),
            &model,
        ) {
            Ok(selection) => selection,
            Err(refusal) => return self.respond_selection_refusal(request_id, &refusal),
        };
        if matches!(
            validated.inventory_status,
            haider_rpc::ModelInventoryStatusWire::Unlisted
        ) {
            tracing::info!(
                provider = %validated.provider,
                model = %validated.model,
                inventory_status = "unlisted",
                inventory_authority = "advisory",
                "admitting a custom provider model absent from its advisory inventory"
            );
        }
        let resolved_provider = validated.provider;
        let resolved_model = validated.model;
        let target_descriptor = descriptors
            .iter()
            .find(|descriptor| descriptor.provider == resolved_provider && descriptor.active)
            .cloned();
        let mut changed_fields = Vec::new();
        if current.provider != resolved_provider {
            changed_fields.push("provider".to_owned());
        }
        if current.model != resolved_model {
            changed_fields.push("model".to_owned());
        }
        let current_scope =
            crate::cache_policy::latest_main_cache_scope(&self.hub.inner.store, &session_id)
                .await?;
        let target_auth_scope =
            target_descriptor
                .as_ref()
                .map(|descriptor| match descriptor.auth_method {
                    haider_protocol::credential::AuthMethod::ApiKey => "api_key",
                    haider_protocol::credential::AuthMethod::OAuth => "oauth_subscription",
                });
        if let Some(scope) = current_scope.as_ref() {
            if let Some(target_auth_scope) = target_auth_scope
                && scope.auth_scope != target_auth_scope
            {
                changed_fields.push("auth".to_owned());
            }
            let target_account = target_descriptor
                .as_ref()
                .map(|descriptor| &descriptor.alias);
            if target_descriptor.is_some() && scope.account_scope.as_ref() != target_account {
                changed_fields.push("account".to_owned());
            }
        }
        if let Some(warning) = crate::cache_policy::assess_cache_change(
            &current,
            current_scope.as_ref(),
            &resolved_provider,
            &resolved_model,
            target_auth_scope,
            changed_fields,
            false,
        ) && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
        {
            return self.respond_cache_confirmation_required(request_id, &warning);
        }

        let command = SessionSelectModelCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            provider: resolved_provider,
            model: resolved_model,
            expected_pair: None,
            event_id: EventId::new(random_id("model-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.select_session_model(command).await {
            Ok(SessionSelectModelOutcome::Committed { selected, .. })
            | Ok(SessionSelectModelOutcome::IdempotentReplay { selected }) => selected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_model_selected(request_id, selected)
    }

    fn respond_selection_refusal(
        &self,
        request_id: RequestId,
        refusal: &crate::model_select::SelectionRefusal,
    ) -> Result<(), SessionHubError> {
        use crate::model_select::SelectionRefusal;
        let (code, data) = match refusal {
            SelectionRefusal::ProviderUnavailable { provider } => (
                haider_rpc::ERROR_CODE_PROVIDER_UNAVAILABLE,
                Some(ErrorData::ProviderUnavailable {
                    provider: provider.clone(),
                }),
            ),
            SelectionRefusal::ModelUnknown {
                provider,
                model,
                inventory_age_ms,
                ..
            } => (
                haider_rpc::ERROR_CODE_MODEL_UNKNOWN,
                Some(ErrorData::ModelUnknown {
                    provider: provider.clone(),
                    model: model.clone(),
                    inventory_age_ms: *inventory_age_ms,
                }),
            ),
            SelectionRefusal::ModelNotResolvable { .. }
            | SelectionRefusal::InvalidSelector { .. } => (ERROR_CODE_INVALID_ARGUMENT, None),
        };
        self.respond_error(request_id, code, &refusal.message(), false, data)
    }

    fn respond_model_selected(
        &self,
        request_id: RequestId,
        selected: SelectedModel,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSelectModel {
                session_id: selected.session_id,
                provider: selected.provider,
                model: selected.model,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    /// `session.rename` (G2) — receipted live-session rename, the exact
    /// `session.select_model` shape: normalization here, durability in the
    /// store's one transaction, receipt replay BEFORE validation so a lost
    /// response stays recoverable, and the same worker-generation fence.
    async fn session_rename(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        title: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session rename needs a command id",
                false,
                None,
            );
        }
        let title = normalize_session_title(title);
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "title": &title,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode session-rename coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes validation so a lost response remains
        // recoverable even after metadata changes.
        match self
            .hub
            .session_rename_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(renamed)) => return self.respond_session_renamed(request_id, renamed),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let command = SessionRenameCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            title,
            only_if_untitled: false,
            event_id: EventId::new(random_id("session-renamed")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let renamed = match self.hub.rename_session(command).await {
            Ok(SessionRenameOutcome::Committed { renamed, .. })
            | Ok(SessionRenameOutcome::IdempotentReplay { renamed }) => renamed,
            Ok(SessionRenameOutcome::Skipped) => {
                // The guard is auto-title-only; an explicit rename never
                // sets it, so this arm is unreachable by construction.
                return Err(SessionHubError::Task(
                    "explicit session rename cannot be skipped".into(),
                ));
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_session_renamed(request_id, renamed)
    }

    fn respond_session_renamed(
        &self,
        request_id: RequestId,
        renamed: RenamedSession,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionRename {
                session_id: renamed.session_id,
                title: renamed.title,
                renamed_seq: renamed.renamed_seq,
                worker_generation: renamed.worker_generation,
            },
        })
    }

    /// Receipt-backed workspace replacement. Receipt replay deliberately
    /// precedes filesystem validation so a committed response remains
    /// recoverable even if the selected path later disappears.
    async fn session_workspace_set(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        path: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "workspace selection needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "path": &path,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode workspace-set coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .session_workspace_set_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_workspace_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        if worker_generation != self.hub.inner.store.worker_generation() {
            return self.respond_error(
                request_id,
                ERROR_CODE_STALE_GENERATION,
                "workspace selection worker generation is stale",
                false,
                None,
            );
        }
        // A workspace is an effect-authority boundary. Serialize this idle
        // check and commit against turn admission so no old-root broker can
        // outlive the workspace-selected fact.
        let _workspace_selection = self.hub.lock_workflow_selection(&session_id).await;
        match self.hub.session_has_nonterminal_runs(&session_id).await {
            Ok(false) => {}
            Ok(true) => {
                return self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_BUSY,
                    "workspace selection requires an idle session; retry after the active turn completes",
                    true,
                    None,
                );
            }
            Err(error) => return self.respond_turn_error(request_id, error),
        }

        let validated = match validate_workspace(path).await {
            Ok(validated) => validated,
            Err(message) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                );
            }
        };
        let ValidatedWorkspace {
            canonical: path,
            descriptor: workspace_descriptor,
        } = validated;
        let command = SessionWorkspaceSetCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            path,
            event_id: EventId::new(random_id("workspace-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.set_session_workspace(command).await {
            Ok(SessionWorkspaceSetOutcome::Committed { selected, .. })
            | Ok(SessionWorkspaceSetOutcome::IdempotentReplay { selected }) => selected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        // Keep the exact validated directory identity alive across the
        // actor/store commit. The stored path is still re-probed at attach and
        // turn start; this handle preserves the validated object for the
        // selection transaction rather than claiming to pin its pathname.
        drop(workspace_descriptor);
        self.respond_workspace_selected(request_id, selected)
    }

    fn respond_workspace_selected(
        &self,
        request_id: RequestId,
        selected: SelectedWorkspace,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionWorkspaceSet {
                session_id: selected.session_id,
                path: selected.path,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    /// `session.seen` uses rename's receipt-first mutation shape. The store
    /// serializes the acknowledgement with every session write and chooses
    /// the maximum durable timestamp, so this handler never trusts wall
    /// clock order for monotonicity.
    async fn session_seen(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session seen needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode session-seen coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .session_seen_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(seen)) => return self.respond_session_seen(request_id, seen),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        let command = SessionSeenCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            event_id: EventId::new(random_id("session-seen")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let seen = match self.hub.mark_session_seen(command).await {
            Ok(SessionSeenOutcome::Committed { seen, .. })
            | Ok(SessionSeenOutcome::IdempotentReplay { seen }) => seen,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_session_seen(request_id, seen)
    }

    fn respond_session_seen(
        &self,
        request_id: RequestId,
        seen: SeenSession,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSeen {
                session_id: seen.session_id,
                seen_at_ms: seen.seen_at_ms,
                seen_seq: seen.seen_seq,
                worker_generation: seen.worker_generation,
            },
        })
    }

    async fn graph_status(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        let status = match self.hub.graph_status(&session_id).await {
            Ok(status) => status,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphStatus { status },
        })
    }

    async fn workflow_instance(
        &self,
        request_id: RequestId,
        workflow_id: String,
        template_digest: Option<String>,
    ) -> Result<(), SessionHubError> {
        let instance = if let Some(template) = haider_protocol::graph::graph_template(&workflow_id)
        {
            let current_digest = haider_protocol::graph::graph_template_digest(&template);
            template_digest
                .as_deref()
                .is_none_or(|expected| expected == current_digest)
                .then(|| WorkflowInstanceV1 {
                    id: template.name.clone(),
                    revision: template.version,
                    digest: None,
                    template_digest: current_digest,
                    pipe_version: None,
                    source: WorkflowInstanceSourceV1::BuiltIn,
                    node_metadata: None,
                    compiled_template: template,
                })
        } else {
            let workflow = match template_digest {
                Some(template_digest) => {
                    self.hub
                        .loom_workflow_revision(&workflow_id, &template_digest)
                        .await?
                }
                None => self.hub.loom_workflow(&workflow_id).await?,
            };
            workflow.map(|workflow| {
                let template_digest =
                    haider_protocol::graph::graph_template_digest(&workflow.template);
                WorkflowInstanceV1 {
                    id: workflow.id,
                    revision: workflow.rev,
                    digest: Some(workflow.digest),
                    template_digest,
                    pipe_version: Some(workflow.pipe_version),
                    source: WorkflowInstanceSourceV1::User,
                    node_metadata: Some(workflow.meta),
                    compiled_template: workflow.template,
                }
            })
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::WorkflowInstance { instance },
        })
    }

    /// B1 — the Loom registry read.
    async fn loom_list(
        &self,
        request_id: RequestId,
        include_archived: bool,
    ) -> Result<(), SessionHubError> {
        let snapshot = match self.hub.inner.store.loom_registry_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        let mut agent_types = Vec::new();
        let mut workflows = Vec::new();
        let mut archived_entries = Vec::new();
        for entry in snapshot.entries {
            if entry.entry.archived && !include_archived {
                continue;
            }
            match entry.record {
                haider_protocol::loom::LoomRegistryRecord::AgentType(record) => {
                    if entry.entry.archived {
                        archived_entries.push(entry.entry);
                    }
                    agent_types.push(record);
                }
                haider_protocol::loom::LoomRegistryRecord::Workflow(record) => {
                    if entry.entry.archived {
                        archived_entries.push(entry.entry);
                    }
                    workflows.push(record);
                }
                _ => {
                    return self.respond_error(
                        request_id,
                        ErrorCode::Internal.as_str(),
                        "durable Loom registry contains an unknown record kind",
                        false,
                        None,
                    );
                }
            }
        }
        // W-flow — probe each DISTINCT declared CLI once and report device
        // presence alongside the registry, so a missing program is visible
        // before the bind instead of at the first failing turn.
        let cli_present: std::collections::BTreeMap<String, bool> = agent_types
            .iter()
            .flat_map(|record| record.clis.iter())
            .map(|cli| cli.to_owned())
            .collect::<std::collections::BTreeSet<String>>()
            .into_iter()
            .map(|cli| {
                let present = haider_platform::program_on_path(&cli);
                (cli, present)
            })
            .collect();
        let workflow_catalog = published_workflow_catalog(&workflows);
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomList {
                agent_types,
                workflows,
                cli_present,
                workflow_catalog,
                archived_entries,
            },
        })
    }

    /// B1 — agent-type registration (registry-owned rev law).
    async fn loom_register_agent_type(
        &self,
        request_id: RequestId,
        record: haider_protocol::loom::LoomAgentType,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<(), SessionHubError> {
        match self
            .hub
            .loom_register_agent_type_cas(record, expected)
            .await
        {
            Ok(haider_core::LoomRegistryMutation::Applied { value, .. }) => {
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::LoomRegistered {
                        registration: value.registration,
                        install_job_id: value.install_job_id,
                    },
                })
            }
            Ok(haider_core::LoomRegistryMutation::Conflict(conflict)) => {
                self.respond_loom_revision_conflict(request_id, conflict)
            }
            Err(error) => self.respond_error(
                request_id,
                error.code.as_str(),
                &error.message,
                error.retryable,
                None,
            ),
        }
    }

    async fn loom_install_status(
        &self,
        request_id: RequestId,
        job_id: Option<String>,
        agent_type_id: Option<String>,
    ) -> Result<(), SessionHubError> {
        let snapshot = match self
            .hub
            .typed_agent_install_status(job_id, agent_type_id)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomInstallStatus {
                jobs: snapshot.jobs,
                items: snapshot.items,
            },
        })
    }

    async fn loom_install_cancel(
        &self,
        request_id: RequestId,
        install_job_id: String,
    ) -> Result<(), SessionHubError> {
        let outcome = match self
            .hub
            .typed_agent_install_cancel(install_job_id.clone())
            .await
        {
            Ok(haider_core::TypedAgentInstallCancelResult::Cancelled) => {
                haider_rpc::TypedAgentInstallCancelOutcomeWire::Cancelled
            }
            Ok(haider_core::TypedAgentInstallCancelResult::AlreadyTerminal { state }) => {
                haider_rpc::TypedAgentInstallCancelOutcomeWire::AlreadyTerminal { state }
            }
            Ok(haider_core::TypedAgentInstallCancelResult::Unknown) => {
                haider_rpc::TypedAgentInstallCancelOutcomeWire::Unknown
            }
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomInstallCancel {
                receipt: haider_rpc::TypedAgentInstallCancelReceiptWire {
                    install_job_id,
                    outcome,
                },
            },
        })
    }

    async fn loom_archive(
        &self,
        request_id: RequestId,
        kind: haider_protocol::loom::LoomRegistryEntryKind,
        id: String,
        archived: bool,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<(), SessionHubError> {
        let outcome = match self
            .hub
            .loom_set_archived(kind, id.clone(), archived, expected)
            .await
        {
            Ok(haider_core::LoomArchiveResult::Changed { entry, .. }) => {
                haider_rpc::LoomArchiveOutcomeWire::Changed { entry }
            }
            Ok(haider_core::LoomArchiveResult::Already(entry)) => {
                haider_rpc::LoomArchiveOutcomeWire::Already { entry }
            }
            Ok(haider_core::LoomArchiveResult::NotFound) => {
                haider_rpc::LoomArchiveOutcomeWire::NotFound
            }
            Ok(haider_core::LoomArchiveResult::Conflict(conflict)) => {
                return self.respond_loom_revision_conflict(request_id, conflict);
            }
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        let receipt = haider_rpc::LoomArchiveReceiptWire { kind, id, outcome };
        self.send(WireFrame::Response {
            request_id,
            body: if archived {
                ResponseBody::LoomArchive { receipt }
            } else {
                ResponseBody::LoomUnarchive { receipt }
            },
        })
    }

    async fn loom_validate(
        &self,
        request_id: RequestId,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
    ) -> Result<(), SessionHubError> {
        if text.len() > haider_protocol::loom::LOOM_AUTHOR_TEXT_MAX_BYTES {
            return self.respond_error(
                request_id,
                ErrorCode::InvalidArgument.as_str(),
                "Loom validation exceeds the 64 KiB limit",
                false,
                None,
            );
        }
        let agent_types = match self.hub.inner.store.loom_agent_types().await {
            Ok(records) => records,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        let (errors, canonical_digest) =
            match crate::loom_author::validate(&text, kind, &agent_types) {
                Ok(validated) => {
                    match crate::loom_author::canonical_digest(&validated, &agent_types) {
                        Ok(digest) => (Vec::new(), Some(digest)),
                        Err(error) => {
                            return self.respond_error(
                                request_id,
                                error.code.as_str(),
                                &error.message,
                                error.retryable,
                                None,
                            );
                        }
                    }
                }
                Err(errors) => (errors, None),
            };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomValidate {
                errors,
                canonical_digest,
            },
        })
    }

    async fn loom_watch(
        &self,
        request_id: RequestId,
        after_cursor: u64,
    ) -> Result<(), SessionHubError> {
        let _setup = self.loom_registry_watch_serial.lock().await;
        // Subscribe before sealing the baseline. A racing commit is therefore
        // either included in that SQLite snapshot or wakes the durable replay.
        let publications = self.hub.inner.loom_registry_publications.subscribe();
        let baseline = match self.hub.inner.store.loom_registry_snapshot().await {
            Ok(baseline) => baseline,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        if after_cursor > baseline.through_cursor {
            return self.respond_error(
                request_id,
                ERROR_CODE_CURSOR_AHEAD,
                "loom.watch cursor is beyond the durable registry head",
                false,
                Some(ErrorData::CursorAhead {
                    requested: after_cursor,
                    head: baseline.through_cursor,
                }),
            );
        }
        let watch_id = random_id("loom-watch")?;
        let baseline_through_cursor = baseline.through_cursor;
        let previous = {
            let mut slot = lock(&self.loom_registry_watch)?;
            slot.take()
        };
        if let Some(previous) = previous {
            previous.cancel.send_replace(true);
            if let Some(task) = previous.task {
                let _ = task.await;
            }
            // Do not purge this ordered lane: its correlated LoomWatch
            // response may be admitted but not yet written. Old queued frames
            // carry the old watch id and are harmless to a client that has
            // replaced the watch, while deleting the response would orphan
            // the completed RPC.
        }
        let response = WireFrame::Response {
            request_id,
            body: ResponseBody::LoomWatch {
                watch_id: watch_id.clone(),
                requested_after_cursor: after_cursor,
                baseline,
            },
        };
        let hub = self.hub.clone();
        let sink = Arc::clone(&self.sink);
        let (cancel, mut cancel_receiver) = watch::channel(false);
        {
            // Ownership precedes the potentially backpressured baseline
            // delivery so close/drop can always cancel this wait.
            let mut slot = lock(&self.loom_registry_watch)?;
            *slot = Some(LoomRegistryWatchState {
                watch_id: watch_id.clone(),
                cancel: cancel.clone(),
                task: None,
            });
        }
        // The correlated baseline is the first record on this watch's FIFO
        // lane. Merely enqueueing it on the shared reply lane before spawning
        // the replay task would not order two independently scheduled lanes;
        // a delta could then name a watch the client had not learned yet.
        match super::replay::deliver_ordered_frame(
            &sink,
            &watch_id,
            &response,
            &mut cancel_receiver,
        )
        .await
        {
            super::replay::FrameDelivery::Delivered => {}
            super::replay::FrameDelivery::Cancelled => {
                if let Ok(mut slot) = self.loom_registry_watch.lock()
                    && slot
                        .as_ref()
                        .is_some_and(|state| state.watch_id == watch_id)
                {
                    slot.take();
                }
                return Ok(());
            }
            super::replay::FrameDelivery::Stuck | super::replay::FrameDelivery::Refused => {
                if let Ok(mut slot) = self.loom_registry_watch.lock()
                    && slot
                        .as_ref()
                        .is_some_and(|state| state.watch_id == watch_id)
                {
                    slot.take();
                }
                sink.close_after_required_delivery_failure();
                return Err(SessionHubError::Delivery);
            }
        }
        let mut slot = lock(&self.loom_registry_watch)?;
        let Some(state) = slot.as_mut().filter(|state| state.watch_id == watch_id) else {
            cancel.send_replace(true);
            return Ok(());
        };
        state.task = Some(tokio::spawn(run_loom_registry_watch(
            hub,
            sink,
            watch_id,
            LoomRegistryReplayWindow {
                after_cursor,
                through_cursor: baseline_through_cursor,
            },
            publications,
            cancel_receiver,
        )));
        Ok(())
    }

    async fn loom_install_retry(
        &self,
        request_id: RequestId,
        job_id: String,
    ) -> Result<(), SessionHubError> {
        let outcome = match self.hub.typed_agent_install_retry(job_id.clone()).await {
            Ok(haider_core::TypedAgentInstallRetryResult::Requeued(job)) => {
                haider_rpc::TypedAgentInstallRetryOutcomeWire::Requeued { job }
            }
            Ok(haider_core::TypedAgentInstallRetryResult::JobNotFound) => {
                haider_rpc::TypedAgentInstallRetryOutcomeWire::Rejected {
                    rejection: haider_rpc::TypedAgentInstallRetryRejectionWire::JobNotFound,
                }
            }
            Ok(haider_core::TypedAgentInstallRetryResult::StateNotRetryable { state }) => {
                haider_rpc::TypedAgentInstallRetryOutcomeWire::Rejected {
                    rejection: haider_rpc::TypedAgentInstallRetryRejectionWire::StateNotRetryable {
                        state,
                    },
                }
            }
            Ok(haider_core::TypedAgentInstallRetryResult::ContractNotCurrent) => {
                haider_rpc::TypedAgentInstallRetryOutcomeWire::Rejected {
                    rejection: haider_rpc::TypedAgentInstallRetryRejectionWire::ContractNotCurrent,
                }
            }
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomInstallRetry {
                receipt: haider_rpc::TypedAgentInstallRetryReceiptWire { job_id, outcome },
            },
        })
    }

    async fn loom_install_watch(
        &self,
        request_id: RequestId,
        job_id: String,
        after_cursor: u64,
    ) -> Result<(), SessionHubError> {
        let outcome = match self
            .hub
            .typed_agent_install_watch(job_id.clone(), after_cursor)
            .await
        {
            Ok(haider_core::TypedAgentInstallWatchResult::Watching(page)) => {
                haider_rpc::TypedAgentInstallWatchOutcomeWire::Watching {
                    requested_after_cursor: page.requested_after_cursor,
                    replay_through_cursor: page.replay_through_cursor,
                    next_cursor: page.next_cursor,
                    events: page.events,
                }
            }
            Ok(haider_core::TypedAgentInstallWatchResult::JobNotFound) => {
                haider_rpc::TypedAgentInstallWatchOutcomeWire::Rejected {
                    rejection: haider_rpc::TypedAgentInstallWatchRejectionWire::JobNotFound,
                }
            }
            Ok(haider_core::TypedAgentInstallWatchResult::CursorAhead { requested, head }) => {
                haider_rpc::TypedAgentInstallWatchOutcomeWire::Rejected {
                    rejection: haider_rpc::TypedAgentInstallWatchRejectionWire::CursorAhead {
                        requested,
                        head,
                    },
                }
            }
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomInstallWatch {
                receipt: haider_rpc::TypedAgentInstallWatchReceiptWire { job_id, outcome },
            },
        })
    }

    async fn workflow_graph_state(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        graph_id: Option<haider_protocol::ids::GraphId>,
    ) -> Result<(), SessionHubError> {
        let state = match self.hub.workflow_graph_state(&session_id, graph_id).await {
            Ok(state) => state,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::WorkflowGraphState { state },
        })
    }

    async fn workflow_graph_watch(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        after_cursor: u64,
        limit: u32,
    ) -> Result<(), SessionHubError> {
        let page = match self
            .hub
            .workflow_graph_watch(&session_id, after_cursor, limit)
            .await
        {
            Ok(page) => page,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::WorkflowGraphWatch { page },
        })
    }

    async fn loom_author_draft(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        kind: haider_protocol::loom::LoomAuthorKind,
        prose: String,
    ) -> Result<(), SessionHubError> {
        if let Err(error) = crate::loom_author::validate_prose(&prose) {
            return self.respond_error(
                request_id,
                error.code.as_str(),
                &error.message,
                error.retryable,
                None,
            );
        }
        let metadata = match self.hub.session_metadata(&session_id).await {
            Ok(Some(metadata)) => metadata,
            Ok(None) => {
                return self.respond_error(
                    request_id,
                    ErrorCode::SessionNotFound.as_str(),
                    "AI Loom drafting session was not found",
                    false,
                    None,
                );
            }
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        let agent_types = match self.hub.inner.store.loom_agent_types().await {
            Ok(records) => records,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        let authoring_id = random_id("loom-author")?;
        let provider = match self.hub.loom_author_provider() {
            Ok(provider) => provider,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    ErrorCode::Internal.as_str(),
                    &error.to_string(),
                    true,
                    None,
                );
            }
        };
        {
            let mut sessions = lock(&self.loom_author_sessions)?;
            if sessions.len() >= crate::loom_author::LOOM_AUTHOR_SESSION_MAX {
                let oldest = sessions
                    .iter()
                    .filter(|(_, session)| !session.confirming)
                    .min_by_key(|(_, session)| session.updated_at)
                    .map(|(id, _)| id.clone());
                if let Some(oldest) = oldest {
                    sessions.remove(&oldest);
                }
            }
            if sessions.len() >= crate::loom_author::LOOM_AUTHOR_SESSION_MAX {
                return self.respond_error(
                    request_id,
                    ErrorCode::Busy.as_str(),
                    "too many Loom drafts are already in progress on this connection",
                    true,
                    None,
                );
            }
            sessions.insert(
                authoring_id.clone(),
                crate::loom_author::LoomAuthorSession::pending(kind),
            );
        }
        let sink = Arc::clone(&self.sink);
        let sessions = Arc::clone(&self.loom_author_sessions);
        let hub = self.hub.clone();
        let correlation_session_id = session_id.clone();
        let mut cancel = self.identity_lease.loom_author_cancel.subscribe();
        let task = tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                closed = cancel.wait_for(|closed| *closed) => {
                    let _ = closed;
                    if let Ok(mut sessions) = sessions.lock() {
                        sessions.remove(&authoring_id);
                    }
                    return;
                }
                result = tokio::time::timeout(
                    Duration::from_secs(180),
                    crate::loom_author::draft_from_prose(
                        authoring_id.clone(),
                        kind,
                        &prose,
                        &agent_types,
                        &metadata,
                        provider.as_ref(),
                        begin_loom_provider_request(
                            &hub,
                            &correlation_session_id,
                            &authoring_id,
                        ),
                    ),
                ) => match result {
                    Ok(result) => result,
                    Err(_) => Err(HaiderError::new(
                        ErrorCode::Busy,
                        "AI Loom drafting timed out",
                        true,
                    )),
                },
            };
            let frame = match result {
                Ok(draft) => match sessions.lock() {
                    Ok(mut sessions) => {
                        // `close` publishes cancellation before it locks and
                        // clears this map. Recheck while holding the map lock
                        // so a provider result can never recreate connection-
                        // owned state after teardown won the race.
                        if *cancel.borrow() {
                            sessions.remove(&authoring_id);
                            return;
                        }
                        sessions.insert(
                            draft.authoring_id.clone(),
                            crate::loom_author::LoomAuthorSession::from_draft(&draft),
                        );
                        WireFrame::Response {
                            request_id,
                            body: ResponseBody::LoomAuthorDraft { draft },
                        }
                    }
                    Err(_) => WireFrame::Response {
                        request_id,
                        body: ResponseBody::Error {
                            code: ErrorCode::Internal.as_str().to_owned(),
                            message: "Loom authoring session registry is unavailable".to_owned(),
                            retryable: true,
                            data: None,
                        },
                    },
                },
                Err(error) => {
                    if let Ok(mut sessions) = sessions.lock() {
                        sessions.remove(&authoring_id);
                    }
                    WireFrame::Response {
                        request_id,
                        body: ResponseBody::Error {
                            code: error.code.as_str().to_owned(),
                            message: error.message,
                            retryable: error.retryable,
                            data: None,
                        },
                    }
                }
            };
            if sink.try_send(frame).is_err() {
                sink.close_after_required_delivery_failure();
            }
        });
        // Dropping a Tokio join handle detaches the bounded task. It is
        // connection-cancelled above and intentionally not added to the
        // hub-wide actor registry, so a slow provider cannot delay shutdown.
        drop(task);
        Ok(())
    }

    async fn loom_author_revise(
        &self,
        request_id: RequestId,
        authoring_id: String,
        expected_revision: u64,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
    ) -> Result<(), SessionHubError> {
        if text.len() > haider_protocol::loom::LOOM_AUTHOR_TEXT_MAX_BYTES {
            return self.respond_error(
                request_id,
                ErrorCode::InvalidArgument.as_str(),
                "Loom authoring edit exceeds the 64 KiB limit",
                false,
                None,
            );
        }
        {
            let sessions = lock(&self.loom_author_sessions)?;
            let Some(session) = sessions.get(&authoring_id) else {
                return self.respond_error(
                    request_id,
                    ErrorCode::InvalidArgument.as_str(),
                    "Loom authoring session was not issued by this daemon",
                    false,
                    None,
                );
            };
            if session.kind != kind || session.revision != expected_revision || session.confirming {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "Loom authoring revision is stale or busy",
                    false,
                    None,
                );
            }
        }
        let agent_types = match self.hub.inner.store.loom_agent_types().await {
            Ok(records) => records,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        let revision = {
            let sessions = lock(&self.loom_author_sessions)?;
            let Some(session) = sessions.get(&authoring_id) else {
                return self.respond_error(
                    request_id,
                    ErrorCode::InvalidArgument.as_str(),
                    "Loom authoring session was not issued by this daemon",
                    false,
                    None,
                );
            };
            if session.kind != kind || session.revision != expected_revision || session.confirming {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "Loom authoring revision is stale or busy",
                    false,
                    None,
                );
            }
            let Some(revision) = session.revision.checked_add(1) else {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "Loom authoring revision space is exhausted",
                    false,
                    None,
                );
            };
            revision
        };
        let draft =
            crate::loom_author::revise(authoring_id.clone(), revision, kind, text, &agent_types);
        {
            let mut sessions = lock(&self.loom_author_sessions)?;
            let Some(session) = sessions.get_mut(&authoring_id) else {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "Loom authoring session disappeared",
                    false,
                    None,
                );
            };
            if session.revision != expected_revision || session.kind != kind || session.confirming {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "Loom authoring revision changed while validating",
                    false,
                    None,
                );
            }
            *session = crate::loom_author::LoomAuthorSession::from_draft(&draft);
        }
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomAuthorRevise { draft },
        })
    }

    async fn loom_author_confirm(
        &self,
        request_id: RequestId,
        authoring_id: String,
        expected_revision: u64,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<(), SessionHubError> {
        if text.len() > haider_protocol::loom::LOOM_AUTHOR_TEXT_MAX_BYTES {
            return self.respond_error(
                request_id,
                ErrorCode::InvalidArgument.as_str(),
                "Loom authoring confirmation exceeds the 64 KiB limit",
                false,
                None,
            );
        }
        {
            let sessions = lock(&self.loom_author_sessions)?;
            let Some(session) = sessions.get(&authoring_id) else {
                return self.respond_error(
                    request_id,
                    ErrorCode::InvalidArgument.as_str(),
                    "Loom authoring session was not issued by this daemon",
                    false,
                    None,
                );
            };
            if session.kind != kind || session.revision != expected_revision {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "confirm requires the latest successfully revised Loom text",
                    false,
                    None,
                );
            }
            if let Some(confirmed) = session.confirmed.clone() {
                let exact_retry = session.text == text
                    || session
                        .confirmed_input_text
                        .as_ref()
                        .is_some_and(|submitted| submitted == &text);
                if exact_retry {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::LoomAuthorConfirm {
                            confirmed: Some(confirmed),
                            errors: Vec::new(),
                        },
                    });
                }
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "a confirmed revision is immutable; revise to create a new revision",
                    false,
                    None,
                );
            }
            if session.text != text {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "confirm requires the latest successfully revised Loom text",
                    false,
                    None,
                );
            }
            if session.confirming {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "Loom authoring confirmation is already in progress",
                    true,
                    None,
                );
            }
        }
        let agent_types = match self.hub.inner.store.loom_agent_types().await {
            Ok(records) => records,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    error.code.as_str(),
                    &error.message,
                    error.retryable,
                    None,
                );
            }
        };
        let validated = match crate::loom_author::validate(&text, kind, &agent_types) {
            Ok(validated) => validated,
            Err(errors) => {
                return self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::LoomAuthorConfirm {
                        confirmed: None,
                        errors,
                    },
                });
            }
        };
        {
            let mut sessions = lock(&self.loom_author_sessions)?;
            let Some(session) = sessions.get_mut(&authoring_id) else {
                return self.respond_error(
                    request_id,
                    ErrorCode::InvalidArgument.as_str(),
                    "Loom authoring session was not issued by this daemon",
                    false,
                    None,
                );
            };
            if session.kind != kind || session.revision != expected_revision {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "confirm requires the latest successfully revised Loom text",
                    false,
                    None,
                );
            }
            if let Some(confirmed) = session.confirmed.clone() {
                let exact_retry = session.text == text
                    || session
                        .confirmed_input_text
                        .as_ref()
                        .is_some_and(|submitted| submitted == &text);
                if exact_retry {
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::LoomAuthorConfirm {
                            confirmed: Some(confirmed),
                            errors: Vec::new(),
                        },
                    });
                }
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "a confirmed revision is immutable; revise to create a new revision",
                    false,
                    None,
                );
            }
            if session.text != text || !session.valid {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "confirm requires the latest successfully revised Loom text",
                    false,
                    None,
                );
            }
            if session.confirming {
                return self.respond_error(
                    request_id,
                    ErrorCode::RevisionConflict.as_str(),
                    "Loom authoring confirmation is already in progress",
                    true,
                    None,
                );
            }
            session.confirming = true;
        }
        let confirmed = match validated {
            haider_protocol::loom::ValidatedLoomAuthorSpec::AgentType {
                record,
                canonical_text,
            } => match self
                .hub
                .loom_register_agent_type_cas(*record, expected.clone())
                .await
            {
                Ok(haider_core::LoomRegistryMutation::Applied { value, .. }) => {
                    haider_protocol::loom::LoomAuthorConfirmed {
                        authoring_id: authoring_id.clone(),
                        kind,
                        canonical_text,
                        execution_digest: value.registration.digest.clone(),
                        registration: value.registration,
                        install_job_id: value.install_job_id,
                    }
                }
                Ok(haider_core::LoomRegistryMutation::Conflict(conflict)) => {
                    if let Ok(mut sessions) = self.loom_author_sessions.lock()
                        && let Some(session) = sessions.get_mut(&authoring_id)
                    {
                        session.confirming = false;
                    }
                    return self.respond_loom_revision_conflict(request_id, conflict);
                }
                Err(error) => {
                    if let Ok(mut sessions) = self.loom_author_sessions.lock()
                        && let Some(session) = sessions.get_mut(&authoring_id)
                    {
                        session.confirming = false;
                    }
                    return self.respond_error(
                        request_id,
                        error.code.as_str(),
                        &error.message,
                        error.retryable,
                        None,
                    );
                }
            },
            haider_protocol::loom::ValidatedLoomAuthorSpec::Workflow {
                source,
                canonical_text,
            } => {
                let registration = match self.hub.loom_register_workflow_cas(source, expected).await
                {
                    Ok(haider_core::LoomRegistryMutation::Applied { value, .. }) => value,
                    Ok(haider_core::LoomRegistryMutation::Conflict(conflict)) => {
                        if let Ok(mut sessions) = self.loom_author_sessions.lock()
                            && let Some(session) = sessions.get_mut(&authoring_id)
                        {
                            session.confirming = false;
                        }
                        return self.respond_loom_revision_conflict(request_id, conflict);
                    }
                    Err(error) => {
                        if let Ok(mut sessions) = self.loom_author_sessions.lock()
                            && let Some(session) = sessions.get_mut(&authoring_id)
                        {
                            session.confirming = false;
                        }
                        return self.respond_error(
                            request_id,
                            error.code.as_str(),
                            &error.message,
                            error.retryable,
                            None,
                        );
                    }
                };
                let registered = match self
                    .hub
                    .inner
                    .store
                    .loom_workflow_registered_revision(
                        registration.id.clone(),
                        registration.rev,
                        registration.digest.clone(),
                    )
                    .await
                {
                    Ok(Some(workflow)) => workflow,
                    Ok(None) => {
                        if let Ok(mut sessions) = self.loom_author_sessions.lock()
                            && let Some(session) = sessions.get_mut(&authoring_id)
                        {
                            session.confirming = false;
                        }
                        return self.respond_error(
                            request_id,
                            ErrorCode::StoreCorrupt.as_str(),
                            "registered Loom workflow revision is missing",
                            false,
                            None,
                        );
                    }
                    Err(error) => {
                        if let Ok(mut sessions) = self.loom_author_sessions.lock()
                            && let Some(session) = sessions.get_mut(&authoring_id)
                        {
                            session.confirming = false;
                        }
                        return self.respond_error(
                            request_id,
                            error.code.as_str(),
                            &error.message,
                            error.retryable,
                            None,
                        );
                    }
                };
                let execution_digest =
                    haider_protocol::graph::graph_template_digest(&registered.template);
                haider_protocol::loom::LoomAuthorConfirmed {
                    authoring_id: authoring_id.clone(),
                    kind,
                    canonical_text,
                    registration,
                    execution_digest,
                    install_job_id: None,
                }
            }
        };
        {
            let mut sessions = lock(&self.loom_author_sessions)?;
            let session = sessions.get_mut(&authoring_id).ok_or_else(|| {
                SessionHubError::Task("confirmed Loom authoring session disappeared".to_owned())
            })?;
            session.confirming = false;
            session.confirmed_input_text = Some(text);
            session.text.clone_from(&confirmed.canonical_text);
            session.valid = true;
            session.updated_at = std::time::Instant::now();
            session.confirmed = Some(confirmed.clone());
        }
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::LoomAuthorConfirm {
                confirmed: Some(confirmed),
                errors: Vec::new(),
            },
        })
    }

    /// B1 — workflow registration from pipe source; the daemon compiles.
    async fn loom_register_workflow(
        &self,
        request_id: RequestId,
        source: String,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<(), SessionHubError> {
        match self.hub.loom_register_workflow_cas(source, expected).await {
            Ok(haider_core::LoomRegistryMutation::Applied { value, .. }) => {
                self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::LoomRegistered {
                        registration: value,
                        install_job_id: None,
                    },
                })
            }
            Ok(haider_core::LoomRegistryMutation::Conflict(conflict)) => {
                self.respond_loom_revision_conflict(request_id, conflict)
            }
            Err(error) => self.respond_error(
                request_id,
                error.code.as_str(),
                &error.message,
                error.retryable,
                None,
            ),
        }
    }

    async fn graph_inspect(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<(), SessionHubError> {
        let inspected = match self.hub.graph_inspect(&session_id, cursor, limit).await {
            Ok(inspected) => inspected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphInspect {
                snapshot: inspected.snapshot,
                next_cursor: inspected.next_cursor,
            },
        })
    }

    async fn graph_pin(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        template: String,
        expected_digest: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph pin needs a command id",
                false,
                None,
            );
        }
        let mut request_value = serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "template": &template,
        });
        if let Some(expected_digest) = expected_digest.as_ref() {
            request_value["expected_digest"] = serde_json::Value::String(expected_digest.clone());
        }
        let request_json = serde_json::to_string(&request_value)
            .map_err(|error| SessionHubError::Task(format!("cannot encode graph pin: {error}")))?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_pin_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(pinned)) => return self.respond_graph_pinned(request_id, pinned),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        // Lost-response replay above is deliberately unfenced. A genuinely
        // new mutation still requires this connection's live control lease.
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph pin requires a control attachment to this session",
                false,
                None,
            );
        }
        // Serialize the idle check and durable selection against turn
        // admission. Once a turn is accepted its nonterminal RunState is
        // visible before this lock can be acquired, so a provider request can
        // never gain authority under one workflow and be switched to another
        // by the native RPC while its response is in flight.
        let _workflow_selection = self.hub.lock_workflow_selection(&session_id).await;
        match self.hub.session_has_nonterminal_runs(&session_id).await {
            Ok(false) => {}
            Ok(true) => {
                return self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_BUSY,
                    "graph pin requires an idle session; retry after the active turn completes",
                    true,
                    None,
                );
            }
            Err(error) => return self.respond_graph_error(request_id, error),
        }
        let command = GraphPinCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            graph_id: GraphId::new(random_id("graph")?),
            template,
            device_id: self.hub.inner.device_id.clone(),
        };
        let result = match expected_digest {
            Some(expected_digest) => {
                self.hub
                    .pin_graph_matching_digest(command, expected_digest)
                    .await
            }
            None => self.hub.pin_graph(command).await,
        };
        let pinned = match result {
            Ok(GraphPinOutcome::Committed { pinned, .. })
            | Ok(GraphPinOutcome::IdempotentReplay { pinned }) => pinned,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_pinned(request_id, pinned)
    }

    fn respond_graph_pinned(
        &self,
        request_id: RequestId,
        pinned: PinnedGraph,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphPin {
                session_id: pinned.session_id,
                graph_id: pinned.graph_id,
                template: pinned.template,
                digest: pinned.digest,
                pinned_seq: pinned.pinned_seq,
                opened_seq: pinned.opened_seq,
                worker_generation: pinned.worker_generation,
            },
        })
    }

    async fn graph_run_set_open(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        plan_item_id: ItemId,
        plan_event_seq: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() || plan_event_seq == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph run-set open needs a command id and nonzero Plan event sequence",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "plan_item_id": &plan_item_id,
            "plan_event_seq": plan_event_seq,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode graph run-set open: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_run_set_open_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(opened)) => return self.respond_graph_run_set_opened(request_id, opened),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph run-set open requires a control attachment to this session",
                false,
                None,
            );
        }
        let command = GraphRunSetOpenCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            plan_item_id,
            plan_event_seq,
            device_id: self.hub.inner.device_id.clone(),
        };
        let opened = match self.hub.open_graph_run_set(command).await {
            Ok(GraphRunSetOpenOutcome::Committed { opened, .. })
            | Ok(GraphRunSetOpenOutcome::IdempotentReplay { opened }) => opened,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_run_set_opened(request_id, opened)
    }

    fn respond_graph_run_set_opened(
        &self,
        request_id: RequestId,
        opened: OpenedGraphRunSet,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphRunSetOpen {
                session_id: opened.session_id,
                run_set_id: opened.run_set_id,
                root_graph_id: opened.root_graph_id,
                plan_item_id: opened.plan_item_id,
                plan_event_seq: opened.plan_event_seq,
                template: opened.template,
                digest: opened.digest,
                run_set_opened_seq: opened.run_set_opened_seq,
                through_seq: opened.through_seq,
                children: opened
                    .children
                    .into_iter()
                    .map(|child| TodoGraphOpenedWire {
                        todo_id: child.todo_id,
                        depends_on_todo_id: child.depends_on_todo_id,
                        child_graph_id: child.child_graph_id,
                        attached_seq: child.attached_seq,
                        pinned_seq: child.pinned_seq,
                        opened_seq: child.opened_seq,
                    })
                    .collect(),
                worker_generation: opened.worker_generation,
            },
        })
    }

    // The wire request's seven independent coordinates stay explicit here.
    #[allow(clippy::too_many_arguments)]
    async fn graph_switch(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        old_graph_id: GraphId,
        template: String,
        expected_digest: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph switch needs a command id",
                false,
                None,
            );
        }
        let mut request_value = serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "old_graph_id": &old_graph_id,
            "template": &template,
        });
        if let Some(expected_digest) = expected_digest.as_ref() {
            request_value["expected_digest"] = serde_json::Value::String(expected_digest.clone());
        }
        let request_json = serde_json::to_string(&request_value).map_err(|error| {
            SessionHubError::Task(format!("cannot encode graph switch: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_switch_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(switched)) => return self.respond_graph_switched(request_id, switched),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph switch requires a control attachment to this session",
                false,
                None,
            );
        }
        let _workflow_selection = self.hub.lock_workflow_selection(&session_id).await;
        match self.hub.session_has_nonterminal_runs(&session_id).await {
            Ok(false) => {}
            Ok(true) => {
                return self.respond_error(
                    request_id,
                    haider_rpc::ERROR_CODE_BUSY,
                    "graph switch requires an idle session; retry after the active turn completes",
                    true,
                    None,
                );
            }
            Err(error) => return self.respond_graph_error(request_id, error),
        }
        let command = GraphSwitchCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            old_graph_id,
            new_graph_id: GraphId::new(random_id("graph")?),
            template,
            template_spec: None,
            device_id: self.hub.inner.device_id.clone(),
        };
        let result = match expected_digest {
            Some(expected_digest) => {
                self.hub
                    .switch_graph_matching_digest(command, expected_digest)
                    .await
            }
            None => self.hub.switch_graph(command).await,
        };
        let switched = match result {
            Ok(GraphSwitchOutcome::Committed { switched, .. })
            | Ok(GraphSwitchOutcome::IdempotentReplay { switched }) => switched,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_switched(request_id, switched)
    }

    fn respond_graph_switched(
        &self,
        request_id: RequestId,
        switched: SwitchedGraph,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphSwitch {
                session_id: switched.session_id,
                old_graph_id: switched.old_graph_id,
                new_graph_id: switched.new_graph_id,
                template: switched.template,
                digest: switched.digest,
                superseded_seq: switched.superseded_seq,
                pinned_seq: switched.pinned_seq,
                opened_seq: switched.opened_seq,
                worker_generation: switched.worker_generation,
            },
        })
    }

    async fn graph_abandon(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        why: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "graph abandon needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "why": &why,
        }))
        .map_err(|error| SessionHubError::Task(format!("cannot encode graph abandon: {error}")))?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .graph_abandon_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(abandoned)) => return self.respond_graph_abandoned(request_id, abandoned),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        // Same receipt-first ordering as graph.pin: recovery is unfenced;
        // only a fresh mutation must still hold the session control lease.
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "graph abandon requires a control attachment to this session",
                false,
                None,
            );
        }
        let command = GraphAbandonCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            why,
            device_id: self.hub.inner.device_id.clone(),
        };
        let abandoned = match self.hub.abandon_graph(command).await {
            Ok(GraphAbandonOutcome::Committed { abandoned, .. })
            | Ok(GraphAbandonOutcome::IdempotentReplay { abandoned }) => abandoned,
            Err(SessionHubError::Store(error)) => {
                return self.respond_graph_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_graph_abandoned(request_id, abandoned)
    }

    fn respond_graph_abandoned(
        &self,
        request_id: RequestId,
        abandoned: AbandonedGraph,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::GraphAbandon {
                session_id: abandoned.session_id,
                graph_id: abandoned.graph_id,
                abandoned_seq: abandoned.abandoned_seq,
                worker_generation: abandoned.worker_generation,
            },
        })
    }

    fn respond_graph_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        if error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str)
            == Some("cursor_ahead")
        {
            let requested = error
                .details
                .as_ref()
                .and_then(|details| details.get("requested"))
                .and_then(serde_json::Value::as_u64);
            let head = error
                .details
                .as_ref()
                .and_then(|details| details.get("head"))
                .and_then(serde_json::Value::as_u64);
            if let (Some(requested), Some(head)) = (requested, head) {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CURSOR_AHEAD,
                    &error.message,
                    error.retryable,
                    Some(ErrorData::CursorAhead { requested, head }),
                );
            }
        }
        if error.code == ErrorCode::RevisionConflict {
            let expected_digest = error
                .details
                .as_ref()
                .and_then(|details| details.get("expected_digest"))
                .and_then(serde_json::Value::as_str);
            let current_digest = error
                .details
                .as_ref()
                .and_then(|details| details.get("current_digest"))
                .and_then(serde_json::Value::as_str);
            let current_revision = error
                .details
                .as_ref()
                .and_then(|details| details.get("current_revision"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|revision| u32::try_from(revision).ok());
            if let (Some(expected_digest), Some(current_digest), Some(current_revision)) =
                (expected_digest, current_digest, current_revision)
            {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_REVISION_CONFLICT,
                    &error.message,
                    error.retryable,
                    Some(ErrorData::WorkflowRevisionConflict {
                        expected_digest: expected_digest.to_owned(),
                        current_digest: current_digest.to_owned(),
                        current_revision,
                    }),
                );
            }
        }
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::GraphAlreadyActive => ERROR_CODE_GRAPH_ALREADY_ACTIVE,
            ErrorCode::GraphNotActive => ERROR_CODE_GRAPH_NOT_ACTIVE,
            ErrorCode::GraphWrongNode => ERROR_CODE_GRAPH_WRONG_NODE,
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(request_id, code, &error.message, error.retryable, None)
    }

    /// `session.select_effort` — receipted live-session effort selection
    /// (G3), the exact `session.select_model` law set: receipt replay
    /// precedes validation, the ONE authority in `crate::model_select`
    /// validates against the CURRENT pair's declared ladder, the store owns
    /// durability, and the next logical turn re-reads the committed
    /// metadata (R6 re-resolution).
    async fn session_select_effort(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        effort: Option<String>,
        confirm_new_epoch: bool,
    ) -> Result<(), SessionHubError> {
        let effort = effort
            .map(|effort| effort.trim().to_owned())
            .filter(|effort| !effort.is_empty());
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "effort selection needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "effort": &effort,
            "confirm_new_epoch": confirm_new_epoch,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode effort-selection coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt replay precedes validation so a lost response remains
        // recoverable even after registry or inventory changes.
        match self
            .hub
            .session_select_effort_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_effort_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let Some(current) = (match self.hub.session_metadata(&session_id).await {
            Ok(metadata) => metadata,
            Err(error) => return self.respond_turn_error(request_id, error),
        }) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "effort selection requires a live session with typed metadata",
                false,
                None,
            );
        };
        let authority = self.tuning_authority()?;
        if let Err(refusal) =
            authority.validate_effort(&current.provider, &current.model, effort.as_deref())
        {
            return self.respond_tuning_refusal(request_id, &refusal);
        }
        let changed_fields = if current.effort != effort {
            vec!["effort/thinking".to_owned()]
        } else {
            Vec::new()
        };
        let current_scope =
            crate::cache_policy::latest_main_cache_scope(&self.hub.inner.store, &session_id)
                .await?;
        if let Some(warning) = crate::cache_policy::assess_cache_change(
            &current,
            current_scope.as_ref(),
            &current.provider,
            &current.model,
            current_scope
                .as_ref()
                .map(|scope| scope.auth_scope.as_str()),
            changed_fields,
            true,
        ) && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
        {
            return self.respond_cache_confirmation_required(request_id, &warning);
        }

        let command = haider_core::SessionSelectEffortCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            effort,
            event_id: EventId::new(random_id("effort-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.select_session_effort(command).await {
            Ok(SessionSelectEffortOutcome::Committed { selected, .. })
            | Ok(SessionSelectEffortOutcome::IdempotentReplay { selected }) => selected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_effort_selected(request_id, selected)
    }

    /// `session.select_agent_type` — the receipted W-flow inline-identity
    /// binding: receipt replay precedes validation, the STORE validates the
    /// id against the Loom registry inside the select transaction, and the
    /// bound job rides the volatile prompt tail — no cache-epoch assessment
    /// because no cache-relevant bytes move.
    async fn session_select_agent_type(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        agent_type: Option<String>,
    ) -> Result<(), SessionHubError> {
        let agent_type = agent_type
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty());
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "agent-type selection needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "agent_type": &agent_type,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode agent-type-selection coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        match self
            .hub
            .session_select_agent_type_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_agent_type_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        if (match self.hub.session_metadata(&session_id).await {
            Ok(metadata) => metadata,
            Err(error) => return self.respond_turn_error(request_id, error),
        })
        .is_none()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "agent-type selection requires a live session with typed metadata",
                false,
                None,
            );
        }

        let command = haider_core::SessionSelectAgentTypeCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            agent_type,
            event_id: EventId::new(random_id("agent-type-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.select_session_agent_type(command).await {
            Ok(haider_core::SessionSelectAgentTypeOutcome::Committed { selected, .. })
            | Ok(haider_core::SessionSelectAgentTypeOutcome::IdempotentReplay { selected }) => {
                selected
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_agent_type_selected(request_id, selected)
    }

    fn respond_agent_type_selected(
        &self,
        request_id: RequestId,
        selected: haider_core::SelectedAgentType,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSelectAgentType {
                session_id: selected.session_id,
                agent_type: selected.agent_type,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    /// `session.select_fast` — the receipted fast-mode toggle (G3), same
    /// law set as `session.select_effort`.
    async fn session_select_fast(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        enabled: bool,
        confirm_new_epoch: bool,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "fast-mode selection needs a command id",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "enabled": enabled,
            "confirm_new_epoch": confirm_new_epoch,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!(
                "cannot encode fast-mode-selection coordinates: {error}"
            ))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        match self
            .hub
            .session_select_fast_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(selected)) => return self.respond_fast_selected(request_id, selected),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }

        let Some(current) = (match self.hub.session_metadata(&session_id).await {
            Ok(metadata) => metadata,
            Err(error) => return self.respond_turn_error(request_id, error),
        }) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "fast-mode selection requires a live session with typed metadata",
                false,
                None,
            );
        };
        let authority = self.tuning_authority()?;
        if let Err(refusal) = authority.validate_fast(&current.provider, &current.model, enabled) {
            return self.respond_tuning_refusal(request_id, &refusal);
        }
        let changed_fields = if current.fast != enabled {
            vec!["fast/speed".to_owned()]
        } else {
            Vec::new()
        };
        let current_scope =
            crate::cache_policy::latest_main_cache_scope(&self.hub.inner.store, &session_id)
                .await?;
        if let Some(warning) = crate::cache_policy::assess_cache_change(
            &current,
            current_scope.as_ref(),
            &current.provider,
            &current.model,
            current_scope
                .as_ref()
                .map(|scope| scope.auth_scope.as_str()),
            changed_fields,
            true,
        ) && crate::cache_policy::blocks_change(&warning, confirm_new_epoch)
        {
            return self.respond_cache_confirmation_required(request_id, &warning);
        }

        let command = haider_core::SessionSelectFastCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            enabled,
            event_id: EventId::new(random_id("fast-selected")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.select_session_fast(command).await {
            Ok(SessionSelectFastOutcome::Committed { selected, .. })
            | Ok(SessionSelectFastOutcome::IdempotentReplay { selected }) => selected,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_fast_selected(request_id, selected)
    }

    /// The one selection authority, loaded with the same summaries
    /// `session.select_model` consults.
    fn tuning_authority(
        &self,
    ) -> Result<crate::model_select::ModelSelectionAuthority, SessionHubError> {
        let summaries = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .map(|view| view.providers)
            .unwrap_or_default();
        Ok(crate::model_select::ModelSelectionAuthority::new(
            self.hub.creatable_providers()?,
            summaries,
        ))
    }

    fn respond_tuning_refusal(
        &self,
        request_id: RequestId,
        refusal: &crate::model_select::TuningRefusal,
    ) -> Result<(), SessionHubError> {
        use crate::model_select::TuningRefusal;
        let (code, data) = match refusal {
            TuningRefusal::EffortUnsupported {
                provider,
                model,
                effort,
                supported,
            } => (
                haider_rpc::ERROR_CODE_EFFORT_UNSUPPORTED,
                Some(ErrorData::EffortUnsupported {
                    provider: provider.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                    supported: supported.clone(),
                }),
            ),
            TuningRefusal::FastUnsupported { provider, model } => (
                haider_rpc::ERROR_CODE_FAST_UNSUPPORTED,
                Some(ErrorData::FastUnsupported {
                    provider: provider.clone(),
                    model: model.clone(),
                }),
            ),
        };
        self.respond_error(request_id, code, &refusal.message(), false, data)
    }

    fn respond_cache_confirmation_required(
        &self,
        request_id: RequestId,
        warning: &crate::cache_policy::CacheChangeWarning,
    ) -> Result<(), SessionHubError> {
        let policy = match warning.policy {
            haider_protocol::cache::CachePolicyMode::Economy => "economy",
            haider_protocol::cache::CachePolicyMode::Balanced => "balanced",
            haider_protocol::cache::CachePolicyMode::Mobility => "mobility",
        };
        self.respond_error(
            request_id,
            haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED,
            &warning.message(),
            false,
            Some(ErrorData::CacheEpochConfirmationRequired {
                changed_fields: warning.changed_fields.clone(),
                invalidated_stable_tokens: warning.invalidated_stable_tokens,
                rewarm_cost_microusd: warning.rewarm_cost_microusd,
                rewarm_api_equivalent_cost_microusd: warning.rewarm_api_equivalent_cost_microusd,
                rewarm_base_input_equivalent_tokens: warning.rewarm_base_input_equivalent_tokens,
                policy: policy.to_owned(),
            }),
        )
    }

    fn respond_effort_selected(
        &self,
        request_id: RequestId,
        selected: SelectedEffort,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSelectEffort {
                session_id: selected.session_id,
                effort: selected.effort,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    fn respond_fast_selected(
        &self,
        request_id: RequestId,
        selected: SelectedFast,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSelectFast {
                session_id: selected.session_id,
                enabled: selected.enabled,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }

    async fn agent_message(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        agent: haider_protocol::ids::AgentId,
        text: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "agent-message command id must not be empty",
                false,
                None,
            );
        }
        if worker_generation != self.hub.worker_generation() {
            return self.respond_error(
                request_id,
                ERROR_CODE_STALE_GENERATION,
                "agent-message worker generation is stale",
                false,
                None,
            );
        }
        let message = match MessageSubagent::from_tool_args(serde_json::json!({
            "agent": agent,
            "message": text,
        })) {
            Ok(message) => message,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.to_string(),
                    false,
                    None,
                );
            }
        };
        let delegation = DelegationHandle::new(self.hub.clone());
        let parent_agent_id = match delegation.agent_for_session(&session_id).await {
            Ok(agent) => agent,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        let receipt = match delegation
            .message(
                MessageCoordinates {
                    parent_session_id: session_id,
                    parent_agent_id,
                    command_id: command_id.0,
                },
                message,
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::AgentMessage { receipt },
        })
    }

    /// Cancels an owned direct child through the same durable `turn.cancel`
    /// transition used by ordinary run control. The target transition is
    /// claimed before the descendant sweep so reusing one command id with
    /// different semantics cannot mutate an unrelated subtree first.
    async fn agent_cancel(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        agent: AgentId,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "agent-cancel command id must not be empty",
                false,
                None,
            );
        }
        let record = match self.hub.delegation(agent.clone()).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CAPABILITY_DENIED,
                    "agent cancellation requires an owned direct child",
                    false,
                    None,
                );
            }
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        let parent_agent_id = match self
            .hub
            .delegation_for_child_session(session_id.clone())
            .await
        {
            Ok(parent) => parent.map(|parent| parent.agent_id),
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        if record.parent_session_id != session_id || record.parent_agent_id != parent_agent_id {
            return self.respond_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "agent cancellation requires an owned direct child",
                false,
                None,
            );
        }

        let mut pending = vec![record.clone()];
        let mut subtree = Vec::new();
        while let Some(current) = pending.pop() {
            match self
                .hub
                .delegations_for_parent_run(
                    current.child_session_id.clone(),
                    current.child_run_id.clone(),
                )
                .await
            {
                Ok(children) => pending.extend(children),
                Err(error) => return self.respond_turn_error(request_id, error),
            }
            subtree.push(current);
        }

        let target_command = match agent_cancel_command(
            &record,
            command_id.as_str(),
            "manual",
            worker_generation,
            self.hub.device_id(),
        ) {
            Ok(command) => command,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        let cancelled = match self.hub.cancel_internal_turn(target_command).await {
            Ok(cancelled) => cancelled,
            Err(error) => return self.respond_turn_error(request_id, error),
        };

        if cancelled.status == TurnCancellationStatus::Accepted {
            for descendant in subtree.into_iter().rev() {
                if descendant.agent_id == record.agent_id {
                    continue;
                }
                let descendant_command_id = format!(
                    "{}-ancestor-{}",
                    command_id.as_str(),
                    descendant.agent_id.as_str()
                );
                let command = match agent_cancel_command(
                    &descendant,
                    &descendant_command_id,
                    "manual_ancestor",
                    worker_generation,
                    self.hub.device_id(),
                ) {
                    Ok(command) => command,
                    Err(error) => return self.respond_turn_error(request_id, error),
                };
                if let Err(error) = self.hub.cancel_internal_turn(command).await {
                    tracing::warn!(
                        agent = %descendant.agent_id,
                        ?error,
                        "manual child cancellation descendant sweep will be reconciled by the cancelled parent"
                    );
                }
            }
        }

        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::AgentCancel {
                agent: record.agent_id,
                child_session_id: cancelled.session_id,
                child_run_id: cancelled.run_id,
                status: match cancelled.status {
                    TurnCancellationStatus::Accepted => CancelStatus::Accepted,
                    TurnCancellationStatus::AlreadyTerminal => CancelStatus::AlreadyTerminal,
                },
                terminal_seq: cancelled.terminal_seq,
            },
        })
    }

    async fn queue_list(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        let snapshot = match self.hub.queue_snapshot(session_id.clone()).await {
            Ok(snapshot) => snapshot,
            Err(SessionHubError::Store(error)) => {
                return self.respond_queue_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::QueueList {
                session_id,
                revision: snapshot.revision,
                rows: snapshot.rows,
            },
        })
    }

    async fn queue_remove(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        id: EventId,
        revision: u64,
    ) -> Result<(), SessionHubError> {
        let outcome = match self
            .hub
            .queue_remove(QueueRemoveCommand {
                session_id: session_id.clone(),
                id: id.clone(),
                revision,
                cancelling_event_id: EventId::new(random_id("queue-remove-cancelling")?),
                delta_event_id: EventId::new(random_id("queue-remove-delta")?),
                device_id: self.hub.inner.device_id.clone(),
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(SessionHubError::Store(error)) => {
                return self.respond_queue_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::QueueRemove {
                session_id,
                id,
                revision: outcome.revision,
            },
        })
    }

    async fn queue_promote_steer(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        id: EventId,
        revision: u64,
    ) -> Result<(), SessionHubError> {
        let (outcome, live_delivered) = match self
            .hub
            .queue_promote_steer(QueuePromoteCommand {
                session_id: session_id.clone(),
                id: id.clone(),
                revision,
                expected_active_run_id: None,
                cancelling_event_id: EventId::new(random_id("queue-promote-cancelling")?),
                delivery_event_id: EventId::new(random_id("queue-promote-delivery")?),
                delta_event_id: EventId::new(random_id("queue-promote-delta")?),
                device_id: self.hub.inner.device_id.clone(),
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(SessionHubError::Store(error)) => {
                return self.respond_queue_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        if !live_delivered
            && let Err(error) = self
                .hub
                .worker_manager()?
                .nudge(
                    session_id.clone(),
                    outcome.active_run_id,
                    outcome.delivery_seq,
                    outcome.text,
                )
                .await
        {
            return self.respond_queue_error(request_id, error);
        }
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::QueuePromoteSteer {
                session_id,
                id,
                revision: outcome.revision,
            },
        })
    }

    async fn turn_submit(
        &self,
        request_id: RequestId,
        input: TurnSubmitInput,
    ) -> Result<(), SessionHubError> {
        let TurnSubmitInput {
            command_id,
            session_id,
            worker_generation,
            branch_id,
            text,
            attachments,
            mode,
            trust_hooks,
            headless_spec,
        } = input;
        if command_id.as_str().is_empty() || text.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "turn command id and text must not be empty",
                false,
                None,
            );
        }
        let mut request_value = serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "branch_id": &branch_id,
            "text": &text,
            "attachments": &attachments,
            "mode": mode,
        });
        if trust_hooks {
            let Some(request) = request_value.as_object_mut() else {
                return Err(SessionHubError::Task(
                    "turn-submit coordinates did not encode as an object".into(),
                ));
            };
            request.insert("trust_hooks".into(), serde_json::Value::Bool(true));
        }
        if let Some(spec) = headless_spec.as_ref() {
            if let Some(budget) = spec.budget.request_budget
                && let Err(message) = budget.validate()
            {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                );
            }
            if spec.cwd.trim().is_empty()
                || spec.provider.trim().is_empty()
                || spec.model.trim().is_empty()
                || spec.max_output_tokens == 0
                || spec.budget.max_tokens == Some(0)
                || spec.budget.max_cost_microusd == Some(0)
                || spec.budget.max_time_ms == Some(0)
            {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "headless execution pins and present budget limits must be non-zero",
                    false,
                    None,
                );
            }
            if spec.trust_hooks != trust_hooks {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "headless hook-trust pin does not match the submitted policy",
                    false,
                    None,
                );
            }
            let Some(request) = request_value.as_object_mut() else {
                return Err(SessionHubError::Task(
                    "turn-submit coordinates did not encode as an object".into(),
                ));
            };
            request.insert(
                "headless".into(),
                serde_json::to_value(spec).map_err(|error| {
                    SessionHubError::Task(format!("cannot encode headless run spec: {error}"))
                })?,
            );
        }
        let request_json = serde_json::to_string(&request_value).map_err(|error| {
            SessionHubError::Task(format!("cannot encode turn-submit coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let first_turn_slug = auto_title_slug(&text);
        if !attachments.is_empty() {
            match self
                .hub
                .turn_accept_receipt(&command_id, &request_digest, &request_json)
                .await
            {
                Ok(Some(_)) => {
                    // Receipt lookup must remain ahead of immutable attachment
                    // validation, but it cannot bypass the fused transaction:
                    // pre-fusion first-turn receipts may still need their
                    // title repaired after the old acceptance->rename crash.
                    let replay_user_event_id = replay_title_user_event_id(&command_id);
                    let replay_command = TurnAcceptCommand {
                        command_id: command_id.0.clone(),
                        request_digest: request_digest.clone(),
                        request_json: request_json.clone(),
                        session_id: session_id.clone(),
                        worker_generation,
                        // These coordinates are unreachable after the durable
                        // receipt match, so fixed typed sentinels avoid making
                        // an idempotent replay depend on fresh entropy.
                        run_id: haider_protocol::ids::RunId::new("receipt-replay"),
                        agent_id: None,
                        branch_id: branch_id.clone(),
                        text: text.clone(),
                        attachments: attachments.clone(),
                        mode,
                        queued_event_id: EventId::new("receipt-replay-queued"),
                        // Unlike the other sentinels this ID is reachable:
                        // title repair derives its globally unique envelope
                        // ID from the user event. Bind it deterministically
                        // to the receipt key so different legacy commands do
                        // not collide while retries remain stable.
                        user_event_id: replay_user_event_id,
                        active_event_id: EventId::new("receipt-replay-active"),
                        device_id: self.hub.inner.device_id.clone(),
                    };
                    let accepted = match self
                        .hub
                        .accept_turn_with_auto_title(replay_command, Some(first_turn_slug.clone()))
                        .await
                    {
                        Ok(TurnAcceptOutcome::Committed { accepted, .. })
                        | Ok(TurnAcceptOutcome::IdempotentReplay { accepted }) => accepted,
                        Err(SessionHubError::Store(error)) => {
                            return self.respond_turn_error(request_id, error);
                        }
                        Err(error) => return Err(error),
                    };
                    if accepted.worker_generation == self.hub.inner.store.worker_generation() {
                        let handoff = match accepted.disposition {
                            TurnAdmissionDisposition::SteerPending => {
                                self.hub
                                    .submit_internal_nudge(accepted.clone(), text.clone())
                                    .await
                            }
                            TurnAdmissionDisposition::SubturnPending => {
                                self.hub
                                    .submit_internal_subturn(accepted.clone(), text.clone())
                                    .await
                            }
                            TurnAdmissionDisposition::Started
                            | TurnAdmissionDisposition::Queued => {
                                self.hub.worker_manager()?.submit(accepted.clone()).await
                            }
                        };
                        if let Err(error) = handoff {
                            return self.respond_turn_error(request_id, error);
                        }
                    }
                    return self.respond_turn_accepted(
                        request_id,
                        accepted,
                        headless_spec.is_some(),
                    );
                }
                Ok(None) => {}
                Err(SessionHubError::Store(error)) => {
                    return self.respond_turn_error(request_id, error);
                }
                Err(error) => return Err(error),
            }
        }
        let pdf_delivery = if attachments.iter().any(|attachment| {
            matches!(
                attachment,
                haider_protocol::tool::AttachmentBlock::Pdf { .. }
            )
        }) {
            let metadata = match self.hub.session_metadata(&session_id).await {
                Ok(Some(metadata)) => metadata,
                Ok(None) => {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "PDF attachment admission requires typed session metadata",
                        false,
                        None,
                    );
                }
                Err(error) => return self.respond_turn_error(request_id, error),
            };
            pdf_delivery_for_provider(&metadata.provider)
        } else {
            haider_protocol::tool::PdfDeliveryMode::ExtractedText
        };
        let attachments = match validate_turn_attachments(
            &self.hub.inner.store,
            &attachments,
            pdf_delivery,
        )
        .await
        {
            Ok(attachments) => attachments,
            Err(failure) => {
                return self.respond_error(
                    request_id,
                    failure.code,
                    &failure.message,
                    false,
                    failure.data,
                );
            }
        };
        let delivery_text = text.clone();
        let command = TurnAcceptCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id: session_id.clone(),
            worker_generation,
            run_id: haider_protocol::ids::RunId::new(random_id("run")?),
            // The acceptance transaction resolves a delegated child session
            // to its durable agent scope under the same SQLite writer order.
            agent_id: None,
            branch_id,
            text,
            attachments,
            mode,
            queued_event_id: EventId::new(random_id("turn-queued")?),
            user_event_id: EventId::new(random_id("turn-user")?),
            active_event_id: EventId::new(random_id("session-active")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let accepted = match self
            .hub
            .accept_turn_with_auto_title(command, Some(first_turn_slug))
            .await
        {
            Ok(TurnAcceptOutcome::Committed { accepted, .. })
            | Ok(TurnAcceptOutcome::IdempotentReplay { accepted }) => accepted,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        // Durable-before-provider: the manager sees this only after the actor
        // committed and synchronously published the acceptance transaction.
        let handoff = match accepted.disposition {
            TurnAdmissionDisposition::SteerPending => {
                self.hub
                    .submit_internal_nudge(accepted.clone(), delivery_text)
                    .await
            }
            TurnAdmissionDisposition::SubturnPending => {
                self.hub
                    .submit_internal_subturn(accepted.clone(), delivery_text)
                    .await
            }
            TurnAdmissionDisposition::Started | TurnAdmissionDisposition::Queued => {
                self.hub.worker_manager()?.submit(accepted.clone()).await
            }
        };
        if let Err(error) = handoff {
            return self.respond_turn_error(request_id, error);
        }
        self.respond_turn_accepted(request_id, accepted, headless_spec.is_some())
    }

    #[allow(clippy::too_many_arguments)]
    async fn shell_exec(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        branch_id: Option<BranchId>,
        agent_id: Option<AgentId>,
        command: String,
        cwd: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() || command.trim().is_empty() || command.len() > 8_192 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "shell command id and 1..=8192 UTF-8 command bytes are required",
                false,
                None,
            );
        }
        let mut request_coordinates = serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "command": &command,
            "cwd": &cwd,
        });
        let Some(request_coordinates) = request_coordinates.as_object_mut() else {
            return Err(SessionHubError::Task(
                "cannot construct shell-exec coordinates".into(),
            ));
        };
        if let Some(branch_id) = branch_id.as_ref() {
            request_coordinates.insert(
                "branch_id".into(),
                serde_json::to_value(branch_id).map_err(|error| {
                    SessionHubError::Task(format!("cannot encode shell branch: {error}"))
                })?,
            );
        }
        if let Some(agent_id) = agent_id.as_ref() {
            request_coordinates.insert(
                "agent_id".into(),
                serde_json::to_value(agent_id).map_err(|error| {
                    SessionHubError::Task(format!("cannot encode shell agent: {error}"))
                })?,
            );
        }
        let request_json = serde_json::to_string(&request_coordinates).map_err(|error| {
            SessionHubError::Task(format!("cannot encode shell-exec coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .shell_exec_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(accepted)) => {
                if accepted.worker_generation == self.hub.inner.store.worker_generation()
                    && let Err(error) = self
                        .hub
                        .worker_manager()?
                        .shell_exec(
                            accepted.clone(),
                            command_id.0.clone(),
                            branch_id.clone(),
                            agent_id.clone(),
                            command,
                            cwd,
                        )
                        .await
                {
                    return self.respond_shell_error(request_id, error);
                }
                return self.respond_shell_accepted(request_id, accepted);
            }
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_shell_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        let trimmed = command.trim();
        if trimmed == "cd"
            || trimmed
                .strip_prefix("cd")
                .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_whitespace))
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN,
                "`!cd` is unsupported: daemon-owned persistent shell cwd is a later design",
                false,
                None,
            );
        }
        if let Some(cwd) = cwd.as_deref()
            && (cwd.is_empty() || std::path::Path::new(cwd).is_absolute())
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "shell cwd must be a non-empty workspace-relative path",
                false,
                None,
            );
        }
        let accepted = match self
            .hub
            .accept_shell_exec(ShellExecAcceptCommand {
                command_id: command_id.0.clone(),
                request_digest,
                request_json,
                session_id: session_id.clone(),
                worker_generation,
                branch_id: branch_id.clone(),
                agent_id: agent_id.clone(),
                run_id: RunId::new(random_id("shell-run")?),
                item_id: haider_protocol::ids::ItemId::new(random_id("shell-item")?),
                command: command.clone(),
                running_event_id: EventId::new(random_id("shell-running")?),
                item_event_id: EventId::new(random_id("shell-item-started")?),
                active_event_id: EventId::new(random_id("shell-session-active")?),
                device_id: self.hub.inner.device_id.clone(),
            })
            .await
        {
            Ok(ShellExecAcceptOutcome::Committed { accepted, .. })
            | Ok(ShellExecAcceptOutcome::IdempotentReplay { accepted }) => accepted,
            Err(SessionHubError::Store(error)) => {
                return self.respond_shell_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = self
            .hub
            .worker_manager()?
            .shell_exec(
                accepted.clone(),
                command_id.0,
                branch_id,
                agent_id,
                command,
                cwd,
            )
            .await
        {
            return self.respond_shell_error(request_id, error);
        }
        self.respond_shell_accepted(request_id, accepted)
    }

    async fn tools_inventory(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let inventory =
            crate::worker::tool_inventory_snapshot(&self.hub.inner.store, &session_id).await?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ToolsInventory {
                session_id,
                inventory,
            },
        })
    }

    /// Cross-provider usage snapshot (U1). No installed service is an honest
    /// empty report (mirrors the missing-facade `account.list` answer), and
    /// per-account meter failures NEVER fail the frame — they ride as typed
    /// unavailability inside the report.
    async fn usage_report(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let Some(service) = self.hub.usage_report_service()? else {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::UsageReport {
                    report: haider_protocol::usage::UsageReportV1 {
                        generated_at_ms: 0,
                        accounts: Vec::new(),
                    },
                    availability: Some(haider_rpc::SnapshotAvailabilityWire::Unavailable {
                        reason: "usage subsystem is not configured".into(),
                    }),
                },
            });
        };
        let report = service.report(&self.hub.inner.store).await?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::UsageReport {
                report,
                availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
            },
        })
    }

    async fn usage_history_day(
        &self,
        request_id: RequestId,
        date: String,
    ) -> Result<(), SessionHubError> {
        // `f` is an explicit freshness request. Fold every newly closed UTC
        // slot before reading; otherwise a successful read can return the
        // same stale header-only day indefinitely.
        self.hub.inner.store.reconcile_usage_history().await?;
        let device_id = self.hub.inner.store.profile_installation_id().await?;
        let day = match self.hub.inner.store.usage_history_day(date.clone()).await {
            Ok(day) => day,
            Err(error) if error.code == ErrorCode::InvalidArgument => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.message,
                    false,
                    None,
                );
            }
            Err(error) => return Err(error.into()),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::UsageHistoryDay {
                date,
                device_id,
                day,
                availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
            },
        })
    }

    async fn usage_history_range(
        &self,
        request_id: RequestId,
        through_date: String,
        requested_days: u16,
    ) -> Result<(), SessionHubError> {
        self.hub.inner.store.reconcile_usage_history().await?;
        let device_id = self.hub.inner.store.profile_installation_id().await?;
        let mut days = match self
            .hub
            .inner
            .store
            .usage_history_range(through_date.clone(), requested_days)
            .await
        {
            Ok(days) => days,
            Err(error) if error.code == ErrorCode::InvalidArgument => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.message,
                    false,
                    None,
                );
            }
            Err(error) => return Err(error.into()),
        };
        crate::usage_report::enrich_usage_history_costs(&mut days);
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::UsageHistoryRange {
                through_date,
                device_id,
                days,
                availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
            },
        })
    }

    async fn computer_permission_open_settings(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        permission_request_id: String,
        permission: SystemPermission,
    ) -> Result<(), SessionHubError> {
        let mut cursor = 0;
        let mut matching_needed = false;
        let mut resolved = false;
        loop {
            let page = self.hub.inner.store.read(&session_id, cursor, 256).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |event| event.seq);
            for event in page {
                match PermissionEventPayload::from_payload_value(event.payload.to_json_value()) {
                    Ok(PermissionEventPayload::PermissionGrantNeeded(needed))
                        if needed.request_id == permission_request_id
                            && needed.permission == permission =>
                    {
                        matching_needed = true;
                    }
                    Ok(PermissionEventPayload::PermissionGrantResolved(done))
                        if done.request_id == permission_request_id =>
                    {
                        resolved = true;
                    }
                    _ => {}
                }
            }
        }
        if !matching_needed || resolved {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "unresolved computer permission request was not found",
                false,
                None,
            );
        }
        if let Err(error) = haider_tools::open_system_permission_settings(permission) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &error.to_string(),
                false,
                None,
            );
        }
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ComputerPermissionOpenSettings { permission },
        })
    }

    async fn session_compact(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        branch_id: Option<BranchId>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-compact command id must not be empty",
                false,
                None,
            );
        }
        let accepted = match self
            .hub
            .worker_manager()?
            .compact(
                session_id.clone(),
                command_id.0,
                worker_generation,
                branch_id,
            )
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        let body = if let Some(branch_id) = accepted.branch_id {
            ResponseBody::SessionCompactOnBranch {
                session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                branch_id,
            }
        } else {
            ResponseBody::SessionCompact {
                session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
            }
        };
        self.send(WireFrame::Response { request_id, body })
    }

    async fn turn_cancel(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        run_id: haider_protocol::ids::RunId,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "turn-cancel command id must not be empty",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "run_id": &run_id,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode turn-cancel coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let cancelled = match self
            .hub
            .turn_cancel_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(cancelled)) => cancelled,
            Ok(None) => {
                let command = TurnCancelCommand {
                    command_id: command_id.0,
                    request_digest,
                    request_json,
                    session_id: session_id.clone(),
                    worker_generation,
                    run_id: run_id.clone(),
                    cancelling_event_id: EventId::new(random_id("turn-cancelling")?),
                    device_id: self.hub.inner.device_id.clone(),
                };
                match self.hub.cancel_turn(command).await {
                    Ok(TurnCancelOutcome::Committed { cancelled, .. })
                    | Ok(TurnCancelOutcome::IdempotentReplay { cancelled }) => cancelled,
                    Err(SessionHubError::Store(error)) => {
                        return self.respond_turn_error(request_id, error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::TurnCancel {
                session_id: cancelled.session_id,
                run_id: cancelled.run_id,
                status: match cancelled.status {
                    TurnCancellationStatus::Accepted => CancelStatus::Accepted,
                    TurnCancellationStatus::AlreadyTerminal => CancelStatus::AlreadyTerminal,
                },
                terminal_seq: cancelled.terminal_seq,
            },
        })
    }

    async fn find_headless_run(
        &self,
        run_id: &haider_protocol::ids::RunId,
    ) -> Result<Option<HeadlessRunLookup>, SessionHubError> {
        for session_id in self.hub.inner.store.session_ids().await? {
            let mut cursor = 0_u64;
            let mut state = None;
            let mut spec = None;
            let mut budget_exhausted = None;
            loop {
                let page = self.hub.inner.store.read(&session_id, cursor, 256).await?;
                if page.is_empty() {
                    break;
                }
                let page_len = page.len();
                for envelope in page {
                    cursor = envelope.seq;
                    if envelope.run_id.as_ref() != Some(run_id) {
                        continue;
                    }
                    match haider_protocol::headless::HeadlessRunEventPayload::from_payload_value(
                        &envelope.payload,
                    ) {
                        Some(
                            haider_protocol::headless::HeadlessRunEventPayload::HeadlessRunConfigured(
                                configured,
                            ),
                        ) => spec = Some(configured),
                        Some(
                            haider_protocol::headless::HeadlessRunEventPayload::RunBudgetExhausted(
                                exhausted,
                            ),
                        ) => budget_exhausted = Some(exhausted),
                        Some(
                            haider_protocol::headless::HeadlessRunEventPayload::RunDeadlineExceeded(
                                _,
                            ),
                        ) => {}
                        None => {}
                    }
                    if let Ok(EventPayload::RunState(run_state)) = envelope.payload.decode_event() {
                        state = Some((run_state, envelope.seq));
                    }
                }
                if page_len < 256 {
                    break;
                }
            }
            if let (Some((state, state_seq)), Some(spec)) = (state, spec) {
                return Ok(Some(HeadlessRunLookup {
                    session_id,
                    run_id: run_id.clone(),
                    state,
                    state_seq,
                    head_seq: cursor,
                    budget_exhausted,
                    spec,
                }));
            }
        }
        Ok(None)
    }

    async fn headless_run_status(
        &self,
        request_id: RequestId,
        run_id: haider_protocol::ids::RunId,
    ) -> Result<(), SessionHubError> {
        let Some(found) = self.find_headless_run(&run_id).await? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "headless run was not found",
                false,
                None,
            );
        };
        let terminal_seq = found.state.is_terminal().then_some(found.state_seq);
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::HeadlessRunStatus {
                session_id: found.session_id,
                run_id: found.run_id,
                worker_generation: self.hub.worker_generation(),
                state: found.state,
                head_seq: found.head_seq,
                terminal_seq,
                budget_exhausted: found.budget_exhausted,
                spec: found.spec,
            },
        })
    }

    async fn headless_run_stop(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        run_id: haider_protocol::ids::RunId,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "headless stop command id must not be empty",
                false,
                None,
            );
        }
        let Some(found) = self.find_headless_run(&run_id).await? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "headless run was not found",
                false,
                None,
            );
        };
        if found.state.is_terminal() {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::HeadlessRunStop {
                    session_id: found.session_id,
                    run_id,
                    status: CancelStatus::AlreadyTerminal,
                    terminal_seq: Some(found.state_seq),
                },
            });
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &found.session_id,
            "worker_generation": self.hub.worker_generation(),
            "run_id": &run_id,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode headless stop coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let cancelled = match self
            .hub
            .turn_cancel_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(cancelled)) => cancelled,
            Ok(None) => {
                let command = TurnCancelCommand {
                    command_id: command_id.0,
                    request_digest,
                    request_json,
                    session_id: found.session_id.clone(),
                    worker_generation: self.hub.worker_generation(),
                    run_id: run_id.clone(),
                    cancelling_event_id: EventId::new(random_id("headless-cancelling")?),
                    device_id: self.hub.inner.device_id.clone(),
                };
                match self.hub.cancel_turn(command).await {
                    Ok(TurnCancelOutcome::Committed { cancelled, .. })
                    | Ok(TurnCancelOutcome::IdempotentReplay { cancelled }) => cancelled,
                    Err(SessionHubError::Store(error)) => {
                        return self.respond_turn_error(request_id, error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::HeadlessRunStop {
                session_id: cancelled.session_id,
                run_id: cancelled.run_id,
                status: match cancelled.status {
                    TurnCancellationStatus::Accepted => CancelStatus::Accepted,
                    TurnCancellationStatus::AlreadyTerminal => CancelStatus::AlreadyTerminal,
                },
                terminal_seq: cancelled.terminal_seq,
            },
        })
    }

    async fn run_retry(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "run-retry command id must not be empty",
                false,
                None,
            );
        }
        let wake_command_id = command_id.0.clone();
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode run-retry coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let accepted = match self
            .hub
            .run_retry_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(accepted)) => accepted,
            Ok(None) => {
                let command = RunRetryCommand {
                    command_id: command_id.0,
                    request_digest,
                    request_json,
                    session_id: session_id.clone(),
                    worker_generation,
                    run_id: RunId::new(random_id("retry-run")?),
                    queued_event_id: EventId::new(random_id("retry-queued")?),
                    retried_event_id: EventId::new(random_id("run-retried")?),
                    active_event_id: EventId::new(random_id("retry-active")?),
                    device_id: self.hub.inner.device_id.clone(),
                };
                match self.hub.accept_run_retry(command).await {
                    Ok(RunRetryOutcome::Committed { accepted, .. })
                    | Ok(RunRetryOutcome::IdempotentReplay { accepted }) => accepted,
                    Err(SessionHubError::Store(error)) => {
                        return self.respond_turn_error(request_id, error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        if accepted.worker_generation == self.hub.inner.store.worker_generation() {
            let handoff = if accepted.backoff_event_id.is_some() {
                // Receipt replay delivers the same command/event pair. Core
                // consumes that pair once; if the natural timer already won,
                // the handoff is a fulfilled idempotent no-op.
                self.hub
                    .worker_manager()?
                    .wake_retry(&accepted, wake_command_id)
                    .await
            } else {
                // Same-command terminal-failure receipt replay may follow a
                // transient manager handoff failure. Re-submitting the SAME
                // fresh run is safe: supervisor admission deduplicates by its
                // durable run id.
                self.hub.worker_manager()?.retry(accepted.clone()).await
            };
            if let Err(error) = handoff {
                return self.respond_turn_error(request_id, error);
            }
        }
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::RunRetry {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                failed_run_id: accepted.failed_run_id,
                user_seq: accepted.user_seq,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
            },
        })
    }

    fn respond_turn_accepted(
        &self,
        request_id: RequestId,
        accepted: AcceptedTurn,
        headless: bool,
    ) -> Result<(), SessionHubError> {
        let disposition = match accepted.disposition {
            TurnAdmissionDisposition::Started => SubmitDisposition::Started,
            TurnAdmissionDisposition::Queued => SubmitDisposition::Queued,
            TurnAdmissionDisposition::SteerPending => SubmitDisposition::SteerPending,
            TurnAdmissionDisposition::SubturnPending => SubmitDisposition::SubturnPending,
        };
        let body = if headless {
            ResponseBody::HeadlessRunStart {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                disposition,
            }
        } else if let Some(branch_id) = accepted.branch_id {
            ResponseBody::TurnSubmitOnBranch {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                branch_id,
                disposition,
            }
        } else {
            ResponseBody::TurnSubmit {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                disposition,
            }
        };
        self.send(WireFrame::Response { request_id, body })
    }

    fn respond_shell_accepted(
        &self,
        request_id: RequestId,
        accepted: AcceptedShellExec,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::ShellExec {
                session_id: accepted.session_id,
                run_id: Some(accepted.run_id),
                item_id: accepted.item_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
            },
        })
    }

    fn respond_shell_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::Busy => ERROR_CODE_BUSY,
            ErrorCode::RunNotActive => ERROR_CODE_RUN_NOT_ACTIVE,
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(request_id, code, &error.message, error.retryable, None)
    }

    fn respond_turn_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::RunNotActive => ERROR_CODE_RUN_NOT_ACTIVE,
            ErrorCode::Busy => ERROR_CODE_OVERLOADED,
            ErrorCode::VisionUnsupported => ERROR_CODE_VISION_UNSUPPORTED,
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(request_id, code, &error.message, error.retryable, None)
    }

    pub(super) fn respond_session_fork_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        let invalid_cut = error.details.as_ref().and_then(|details| {
            if details.get("kind").and_then(serde_json::Value::as_str)
                != Some("session_fork_invalid_cut")
            {
                return None;
            }
            let session_id = details
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(SessionId::new)?;
            let seq = details.get("seq").and_then(serde_json::Value::as_u64)?;
            let reason = serde_json::from_value(details.get("reason")?.clone()).ok()?;
            Some(ErrorData::SessionForkInvalidCut {
                session_id,
                seq,
                reason,
            })
        });
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::ForkCutUnstable => ERROR_CODE_FORK_CUT_UNSTABLE,
            ErrorCode::StoreReadOnly
            | ErrorCode::StoreCorrupt
            | ErrorCode::StoreUnavailable
            | ErrorCode::StoreFull => error.code.as_str(),
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(
            request_id,
            code,
            &error.message,
            error.retryable,
            invalid_cut,
        )
    }

    fn respond_queue_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        if error.code == ErrorCode::RevisionConflict {
            let expected_revision = error
                .details
                .as_ref()
                .and_then(|details| details.get("expected_revision"))
                .and_then(serde_json::Value::as_u64);
            let current_revision = error
                .details
                .as_ref()
                .and_then(|details| details.get("current_revision"))
                .and_then(serde_json::Value::as_u64);
            if let (Some(expected_revision), Some(current_revision)) =
                (expected_revision, current_revision)
            {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_REVISION_CONFLICT,
                    &error.message,
                    error.retryable,
                    Some(ErrorData::RevisionConflict {
                        expected_revision,
                        current_revision,
                    }),
                );
            }
        }
        self.respond_turn_error(request_id, error)
    }

    #[allow(clippy::too_many_arguments)]
    async fn session_create(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        cwd: String,
        mut provider: String,
        mut model: String,
        max_tokens: u64,
        permission_overrides: Option<haider_protocol::session::SessionPermissionOverridesV1>,
        cache_policy: haider_protocol::cache::CachePolicySettingsV1,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
        ssh_scope: Option<haider_rpc::SshScopeWire>,
        account_alias: Option<CredentialAlias>,
        resolve_provider: bool,
        resolve_model: bool,
        effort: Option<String>,
        fast: Option<bool>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-create command id must not be empty",
                false,
                None,
            );
        }
        let mut request_coordinates = serde_json::json!({
            "cwd": &cwd,
            "provider": &provider,
            "model": &model,
            "max_tokens": max_tokens,
        });
        if resolve_provider {
            request_coordinates["resolve_provider"] = serde_json::Value::Bool(true);
        }
        if resolve_model {
            request_coordinates["resolve_model"] = serde_json::Value::Bool(true);
        }
        if let Some(alias) = &account_alias {
            if alias.as_str().trim().is_empty() {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "session-create account alias must not be empty",
                    false,
                    None,
                );
            }
            request_coordinates["account_alias"] =
                serde_json::Value::String(alias.as_str().to_owned());
        }
        if let Some(effort) = &effort {
            request_coordinates["effort"] = serde_json::Value::String(effort.clone());
        }
        if let Some(fast) = fast {
            request_coordinates["fast"] = serde_json::Value::Bool(fast);
        }
        let ssh_scope = match ssh_scope {
            Some(scope) => match crate::ssh::SshScope::from_wire(scope) {
                Ok(scope) => scope,
                Err(error) => return self.respond_ssh_error(request_id, error),
            },
            None => crate::ssh::SshScope::All,
        };
        if !matches!(&ssh_scope, crate::ssh::SshScope::All) {
            request_coordinates["ssh_scope"] =
                serde_json::to_value(ssh_scope.to_wire()).map_err(|error| {
                    SessionHubError::Task(format!("cannot encode session SSH scope: {error}"))
                })?;
        }
        if let Some(overrides) = permission_overrides {
            request_coordinates["permission_overrides"] =
                serde_json::to_value(overrides).map_err(|error| {
                    SessionHubError::Task(format!(
                        "cannot encode session permission overrides: {error}"
                    ))
                })?;
        }
        if !cache_policy.is_default() {
            request_coordinates["cache_policy"] =
                serde_json::to_value(cache_policy).map_err(|error| {
                    SessionHubError::Task(format!("cannot encode session cache policy: {error}"))
                })?;
        }
        if !interaction_mode.is_interactive() {
            request_coordinates["interaction_mode"] = serde_json::to_value(interaction_mode)
                .map_err(|error| {
                    SessionHubError::Task(format!(
                        "cannot encode session interaction mode: {error}"
                    ))
                })?;
        }
        let request_json = serde_json::to_string(&request_coordinates).map_err(|error| {
            SessionHubError::Task(format!("cannot encode session-create coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt lookup deliberately precedes path validation. A response
        // lost after commit remains recoverable even if the workspace was
        // deleted before the retry reached a new connection.
        match self
            .hub
            .session_create_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(created)) => {
                return self.respond_created(request_id, created);
            }
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.message,
                    error.retryable,
                    None,
                );
            }
            Err(error) => return Err(error),
        }

        let selected_account_provider = if let Some(alias) = account_alias.as_ref() {
            let Some(facade) = self.hub.accounts()? else {
                return self.respond_error(
                    request_id,
                    "account_not_found",
                    "the selected account is not available",
                    false,
                    None,
                );
            };
            let Some(selected) = facade.management.inspect(|view| {
                view.descriptors
                    .iter()
                    .find(|descriptor| descriptor.alias == *alias)
                    .cloned()
            }) else {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_DRAINING,
                    "management snapshot is unavailable",
                    true,
                    None,
                );
            };
            let Some(selected) = selected else {
                return self.respond_error(
                    request_id,
                    "account_not_found",
                    "the selected account alias does not exist",
                    false,
                    None,
                );
            };
            Some(selected.provider)
        } else {
            None
        };
        if resolve_provider {
            if !provider.is_empty() {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "session-create provider must be empty when daemon resolution is requested",
                    false,
                    None,
                );
            }
            if let Some(selected_provider) = selected_account_provider.as_ref() {
                provider.clone_from(selected_provider);
            } else {
                let Some(facade) = self.hub.accounts()? else {
                    return self.respond_error(
                        request_id,
                        "no_active_account",
                        "no active daemon account is configured",
                        false,
                        None,
                    );
                };
                let Some(active_provider) = facade.management.inspect(|view| {
                    view.descriptors
                        .iter()
                        .find(|descriptor| descriptor.active)
                        .map(|descriptor| descriptor.provider.clone())
                }) else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_DRAINING,
                        "management snapshot is unavailable",
                        true,
                        None,
                    );
                };
                let Some(active_provider) = active_provider else {
                    return self.respond_error(
                        request_id,
                        "no_active_account",
                        "no active daemon account is configured",
                        false,
                        None,
                    );
                };
                provider = active_provider;
            }
        } else if let Some(selected_provider) = selected_account_provider.as_ref()
            && provider != *selected_provider
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-create provider does not match the selected account",
                false,
                None,
            );
        }
        if resolve_model {
            if !model.is_empty() {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "session-create model must be empty when daemon resolution is requested",
                    false,
                    None,
                );
            }
            let provider_default = match self.hub.accounts()? {
                Some(facade) => {
                    let Some(provider_default) = facade.management.inspect(|view| {
                        view.providers
                            .iter()
                            .find(|summary| summary.provider == provider)
                            .map(|summary| summary.default_model.clone())
                    }) else {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_DRAINING,
                            "management snapshot is unavailable",
                            true,
                            None,
                        );
                    };
                    provider_default
                }
                None => None,
            };
            if let Some(Some(default_model)) = provider_default {
                model = default_model;
            } else if provider_default.is_some() || resolve_provider {
                let message = format!("provider `{provider}` publishes no default model");
                return self.respond_error(request_id, "no_default_model", &message, false, None);
            }
        }

        // D3-5: the dependency configuration is the ONE authority on
        // creatable providers. Production answers the built-in adapter set;
        // "fake" exists only under injected test configurations. Since
        // W5g-5 an enabled custom OpenAI/Anthropic profile is creatable
        // too — it exists only because a durable, validated
        // `provider.configure` committed it, and the turn path routes it
        // by family.
        let creatable = self.hub.creatable_providers()?;
        let static_creatable = creatable
            .as_ref()
            .is_some_and(|providers| providers.contains(provider.as_str()));
        let custom_creatable = || {
            self.hub.accounts().ok().flatten().is_some_and(|facade| {
                facade.management.read().is_some_and(|view| {
                    view.providers.iter().any(|profile| {
                        profile.provider == provider
                            && profile.enabled
                            && matches!(
                                profile.api_family,
                                haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
                                    | haider_rpc::ProviderApiFamilyWire::AnthropicMessages
                            )
                    })
                })
            })
        };
        if !static_creatable && !custom_creatable() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unsupported session provider",
                false,
                None,
            );
        }
        const MAX_DAEMON_OUTPUT_RESERVE: u64 = 30_000;
        if model.trim().is_empty() || max_tokens == 0 || max_tokens > MAX_DAEMON_OUTPUT_RESERVE {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session model must be non-empty and max_tokens must be in 1..=30000",
                false,
                None,
            );
        }
        let mut summaries = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read())
            .map_or_else(Vec::new, |view| view.providers);
        match self
            .refresh_inventory_if_needed(&mut summaries, &provider, model.trim())
            .await
        {
            Ok(()) => {}
            Err(InventoryRefreshError::Hub(error)) => return Err(error),
            Err(InventoryRefreshError::Provider(error)) => {
                return self.respond_error(
                    request_id,
                    &error.code,
                    &error.message,
                    error.retryable,
                    error.data,
                );
            }
        }
        let authority = crate::model_select::ModelSelectionAuthority::new(creatable, summaries);
        let validated = match authority.validate_selection_with_status(&provider, None, &model) {
            Ok(selection) => selection,
            Err(refusal) => return self.respond_selection_refusal(request_id, &refusal),
        };
        if let Err(refusal) = authority.validate_effort(&provider, &model, effort.as_deref()) {
            return self.respond_tuning_refusal(request_id, &refusal);
        }
        if let Some(enabled) = fast
            && let Err(refusal) = authority.validate_fast(&provider, &model, enabled)
        {
            return self.respond_tuning_refusal(request_id, &refusal);
        }
        if matches!(
            validated.inventory_status,
            haider_rpc::ModelInventoryStatusWire::Unlisted
        ) {
            tracing::info!(
                provider = %validated.provider,
                model = %validated.model,
                inventory_status = "unlisted",
                inventory_authority = "advisory",
                "admitting a custom provider model absent from its advisory inventory"
            );
        }
        let workspace = match validate_workspace(cwd).await {
            Ok(workspace) => workspace,
            Err(message) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                );
            }
        };
        let session_id = SessionId::new(random_id("session")?);
        let command = SessionCreateCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id: session_id.clone(),
            cwd: workspace.canonical,
            provider,
            model,
            max_tokens,
            permission_overrides,
            effort,
            fast: fast.unwrap_or(false),
            cache_policy,
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(random_id("session-created")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        // Keep the opened directory descriptor alive until the transaction
        // returns. M3 transfers the same canonical identity into its broker.
        let _descriptor = workspace.descriptor;
        // Absence is the durable representation of the default `All` scope.
        // A non-default scope must still commit before the session can become
        // visible. Concurrent creators each stage their own candidate; the
        // transaction winner therefore already owns its exact durable scope.
        if !matches!(&ssh_scope, crate::ssh::SshScope::All) {
            self.hub.set_ssh_scope(session_id, ssh_scope.clone())?;
        }
        match self
            .hub
            .create_session_with_interaction_mode(
                command,
                interaction_mode,
                account_alias.map(|alias| alias.as_str().to_owned()),
            )
            .await
        {
            Ok(SessionCreateOutcome::Committed { created, .. }) => {
                if matches!(&ssh_scope, crate::ssh::SshScope::All) {
                    self.hub
                        .cache_default_ssh_scope_after_create(created.session_id.clone())?;
                }
                self.respond_created(request_id, created)
            }
            Ok(SessionCreateOutcome::IdempotentReplay { created }) => {
                self.respond_created(request_id, created)
            }
            Err(SessionHubError::Store(error)) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &error.message,
                error.retryable,
                None,
            ),
            Err(error) => Err(error),
        }
    }

    fn respond_created(
        &self,
        request_id: RequestId,
        created: CreatedSession,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionCreate {
                session_id: created.session_id,
                created_seq: created.created_seq,
                worker_generation: created.worker_generation,
                metadata: created.metadata,
            },
        })
    }

    async fn session_list(
        &self,
        request_id: RequestId,
        cursor: Option<String>,
        limit: u32,
        order: haider_rpc::SessionListOrderWire,
    ) -> Result<(), SessionHubError> {
        let limit = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(MAX_LIST_PAGE);
        if limit == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-list limit must be greater than zero",
                false,
                None,
            );
        }
        let (sessions, next_cursor) = match order {
            haider_rpc::SessionListOrderWire::IdAsc => {
                let after = match cursor.as_deref().map(decode_cursor).transpose() {
                    Ok(after) => after,
                    Err(()) => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_CURSOR,
                            "session-list cursor is invalid",
                            false,
                            None,
                        );
                    }
                };
                let ids = self.hub.roster_session_ids().await?;
                let mut selected = ids
                    .into_iter()
                    .filter(|session_id| {
                        after
                            .as_ref()
                            .is_none_or(|after| session_id.as_str() > after.as_str())
                    })
                    .take(limit.saturating_add(1))
                    .collect::<Vec<_>>();
                let has_more = selected.len() > limit;
                if has_more {
                    selected.truncate(limit);
                }
                let sessions = session_summaries(&self.hub, &selected).await?;
                let next_cursor = has_more
                    .then(|| selected.last().map(encode_cursor))
                    .flatten();
                (sessions, next_cursor)
            }
            haider_rpc::SessionListOrderWire::RecencyDesc => {
                let mut after = match cursor.as_deref().map(decode_recency_cursor).transpose() {
                    Ok(after) => after,
                    Err(()) => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_CURSOR,
                            "session-list cursor is invalid",
                            false,
                            None,
                        );
                    }
                };
                let target = limit.saturating_add(1);
                let mut selected = Vec::with_capacity(target);
                while selected.len() < target {
                    let fetch_limit = target.saturating_sub(selected.len());
                    let page = self
                        .hub
                        .inner
                        .store
                        .session_recency_page(after.clone(), fetch_limit)
                        .await?;
                    if page.is_empty() {
                        break;
                    }
                    let page_was_full = page.len() == fetch_limit;
                    for row in page {
                        after = Some(row.key.clone());
                        if self.hub.is_roster_visible(&row.key.session_id)? {
                            selected.push(row);
                        }
                    }
                    if !page_was_full {
                        break;
                    }
                }
                let has_more = selected.len() > limit;
                if has_more {
                    selected.truncate(limit);
                }
                let ids = selected
                    .iter()
                    .map(|row| row.key.session_id.clone())
                    .collect::<Vec<_>>();
                let recencies = selected
                    .iter()
                    .map(|row| {
                        (
                            row.key.session_id.as_str().to_owned(),
                            row.key.last_activity_ms,
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                let sessions = session_summaries_with_recency(&self.hub, &ids, &recencies).await?;
                let next_cursor = has_more
                    .then(|| selected.last().map(|row| encode_recency_cursor(&row.key)))
                    .flatten();
                (sessions, next_cursor)
            }
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionList {
                sessions,
                next_cursor,
            },
        })
    }

    async fn status_snapshot(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let (active_account, adoption_available, caching) = match self.hub.accounts()? {
            Some(facade) => {
                let Some((active, caching)) = facade.management.inspect(|view| {
                    let active = view
                        .descriptors
                        .iter()
                        .find(|descriptor| descriptor.active)
                        .cloned();
                    let caching = status_caching(
                        self.daemon_idle_ttl_ms,
                        active.as_ref().map(|account| account.provider.as_str()),
                        &view.providers,
                    );
                    (active, caching)
                }) else {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_DRAINING,
                        "management snapshot is unavailable",
                        true,
                        None,
                    );
                };
                (active, facade.status_adoption_snapshot(), caching)
            }
            None => (
                None,
                Vec::new(),
                status_caching(self.daemon_idle_ttl_ms, None, &[]),
            ),
        };
        let session_count = self.hub.roster_session_count().await?;
        let session_ids = self.hub.roster_session_ids().await?;
        let waiting_for_route_count = u64::try_from(
            session_summaries(&self.hub, &session_ids)
                .await?
                .iter()
                .filter(|summary| {
                    summary.run_state == Some(haider_rpc::ObserveRunStateWire::WaitingForRoute)
                })
                .count(),
        )
        .unwrap_or(u64::MAX);
        let runtime = self.runtime_paths.as_ref();
        let readiness = self
            .daemon_readiness
            .as_ref()
            .map(crate::Readiness::snapshot);
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::StatusSnapshot {
                active_account,
                session_count,
                waiting_for_route_count,
                adoption_available,
                daemon_pid: Some(std::process::id()),
                socket_path: runtime.map(|(socket_path, _)| socket_path.display().to_string()),
                pid_file_path: runtime
                    .map(|(_, pid_file_path)| pid_file_path.display().to_string()),
                idle_ttl_ms: self.daemon_idle_ttl_ms,
                warm: self.daemon_warm,
                caching: Some(caching),
                ready: readiness.is_some_and(|snapshot| snapshot.ready),
                ready_since: readiness.and_then(|snapshot| snapshot.ready_since_unix_ms),
                providers_loaded: readiness.is_some_and(|snapshot| snapshot.providers_loaded),
            },
        })
    }

    async fn monitor_list(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        let receipt = match self.hub.inner.store.latest_seq(&session_id).await {
            Ok(0) => crate::monitor::monitor_list_rejected(
                session_id,
                haider_rpc::MonitorControlRejectionWire::SessionNotFound,
            ),
            Ok(_) => {
                self.hub
                    .inner_monitor()
                    .client_list(&self.hub, session_id)
                    .await
            }
            Err(error) => crate::monitor::monitor_list_rejected(
                session_id,
                haider_rpc::MonitorControlRejectionWire::StoreUnavailable {
                    retryable: error.retryable,
                    detail: error.message,
                },
            ),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::MonitorList { receipt },
        })
    }

    async fn monitor_register(
        &self,
        request_id: RequestId,
        request: crate::monitor::MonitorClientRegistrationRequest,
    ) -> Result<(), SessionHubError> {
        let receipt = self
            .hub
            .inner_monitor()
            .client_register(&self.hub, request)
            .await;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::MonitorRegister { receipt },
        })
    }

    async fn monitor_remove(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        monitor_id: String,
    ) -> Result<(), SessionHubError> {
        let receipt = self
            .hub
            .inner_monitor()
            .client_remove(
                &self.hub,
                command_id,
                session_id,
                worker_generation,
                monitor_id,
            )
            .await;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::MonitorRemove { receipt },
        })
    }

    async fn monitor_mutate(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        mutation: haider_rpc::MonitorMutationWire,
    ) -> Result<(), SessionHubError> {
        let receipt = self
            .hub
            .inner_monitor()
            .client_mutate(
                &self.hub,
                command_id,
                session_id,
                worker_generation,
                mutation,
            )
            .await;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::MonitorMutate { receipt },
        })
    }

    async fn monitor_watch(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        after_cursor: u64,
    ) -> Result<(), SessionHubError> {
        // Subscribe before sealing the head: a racing append is either in the
        // initial replay seal or retained as a wake for the next seal.
        let publications = self.hub.inner.roster_publications.subscribe();
        let head = match self.hub.inner.store.latest_seq(&session_id).await {
            Ok(0) => {
                return self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::MonitorWatch {
                        receipt: crate::monitor::monitor_watch_rejected(
                            session_id,
                            haider_rpc::MonitorControlRejectionWire::SessionNotFound,
                        ),
                    },
                });
            }
            Ok(head) => head,
            Err(error) => {
                return self.send(WireFrame::Response {
                    request_id,
                    body: ResponseBody::MonitorWatch {
                        receipt: crate::monitor::monitor_watch_rejected(
                            session_id,
                            haider_rpc::MonitorControlRejectionWire::StoreUnavailable {
                                retryable: error.retryable,
                                detail: error.message,
                            },
                        ),
                    },
                });
            }
        };
        if after_cursor > head {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::MonitorWatch {
                    receipt: crate::monitor::monitor_watch_rejected(
                        session_id,
                        haider_rpc::MonitorControlRejectionWire::CursorAhead {
                            requested: after_cursor,
                            head,
                        },
                    ),
                },
            });
        }
        let watch_id = random_id("monitor-watch")?;
        let previous = {
            let mut slot = lock(&self.monitor_watch)?;
            slot.take()
        };
        if let Some(previous) = previous {
            previous.cancel.send_replace(true);
            let _ = previous.task.await;
            self.sink.purge_ordered(&previous.stream_id);
        }
        let mut slot = lock(&self.monitor_watch)?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::MonitorWatch {
                receipt: haider_rpc::MonitorWatchReceiptWire {
                    session_id: session_id.clone(),
                    policy: crate::monitor::monitor_control_policy(),
                    sources: crate::monitor::monitor_source_availability(),
                    outcome: haider_rpc::MonitorWatchOutcomeWire::Watching {
                        watch_id: watch_id.clone(),
                        requested_after_cursor: after_cursor,
                        replay_through_cursor: head,
                    },
                },
            },
        })?;
        let hub = self.hub.clone();
        let sink = Arc::clone(&self.sink);
        let stream_id = watch_id.clone();
        let (cancel, cancel_receiver) = watch::channel(false);
        *slot = Some(MonitorWatchState {
            stream_id,
            cancel,
            task: tokio::spawn(run_monitor_watch(
                hub,
                sink,
                watch_id,
                session_id,
                after_cursor,
                head,
                publications,
                cancel_receiver,
            )),
        });
        Ok(())
    }

    /// `account.list_watch`: a change SIGNAL, not a delta stream. The
    /// management view is small and already revision-stamped, so a watcher
    /// re-reads `account.list` on notice — which also means this frame can
    /// never disagree with the snapshot it announces, and removals need no
    /// special reconciliation.
    fn account_list_watch(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let mut watch = lock(&self.accounts_watch)?;
        let accepted = watch.as_ref().is_none_or(JoinHandle::is_finished);
        // Subscribe BEFORE acknowledging so a publication racing registration
        // is observed as a change rather than missed.
        let receiver = self
            .hub
            .accounts()?
            .map(|facade| facade.management.subscribe());
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::AccountListWatch {
                accepted: accepted && receiver.is_some(),
            },
        })?;
        let (Some(mut receiver), true) = (receiver, accepted) else {
            return Ok(());
        };

        let sink = Arc::clone(&self.sink);
        *watch = Some(tokio::spawn(async move {
            // `changed()` resolves once per publication; the revision read
            // afterwards is the latest, so a burst collapses into one frame
            // carrying the newest revision rather than a queue of stale ones.
            while receiver.changed().await.is_ok() {
                let revision = *receiver.borrow_and_update();
                if sink
                    .try_send(WireFrame::AccountsChanged { revision })
                    .is_err()
                {
                    break;
                }
            }
        }));
        Ok(())
    }

    fn session_list_watch(&self, request_id: RequestId) -> Result<(), SessionHubError> {
        let mut watch = lock(&self.roster_watch)?;
        let accepted = watch.as_ref().is_none_or(JoinHandle::is_finished);
        // Subscribe before acknowledging so a commit racing registration is
        // either present in the baseline or queued as a dirty-session wake.
        let mut publications = self.hub.inner.roster_publications.subscribe();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionListWatch { accepted },
        })?;
        if !accepted {
            return Ok(());
        }

        let hub = self.hub.clone();
        let sink = Arc::clone(&self.sink);
        *watch = Some(tokio::spawn(async move {
            let period = std::time::Duration::from_secs(30);
            let mut ticker = interval_at(tokio::time::Instant::now() + period, period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut pushed_heads = BTreeMap::<String, u64>::new();
            let mut reconcile_all = true;
            loop {
                if reconcile_all {
                    let Ok(ids) = hub.roster_session_ids().await else {
                        ticker.tick().await;
                        continue;
                    };
                    let mut current_heads = Vec::with_capacity(ids.len());
                    let mut head_read_failed = false;
                    for session_id in &ids {
                        match hub.inner.store.latest_seq(session_id).await {
                            Ok(head_seq) => current_heads.push((session_id.clone(), head_seq)),
                            Err(_) => {
                                head_read_failed = true;
                                break;
                            }
                        }
                    }
                    if head_read_failed {
                        ticker.tick().await;
                        continue;
                    }
                    // Removed sessions are deliberately silent in v1.
                    let current_ids = current_heads
                        .iter()
                        .map(|(session_id, _)| session_id.as_str())
                        .collect::<BTreeSet<_>>();
                    pushed_heads.retain(|session_id, _| current_ids.contains(session_id.as_str()));
                    let changed_ids = roster_fold_candidates(&pushed_heads, &current_heads);
                    if !changed_ids.is_empty()
                        && let Ok(changed) = session_summaries(&hub, &changed_ids).await
                    {
                        push_roster_chunks(sink.as_ref(), changed, &mut pushed_heads);
                    }
                    reconcile_all = false;
                }

                let first = tokio::select! {
                    received = publications.recv() => received,
                    _ = ticker.tick() => {
                        reconcile_all = true;
                        continue;
                    }
                };
                let mut dirty = BTreeMap::<String, SessionId>::new();
                match first {
                    Ok(session_id) => {
                        dirty.insert(session_id.as_str().to_owned(), session_id);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        reconcile_all = true;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                while let Ok(session_id) = publications.try_recv() {
                    dirty.insert(session_id.as_str().to_owned(), session_id);
                }
                for session_id in dirty.into_values() {
                    let Ok(head_seq) = hub.inner.store.latest_seq(&session_id).await else {
                        continue;
                    };
                    if head_seq == 0 {
                        pushed_heads.remove(session_id.as_str());
                        continue;
                    }
                    if pushed_heads.get(session_id.as_str()) == Some(&head_seq) {
                        continue;
                    }
                    let Ok(changed) = session_summaries(&hub, &[session_id]).await else {
                        continue;
                    };
                    push_roster_chunks(sink.as_ref(), changed, &mut pushed_heads);
                }
            }
        }));
        Ok(())
    }

    async fn session_surface_publish(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        input: Option<SurfaceInputPublishWire>,
        status: Option<SurfaceStatusPublishWire>,
    ) -> Result<(), SessionHubError> {
        let input = match input {
            Some(input) => {
                if input.text.len() > SURFACE_INPUT_MAX_BYTES {
                    return self.surface_text_too_large(
                        request_id,
                        "input",
                        input.text.len(),
                        SURFACE_INPUT_MAX_BYTES,
                    );
                }
                Some(SurfaceInputPublishWire {
                    text: strip_input_controls(input.text),
                    attachments: input.attachments,
                    revision: input.revision,
                })
            }
            None => None,
        };
        let status = match status {
            Some(status) => {
                if status.line.len() > SURFACE_STATUS_MAX_BYTES {
                    return self.surface_text_too_large(
                        request_id,
                        "status",
                        status.line.len(),
                        SURFACE_STATUS_MAX_BYTES,
                    );
                }
                Some(SurfaceStatusPublishWire {
                    line: strip_status_controls(status.line),
                    state: status.state,
                    detail: status.detail,
                    revision: status.revision,
                })
            }
            None => None,
        };
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        let Some(outcome) =
            self.hub
                .publish_surface(&self.connection_id, &session_id, input, status)?
        else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        };
        if self.closed.load(Ordering::Acquire) {
            self.hub.clear_surface_owner(&self.connection_id);
            return Err(SessionHubError::Closed);
        }
        let changed =
            outcome.accepted_input_revision.is_some() || outcome.accepted_status_revision.is_some();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSurfacePublished {
                session_id,
                accepted_input_revision: outcome.accepted_input_revision,
                accepted_status_revision: outcome.accepted_status_revision,
            },
        })?;
        // Preserve response-before-delta ordering while removing the polling
        // delay: generations, not wake count, remain the dedupe authority.
        if changed {
            self.hub.notify_surface_watchers();
        }
        Ok(())
    }

    async fn session_surface_watch(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        // rev933c finding 8: a legal max-size snapshot (input + status caps
        // plus envelope overhead) must fit this connection's negotiated
        // frame limit, or a later publish would kill the watcher's
        // connection mid-delivery. Refuse the registration upfront instead.
        // rev933d finding 8: a legal snapshot's JSON encoding can be ~2×
        // its raw bytes (a string of all `\"`/`\\`), plus keys, the echoed
        // request id, and framing. Size the gate for that worst case, not
        // the raw caps, so a later publish can never overflow the watcher's
        // negotiated frame and kill its connection mid-delivery.
        let snapshot_ceiling = 2 * (SURFACE_INPUT_MAX_BYTES + SURFACE_STATUS_MAX_BYTES) + 16_384;
        if let Some(limit) = self.sink.max_frame_bytes()
            && limit < snapshot_ceiling
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!(
                    "surface watch needs a negotiated frame limit of at least \
                     {snapshot_ceiling} bytes; this connection negotiated {limit}"
                ),
                false,
                None,
            );
        }
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let Some(snapshot) = self.hub.live_surface_snapshot(&session_id)? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        };

        let mut watch = lock(&self.surface_watch)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if watch.as_ref().is_some_and(|watch| watch.task.is_finished()) {
            watch.take();
        }
        if watch.is_none() {
            let registrations = Arc::new(Mutex::new(HashMap::new()));
            let hub = self.hub.clone();
            let sink = Arc::clone(&self.sink);
            let task_registrations = Arc::clone(&registrations);
            let publications = self.hub.inner.surface_publications.subscribe();
            let task = tokio::spawn(run_surface_watch(
                hub,
                sink,
                task_registrations,
                publications,
            ));
            *watch = Some(SurfaceWatchState {
                registrations,
                task,
            });
        }
        let Some(watch_state) = watch.as_ref() else {
            return Err(SessionHubError::Task(
                "surface watch failed to initialize".into(),
            ));
        };
        let registrations = Arc::clone(&watch_state.registrations);
        {
            let registrations = lock(&registrations)?;
            if !registrations.contains_key(&session_id)
                && registrations.len() >= MAX_SURFACE_WATCHES_PER_CONNECTION
            {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_OVERLOADED,
                    "connection surface-watch limit reached",
                    true,
                    None,
                );
            }
        }
        // The ack enters the system lane before the registration can produce
        // a delta. Its captured generation is installed afterwards so a
        // publication racing this response remains visible to the ticker.
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionSurfaceWatching {
                session_id: session_id.clone(),
                input: snapshot.input,
                status: snapshot.status,
            },
        })?;
        lock(&registrations)?.insert(session_id, snapshot.change_generation);
        // Close the registration race: if a publication's coalesced wake ran
        // before the generation was installed, this wake rechecks it.
        self.hub.notify_surface_watchers();
        if self.closed.load(Ordering::Acquire) {
            if let Some(watch) = watch.take() {
                watch.task.abort();
            }
            return Err(SessionHubError::Closed);
        }
        Ok(())
    }

    fn session_input_inject(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        op: SurfaceInjectOp,
    ) -> Result<(), SessionHubError> {
        let op = match op {
            SurfaceInjectOp::Set { text } => {
                if text.len() > SURFACE_INPUT_MAX_BYTES {
                    return self.surface_text_too_large(
                        request_id,
                        "input",
                        text.len(),
                        SURFACE_INPUT_MAX_BYTES,
                    );
                }
                SurfaceInjectOp::Set {
                    text: strip_input_controls(text),
                }
            }
            SurfaceInjectOp::Insert { text } => {
                if text.len() > SURFACE_INPUT_MAX_BYTES {
                    return self.surface_text_too_large(
                        request_id,
                        "input",
                        text.len(),
                        SURFACE_INPUT_MAX_BYTES,
                    );
                }
                SurfaceInjectOp::Insert {
                    text: strip_input_controls(text),
                }
            }
            SurfaceInjectOp::Clear => SurfaceInjectOp::Clear,
            SurfaceInjectOp::Submit => SurfaceInjectOp::Submit,
            _ => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "input injection operation is not supported",
                    false,
                    None,
                );
            }
        };
        let delivered = self.hub.inject_session_input(&session_id, op)?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionInputInjectAck {
                session_id,
                delivered,
            },
        })
    }

    fn surface_text_too_large(
        &self,
        request_id: RequestId,
        field: &str,
        actual_bytes: usize,
        max_bytes: usize,
    ) -> Result<(), SessionHubError> {
        let actual_bytes = u64::try_from(actual_bytes).unwrap_or(u64::MAX);
        let max_bytes = u64::try_from(max_bytes).unwrap_or(u64::MAX);
        self.respond_error(
            request_id,
            ERROR_CODE_SURFACE_TEXT_TOO_LARGE,
            &format!("surface {field} is {actual_bytes} bytes; the hard limit is {max_bytes}"),
            false,
            Some(ErrorData::SurfaceTextTooLarge {
                field: field.to_owned(),
                actual_bytes,
                max_bytes,
            }),
        )
    }

    async fn session_pipe_path(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let path = match self.hub.inner.pipe_native.sidecar_path(&session_id) {
            Ok(path) => path,
            Err(error) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.to_string(),
                    false,
                    None,
                );
            }
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionPipePath {
                path: path.to_string_lossy().into_owned(),
            },
        })
    }

    async fn session_read(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        range: SeqRange,
    ) -> Result<(), SessionHubError> {
        let head = self.hub.inner.store.latest_seq(&session_id).await?;
        if head == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        if range.start_seq == 0 || range.end_seq < range.start_seq {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range must be non-empty and start at sequence one or later",
                false,
                None,
            );
        }
        let count = range
            .end_seq
            .saturating_sub(range.start_seq)
            .saturating_add(1);
        let limit = usize::try_from(count).unwrap_or(usize::MAX);
        if limit > MAX_READ_ENVELOPES {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range exceeds the maximum of 1024 envelopes",
                false,
                None,
            );
        }
        let envelopes = self
            .hub
            .inner
            .store
            .read(&session_id, range.start_seq.saturating_sub(1), limit)
            .await?
            .into_iter()
            .take_while(|envelope| envelope.seq <= range.end_seq)
            .collect::<Vec<_>>();
        let metadata = self.hub.inner.store.session_metadata(&session_id).await?;
        let initial_model = metadata
            .as_ref()
            .map_or("", |metadata| metadata.model.as_str());
        let latest_context_footprint =
            cached_observe_snapshot(&self.hub, &session_id, initial_model)
                .await?
                .footprint;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionRead {
                result: SessionReadResult {
                    metadata,
                    session_id,
                    range,
                    head_seq: head,
                    latest_context_footprint,
                    envelopes,
                },
            },
        })
    }

    async fn session_observe(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        last_event_limit: u32,
        metadata_only: bool,
    ) -> Result<(), SessionHubError> {
        let Some(digest) =
            session_observe_digest(&self.hub, session_id, last_event_limit, metadata_only).await?
        else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionObserve { digest },
        })
    }

    async fn session_observe_batch(
        &self,
        request_id: RequestId,
        session_ids: Vec<SessionId>,
        last_event_limit: u32,
        metadata_only: bool,
    ) -> Result<(), SessionHubError> {
        if session_ids.is_empty() || session_ids.len() > MAX_OBSERVE_BATCH {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session.observe_batch requires between 1 and 64 session ids",
                false,
                None,
            );
        }
        let mut digests = Vec::with_capacity(session_ids.len());
        for session_id in session_ids {
            let Some(digest) =
                session_observe_digest(&self.hub, session_id, last_event_limit, metadata_only)
                    .await?
            else {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_NOT_FOUND,
                    "session was not found",
                    false,
                    None,
                );
            };
            digests.push(digest);
        }
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionObserveBatch { digests },
        })
    }

    async fn session_fleet(
        &self,
        request_id: RequestId,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let bounded = self
            .hub
            .delegation_descendants(
                session_id.clone(),
                haider_rpc::FLEET_MAX_NODES as usize,
                haider_rpc::FLEET_MAX_DEPTH,
            )
            .await?;
        let generated_at_ms = fleet_now_ms();
        let mut nodes = Vec::with_capacity(bounded.descendants.len());
        for descendant in bounded.descendants {
            let direct_child_count = descendant.direct_child_count;
            let record = descendant.record;
            let head_seq = self
                .hub
                .inner
                .store
                .latest_seq(&record.child_session_id)
                .await?;
            let initial_model = self
                .hub
                .inner
                .store
                .session_metadata(&record.child_session_id)
                .await?
                .map_or_else(String::new, |metadata| metadata.model);
            let truth =
                fleet_child_truth(&self.hub.inner.store, &record, head_seq, &initial_model).await?;
            nodes.push(FleetFlatNode {
                record,
                state: truth.state,
                metrics: truth.metrics,
                direct_child_count,
            });
        }
        let snapshot = fleet_snapshot(session_id, generated_at_ms, nodes, bounded.truncated)?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionFleet { snapshot },
        })
    }

    async fn session_descendants_attach(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        cursors: Vec<haider_rpc::DescendantReplayCursorWire>,
        max_children: u32,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let prepared = match super::descendant_stream::prepare_descendant_stream(
            &self.hub,
            session_id.clone(),
            cursors,
            max_children,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(super::descendant_stream::PrepareDescendantStreamError::Invalid(message)) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                );
            }
            Err(super::descendant_stream::PrepareDescendantStreamError::CursorAhead {
                cursor,
                head,
            }) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CURSOR_AHEAD,
                    &format!(
                        "descendant cursor for session {} and agent {} is beyond its committed head",
                        cursor.session_id, cursor.agent_id
                    ),
                    false,
                    Some(ErrorData::CursorAhead {
                        requested: cursor.after_seq,
                        head,
                    }),
                );
            }
            Err(super::descendant_stream::PrepareDescendantStreamError::Hub(error)) => {
                return Err(error);
            }
        };
        let baseline = prepared.baseline.clone();
        let streamed_session_ids = prepared.streamed_session_ids();
        let repair_identities = prepared.repair_identities();
        let (attachment_id, cancel) = match self.hub.register_descendant_attachment(
            &self.connection_id,
            session_id,
            streamed_session_ids,
        ) {
            Ok(DescendantRegisterResult::Registered {
                attachment_id,
                cancel,
            }) => (attachment_id, cancel),
            Ok(DescendantRegisterResult::Overloaded { message }) => {
                return self.respond_error(request_id, ERROR_CODE_OVERLOADED, &message, true, None);
            }
            Ok(DescendantRegisterResult::SessionUnavailable) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_NOT_FOUND,
                    "session was not found",
                    false,
                    None,
                );
            }
            Err(error) => return Err(error),
        };
        if self.closed.load(Ordering::Acquire) {
            let _ = self.hub.detach_descendant(&attachment_id);
            return Err(SessionHubError::Closed);
        }
        if self
            .sink
            .try_send_for(
                &attachment_id,
                WireFrame::Response {
                    request_id,
                    body: ResponseBody::SessionDescendantsAttach {
                        attachment_id: attachment_id.clone(),
                        baseline,
                    },
                },
            )
            .is_err()
        {
            let _ = self.hub.detach_descendant(&attachment_id);
            return Err(SessionHubError::Delivery);
        }
        if self
            .hub
            .spawn_descendant_stream(
                attachment_id.clone(),
                prepared,
                Arc::clone(&self.sink),
                cancel,
            )
            .is_err()
        {
            // The success may still be staged behind the attachment lane.
            // Atomically purge/replace it, or send an identity-only repair if
            // it already reached the peer; never leave a ghost attachment.
            self.hub
                .repair_and_detach_descendant(&self.sink, &attachment_id, repair_identities);
        }
        Ok(())
    }

    async fn session_diagnostic(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        code: String,
        message: String,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty()
            || code != "client-daemon-incompatible"
            || message.trim().is_empty()
            || message.len() > 1_024
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session diagnostic coordinates are invalid",
                false,
                None,
            );
        }
        let mut cursor = 0_u64;
        let mut last = None;
        loop {
            let page = self
                .hub
                .inner
                .store
                .read(&session_id, cursor, REPLAY_PAGE_SIZE)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                if let Ok(EventPayload::ClientDiagnostic {
                    command_id: existing,
                    code: existing_code,
                    message: existing_message,
                }) = envelope.payload.decode_event()
                    && existing == command_id.as_str()
                {
                    if existing_code != code || existing_message != message {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "session diagnostic command id was reused with different content",
                            false,
                            None,
                        );
                    }
                    return self.send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::SessionDiagnostic {
                            recorded_seq: envelope.seq,
                        },
                    });
                }
                last = Some(envelope);
            }
        }
        let Some(last) = last else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        };
        let payload = EventPayload::ClientDiagnostic {
            command_id: command_id.as_str().to_owned(),
            code,
            message,
        };
        let mut envelopes = [haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: EventId::new(format!(
                "client-diagnostic-{}",
                blake3::hash(command_id.as_str().as_bytes()).to_hex()
            )),
            seq: 0,
            session_id,
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: self.hub.inner.device_id.clone(),
            authority_epoch: last.authority_epoch,
            worker_generation: self.hub.inner.store.worker_generation(),
            causation_id: Some(last.event_id),
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: haider_protocol::envelope::RawPayload::from_event(payload).map_err(
                |error| SessionHubError::Task(format!("cannot encode session diagnostic: {error}")),
            )?,
        }];
        let recorded = match self.hub.append(&mut envelopes).await {
            Ok(_) => envelopes[0].seq,
            Err(error) => return self.respond_turn_error(request_id, error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionDiagnostic {
                recorded_seq: recorded,
            },
        })
    }

    async fn session_attach(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
        sealed_replay: bool,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let registration = match self
            .hub
            .register(&self.connection_id, session_id, after_seq, mode)
            .await?
        {
            RegisterResult::Registered(registration) => registration,
            RegisterResult::CursorAhead { requested, head } => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CURSOR_AHEAD,
                    "replay cursor is beyond the committed session head",
                    false,
                    Some(ErrorData::CursorAhead { requested, head }),
                );
            }
            // Same stable code the connection cap uses (its doc names
            // admission caps as the family); correlated and retryable here.
            RegisterResult::Overloaded { message } => {
                return self.respond_error(request_id, ERROR_CODE_OVERLOADED, &message, true, None);
            }
        };
        let attachment_id = registration.attachment_id.clone();
        let attach_state = registration.attach_state.clone();
        let workspace_unavailable = self
            .hub
            .inner
            .store
            .session_metadata(&attach_state.session_id)
            .await?
            .and_then(|metadata| {
                crate::workspace::unavailable(std::path::Path::new(&metadata.cwd))
            });
        if let Some(unavailable) = workspace_unavailable {
            tracing::info!(
                target: "haider.workspace",
                session_id = %attach_state.session_id,
                path = %unavailable.path,
                reason = unavailable.reason.as_str(),
                "attached session has an unavailable stored workspace"
            );
        }
        // Close-vs-registration sweep (P2-4): `close` sets `closed` BEFORE
        // it snapshots the owners map, so a registration that landed after
        // that snapshot always observes `closed` here and detaches itself;
        // one that landed before it was swept by close. Either way no
        // attachment survives on a closed connection.
        if self.closed.load(Ordering::Acquire) {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Closed);
        }
        // Response-before-first-event: the response is staged with a marker
        // that gates this attachment's event offers until it has left the
        // queue, so no replayed event can precede the response that names
        // the attachment id (and a purge that still finds it answers the
        // request — see the unknown-id rule on `lag_and_detach`).
        if self
            .sink
            .try_send_for(
                &attachment_id,
                WireFrame::Response {
                    request_id,
                    body: ResponseBody::SessionAttach {
                        attachment_id: attachment_id.clone(),
                        attach_state,
                    },
                },
            )
            .is_err()
        {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Delivery);
        }
        self.hub.spawn_replay(
            registration,
            after_seq,
            sealed_replay,
            Arc::clone(&self.sink),
        )
    }

    async fn hooks_list(&self, request_id: RequestId, cwd: String) -> Result<(), SessionHubError> {
        let Some(hooks) = self.hub.hooks()? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "hook service is unavailable",
                false,
                None,
            );
        };
        match hooks.list(std::path::PathBuf::from(cwd)).await {
            Ok((policy, revision, hooks)) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::HooksList {
                    policy: policy.as_str().to_owned(),
                    revision,
                    hooks,
                },
            }),
            Err(message) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &message,
                false,
                None,
            ),
        }
    }

    async fn hooks_trust(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        digest: String,
        trusted: bool,
    ) -> Result<(), SessionHubError> {
        let Some(hooks) = self.hub.hooks()? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "hook service is unavailable",
                false,
                None,
            );
        };
        match hooks.apply_trust(command_id, digest, trusted).await {
            Ok(change) => self.send(WireFrame::Response {
                request_id,
                body: if trusted {
                    ResponseBody::HooksTrust {
                        digest: change.digest,
                        trusted: change.trusted,
                    }
                } else {
                    ResponseBody::HooksRevoke {
                        digest: change.digest,
                        trusted: change.trusted,
                    }
                },
            }),
            Err(error) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &error.message,
                error.retryable,
                None,
            ),
        }
    }

    async fn session_detach(
        &self,
        request_id: RequestId,
        attachment_id: AttachmentId,
    ) -> Result<(), SessionHubError> {
        let owner = self
            .hub
            .take_attachment(&attachment_id, Some(&self.connection_id))?;
        if owner.is_none()
            && self
                .hub
                .take_descendant_attachment(&attachment_id, Some(&self.connection_id))?
                .is_some()
        {
            let _ = self.sink.purge_attachment(&attachment_id);
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::SessionDetach { attachment_id },
            });
        }
        let Some(owner) = owner else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "attachment was not found on this connection",
                false,
                None,
            );
        };
        // Removal/cancellation happened under the same ownership lock used by
        // replay delivery. Purging now is therefore a terminal lane barrier.
        // (The purge cannot report a pending response: the client could only
        // name this attachment id after receiving that response.)
        let _ = self.sink.purge_attachment(&attachment_id);
        SessionHub::finish_detach(&attachment_id, owner).await;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionDetach { attachment_id },
        })
    }

    async fn command_menu_lookup(
        &self,
        session_id: &SessionId,
        request_seq: u64,
        menu_id: &MenuId,
        answer: &DurableMenuAnswer,
    ) -> Result<CommandMenuLookup, SessionHubError> {
        let opening = self
            .hub
            .inner
            .store
            .read(session_id, request_seq.saturating_sub(1), 1)
            .await?
            .into_iter()
            .find(|envelope| envelope.seq == request_seq);
        let Some(opening) = opening else {
            return Ok(CommandMenuLookup::Ordinary);
        };
        let Ok(EventPayload::MenuOpened(menu)) = opening.payload.decode_event() else {
            return Ok(CommandMenuLookup::Ordinary);
        };
        if menu.id != *menu_id {
            return Ok(CommandMenuLookup::Ordinary);
        }
        let (command, encoded_continuation) = match command_menu_origin_parts(&menu.origin) {
            Ok(Some(origin)) => (origin.command, origin.encoded_continuation),
            Ok(None) => return Ok(CommandMenuLookup::Ordinary),
            Err(()) => {
                return Ok(CommandMenuLookup::Invalid(
                    "unknown parked command origin version; no action was taken".into(),
                ));
            }
        };
        let selected_key = || {
            let key = answer.option_key.as_deref().ok_or_else(|| {
                "command choice answer is missing its stable option key".to_owned()
            })?;
            if !menu.options.iter().any(|option| option.key == key) {
                return Err("command choice key is not in the parked option set".to_owned());
            }
            Ok(key.to_owned())
        };
        let (continuation, confirm_new_epoch) = if let Some(encoded) = encoded_continuation {
            if !matches!(menu.kind, MenuKind::Choice) {
                return Ok(CommandMenuLookup::Invalid(
                    "cache confirmation menu is not a choice".into(),
                ));
            }
            match selected_key() {
                Ok(key) if key == "confirm" => match decode_cache_confirmation(encoded) {
                    Ok(continuation) => (continuation, true),
                    Err(message) => return Ok(CommandMenuLookup::Invalid(message)),
                },
                Ok(_) => {
                    return Ok(CommandMenuLookup::Invalid(
                        "cache confirmation answer is not confirm".into(),
                    ));
                }
                Err(message) => return Ok(CommandMenuLookup::Invalid(message)),
            }
        } else {
            let continuation = match command {
                "rename" if matches!(menu.kind, MenuKind::Question) => {
                    let value = answer
                        .value
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| "rename command answer needs a non-empty value".to_owned());
                    match value {
                        Ok(value) => ParkedCommandContinuation::Rename(value.to_owned()),
                        Err(message) => return Ok(CommandMenuLookup::Invalid(message)),
                    }
                }
                "model" if matches!(menu.kind, MenuKind::Choice) => match selected_key() {
                    Ok(model) => ParkedCommandContinuation::Model(model),
                    Err(message) => return Ok(CommandMenuLookup::Invalid(message)),
                },
                "provider" if matches!(menu.kind, MenuKind::Choice) => match selected_key() {
                    Ok(provider) => ParkedCommandContinuation::Provider(provider),
                    Err(message) => return Ok(CommandMenuLookup::Invalid(message)),
                },
                "effort" if matches!(menu.kind, MenuKind::Choice) => match selected_key() {
                    Ok(effort) if effort == "default" => ParkedCommandContinuation::Effort(None),
                    Ok(effort) => ParkedCommandContinuation::Effort(Some(effort)),
                    Err(message) => return Ok(CommandMenuLookup::Invalid(message)),
                },
                "fast" if matches!(menu.kind, MenuKind::Choice) => match selected_key() {
                    Ok(value) if value == "on" => ParkedCommandContinuation::Fast(true),
                    Ok(value) if value == "off" => ParkedCommandContinuation::Fast(false),
                    Ok(_) => {
                        return Ok(CommandMenuLookup::Invalid(
                            "fast command choice is neither on nor off".into(),
                        ));
                    }
                    Err(message) => return Ok(CommandMenuLookup::Invalid(message)),
                },
                "rename" | "model" | "provider" | "effort" | "fast" => {
                    return Ok(CommandMenuLookup::Invalid(
                        "parked command menu kind does not match its command".into(),
                    ));
                }
                _ => {
                    return Ok(CommandMenuLookup::Invalid(
                        "unknown parked command origin; no action was taken".into(),
                    ));
                }
            };
            (continuation, false)
        };
        Ok(CommandMenuLookup::Continuation(ResolvedCommandMenu {
            action: continuation,
            opening_generation: opening.worker_generation,
            confirm_new_epoch,
        }))
    }

    async fn delegated_menu_opening(
        &self,
        session_id: &SessionId,
        request_seq: u64,
        menu_id: &MenuId,
    ) -> Result<bool, SessionHubError> {
        let opening = self
            .hub
            .inner
            .store
            .read(session_id, request_seq.saturating_sub(1), 1)
            .await?
            .into_iter()
            .find(|envelope| envelope.seq == request_seq);
        let Some(opening) = opening else {
            return Ok(false);
        };
        let Ok(EventPayload::MenuOpened(menu)) = opening.payload.decode_event() else {
            return Ok(false);
        };
        let MenuScope::Subagent { agent } = &menu.scope else {
            return Ok(false);
        };
        if menu.id != *menu_id
            || menu.origin != crate::delegation::DELEGATED_MENU_ORIGIN
            || opening.agent_id.as_ref() != Some(agent)
        {
            return Ok(false);
        }
        let Some(record) = self.hub.delegation(agent.clone()).await? else {
            return Ok(false);
        };
        Ok(record.parent_session_id == *session_id
            && opening.run_id.as_ref() == Some(&record.parent_run_id))
    }

    async fn execute_parked_command(
        &self,
        session_id: SessionId,
        menu_id: &MenuId,
        continuation: ParkedCommandContinuation,
        opening_generation: u64,
        confirm_new_epoch: bool,
    ) -> Result<(ResponseBody, CommandReceiptKind), SessionHubError> {
        let internal_request_id =
            RequestId::new(format!("command-menu-execute-{}", menu_id.as_str()));
        let operation_command_id =
            CommandId::new(format!("command-menu-{}-execute", menu_id.as_str()));
        let expected = continuation.receipt_kind();
        let receipt_generation = self
            .hub
            .command_receipt_worker_generation(
                &operation_command_id,
                continuation.canonical_method(),
            )
            .await?;
        let first_generation = receipt_generation.unwrap_or(opening_generation);
        let mut body = self
            .execute_command_continuation(
                internal_request_id.clone(),
                operation_command_id.clone(),
                session_id.clone(),
                continuation.clone(),
                first_generation,
                confirm_new_epoch,
            )
            .await?;
        let current_generation = self.hub.worker_generation();
        if receipt_generation.is_none()
            && first_generation != current_generation
            && matches!(
                &body,
                ResponseBody::Error { code, .. } if code == ERROR_CODE_STALE_GENERATION
            )
        {
            // If the operation committed before a crash, the old-generation
            // attempt above replays its exact receipt. If only MenuAnswered
            // committed, it returns stale and this current-generation retry
            // completes the deterministic continuation command.
            body = self
                .execute_command_continuation(
                    internal_request_id,
                    operation_command_id,
                    session_id,
                    continuation,
                    current_generation,
                    confirm_new_epoch,
                )
                .await?;
        }
        Ok((body, expected))
    }

    async fn finish_command_menu(
        &self,
        request_id: Option<RequestId>,
        session_id: SessionId,
        menu_id: &MenuId,
        resolved: ResolvedCommandMenu,
        resolution_seq: u64,
    ) -> Result<(), SessionHubError> {
        let continuation = resolved.action;
        let confirm_new_epoch = resolved.confirm_new_epoch;
        let (body, expected) = self
            .execute_parked_command(
                session_id.clone(),
                menu_id,
                continuation.clone(),
                resolved.opening_generation,
                confirm_new_epoch,
            )
            .await?;
        if let ResponseBody::Error { code, message, .. } = &body
            && code == haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED
            && !confirm_new_epoch
        {
            let confirmation_id =
                CommandId::new(format!("command-cache-confirm-{}", menu_id.as_str()));
            self.ensure_cache_confirmation_menu(
                &session_id,
                &confirmation_id,
                continuation.slash_name(),
                &continuation,
                message.clone(),
            )
            .await?;
            return self.menu_success(request_id, resolution_seq);
        }
        match body {
            ResponseBody::Error {
                code,
                message,
                retryable,
                data,
            } => self.menu_error(request_id, &code, &message, retryable, data),
            body if expected.accepts(&body) => self.menu_success(request_id, resolution_seq),
            _ => self.menu_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "command handler returned an unexpected response; no success was asserted",
                false,
                None,
            ),
        }
    }

    /// Handles the durable top-level `MenuAnswer` command.
    ///
    /// The arbitration law — first COMMITTED answer wins, losers get the
    /// winner's `resolution_seq` — is stated on
    /// `haider_store::Store::resolve_menu`; this method adds transport
    /// concerns only: capability + attachment policy, wire error mapping, and
    /// the correlated reply. Every attachment learns the outcome from the
    /// event stream (the actor publishes the committed envelope); the reply
    /// is a convenience, never the authority.
    ///
    /// Policy decision (brief §6): answering requires a CONTROL attachment to
    /// the target session — v0.1 has no "controller without a viewport"
    /// allowance.
    #[allow(clippy::too_many_arguments)]
    pub async fn menu_answer(
        &self,
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: haider_protocol::ids::MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: String,
        option_index: u32,
        input: Option<MenuInput>,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.menu_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        if let Err(message) = authorize(&self.capabilities, Operation::Control) {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                message,
                false,
                None,
            );
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "menu answers require a control attachment to this session",
                false,
                None,
            );
        }
        let (value, secret_reference) = match input {
            Some(MenuInput::Text { text }) => (Some(text), false),
            Some(MenuInput::SecretVaultReference { vault_reference }) => {
                (Some(vault_reference), true)
            }
            None => (None, false),
            Some(_) => {
                return self.menu_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "unknown menu input kind",
                    false,
                    None,
                );
            }
        };
        let answer = DurableMenuAnswer {
            menu: menu_id,
            option_key: (!option_key.is_empty()).then_some(option_key),
            option_index,
            value,
            via: AnswerVia::Rpc,
        };
        // Symmetric with `session_attach` (durable existence precedes actor
        // creation), so a bad session id can never mint a permanent actor.
        // Kept after the attachment-policy check to preserve that check's
        // pinned `capability_denied` for unattached callers.
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.menu_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let delegated_menu = self
            .delegated_menu_opening(&session_id, request_seq, &answer.menu)
            .await?;
        let command_menu = match self
            .command_menu_lookup(&session_id, request_seq, &answer.menu, &answer)
            .await?
        {
            CommandMenuLookup::Ordinary => None,
            CommandMenuLookup::Continuation(resolved) => Some(resolved),
            CommandMenuLookup::Invalid(message) => {
                return self.menu_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                );
            }
        };
        let actor = self.hub.actor_for(session_id.clone()).await?;
        let recovery_answer = answer.clone();
        let recovery_session = session_id.clone();
        let command = MenuResolutionCommand {
            command_id: command_id.0,
            session_id,
            request_seq,
            worker_generation,
            // The durable origin was validated above. Unlike a worker-owned
            // checkpoint, a command-door menu has no volatile harness
            // registration to recover after restart, so its exact opening
            // coordinates are the authority for prior-generation recovery.
            allow_prior_generation: command_menu.is_some() || delegated_menu,
            answer,
            device_id: self.hub.inner.device_id.clone(),
            input_is_secret_reference: secret_reference,
        };
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::MenuAnswer { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        match result.await.map_err(|_| SessionHubError::Closed)? {
            Ok(MenuResolutionOutcome::Committed {
                ref envelope,
                ref menu,
                ..
            }) => {
                if let MenuKind::Recovery { effect, .. } = &menu.kind {
                    let action =
                        super::actor::selected_effect_recovery_action(menu, &recovery_answer);
                    let follow_up = match action {
                        Some(EffectRecoveryAction::Probe) => {
                            self.hub
                                .probe_effect_outcome(
                                    recovery_session.clone(),
                                    effect.clone(),
                                    menu.id.clone(),
                                    envelope,
                                )
                                .await
                        }
                        Some(EffectRecoveryAction::Retry) => {
                            self.hub
                                .submit_effect_retry(
                                    recovery_session.clone(),
                                    effect.clone(),
                                    menu.id.clone(),
                                    envelope.seq,
                                )
                                .await
                        }
                        _ => Ok(()),
                    };
                    if let Err(error) = follow_up {
                        return self.menu_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            &error.message,
                            error.retryable,
                            None,
                        );
                    }
                }
                if let Some(resolved) = command_menu {
                    return self
                        .finish_command_menu(
                            request_id,
                            recovery_session,
                            &recovery_answer.menu,
                            resolved,
                            envelope.seq,
                        )
                        .await;
                }
                self.menu_success(request_id, envelope.seq)
            }
            Ok(MenuResolutionOutcome::IdempotentReplay { resolution_seq }) => {
                if let Some(resolved) = command_menu {
                    return self
                        .finish_command_menu(
                            request_id,
                            recovery_session,
                            &recovery_answer.menu,
                            resolved,
                            resolution_seq,
                        )
                        .await;
                }
                self.menu_success(request_id, resolution_seq)
            }
            Ok(MenuResolutionOutcome::AlreadyResolved { resolution_seq }) => self.menu_error(
                request_id,
                ERROR_CODE_ALREADY_RESOLVED,
                "menu was already resolved",
                false,
                Some(ErrorData::AlreadyResolved { resolution_seq }),
            ),
            Err(error) => {
                let code = match error.code {
                    ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
                    ErrorCode::MenuAlreadyAnswered => ERROR_CODE_ALREADY_RESOLVED,
                    ErrorCode::MenuNotFound | ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
                    _ => ERROR_CODE_INVALID_ARGUMENT,
                };
                self.menu_error(request_id, code, &error.message, error.retryable, None)
            }
        }
    }

    fn menu_success(
        &self,
        request_id: Option<RequestId>,
        resolution_seq: u64,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { resolution_seq },
            }),
            None => Ok(()),
        }
    }

    fn menu_error(
        &self,
        request_id: Option<RequestId>,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.respond_error(request_id, code, message, retryable, data),
            None => self.send(WireFrame::ProtocolError(ProtocolError {
                code: code.into(),
                message: message.into(),
                fatal: false,
                presentation: None,
                failed_write_ids: Vec::new(),
            })),
        }
    }

    fn respond_error(
        &self,
        request_id: RequestId,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                code: code.into(),
                message: message.into(),
                retryable,
                data,
            },
        })
    }

    fn respond_ssh_error(
        &self,
        request_id: RequestId,
        error: crate::ssh::SshError,
    ) -> Result<(), SessionHubError> {
        let retryable = matches!(
            &error,
            crate::ssh::SshError::SshConnection { .. }
                | crate::ssh::SshError::SshChannelQuota { .. }
                | crate::ssh::SshError::Vault { .. }
        );
        self.respond_error(
            request_id,
            error.code(),
            &error.to_string(),
            retryable,
            None,
        )
    }

    fn respond_shell_control_error(
        &self,
        request_id: RequestId,
        error: crate::shell_registry::ShellRegistryError,
    ) -> Result<(), SessionHubError> {
        let retryable = matches!(
            &error,
            crate::shell_registry::ShellRegistryError::ControlBusy(_)
        );
        let code = match &error {
            crate::shell_registry::ShellRegistryError::NotFound(_) => ERROR_CODE_NOT_FOUND,
            crate::shell_registry::ShellRegistryError::NotInteractive(_) => {
                ERROR_CODE_INVALID_ARGUMENT
            }
            crate::shell_registry::ShellRegistryError::ControlDenied(_) => {
                ERROR_CODE_CAPABILITY_DENIED
            }
            crate::shell_registry::ShellRegistryError::ControlBusy(_) => ERROR_CODE_OVERLOADED,
            crate::shell_registry::ShellRegistryError::ControlClosed(_) => "ssh_channel_closed",
            crate::shell_registry::ShellRegistryError::Poisoned
            | crate::shell_registry::ShellRegistryError::IdGeneration(_) => {
                return Err(SessionHubError::Task(error.to_string()));
            }
        };
        self.respond_error(request_id, code, &error.to_string(), retryable, None)
    }

    fn ssh_auth_from_wire(
        &self,
        request_id: &RequestId,
        store: &crate::ssh::SshProfileStore,
        name: &str,
        auth: haider_rpc::SshAuthInputWire,
    ) -> Result<Option<crate::ssh::SshAuth>, SessionHubError> {
        match auth {
            haider_rpc::SshAuthInputWire::KeyFile {
                path,
                passphrase_vault_reference: None,
            } => Ok(Some(crate::ssh::SshAuth::KeyFile {
                path,
                passphrase_vault_ref: None,
            })),
            haider_rpc::SshAuthInputWire::KeyFile {
                path,
                passphrase_vault_reference: Some(vault_reference),
            } => self
                .claim_ssh_secret_reference(
                    request_id,
                    store,
                    name,
                    vault_reference,
                    haider_rpc::StagePurpose::SshPassword,
                )
                .map(|reference| {
                    reference.map(|passphrase_vault_ref| crate::ssh::SshAuth::KeyFile {
                        path,
                        passphrase_vault_ref: Some(passphrase_vault_ref),
                    })
                }),
            haider_rpc::SshAuthInputWire::Agent => Ok(Some(crate::ssh::SshAuth::Agent)),
            haider_rpc::SshAuthInputWire::KeyMaterial { vault_reference } => self.claim_ssh_secret(
                request_id,
                store,
                name,
                vault_reference,
                haider_rpc::StagePurpose::SshKeyMaterial,
                true,
            ),
            haider_rpc::SshAuthInputWire::Password { vault_reference } => self.claim_ssh_secret(
                request_id,
                store,
                name,
                vault_reference,
                haider_rpc::StagePurpose::SshPassword,
                false,
            ),
        }
    }

    fn claim_ssh_secret(
        &self,
        request_id: &RequestId,
        store: &crate::ssh::SshProfileStore,
        name: &str,
        vault_reference: String,
        expected_purpose: haider_rpc::StagePurpose,
        key_material: bool,
    ) -> Result<Option<crate::ssh::SshAuth>, SessionHubError> {
        let reference = self.claim_ssh_secret_reference(
            request_id,
            store,
            name,
            vault_reference,
            expected_purpose,
        )?;
        Ok(reference.map(|vault_ref| {
            if key_material {
                crate::ssh::SshAuth::KeyMaterial { vault_ref }
            } else {
                crate::ssh::SshAuth::Password { vault_ref }
            }
        }))
    }

    fn claim_ssh_secret_reference(
        &self,
        request_id: &RequestId,
        store: &crate::ssh::SshProfileStore,
        name: &str,
        vault_reference: String,
        expected_purpose: haider_rpc::StagePurpose,
    ) -> Result<Option<String>, SessionHubError> {
        if self.secret_surface_facade(request_id)?.is_none() {
            return Ok(None);
        }
        let claimed = lock(&self.stages)?.claim(&vault_reference);
        let Some((purpose, secret)) = claimed else {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_RESTAGE_REQUIRED,
                "SSH credential stage is unknown, expired, or already consumed",
                false,
                None,
            )?;
            return Ok(None);
        };
        if purpose != expected_purpose {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_INVALID_ARGUMENT,
                "staged secret purpose does not match the SSH authentication method",
                false,
                None,
            )?;
            return Ok(None);
        }
        let vault_ref = match store.put_auth_secret(name, &secret) {
            Ok(vault_ref) => vault_ref,
            Err(error) => {
                self.respond_ssh_error(request_id.clone(), error)?;
                return Ok(None);
            }
        };
        Ok(Some(vault_ref))
    }

    fn respond_loom_revision_conflict(
        &self,
        request_id: RequestId,
        conflict: haider_protocol::loom::LoomRevisionConflict,
    ) -> Result<(), SessionHubError> {
        self.respond_error(
            request_id,
            ErrorCode::RevisionConflict.as_str(),
            "Loom registry revision fence did not match durable truth",
            false,
            Some(ErrorData::LoomRevisionConflict {
                expected: conflict.expected,
                current_rev: conflict.current_rev,
                current_digest: conflict.current_digest,
            }),
        )
    }

    fn send(&self, frame: WireFrame) -> Result<(), SessionHubError> {
        self.sink
            .try_send(frame)
            .map_err(|_| SessionHubError::Delivery)
    }

    /// Detaches every attachment owned by this connection and wipes every
    /// staged secret (R7: disconnect wipes all staged secrets; a secret a
    /// login command already claimed lives on with the command).
    pub async fn close(&self) -> Result<(), SessionHubError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.identity_lease.loom_author_cancel.send_replace(true);
        if let Ok(mut sessions) = self.loom_author_sessions.lock() {
            sessions.clear();
        }
        if let Ok(mut stages) = self.stages.lock() {
            *stages = crate::accounts::StagedSecrets::default();
        }
        if let Ok(mut watch) = self.roster_watch.lock()
            && let Some(task) = watch.take()
        {
            task.abort();
        }
        if let Ok(mut watch) = self.surface_watch.lock()
            && let Some(watch) = watch.take()
        {
            watch.task.abort();
        }
        if let Ok(mut watch) = self.monitor_watch.lock()
            && let Some(watch) = watch.take()
        {
            watch.cancel.send_replace(true);
            self.sink.purge_ordered(&watch.stream_id);
        }
        if let Ok(mut watch) = self.loom_registry_watch.lock()
            && let Some(watch) = watch.take()
        {
            watch.cancel.send_replace(true);
        }
        if let Ok(Some(facade)) = self.hub.accounts()
            && let Some(oauth) = facade.oauth
        {
            oauth.cancel_connection(&self.connection_id);
        }
        self.clear_resident_binding();
        self.hub.detach_connection(&self.connection_id).await
    }
}

/// One publication projection over the existing authorities. Built-in
/// templates remain byte-for-byte graph records; registered workflows remain
/// byte-for-byte Loom records. Eligibility classifies where a workflow may be
/// selected and does not bypass any graph or typed-agent execution gate.
fn published_workflow_catalog(
    workflows: &[haider_protocol::loom::LoomWorkflow],
) -> Vec<WorkflowCatalogEntryV1> {
    let mut catalog = haider_protocol::graph::built_in_workflow_catalog()
        .into_iter()
        .map(|entry| WorkflowCatalogEntryV1::BuiltIn {
            id: entry.template.name.clone(),
            main_session_eligible: entry.main_session_eligible,
            template: entry.template,
        })
        .collect::<Vec<_>>();
    catalog.extend(
        workflows
            .iter()
            .cloned()
            .map(|workflow| WorkflowCatalogEntryV1::User {
                id: workflow.id.clone(),
                main_session_eligible: true,
                workflow,
            }),
    );
    catalog
}

#[cfg(test)]
#[path = "rpc_workflow_catalog_tests.rs"]
mod workflow_catalog_tests;

#[cfg(test)]
#[path = "loom_registry_watch_tests.rs"]
mod loom_registry_watch_tests;

async fn run_surface_watch(
    hub: SessionHub,
    sink: Arc<dyn FrameSink>,
    registrations: Arc<Mutex<HashMap<SessionId, u64>>>,
    mut publications: watch::Receiver<u64>,
) {
    let period = std::time::Duration::from_secs(1);
    let mut ticker = interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = publications.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = ticker.tick() => {}
        }
        let watched = match registrations.lock() {
            Ok(registrations) => registrations
                .iter()
                .map(|(session_id, generation)| (session_id.clone(), *generation))
                .collect::<Vec<_>>(),
            Err(_) => return,
        };
        for (session_id, pushed_generation) in watched {
            // Generation compare happens under the surfaces lock; an idle
            // tick clones nothing.
            let Some(snapshot) = hub.surface_snapshot_if_changed(&session_id, pushed_generation)
            else {
                continue;
            };
            let sent = sink
                .try_send_droppable(WireFrame::SessionSurfaceDelta {
                    session_id: session_id.clone(),
                    input: snapshot.input,
                    status: snapshot.status,
                })
                .is_ok();
            if sent
                && let Ok(mut registrations) = registrations.lock()
                && registrations.get(&session_id) == Some(&pushed_generation)
            {
                registrations.insert(session_id, snapshot.change_generation);
            }
        }
    }
}

// Watch identity, replay bounds, publication source, and cancellation stay explicit.
#[allow(clippy::too_many_arguments)]
async fn run_monitor_watch(
    hub: SessionHub,
    sink: Arc<dyn FrameSink>,
    watch_id: String,
    session_id: SessionId,
    mut cursor: u64,
    initial_head: u64,
    mut publications: tokio::sync::broadcast::Receiver<SessionId>,
    mut cancel: watch::Receiver<bool>,
) {
    if !replay_monitor_delivery_range(
        &hub,
        &sink,
        &watch_id,
        &session_id,
        &mut cursor,
        initial_head,
        &mut cancel,
    )
    .await
    {
        return;
    }

    let period = std::time::Duration::from_secs(30);
    let mut ticker = interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let reconcile = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return;
                }
                false
            },
            received = publications.recv() => match received {
                Ok(changed) => changed == session_id,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = ticker.tick() => true,
        };
        if !reconcile {
            continue;
        }
        let head = match hub.inner.store.latest_seq(&session_id).await {
            Ok(head) => head,
            Err(_) => {
                sink.close_after_required_delivery_failure();
                return;
            }
        };
        if head < cursor {
            sink.close_after_required_delivery_failure();
            return;
        }
        if head == cursor {
            continue;
        }
        if !replay_monitor_delivery_range(
            &hub,
            &sink,
            &watch_id,
            &session_id,
            &mut cursor,
            head,
            &mut cancel,
        )
        .await
        {
            return;
        }
    }
}

/// Scans the complete durable interval, emitting only monitor pending facts.
/// Advancing across non-report envelopes is made visible by the caught-up
/// cursor, so reconnect never guesses whether an empty suffix was inspected.
async fn replay_monitor_delivery_range(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    watch_id: &str,
    session_id: &SessionId,
    cursor: &mut u64,
    high_water: u64,
    cancel: &mut watch::Receiver<bool>,
) -> bool {
    while *cursor < high_water {
        if *cancel.borrow() {
            return false;
        }
        let page = match hub
            .read_internal_session(session_id, *cursor, REPLAY_PAGE_SIZE)
            .await
        {
            Ok(page) => page,
            Err(_) => {
                sink.close_after_required_delivery_failure();
                return false;
            }
        };
        let mut advanced = false;
        for envelope in page
            .into_iter()
            .take_while(|envelope| envelope.seq <= high_water)
        {
            if *cancel.borrow() {
                return false;
            }
            let expected = (*cursor).saturating_add(1);
            if envelope.seq != expected {
                sink.close_after_required_delivery_failure();
                return false;
            }
            if let Some(report) = crate::monitor::monitor_delivery_report(&envelope) {
                let frame = WireFrame::MonitorDelivery {
                    watch_id: watch_id.to_owned(),
                    report,
                };
                match super::replay::deliver_ordered_frame(sink, watch_id, &frame, cancel).await {
                    super::replay::FrameDelivery::Delivered => {}
                    super::replay::FrameDelivery::Cancelled => return false,
                    super::replay::FrameDelivery::Stuck | super::replay::FrameDelivery::Refused => {
                        sink.close_after_required_delivery_failure();
                        return false;
                    }
                }
            }
            *cursor = envelope.seq;
            advanced = true;
        }
        if !advanced {
            sink.close_after_required_delivery_failure();
            return false;
        }
    }
    let caught_up = WireFrame::MonitorDeliveryCaughtUp {
        watch_id: watch_id.to_owned(),
        session_id: session_id.clone(),
        high_water_cursor: high_water,
    };
    match super::replay::deliver_ordered_frame(sink, watch_id, &caught_up, cancel).await {
        super::replay::FrameDelivery::Delivered => {}
        super::replay::FrameDelivery::Cancelled => return false,
        super::replay::FrameDelivery::Stuck | super::replay::FrameDelivery::Refused => {
            sink.close_after_required_delivery_failure();
            return false;
        }
    }
    true
}

/// Required-delivery registry stream. Publications are only wakeups: every
/// payload is rebuilt from the durable event log, and broadcast lag repairs
/// from the last delivered cursor instead of dropping registry facts.
struct LoomRegistryReplayWindow {
    after_cursor: u64,
    through_cursor: u64,
}

async fn run_loom_registry_watch(
    hub: SessionHub,
    sink: Arc<dyn FrameSink>,
    watch_id: String,
    replay: LoomRegistryReplayWindow,
    mut publications: tokio::sync::broadcast::Receiver<u64>,
    mut cancel: watch::Receiver<bool>,
) {
    // The snapshot repairs the client's projection at `initial_high_water`,
    // while replay from the requested cursor preserves the complete durable
    // transition history and cursor continuity. Seal even an empty initial
    // suffix so every successful attach receives an explicit caught-up fact.
    let mut cursor = replay.after_cursor;
    let mut initial_high_water = Some(replay.through_cursor);
    loop {
        let (high_water, seal_empty) = if let Some(high_water) = initial_high_water.take() {
            (high_water, true)
        } else {
            let high_water = tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return;
                    }
                    continue;
                },
                received = publications.recv() => match received {
                    Ok(cursor) => cursor,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        match hub.inner.store.loom_registry_head().await {
                            Ok(head) => head,
                            Err(_) => {
                                sink.close_after_required_delivery_failure();
                                return;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
            };
            (high_water, false)
        };
        if high_water < cursor || (high_water == cursor && !seal_empty) {
            continue;
        }
        while cursor < high_water {
            let page = match hub
                .inner
                .store
                .loom_registry_watch_page(cursor, high_water)
                .await
            {
                Ok(page) => page,
                Err(_) => {
                    sink.close_after_required_delivery_failure();
                    return;
                }
            };
            if page.replay_through_cursor != high_water || page.next_cursor <= cursor {
                sink.close_after_required_delivery_failure();
                return;
            }
            for delta in page.deltas {
                if delta.cursor != cursor.saturating_add(1) {
                    sink.close_after_required_delivery_failure();
                    return;
                }
                let next = delta.cursor;
                let frame = WireFrame::LoomRegistryDelta {
                    watch_id: watch_id.clone(),
                    delta,
                };
                match super::replay::deliver_ordered_frame(&sink, &watch_id, &frame, &mut cancel)
                    .await
                {
                    super::replay::FrameDelivery::Delivered => cursor = next,
                    super::replay::FrameDelivery::Cancelled => return,
                    super::replay::FrameDelivery::Stuck | super::replay::FrameDelivery::Refused => {
                        sink.close_after_required_delivery_failure();
                        return;
                    }
                }
            }
        }
        let caught_up = WireFrame::LoomRegistryCaughtUp {
            watch_id: watch_id.clone(),
            high_water_cursor: high_water,
        };
        match super::replay::deliver_ordered_frame(&sink, &watch_id, &caught_up, &mut cancel).await
        {
            super::replay::FrameDelivery::Delivered => {}
            super::replay::FrameDelivery::Cancelled => return,
            super::replay::FrameDelivery::Stuck | super::replay::FrameDelivery::Refused => {
                sink.close_after_required_delivery_failure();
                return;
            }
        }
    }
}

pub(crate) async fn session_summaries(
    hub: &SessionHub,
    session_ids: &[SessionId],
) -> Result<Vec<SessionSummary>, SessionHubError> {
    let recencies = hub
        .inner
        .store
        .session_recencies(session_ids.to_vec())
        .await?;
    let recencies = recencies
        .into_iter()
        .map(|row| {
            (
                row.key.session_id.as_str().to_owned(),
                row.key.last_activity_ms,
            )
        })
        .collect::<BTreeMap<_, _>>();
    session_summaries_with_recency(hub, session_ids, &recencies).await
}

async fn session_summaries_with_recency(
    hub: &SessionHub,
    session_ids: &[SessionId],
    durable_recencies: &BTreeMap<String, u64>,
) -> Result<Vec<SessionSummary>, SessionHubError> {
    let mut sessions = Vec::with_capacity(session_ids.len());
    for session_id in session_ids {
        if !hub.is_roster_visible(session_id)? {
            continue;
        }
        // Seal the head before metadata and accept the pair only when the
        // cached fold lands on that exact seal. A commit during either read
        // forces a retry, preventing OLD metadata from being published with
        // a NEW head that the watcher would then consider fully delivered.
        let (metadata, seen_at_ms, snapshot) = loop {
            let sealed_head = hub.inner.store.latest_seq(session_id).await?;
            let metadata = hub.inner.store.session_metadata(session_id).await?;
            let seen_at_ms = hub.inner.store.session_seen_at(session_id).await?;
            let initial_model = metadata
                .as_ref()
                .map_or("", |metadata| metadata.model.as_str());
            let snapshot = cached_observe_snapshot(hub, session_id, initial_model).await?;
            if snapshot.head_seq == sealed_head {
                break (metadata, seen_at_ms, snapshot);
            }
        };
        let head_seq = snapshot.head_seq;
        let turns = snapshot.turns;
        let footprint = snapshot.footprint;
        let run_state = snapshot.run_state;
        // Same selection as `run_state` — the pair is one observation.
        let run_id = snapshot.run_id.clone();
        // This is the exact key used by recency pagination. Every committed
        // live event already advances the durable journal head, so one
        // store-derived value keeps summaries and cursors consistent even
        // when historical timestamps regress.
        let last_activity_ms = durable_recencies.get(session_id.as_str()).copied();
        let waiting_why = waiting_why(run_state, &snapshot.pending_menus);
        let needs_input = needs_input(run_state, &snapshot.pending_menus);
        let (footprint_tokens, footprint_truth) =
            summary_footprint_fields(turns, footprint.as_ref());
        let agent_metrics = snapshot.agent_metrics;
        // Promotion, not a second calculation: both top-level values are
        // copied from the exact usage snapshot that remains published below.
        let cache_lifetime_hit_basis_points = agent_metrics
            .as_ref()
            .and_then(|metrics| metrics.usage.as_ref())
            .and_then(|usage| usage.cache_hit_basis_points);
        let cache_reread_hit_basis_points = agent_metrics
            .as_ref()
            .and_then(|metrics| metrics.usage.as_ref())
            .and_then(|usage| usage.cache_reread_hit_basis_points);
        // Promote the provider from the exact metadata value published below.
        // Keeping one source makes disagreement between the two locations
        // impossible.
        let provider = metadata.as_ref().map(|metadata| metadata.provider.clone());
        let account_alias = metadata
            .as_ref()
            .and_then(|metadata| metadata.account_alias.clone());
        let last_model = snapshot.last_model;
        let title = metadata
            .as_ref()
            .and_then(|metadata| metadata.title.clone());
        // Lineage truth (session_lineage_v1): the durable delegation record
        // is the authority — a session it names as child is a subagent with
        // that parent; any other session is a root. Never id-shape sniffing.
        let delegation = hub
            .inner
            .store
            .delegation_for_child_session(session_id.clone())
            .await?;
        let (kind, parent_session_id) = match delegation {
            Some(record) => (
                haider_rpc::SessionKindWire::Subagent,
                Some(record.parent_session_id),
            ),
            None => (haider_rpc::SessionKindWire::Root, None),
        };
        let metadata_agent_type = metadata
            .as_ref()
            .and_then(|metadata| metadata.agent_type.clone());
        let effort = metadata
            .as_ref()
            .and_then(|metadata| metadata.effort.clone());
        let fast = metadata.as_ref().map(|metadata| metadata.fast);
        let forked_from = if metadata.is_some() {
            hub.inner
                .store
                .session_fork_provenance(session_id.clone())
                .await?
        } else {
            None
        };
        sessions.push(SessionSummary {
            session_id: session_id.clone(),
            head_seq,
            worker_generation: hub.inner.store.worker_generation(),
            run_state: Some(run_state),
            run_id,
            seen_at_ms,
            last_activity_ms,
            waiting_why,
            needs_input,
            workspace_cwd: metadata.as_ref().map(|metadata| metadata.cwd.clone()),
            metadata,
            provider,
            last_model,
            cache_lifetime_hit_basis_points,
            cache_reread_hit_basis_points,
            turn_count: Some(turns),
            footprint_tokens,
            footprint_truth,
            title,
            agent_metrics,
            parent_session_id,
            kind: Some(kind),
            agent_type: metadata_agent_type,
            effort,
            fast,
            account_alias,
            forked_from,
        });
    }
    Ok(sessions)
}

/// v0.0.937 unified input-required contract: whenever the run state is
/// parked on a human, project the oldest answerable pending menu into ONE
/// typed, secret-free card carrying its exact `menu.answer` coordinates.
/// A parked state with no menu still reports a kind (from the state) so a
/// surface can badge it, just without an answerable card.
fn needs_input(
    run_state: haider_rpc::ObserveRunStateWire,
    pending_menus: &[haider_rpc::ObserveMenuWire],
) -> Option<haider_rpc::NeedsInputWire> {
    use haider_rpc::{NeedsInputKindWire, NeedsInputWire, ObserveRunStateWire};

    let parked = matches!(
        run_state,
        ObserveRunStateWire::ParkedPermission
            | ObserveRunStateWire::ParkedInput
            | ObserveRunStateWire::EffectUnknown
    );
    if !parked {
        return None;
    }
    let menu = pending_menus
        .iter()
        .min_by_key(|menu| menu.request_seq.unwrap_or(u64::MAX));
    let kind = match menu.map(|menu| menu.kind.as_str()) {
        Some("permission") => NeedsInputKindWire::Permission,
        Some("question") => NeedsInputKindWire::Question,
        Some("recovery" | "error_recovery") => NeedsInputKindWire::Recovery,
        Some("secret") => NeedsInputKindWire::Secret,
        Some("update") => NeedsInputKindWire::Update,
        Some("trust_hook") => NeedsInputKindWire::TrustHook,
        Some("choice") => NeedsInputKindWire::Choice,
        Some("conflict") => NeedsInputKindWire::Conflict,
        Some("file") => NeedsInputKindWire::File,
        Some("exhausted") => NeedsInputKindWire::Exhausted,
        Some("graph_human_confirm" | "graph_abandon_confirm") => NeedsInputKindWire::Approval,
        Some(_) => NeedsInputKindWire::Unknown,
        // Parked with no visible menu: badgeable, not answerable.
        None => match run_state {
            ObserveRunStateWire::ParkedPermission => NeedsInputKindWire::Permission,
            ObserveRunStateWire::EffectUnknown => NeedsInputKindWire::Recovery,
            _ => NeedsInputKindWire::Question,
        },
    };
    let secret_answer = matches!(kind, NeedsInputKindWire::Secret);
    Some(NeedsInputWire {
        kind,
        title: menu
            .map(|menu| menu.title.clone())
            .unwrap_or_else(|| match run_state {
                ObserveRunStateWire::EffectUnknown => "Effect outcome unknown".to_owned(),
                _ => "Input required".to_owned(),
            }),
        safe_body: menu.map(|menu| menu.body.clone()).unwrap_or_default(),
        menu_id: menu.and_then(|menu| menu.menu_id.clone()),
        request_seq: menu.and_then(|menu| menu.request_seq),
        worker_generation: menu.and_then(|menu| menu.worker_generation),
        since_ms: menu.and_then(|menu| menu.opened_at_ms),
        options: menu.map(|menu| menu.options.clone()).unwrap_or_default(),
        secret_answer,
    })
}

fn waiting_why(
    run_state: haider_rpc::ObserveRunStateWire,
    pending_menus: &[haider_rpc::ObserveMenuWire],
) -> Option<haider_rpc::WaitingWhyWire> {
    use haider_rpc::{WaitingWhyKindWire, WaitingWhyWire};

    let menu_for = |predicate: fn(&str) -> bool| {
        pending_menus
            .iter()
            .find(|menu| predicate(&menu.kind))
            .or_else(|| pending_menus.first())
    };
    match run_state {
        haider_rpc::ObserveRunStateWire::ParkedPermission => {
            let menu = menu_for(|kind| kind == "permission");
            Some(WaitingWhyWire {
                kind: WaitingWhyKindWire::Permission,
                pending_menu_id: menu.and_then(|menu| menu.menu_id.clone()),
            })
        }
        haider_rpc::ObserveRunStateWire::ParkedInput => {
            let menu = menu_for(|_| true);
            let kind = menu.map_or(WaitingWhyKindWire::Question, |menu| {
                if matches!(
                    menu.kind.as_str(),
                    "graph_human_confirm" | "graph_abandon_confirm" | "trust_hook" | "update"
                ) {
                    WaitingWhyKindWire::Approval
                } else {
                    WaitingWhyKindWire::Question
                }
            });
            Some(WaitingWhyWire {
                kind,
                pending_menu_id: menu.and_then(|menu| menu.menu_id.clone()),
            })
        }
        _ => None,
    }
}

const ROSTER_DELTA_CHUNK_SIZE: usize = 64;

fn strip_input_controls(text: String) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

fn strip_status_controls(line: String) -> String {
    line.chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn roster_fold_candidates(
    pushed_heads: &BTreeMap<String, u64>,
    current_heads: &[(SessionId, u64)],
) -> Vec<SessionId> {
    current_heads
        .iter()
        .filter(|(session_id, head_seq)| pushed_heads.get(session_id.as_str()) != Some(head_seq))
        .map(|(session_id, _)| session_id.clone())
        .collect()
}

fn push_roster_chunks(
    sink: &dyn FrameSink,
    summaries: Vec<SessionSummary>,
    pushed_heads: &mut BTreeMap<String, u64>,
) {
    let mut summaries = summaries.into_iter();
    loop {
        let chunk = summaries
            .by_ref()
            .take(ROSTER_DELTA_CHUNK_SIZE)
            .collect::<Vec<_>>();
        if chunk.is_empty() {
            break;
        }
        let sent_heads = chunk
            .iter()
            .map(|summary| (summary.session_id.as_str().to_owned(), summary.head_seq))
            .collect::<Vec<_>>();
        if sink
            .try_send_droppable(WireFrame::SessionRosterDelta { summaries: chunk })
            .is_ok()
        {
            pushed_heads.extend(sent_heads);
        }
        // A refused chunk leaves its heads unchanged, so every member is
        // selected and folded again on the next tick. Later independent
        // chunks may still make progress.
    }
}

fn standard_base64_decoded_len(encoded: &str) -> Result<usize, &'static str> {
    let bytes = encoded.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("artifact.put data_base64 must use padded RFC 4648 encoding");
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2
        || bytes[..bytes.len().saturating_sub(padding)]
            .iter()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(*byte, b'+' | b'/')))
        || bytes[..bytes.len().saturating_sub(padding)].contains(&b'=')
    {
        return Err("artifact.put data_base64 is not standard RFC 4648 base64");
    }
    Ok(bytes.len() / 4 * 3 - padding)
}

/// The ONE title-normalization seam (G2): control characters stripped,
/// trimmed, capped at 80 characters, empty collapses to `None`. The store
/// transaction re-asserts these bounds.
fn normalize_session_title(title: Option<String>) -> Option<String> {
    let cleaned: String = title?.chars().filter(|c| !c.is_control()).collect();
    let capped: String = cleaned.trim().chars().take(80).collect();
    let capped = capped.trim_end();
    if capped.is_empty() {
        None
    } else {
        Some(capped.to_owned())
    }
}

fn validate_metafork_review_shape(
    command_id: &CommandId,
    fork_node_id: &haider_protocol::ids::NodeId,
    fork_seq: u64,
    description: &str,
    proposal: &haider_protocol::session_fork::SessionMetaforkProposal,
) -> Result<(), String> {
    if command_id.as_str().is_empty()
        || fork_node_id.as_str().is_empty()
        || fork_seq == 0
        || description.trim().is_empty()
        || description.len() > 16 * 1024
        || proposal.removals.is_empty()
        || proposal.removals.len() > 256
    {
        return Err(
            "metafork command, fork coordinate, description, and model proposal must be valid"
                .into(),
        );
    }
    for (index, removal) in proposal.removals.iter().enumerate() {
        if removal.from_seq == 0
            || removal.through_seq < removal.from_seq
            || removal.reason.trim().is_empty()
            || removal.reason.len() > 4 * 1024
            || removal
                .preview
                .as_ref()
                .is_some_and(|preview| preview.trim().is_empty() || preview.len() > 2 * 1024)
            || proposal.removals.iter().take(index).any(|prior| {
                removal.from_seq <= prior.through_seq && prior.from_seq <= removal.through_seq
            })
        {
            return Err(
                "metafork removal ranges must be bounded, non-overlapping, and carry reasons"
                    .into(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod roster_wave_tests {
    use super::*;
    use std::sync::Mutex;

    struct ChunkSink {
        calls: Mutex<Vec<usize>>,
        fail_call: usize,
    }

    impl FrameSink for ChunkSink {
        fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
            Err(FrameSendError)
        }

        fn try_send_droppable(&self, frame: WireFrame) -> Result<(), FrameSendError> {
            let WireFrame::SessionRosterDelta { summaries } = frame else {
                return Err(FrameSendError);
            };
            let mut calls = self.calls.lock().map_err(|_| FrameSendError)?;
            calls.push(summaries.len());
            if calls.len() == self.fail_call {
                Err(FrameSendError)
            } else {
                Ok(())
            }
        }
    }

    fn summary(index: usize) -> SessionSummary {
        SessionSummary {
            session_id: SessionId::new(format!("session-{index:03}")),
            head_seq: u64::try_from(index).expect("test index fits") + 1,
            worker_generation: 1,
            metadata: None,
            provider: None,
            last_model: None,
            cache_lifetime_hit_basis_points: None,
            cache_reread_hit_basis_points: None,
            workspace_cwd: None,
            turn_count: None,
            footprint_tokens: None,
            footprint_truth: None,
            title: None,
            agent_metrics: None,
            parent_session_id: None,
            kind: None,
            agent_type: None,
            run_state: None,
            run_id: None,
            seen_at_ms: None,
            last_activity_ms: None,
            waiting_why: None,
            needs_input: None,
            effort: None,
            fast: None,
            account_alias: None,
            forked_from: None,
        }
    }

    #[test]
    fn unchanged_roster_heads_select_no_summary_folds() {
        let pushed_heads = BTreeMap::from([("session-000".to_owned(), 7)]);
        let current_heads = vec![(SessionId::new("session-000"), 7)];

        assert!(roster_fold_candidates(&pushed_heads, &current_heads).is_empty());
    }

    #[test]
    fn sixty_five_roster_rows_chunk_and_only_successful_chunks_advance_heads() {
        let summaries = (0..65).map(summary).collect::<Vec<_>>();
        let current_heads = summaries
            .iter()
            .map(|summary| (summary.session_id.clone(), summary.head_seq))
            .collect::<Vec<_>>();
        let sink = ChunkSink {
            calls: Mutex::new(Vec::new()),
            fail_call: 2,
        };
        let mut pushed_heads = BTreeMap::new();

        push_roster_chunks(&sink, summaries, &mut pushed_heads);

        assert_eq!(*sink.calls.lock().expect("calls lock"), vec![64, 1]);
        assert_eq!(pushed_heads.len(), 64);
        assert_eq!(
            roster_fold_candidates(&pushed_heads, &current_heads),
            vec![SessionId::new("session-064")]
        );
    }
}

/// Daemon-side mirror of the TUI's `slug_name` (G2 auto-title): first three
/// whitespace words, joined by `-`, lowercased, `[a-z0-9-]` only, at most
/// 28 characters, fallback `session`.
fn auto_title_slug(text: &str) -> String {
    let joined = text
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    let slug: String = joined
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(28)
        .collect();
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

/// Stable, globally unique input for a legacy receipt's auto-title repair.
///
/// Receipt command IDs are globally unique inside a profile. Hashing that
/// identity keeps the synthetic event ID bounded while preserving the same
/// value on every replay.
pub(super) fn replay_title_user_event_id(command_id: &CommandId) -> EventId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.turn-title-replay.v1\0");
    hasher.update(command_id.as_str().as_bytes());
    EventId::new(format!(
        "receipt-replay-user-{}",
        hasher.finalize().to_hex()
    ))
}

/// The capability→delivery JOIN: the single decision that turns a session
/// provider's PDF capability into the delivery mode journaled on the user
/// message. Pinned directly (wd_pdf_runtime_tests) because the capability
/// table and the downstream shaping are each pinned yet an inverted join
/// survived both.
pub(crate) fn pdf_delivery_for_provider(provider: &str) -> haider_protocol::tool::PdfDeliveryMode {
    if haider_provider::pdf_document_capability(provider)
        == haider_protocol::provider::FeatureResolve::Native
    {
        haider_protocol::tool::PdfDeliveryMode::NativeDocument
    } else {
        haider_protocol::tool::PdfDeliveryMode::ExtractedText
    }
}

async fn validate_turn_attachments(
    store: &haider_core::SqliteStoreHandle,
    attachments: &[haider_protocol::tool::AttachmentBlock],
    pdf_delivery: haider_protocol::tool::PdfDeliveryMode,
) -> Result<Vec<haider_protocol::tool::AttachmentBlock>, AttachmentValidationFailure> {
    if attachments.len() > MAX_ATTACHMENTS_PER_TURN {
        let actual_count = u32::try_from(attachments.len()).unwrap_or(u32::MAX);
        return Err(AttachmentValidationFailure {
            code: ERROR_CODE_TOO_MANY_ATTACHMENTS,
            message: format!(
                "turn carries {actual_count} attachments; the limit is {MAX_ATTACHMENTS_PER_TURN}"
            ),
            data: Some(ErrorData::TooManyAttachments {
                actual_count,
                max_count: MAX_ATTACHMENTS_PER_TURN as u32,
            }),
        });
    }

    let mut total_bytes = 0_usize;
    let aggregate_limit = if attachments.iter().any(|attachment| {
        matches!(
            attachment,
            haider_protocol::tool::AttachmentBlock::Pdf { .. }
        )
    }) {
        // PDF is the only attachment lane whose typed per-file budget is
        // larger than the historical 16 MiB turn aggregate. Keep the legacy
        // law exact for every pre-PDF turn.
        MAX_ATTACHMENT_BYTES_PER_PDF_TURN
    } else {
        MAX_ATTACHMENT_BYTES_PER_TURN
    };
    let mut canonical = Vec::with_capacity(attachments.len());
    for (index, attachment) in attachments.iter().enumerate() {
        let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
        let artifact = match attachment {
            haider_protocol::tool::AttachmentBlock::Image { artifact, mime, .. } => {
                if !IMAGE_ATTACHMENT_MIME_ALLOWLIST.contains(&mime.as_str()) {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED,
                        message: format!(
                            "attachment {index} declares unsupported image MIME `{mime}`; use image/jpeg, image/png, image/gif, or image/webp"
                        ),
                        data: Some(ErrorData::AttachmentMimeUnsupported {
                            index: index_u32,
                            mime: mime.clone(),
                        }),
                    });
                }
                artifact
            }
            haider_protocol::tool::AttachmentBlock::PastedText { artifact, .. } => artifact,
            haider_protocol::tool::AttachmentBlock::File { artifact, name, .. } => {
                // Name sanity (G2): a display basename, never a path and
                // never terminal-control bytes. The cap mirrors the client
                // loader; violation is a client bug, refused honestly.
                if name.is_empty()
                    || name.chars().count() > 120
                    || name.chars().any(char::is_control)
                    || name.contains('/')
                    || name.contains('\\')
                {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "attachment {index} declares an invalid file name; names are non-empty basenames of at most 120 characters with no control characters"
                        ),
                        data: None,
                    });
                }
                artifact
            }
            haider_protocol::tool::AttachmentBlock::Pdf { artifact, name, .. } => {
                if name.is_empty()
                    || name.chars().count() > 120
                    || name.chars().any(char::is_control)
                    || name.contains('/')
                    || name.contains('\\')
                {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "attachment {index} declares an invalid PDF name; names are non-empty basenames of at most 120 characters with no control characters"
                        ),
                        data: None,
                    });
                }
                artifact
            }
            haider_protocol::tool::AttachmentBlock::Skill { name, .. } => {
                return Err(AttachmentValidationFailure {
                    code: ERROR_CODE_INVALID_ARGUMENT,
                    message: format!("skill attachment `{name}` is reserved but not yet supported"),
                    data: None,
                });
            }
        };
        let missing = || AttachmentValidationFailure {
            code: ERROR_CODE_ATTACHMENT_NOT_FOUND,
            message: format!(
                "attachment {index} references unavailable or unverified artifact {artifact}; upload it with artifact.put and retry"
            ),
            data: Some(ErrorData::AttachmentNotFound {
                index: index_u32,
                artifact: artifact.clone(),
            }),
        };
        let reader = store
            .open_cas_reader(artifact)
            .await
            .map_err(|_| missing())?;
        let actual_bytes = reader.metadata().map_err(|_| missing())?.len();
        let is_pdf = matches!(
            attachment,
            haider_protocol::tool::AttachmentBlock::Pdf { .. }
        );
        let cap = if is_pdf {
            haider_pdf::MAX_PDF_BYTES
        } else {
            MAX_ATTACHMENT_BYTES
        };
        if is_pdf && actual_bytes > haider_pdf::MAX_PDF_BYTES as u64 {
            let message = format!(
                "PDF attachment {index} is {actual_bytes} bytes; the PDF limit is {}",
                haider_pdf::MAX_PDF_BYTES
            );
            let presentation = haider_protocol::error::ErrorPresentation::new(
                "pdf-too-large",
                "PDF is too large",
                &message,
                haider_protocol::error::ErrorScope::Turn,
                [haider_protocol::error::ErrorAction::None],
            );
            return Err(AttachmentValidationFailure {
                code: ERROR_CODE_PDF_TOO_LARGE,
                message,
                data: Some(ErrorData::PdfTooLarge {
                    index: index_u32,
                    artifact: artifact.clone(),
                    actual_bytes,
                    max_bytes: haider_pdf::MAX_PDF_BYTES as u64,
                    presentation,
                }),
            });
        }
        if !is_pdf && actual_bytes > cap as u64 {
            return Err(oversized_attachment(
                index_u32,
                artifact,
                usize::try_from(actual_bytes).unwrap_or(usize::MAX),
            ));
        }
        // The verified file handle is read in bounded chunks. PDF inspection
        // still requires contiguous bytes, but only after enforcing its cap.
        use tokio::io::AsyncReadExt as _;
        let mut reader = tokio::fs::File::from_std(reader).take(cap as u64 + 1);
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| missing())?;
        if bytes.len() as u64 != actual_bytes {
            return Err(missing());
        }
        let canonical_attachment = match attachment {
            haider_protocol::tool::AttachmentBlock::Pdf { name, .. } => {
                let metadata = haider_pdf::inspect_pdf(&bytes).map_err(|error| {
                    let message = format!("PDF attachment {index} could not be parsed: {error}");
                    AttachmentValidationFailure {
                        code: ERROR_CODE_PDF_MALFORMED,
                        data: Some(ErrorData::PdfMalformed {
                            index: index_u32,
                            artifact: artifact.clone(),
                            presentation: haider_protocol::error::ErrorPresentation::new(
                                "pdf-malformed",
                                "PDF could not be read",
                                &message,
                                haider_protocol::error::ErrorScope::Turn,
                                [haider_protocol::error::ErrorAction::None],
                            ),
                        }),
                        message,
                    }
                })?;
                if metadata.pages > haider_pdf::MAX_PDF_PAGES {
                    let message = format!(
                        "PDF attachment {index} has {} pages; the limit is {} pages",
                        metadata.pages,
                        haider_pdf::MAX_PDF_PAGES
                    );
                    let presentation = haider_protocol::error::ErrorPresentation::new(
                        "pdf-too-many-pages",
                        "PDF has too many pages",
                        &message,
                        haider_protocol::error::ErrorScope::Turn,
                        [haider_protocol::error::ErrorAction::None],
                    );
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_PDF_TOO_MANY_PAGES,
                        message,
                        data: Some(ErrorData::PdfTooManyPages {
                            index: index_u32,
                            artifact: artifact.clone(),
                            actual_pages: metadata.pages,
                            max_pages: haider_pdf::MAX_PDF_PAGES,
                            presentation,
                        }),
                    });
                }
                haider_protocol::tool::AttachmentBlock::Pdf {
                    artifact: artifact.clone(),
                    name: name.clone(),
                    pages: metadata.pages,
                    delivery: pdf_delivery,
                }
            }
            haider_protocol::tool::AttachmentBlock::File { .. } => {
                if bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Err(oversized_attachment(index_u32, artifact, bytes.len()));
                }
                if std::str::from_utf8(&bytes).is_err() {
                    return Err(AttachmentValidationFailure {
                        code: ERROR_CODE_INVALID_ARGUMENT,
                        message: format!(
                            "attachment {index} is not UTF-8 text; only UTF-8 text files can be attached (unsupported_attachment_encoding)"
                        ),
                        data: None,
                    });
                }
                attachment.clone()
            }
            _ => {
                if bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Err(oversized_attachment(index_u32, artifact, bytes.len()));
                }
                attachment.clone()
            }
        };
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > aggregate_limit {
            let actual_bytes = u64::try_from(total_bytes).unwrap_or(u64::MAX);
            return Err(AttachmentValidationFailure {
                code: ERROR_CODE_ATTACHMENTS_TOO_LARGE,
                message: format!(
                    "turn attachments total {actual_bytes} bytes; the aggregate limit is {aggregate_limit}"
                ),
                data: Some(ErrorData::AttachmentsTooLarge {
                    actual_bytes,
                    max_bytes: aggregate_limit as u64,
                }),
            });
        }
        canonical.push(canonical_attachment);
    }
    Ok(canonical)
}

fn oversized_attachment(
    index: u32,
    artifact: &haider_protocol::ids::ArtifactRef,
    bytes: usize,
) -> AttachmentValidationFailure {
    let actual_bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    AttachmentValidationFailure {
        code: ERROR_CODE_ATTACHMENT_TOO_LARGE,
        message: format!(
            "attachment {index} is {actual_bytes} bytes; the per-attachment limit is {MAX_ATTACHMENT_BYTES}"
        ),
        data: Some(ErrorData::AttachmentTooLarge {
            index,
            artifact: artifact.clone(),
            actual_bytes,
            max_bytes: MAX_ATTACHMENT_BYTES as u64,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    View,
    Control,
}

fn authorize(capabilities: &CapabilitySet, operation: Operation) -> Result<(), &'static str> {
    let allowed = match operation {
        Operation::View => {
            capabilities.contains(&Capability::View) || capabilities.contains(&Capability::Control)
        }
        Operation::Control => capabilities.contains(&Capability::Control),
    };
    allowed.then_some(()).ok_or(match operation {
        Operation::View => "this method requires the view capability",
        Operation::Control => "this method requires the control capability",
    })
}

fn agent_cancel_command(
    record: &haider_core::DelegationRecord,
    command_id: &str,
    reason: &str,
    worker_generation: u64,
    device_id: DeviceId,
) -> Result<TurnCancelCommand, HaiderError> {
    let request_json = serde_json::to_string(&serde_json::json!({
        "parent_session_id": record.parent_session_id,
        "agent": record.agent_id,
        "child_session_id": record.child_session_id,
        "child_run_id": record.child_run_id,
        "worker_generation": worker_generation,
        "reason": reason,
    }))
    .map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("agent cancellation coordinates could not serialize: {error}"),
            false,
        )
    })?;
    let event_identity =
        blake3::hash(format!("{command_id}:{}:{reason}", record.agent_id.as_str()).as_bytes());
    Ok(TurnCancelCommand {
        command_id: command_id.to_owned(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        session_id: record.child_session_id.clone(),
        worker_generation,
        run_id: record.child_run_id.clone(),
        cancelling_event_id: EventId::new(format!("agent-cancelling-{event_identity}")),
        device_id,
    })
}

fn valid_device_candidate_id(candidate: &str) -> bool {
    candidate.len() == 68
        && candidate.starts_with("dc1_")
        && candidate.as_bytes()[4..].iter().all(u8::is_ascii_hexdigit)
}

struct ValidatedWorkspace {
    canonical: String,
    descriptor: std::fs::File,
}

#[cfg(unix)]
fn open_workspace_descriptor(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

#[cfg(windows)]
fn open_workspace_descriptor(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

async fn validate_workspace(cwd: String) -> Result<ValidatedWorkspace, String> {
    if !std::path::Path::new(&cwd).is_absolute() {
        return Err("session cwd must be an absolute path".into());
    }
    tokio::task::spawn_blocking(move || {
        let canonical = std::fs::canonicalize(&cwd)
            .map_err(|error| format!("cannot canonicalize session cwd: {error}"))?;
        let canonical_text = canonical
            .to_str()
            .ok_or_else(|| "canonical session cwd is not valid UTF-8".to_owned())?
            .to_owned();
        let metadata = std::fs::metadata(&canonical)
            .map_err(|error| format!("cannot inspect session cwd: {error}"))?;
        if !metadata.is_dir() {
            return Err("session cwd must identify a directory".into());
        }
        let descriptor = open_workspace_descriptor(&canonical)
            .map_err(|error| format!("cannot open session cwd: {error}"))?;
        Ok(ValidatedWorkspace {
            canonical: canonical_text,
            descriptor,
        })
    })
    .await
    .map_err(|error| format!("session cwd validation task failed: {error}"))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod error_wave3_tests {
    use super::*;

    #[tokio::test]
    async fn session_workspace_validation_keeps_an_open_directory_handle() {
        let workspace = tempfile::tempdir().expect("workspace");
        let requested = workspace.path().display().to_string();
        let validated = validate_workspace(requested)
            .await
            .expect("directory workspace validates");
        assert_eq!(
            validated.canonical,
            std::fs::canonicalize(workspace.path())
                .expect("canonical workspace")
                .to_str()
                .expect("UTF-8 workspace")
        );
        assert!(
            validated
                .descriptor
                .metadata()
                .expect("open directory metadata")
                .is_dir()
        );
    }

    #[tokio::test]
    async fn e6a_probe_reexecutes_the_filesystem_check() {
        let root = tempfile::tempdir().expect("workspace");
        let target = root.path().join("outcome.txt");
        std::fs::write(&target, b"first").expect("first write");
        let intent = EffectIntent {
            effect: haider_protocol::ids::EffectId::new("effect-e6a-probe"),
            class: EffectClass::FsWrite,
            summary: "write outcome.txt".into(),
            args_digest: "args-e6a".into(),
            workspace_revision: None,
        };

        let first =
            effect_probe_observation(&intent, root.path().to_str().expect("utf8 workspace"), None)
                .await;
        std::fs::write(&target, b"second content").expect("second write");
        let second =
            effect_probe_observation(&intent, root.path().to_str().expect("utf8 workspace"), None)
                .await;

        assert!(first.contains("5 bytes"), "{first}");
        assert!(second.contains("14 bytes"), "{second}");
        assert_ne!(first, second, "probe must inspect current state each time");
    }
}

/// W-flow (owner 2026-08-22) — the active run's IDENTITY on every observation
/// surface, so a client can cancel the run it is rendering.
#[cfg(test)]
#[allow(clippy::expect_used)]
mod run_identity_tests {
    use super::*;
    use haider_protocol::EventPayload;
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::graph::{
        GraphAttemptOpened, GraphPhase, GraphPinned, GraphSuperseded, SHIP_LOOP_TEMPLATE,
        STAGGERED_TEMPLATE, graph_template, graph_template_digest,
    };
    use haider_protocol::ids::{DeviceId, EventId, GraphId};

    fn state_envelope(run: &str, seq: u64, state: RunState) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("run-identity-{run}-{seq}")),
            seq,
            session_id: SessionId::new("session-run-identity"),
            branch_id: None,
            run_id: Some(RunId::new(run)),
            agent_id: None,
            device_id: DeviceId::new("run-identity-test"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: seq,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(EventPayload::RunState(state))
                .expect("state serializes")
                .into(),
        }
    }

    fn digest_of(envelopes: Vec<RawEnvelope>) -> haider_rpc::SessionObserveDigest {
        let mut projection = ObserveProjection::new(8);
        let head = envelopes.len() as u64;
        for envelope in envelopes {
            projection.apply(envelope);
        }
        projection.finish(SessionId::new("session-run-identity"), head, 1, None)
    }

    #[test]
    fn observe_projection_bounds_terminal_run_history() {
        let mut projection = ObserveProjection::new(8);
        let total = MAX_OBSERVED_RUNS_PER_SESSION + 5;
        for ordinal in 0..total {
            projection.apply(state_envelope(
                &format!("terminal-{ordinal}"),
                ordinal as u64 + 1,
                RunState::Done,
            ));
        }
        assert_eq!(projection.runs.len(), MAX_OBSERVED_RUNS_PER_SESSION);
        assert!(!projection.runs.contains_key(&RunId::new("terminal-0")));
        assert!(
            projection
                .runs
                .contains_key(&RunId::new(format!("terminal-{}", total - 1)))
        );
    }

    #[test]
    fn provider_operation_reservation_does_not_replace_the_observed_conversation_run() {
        let visible_failure = state_envelope("visible-failure", 1, RunState::Errored);
        let mut reservation = state_envelope("loom-authoring", 2, RunState::Done);
        reservation.render.ui = false;
        reservation.payload = ProviderOperationEventPayload::ProviderOperationReserved {
            request_kind: ProviderRequestKind::Side,
        }
        .to_payload_value()
        .expect("provider operation reservation")
        .into();

        let digest = digest_of(vec![visible_failure, reservation]);
        assert_eq!(
            digest.run_state,
            haider_rpc::ObserveRunStateWire::Errored,
            "a provider-support ordinal reservation must not mask the visible failure"
        );
        assert_eq!(
            digest.run_id.as_ref().map(RunId::as_str),
            Some("visible-failure"),
            "the hidden operation id must not escape through session observation"
        );
    }

    fn graph_envelope(seq: u64, payload: EventPayload) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("workflow-projection-{seq}")),
            seq,
            session_id: SessionId::new("session-run-identity"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("workflow-projection-test"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: seq,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload)
                .expect("graph fact serializes")
                .into(),
        }
    }

    fn pinned_graph_events(
        graph_id: &GraphId,
        template_name: &str,
        pin_seq: u64,
    ) -> Vec<RawEnvelope> {
        let template = graph_template(template_name).expect("built-in graph template exists");
        let start_node = template
            .start_node
            .clone()
            .expect("built-in graph template has a start node");
        let digest = graph_template_digest(&template);
        vec![
            graph_envelope(
                pin_seq,
                EventPayload::GraphPinned(GraphPinned {
                    graph_id: graph_id.clone(),
                    template: template.name,
                    digest,
                    template_version: template.version,
                    start_node: template.start_node,
                    nodes: template.nodes,
                }),
            ),
            graph_envelope(
                pin_seq + 1,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: graph_id.clone(),
                    node: start_node,
                    attempt: 1,
                }),
            ),
        ]
    }

    #[test]
    fn observe_digest_projects_the_session_pinned_workflow() {
        let graph_id = GraphId::new("graph-observe-pin");
        let expected_template = graph_template(SHIP_LOOP_TEMPLATE).expect("template exists");
        let expected_digest = graph_template_digest(&expected_template);

        let digest = digest_of(pinned_graph_events(&graph_id, SHIP_LOOP_TEMPLATE, 1));
        let workflow = digest
            .workflow
            .expect("the observer projects the session's pinned workflow");

        assert_eq!(workflow.graph_id, graph_id);
        assert_eq!(workflow.template, SHIP_LOOP_TEMPLATE);
        assert_eq!(workflow.digest, expected_digest);
        assert_eq!(workflow.phase, GraphPhase::Active);
        assert_eq!(workflow.current_node, expected_template.start_node);
        assert_eq!(workflow.attempt, 1);
    }

    #[test]
    fn observe_digest_selects_the_replacement_workflow_from_one_switch_fold() {
        let old_graph_id = GraphId::new("graph-observe-old");
        let new_graph_id = GraphId::new("graph-observe-new");
        let mut envelopes = pinned_graph_events(&old_graph_id, SHIP_LOOP_TEMPLATE, 1);
        envelopes.push(graph_envelope(
            3,
            EventPayload::GraphSuperseded(GraphSuperseded {
                old: old_graph_id.clone(),
                new: new_graph_id.clone(),
            }),
        ));
        envelopes.extend(pinned_graph_events(&new_graph_id, STAGGERED_TEMPLATE, 4));

        let mut projection = ObserveProjection::new(8);
        for envelope in envelopes {
            projection.apply(envelope);
        }
        let old_status = projection
            .graphs
            .graph(&old_graph_id)
            .and_then(|reduction| reduction.status.as_ref())
            .expect("the superseded workflow remains queryable in the same fold");
        assert_eq!(old_status.phase, GraphPhase::Superseded);

        let digest = projection.finish(SessionId::new("session-run-identity"), 5, 1, None);
        let workflow = digest
            .workflow
            .expect("the observer selects the replacement pinned workflow");
        let expected_template = graph_template(STAGGERED_TEMPLATE).expect("template exists");

        assert_eq!(workflow.graph_id, new_graph_id);
        assert_eq!(workflow.template, STAGGERED_TEMPLATE);
        assert_eq!(workflow.digest, graph_template_digest(&expected_template));
        assert_eq!(workflow.phase, GraphPhase::Active);
        assert_eq!(workflow.current_node, expected_template.start_node);
        assert_eq!(workflow.attempt, 1);
    }

    /// The id and the state are ONE observation. A client told "running" must
    /// be told *which* run is running, or its stop button cancels whatever
    /// happens to be live when the call lands rather than what it displayed.
    ///
    /// MUTATION CHECK (executed): make `select_observed_run` return the id of
    /// a different entry than the one it returns the state for — e.g. pair
    /// `runs.keys().next()` with the selected run. Expected RUNTIME failure:
    /// the same-selection assertion below, since the ranked selector must
    /// pick the PARKED run while an unranked pick lands on either.
    #[test]
    fn the_reported_run_id_names_the_run_the_state_describes() {
        // Two live runs. The selector ranks a parked run above a plain
        // running one regardless of seq, so an id taken from anywhere else
        // in the map would disagree with the state.
        let digest = digest_of(vec![
            state_envelope("run-parked", 1, RunState::EffectOutcomeUnknown),
            state_envelope("run-plain", 2, RunState::Streaming),
        ]);
        assert_eq!(
            digest.run_state,
            haider_rpc::ObserveRunStateWire::EffectUnknown,
            "the parked run outranks the newer plain one"
        );
        assert_eq!(
            digest.run_id.as_ref().map(RunId::as_str),
            Some("run-parked"),
            "the id must name the SAME run the state describes"
        );
    }

    /// MUTATION CHECK (executed): report an id for a settled session (drop
    /// the `Idle` correlation by always emitting the newest run's key).
    /// Expected RUNTIME failure: the terminal-run assertion — a stop button
    /// would appear on a session with nothing to stop.
    #[test]
    fn a_session_with_no_live_run_reports_no_run_id() {
        // No runs at all.
        let empty = digest_of(Vec::new());
        assert_eq!(empty.run_state, haider_rpc::ObserveRunStateWire::Idle);
        assert!(
            empty.run_id.is_none(),
            "an unstarted session has no run to cancel"
        );
    }

    /// The `metadata_only` fast path skips the projection entirely, so it
    /// reports no run id — and that ABSENCE is honest rather than a claim of
    /// idleness. Pinned so nobody later "fixes" it into a lie.
    ///
    /// MUTATION CHECK (executed): have the metadata-only branch fabricate a
    /// run id. Expected RUNTIME failure: the assertion below.
    #[test]
    fn the_metadata_only_fast_path_reports_no_run_id() {
        let mut projection = ObserveProjection::new(8);
        projection.title = Some("titled".to_owned());
        let digest = projection.finish(SessionId::new("session-run-identity"), 42, 1, None);
        assert!(
            digest.run_id.is_none(),
            "the fast path projects nothing, so it must claim nothing"
        );
        assert_eq!(digest.head_seq, 42, "authoritative fields still hold");
    }

    /// The wire field is additive: an older daemon omits it entirely, and a
    /// client must decode that as "no active run", never as a decode error.
    ///
    /// MUTATION CHECK (executed): drop `#[serde(default)]` from
    /// `SessionSummary::run_id`. Expected RUNTIME failure: the decode below.
    #[test]
    fn an_older_daemons_summary_still_decodes() {
        let legacy = serde_json::json!({
            "session_id": "session-legacy",
            "head_seq": 7,
            "worker_generation": 3,
        });
        let summary: SessionSummary =
            serde_json::from_value(legacy).expect("a summary without run_id must decode");
        assert!(summary.run_id.is_none(), "absent decodes as no active run");
    }
}

#[path = "provider_rebind.rs"]
mod provider_rebind;
