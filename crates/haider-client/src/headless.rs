//! Reusable daemon-backed one-shot transaction for non-interactive clients.
//!
//! This module owns execution ordering, durable command retries, cursor replay,
//! fail-closed permission handling, and terminal reduction. A lossless
//! forwarding spool decouples that control plane from the caller's bounded
//! [`HeadlessEvent`] stream; presentation owns only delivery and formatting.

#[cfg(test)]
#[path = "headless_tests.rs"]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::{Future, pending};
use std::io::Read as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use haider_rpc::haider_protocol::EventPayload;
use haider_rpc::haider_protocol::envelope::RawEnvelope;
use haider_rpc::haider_protocol::error::ErrorCode;
use haider_rpc::haider_protocol::ids::{ArtifactRef, MenuId, RunId, SessionId};
use haider_rpc::haider_protocol::item::{ItemEvent, TurnItem};
use haider_rpc::haider_protocol::menu::{DecisionKind, MenuKind};
use haider_rpc::haider_protocol::provider::Usage;
use haider_rpc::haider_protocol::session::SessionPermissionOverridesV1;
use haider_rpc::haider_protocol::state::RunState;
use haider_rpc::haider_protocol::tool::AttachmentBlock;
use haider_rpc::{
    AttachMode, AttachmentId, Capability, CapabilitySet, ClientKind, CommandId,
    ERROR_CODE_ALREADY_RESOLVED, FEATURE_ARTIFACT_PUT_V1, FEATURE_SESSION_PERMISSION_OVERRIDES_V1,
    RequestBody, ResponseBody, WireFrame,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::client::{ClientConfig, ClientError, ConnectionState, DisconnectReason, RpcClient};
use crate::profile::ResolvedProfile;
use crate::spawn::{EnsureError, EnsureOptions, ensure_daemon, required_live_features};

/// Default time allowed for a durable cancellation to reach a correlated
/// terminal after timeout or blocked-input detection.
pub const DEFAULT_TERMINAL_GRACE: Duration = Duration::from_secs(2);

/// No daemon account descriptor is currently active for headless bootstrap.
pub const ERROR_CODE_NO_ACTIVE_ACCOUNT: &str = "no_active_account";
/// The selected provider publishes neither a default nor a fallback model.
pub const ERROR_CODE_NO_DEFAULT_MODEL: &str = "no_default_model";

const MAX_RECONNECTS: u8 = 3;
const ATTACH_HEALTH_POLL: Duration = Duration::from_millis(10);
static COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const MAX_HEADLESS_ATTACHMENTS: usize = 5;
const MAX_HEADLESS_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// One magic-sniffed image ready for receipt-free daemon upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessImageAttachment {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// One UTF-8 text file ready for receipt-free daemon upload (G2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessFileAttachment {
    pub bytes: Vec<u8>,
    /// Sanitized display BASENAME (never a full path — privacy): ≤ 120
    /// chars, control characters stripped, `file` when nothing survives.
    pub name: String,
    /// Text line count, the chip/wire display figure.
    pub lines: u32,
}

/// One loaded attachment of either supported kind (G2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessAttachment {
    Image(HeadlessImageAttachment),
    File(HeadlessFileAttachment),
}

impl HeadlessAttachment {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Image(image) => &image.bytes,
            Self::File(file) => &file.bytes,
        }
    }
}

impl From<HeadlessImageAttachment> for HeadlessAttachment {
    fn from(image: HeadlessImageAttachment) -> Self {
        Self::Image(image)
    }
}

impl From<HeadlessFileAttachment> for HeadlessAttachment {
    fn from(file: HeadlessFileAttachment) -> Self {
        Self::File(file)
    }
}

/// Reads at most the accepted image size plus one byte and identifies the
/// format from magic bytes rather than the path extension.
pub fn load_image_attachment(path: &Path) -> Result<HeadlessImageAttachment, HeadlessRunError> {
    let file = std::fs::File::open(path).map_err(|error| HeadlessRunError::Attachment {
        code: "attachment_io".into(),
        message: format!("cannot open attachment {}: {error}", path.display()),
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_HEADLESS_ATTACHMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| HeadlessRunError::Attachment {
            code: "attachment_io".into(),
            message: format!("cannot read attachment {}: {error}", path.display()),
        })?;
    if bytes.len() > MAX_HEADLESS_ATTACHMENT_BYTES {
        return Err(HeadlessRunError::Attachment {
            code: "attachment_too_large".into(),
            message: format!(
                "attachment {} exceeds the 5 MiB per-attachment limit",
                path.display()
            ),
        });
    }
    let mime = sniff_image_mime(&bytes).ok_or_else(|| HeadlessRunError::Attachment {
        code: "unsupported_attachment_type".into(),
        message: format!(
            "attachment {} is not a JPEG, PNG, GIF, or WebP image",
            path.display()
        ),
    })?;
    Ok(HeadlessImageAttachment {
        bytes,
        mime: mime.into(),
    })
}

/// Reads at most the accepted attachment size plus one byte and validates
/// strict UTF-8 (G2). PDFs and other binary formats are not supported: a
/// non-UTF-8 payload is a DISTINCT typed refusal
/// (`unsupported_attachment_encoding`), never a lossy re-encode.
pub fn load_text_attachment(path: &Path) -> Result<HeadlessFileAttachment, HeadlessRunError> {
    let file = std::fs::File::open(path).map_err(|error| HeadlessRunError::Attachment {
        code: "attachment_io".into(),
        message: format!("cannot open attachment {}: {error}", path.display()),
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_HEADLESS_ATTACHMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| HeadlessRunError::Attachment {
            code: "attachment_io".into(),
            message: format!("cannot read attachment {}: {error}", path.display()),
        })?;
    if bytes.len() > MAX_HEADLESS_ATTACHMENT_BYTES {
        return Err(HeadlessRunError::Attachment {
            code: "attachment_too_large".into(),
            message: format!(
                "attachment {} exceeds the 5 MiB per-attachment limit",
                path.display()
            ),
        });
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| HeadlessRunError::Attachment {
        code: "unsupported_attachment_encoding".into(),
        message: format!(
            "attachment {} is not UTF-8 text (PDFs and other binary formats are not supported)",
            path.display()
        ),
    })?;
    let lines = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
    let name = sanitize_attachment_name(
        &path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
    );
    Ok(HeadlessFileAttachment { bytes, name, lines })
}

/// The ONE name-sanitizing seam (G2): basename in, ≤ 120 chars out, control
/// characters stripped, `file` when nothing survives. The daemon re-checks
/// the same bounds at validation.
fn sanitize_attachment_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "file".to_owned()
    } else {
        cleaned
    }
}

fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// One daemon-backed run request.
#[derive(Debug, Clone)]
pub struct HeadlessRunRequest {
    pub cwd: String,
    pub prompt: String,
    pub attachments: Vec<HeadlessAttachment>,
    /// Explicit provider override. `None` follows the daemon's active account.
    pub provider: Option<String>,
    /// Explicit model override. `None` follows the selected provider summary.
    pub model: Option<String>,
    pub max_tokens: u64,
    pub permission_overrides: SessionPermissionOverridesV1,
    /// Trust discovered hooks for this run only. The daemon journals the
    /// grant in the same atomic acceptance transaction as the turn.
    pub trust_hooks: bool,
    pub timeout: Option<Duration>,
    pub terminal_grace: Duration,
}

/// Incremental facts exposed to output adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum HeadlessEvent {
    /// One fully applied durable envelope. Duplicates and gap-crossing frames
    /// are never emitted.
    Envelope(Box<RawEnvelope>),
    /// Observable fail-closed decision made for a permission menu.
    PermissionDenied(HeadlessPermissionDenial),
}

/// Machine-readable permission denial made by the headless default policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessPermissionDenial {
    pub menu_id: String,
    pub effect_summary: String,
    pub notice: String,
}

/// Stable final outcome categories consumed by non-interactive surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadlessOutcome {
    Done,
    Errored,
    Cancelled,
    Timeout,
    InputRequired,
}

/// Why a non-interactive run could not honestly continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessBlockingReason {
    InputRequired,
    EffectOutcomeUnknown,
    PermissionRejectUnavailable,
    PermissionResolutionConflict,
}

impl HeadlessBlockingReason {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::InputRequired => "input_required",
            Self::EffectOutcomeUnknown => "effect_outcome_unknown",
            Self::PermissionRejectUnavailable => "permission_reject_unavailable",
            Self::PermissionResolutionConflict => "permission_resolution_conflict",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::InputRequired => "run requires interactive input",
            Self::EffectOutcomeUnknown => {
                "run requires interactive recovery for an unknown effect outcome"
            }
            Self::PermissionRejectUnavailable => {
                "permission menu did not enumerate a RejectOnce decision"
            }
            Self::PermissionResolutionConflict => {
                "permission menu was resolved with a decision other than the selected RejectOnce"
            }
        }
    }
}

/// Typed terminal failure reduced from the correlated durable stream.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessRunFailure {
    pub code: HeadlessFailureCode,
    pub message: String,
    pub retryable: bool,
}

/// Failure-code source retained without string-parsing durable run codes.
#[derive(Debug, Clone, PartialEq)]
pub enum HeadlessFailureCode {
    Run(ErrorCode),
    Rpc(String),
    Timeout,
    Blocked(HeadlessBlockingReason),
    Internal,
}

impl HeadlessFailureCode {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Run(code) => error_code_name(*code),
            Self::Rpc(code) => code,
            Self::Timeout => "timeout",
            Self::Blocked(reason) => reason.code(),
            Self::Internal => "internal",
        }
    }
}

/// One background task still running when the headless run finished (W-A
/// decision 8): `haider run` exits when the TURN completes; the daemon
/// keeps ownership and the task dies with the session per the fence law.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessBackgroundTask {
    pub task_id: String,
    pub name: String,
}

/// Correlated result of one accepted daemon run.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessRunResult {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub provider: String,
    pub model: String,
    pub attachments: Vec<ArtifactRef>,
    pub outcome: HeadlessOutcome,
    pub response: Option<String>,
    pub usage: Option<Usage>,
    pub permission_denials: Vec<HeadlessPermissionDenial>,
    pub failure: Option<HeadlessRunFailure>,
    pub terminal_seq: Option<u64>,
    /// Background tasks with a durable started fact and no completed fact
    /// when the run's terminal was observed (W-A decision 8).
    pub background_tasks_running: Vec<HeadlessBackgroundTask>,
}

/// Failure before a correlated final result could be produced.
#[derive(Debug)]
pub enum HeadlessRunError {
    Attachment {
        code: String,
        message: String,
    },
    Ensure(EnsureError),
    Transport {
        stage: &'static str,
        reason: DisconnectReason,
    },
    Encode {
        stage: &'static str,
        message: String,
    },
    Rpc {
        stage: &'static str,
        code: String,
        message: String,
        retryable: bool,
    },
    Bootstrap {
        stage: &'static str,
        code: &'static str,
        message: String,
        retryable: bool,
    },
    Protocol {
        stage: &'static str,
        message: String,
    },
}

impl std::fmt::Display for HeadlessRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Attachment { code, message } => {
                write!(formatter, "headless attachment failed ({code}): {message}")
            }
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::Transport { stage, reason } => {
                write!(
                    formatter,
                    "daemon transport failed during {stage}: {reason}"
                )
            }
            Self::Encode { stage, message } => {
                write!(formatter, "cannot encode {stage} request: {message}")
            }
            Self::Rpc {
                stage,
                code,
                message,
                ..
            } => write!(formatter, "daemon rejected {stage} ({code}): {message}"),
            Self::Bootstrap {
                stage,
                code,
                message,
                ..
            } => write!(
                formatter,
                "headless bootstrap failed during {stage} ({code}): {message}"
            ),
            Self::Protocol { stage, message } => {
                write!(
                    formatter,
                    "unexpected daemon response during {stage}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for HeadlessRunError {}

struct HeadlessConnection {
    client: RpcClient,
    events: mpsc::Receiver<WireFrame>,
    attachment_id: Option<AttachmentId>,
    worker_generation: u64,
    observed_lost_events: u64,
}

fn headless_submit_body(
    trust_hooks: bool,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    text: String,
    attachments: Vec<AttachmentBlock>,
) -> RequestBody {
    if trust_hooks {
        RequestBody::TurnSubmitWithHookTrust {
            command_id,
            session_id,
            worker_generation,
            branch_id: None,
            text,
            attachments,
            mode: haider_rpc::haider_protocol::DeliveryMode::Queue,
        }
    } else {
        RequestBody::TurnSubmit {
            command_id,
            session_id,
            worker_generation,
            text,
            attachments,
            mode: haider_rpc::haider_protocol::DeliveryMode::Queue,
        }
    }
}

impl HeadlessConnection {
    async fn open(
        profile: &ResolvedProfile,
        options: EnsureOptions,
    ) -> Result<Self, HeadlessRunError> {
        let ensured = ensure_daemon(profile, options)
            .await
            .map_err(HeadlessRunError::Ensure)?;
        let events = ensured
            .client
            .take_events()
            .ok_or_else(|| HeadlessRunError::Protocol {
                stage: "connect",
                message: "headless runner could not take the daemon event stream".into(),
            })?;
        let observed_lost_events = ensured.client.lost_events();
        Ok(Self {
            client: ensured.client,
            events,
            attachment_id: None,
            worker_generation: 0,
            observed_lost_events,
        })
    }
}

#[derive(Debug, Clone)]
enum ReducerAction {
    RejectPermission {
        command_id: CommandId,
        menu_id: MenuId,
        request_seq: u64,
        option_key: String,
        option_index: u32,
    },
    Block(HeadlessBlockingReason),
}

#[derive(Debug, Clone)]
struct NaturalTerminal {
    outcome: HeadlessOutcome,
    failure: Option<HeadlessRunFailure>,
    seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableMenuResolution {
    Answer {
        option_key: Option<String>,
        option_index: u32,
    },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyStatus {
    Duplicate,
    Gap,
    Applied,
}

struct HeadlessReducer {
    session_id: SessionId,
    run_id: Option<RunId>,
    last_applied: u64,
    response: Option<String>,
    usage: Option<Usage>,
    permission_denials: Vec<HeadlessPermissionDenial>,
    pending_run_failure: Option<(u64, HeadlessRunFailure)>,
    terminal: Option<NaturalTerminal>,
    menu_resolutions: BTreeMap<String, DurableMenuResolution>,
    cancel_observed: bool,
    actions: VecDeque<ReducerAction>,
    output: mpsc::UnboundedSender<HeadlessEvent>,
    output_closed: bool,
    /// W-A: task id → (name, running) from the additive task facts, in
    /// deterministic id order for the run summary.
    background_tasks: BTreeMap<String, (String, bool)>,
}

impl HeadlessReducer {
    fn new(session_id: SessionId, output: mpsc::UnboundedSender<HeadlessEvent>) -> Self {
        Self {
            session_id,
            run_id: None,
            last_applied: 0,
            response: None,
            usage: None,
            permission_denials: Vec::new(),
            pending_run_failure: None,
            terminal: None,
            menu_resolutions: BTreeMap::new(),
            cancel_observed: false,
            actions: VecDeque::new(),
            background_tasks: BTreeMap::new(),
            output,
            output_closed: false,
        }
    }

    fn is_correlated(&self, envelope: &RawEnvelope) -> bool {
        self.run_id
            .as_ref()
            .is_some_and(|run_id| envelope.run_id.as_ref() == Some(run_id))
    }

    async fn emit(&mut self, event: HeadlessEvent) {
        if !self.output_closed && self.output.send(event).is_err() {
            self.output_closed = true;
        }
    }

    async fn apply(&mut self, envelope: RawEnvelope) -> ApplyStatus {
        if envelope.session_id != self.session_id || envelope.seq <= self.last_applied {
            return ApplyStatus::Duplicate;
        }
        let Some(expected) = self.last_applied.checked_add(1) else {
            return ApplyStatus::Gap;
        };
        if envelope.seq != expected {
            return ApplyStatus::Gap;
        }

        self.emit(HeadlessEvent::Envelope(Box::new(envelope.clone())))
            .await;
        self.last_applied = envelope.seq;
        let correlated = self.is_correlated(&envelope);
        // W-A: background task facts are SESSION-scoped (they outlive turns
        // by design) and ride the additive union outside `EventPayload` —
        // track them regardless of run correlation so the run summary can
        // name still-running tasks honestly at exit.
        if let Some(fact) = haider_rpc::haider_protocol::task::TaskEventPayload::from_payload_value(
            &envelope.payload,
        ) {
            match fact {
                haider_rpc::haider_protocol::task::TaskEventPayload::TaskStarted(started) => {
                    self.background_tasks
                        .entry(started.task.as_str().to_owned())
                        .or_insert((started.name, true));
                }
                haider_rpc::haider_protocol::task::TaskEventPayload::TaskCompleted(completed) => {
                    self.background_tasks
                        .insert(completed.task.as_str().to_owned(), (completed.name, false));
                }
            }
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
            return ApplyStatus::Applied;
        };
        if !correlated {
            return ApplyStatus::Applied;
        }

        match payload {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => self.response = Some(text),
            EventPayload::Usage(usage) => self.usage = Some(usage),
            EventPayload::RunFailed {
                code,
                message,
                retryable,
            } => {
                self.pending_run_failure = Some((
                    envelope.seq,
                    HeadlessRunFailure {
                        code: HeadlessFailureCode::Run(code),
                        message,
                        retryable,
                    },
                ));
            }
            EventPayload::RunState(state) => self.reduce_run_state(state, envelope.seq),
            EventPayload::MenuOpened(menu) => match menu.kind {
                MenuKind::Permission { effect_summary } => {
                    let selected = menu
                        .options
                        .iter()
                        .enumerate()
                        .find(|(_, option)| option.decision == Some(DecisionKind::RejectOnce));
                    let Some((index, option)) = selected else {
                        self.actions.push_back(ReducerAction::Block(
                            HeadlessBlockingReason::PermissionRejectUnavailable,
                        ));
                        return ApplyStatus::Applied;
                    };
                    let Ok(option_index) = u32::try_from(index) else {
                        self.actions.push_back(ReducerAction::Block(
                            HeadlessBlockingReason::PermissionRejectUnavailable,
                        ));
                        return ApplyStatus::Applied;
                    };
                    let denial = HeadlessPermissionDenial {
                        menu_id: menu.id.as_str().to_owned(),
                        effect_summary,
                        notice: "permission_denied_by_headless_default".into(),
                    };
                    self.permission_denials.push(denial.clone());
                    self.emit(HeadlessEvent::PermissionDenied(denial)).await;
                    self.actions.push_back(ReducerAction::RejectPermission {
                        command_id: CommandId::new(command_id("headless-menu")),
                        menu_id: menu.id,
                        request_seq: envelope.seq,
                        option_key: option.key.clone(),
                        option_index,
                    });
                }
                _ => self
                    .actions
                    .push_back(ReducerAction::Block(HeadlessBlockingReason::InputRequired)),
            },
            EventPayload::MenuAnswered(answer) => {
                self.menu_resolutions.insert(
                    answer.menu.as_str().to_owned(),
                    DurableMenuResolution::Answer {
                        option_key: answer.option_key,
                        option_index: answer.option_index,
                    },
                );
            }
            EventPayload::MenuClosed { menu, .. } => {
                self.menu_resolutions
                    .insert(menu.as_str().to_owned(), DurableMenuResolution::Closed);
            }
            _ => {}
        }
        ApplyStatus::Applied
    }

    fn reduce_run_state(&mut self, state: RunState, seq: u64) {
        match state {
            RunState::Done => {
                self.terminal = Some(NaturalTerminal {
                    outcome: HeadlessOutcome::Done,
                    failure: None,
                    seq,
                });
            }
            RunState::Cancelled => {
                self.cancel_observed = true;
                self.terminal = Some(NaturalTerminal {
                    outcome: HeadlessOutcome::Cancelled,
                    failure: None,
                    seq,
                });
            }
            RunState::Errored => {
                let failure = self
                    .pending_run_failure
                    .take()
                    .filter(|(failure_seq, _)| failure_seq.saturating_add(1) == seq)
                    .map(|(_, failure)| failure)
                    .unwrap_or_else(|| HeadlessRunFailure {
                        code: HeadlessFailureCode::Internal,
                        message: "errored run had no adjacent correlated RunFailed".into(),
                        retryable: false,
                    });
                self.terminal = Some(NaturalTerminal {
                    outcome: HeadlessOutcome::Errored,
                    failure: Some(failure),
                    seq,
                });
            }
            RunState::InputRequired { .. } => self
                .actions
                .push_back(ReducerAction::Block(HeadlessBlockingReason::InputRequired)),
            RunState::EffectOutcomeUnknown => self.actions.push_back(ReducerAction::Block(
                HeadlessBlockingReason::EffectOutcomeUnknown,
            )),
            RunState::Cancelling => self.cancel_observed = true,
            RunState::Queued
            | RunState::Thinking
            | RunState::Streaming
            | RunState::RunningTool
            | RunState::Waiting { .. }
            // M4: a retry wait is mid-run, non-terminal work — it never blocks
            // or ends a headless run (and never a terminal notification).
            | RunState::Retrying { .. }
            | RunState::PermissionRequired { .. }
            | RunState::Compacting
            | RunState::Verifying { .. }
            | RunState::Concluding => {}
        }
    }
}

#[derive(Debug, Clone)]
enum ForcedOutcome {
    Timeout,
    Blocked(HeadlessBlockingReason),
}

#[derive(Debug, Clone)]
struct CancelCommand {
    command_id: CommandId,
}

struct ReconnectBudget {
    remaining: u8,
}

impl ReconnectBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_RECONNECTS,
        }
    }

    fn spend(
        &mut self,
        stage: &'static str,
        reason: DisconnectReason,
    ) -> Result<(), HeadlessRunError> {
        if self.remaining == 0 {
            return Err(HeadlessRunError::Transport { stage, reason });
        }
        self.remaining -= 1;
        Ok(())
    }
}

/// Runs create → Control attach → submit → cursor reduction to a correlated
/// terminal, reconnecting and replaying from the last fully applied sequence.
pub async fn run_headless(
    profile: &ResolvedProfile,
    ensure: EnsureOptions,
    request: HeadlessRunRequest,
    output: mpsc::Sender<HeadlessEvent>,
) -> Result<HeadlessRunResult, HeadlessRunError> {
    let (reducer_output, mut pending_output) = mpsc::unbounded_channel();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = pending_output.recv().await {
            if output.send(event).await.is_err() {
                break;
            }
        }
    });
    let result = run_headless_inner(profile, ensure, request, reducer_output).await;
    let _ = forwarder.await;
    result
}

async fn run_headless_inner(
    profile: &ResolvedProfile,
    mut ensure: EnsureOptions,
    request: HeadlessRunRequest,
    output: mpsc::UnboundedSender<HeadlessEvent>,
) -> Result<HeadlessRunResult, HeadlessRunError> {
    if request.attachments.len() > MAX_HEADLESS_ATTACHMENTS {
        return Err(HeadlessRunError::Attachment {
            code: "too_many_attachments".into(),
            message: format!(
                "headless run carries {} attachments; the limit is {MAX_HEADLESS_ATTACHMENTS}",
                request.attachments.len()
            ),
        });
    }
    normalize_ensure_options(
        &mut ensure,
        request.permission_overrides,
        !request.attachments.is_empty(),
        request.trust_hooks,
    );
    let timeout_deadline = request.timeout.map(|timeout| Instant::now() + timeout);
    let submit_command_id = CommandId::new(command_id("headless-submit"));
    let mut reconnects = ReconnectBudget::new();
    let mut connection = before_acceptance_deadline(
        timeout_deadline,
        "connect",
        HeadlessConnection::open(profile, ensure.clone()),
    )
    .await?;
    let (provider, model) = before_acceptance_deadline(
        timeout_deadline,
        "identity bootstrap",
        resolve_run_identity(
            profile,
            &ensure,
            &mut connection,
            &mut reconnects,
            request.provider.clone(),
            request.model.clone(),
        ),
    )
    .await?;
    let (submit_attachments, attachment_refs) = before_acceptance_deadline(
        timeout_deadline,
        "artifact.put",
        upload_attachments(
            profile,
            &ensure,
            &mut connection,
            &mut reconnects,
            &request.attachments,
        ),
    )
    .await?;
    let create_body = RequestBody::SessionCreateWithPermissionOverrides {
        command_id: CommandId::new(command_id("headless-create")),
        cwd: request.cwd.clone(),
        provider: provider.clone(),
        model: model.clone(),
        max_tokens: request.max_tokens,
        permission_overrides: (!request.permission_overrides.is_empty())
            .then_some(request.permission_overrides),
        cache_policy: None,
    };

    let (session_id, created_generation) =
        before_acceptance_deadline(timeout_deadline, "session.create", async {
            loop {
                match connection.client.request(create_body.clone()).await {
                    Ok(ResponseBody::SessionCreate {
                        session_id,
                        worker_generation,
                        metadata,
                        ..
                    }) => {
                        let expected = (!request.permission_overrides.is_empty())
                            .then_some(request.permission_overrides);
                        if metadata.permission_overrides != expected {
                            return Err(HeadlessRunError::Protocol {
                                stage: "session.create",
                                message:
                                    "daemon did not persist the requested permission overrides"
                                        .into(),
                            });
                        }
                        break Ok((session_id, worker_generation));
                    }
                    Ok(ResponseBody::Error {
                        code,
                        message,
                        retryable,
                        ..
                    }) => break Err(rpc_error("session.create", code, message, retryable)),
                    Ok(_) => {
                        break Err(protocol_error(
                            "session.create",
                            "response method did not match request",
                        ));
                    }
                    Err(error) => {
                        reconnect_before_session(
                            profile,
                            &ensure,
                            &mut connection,
                            &mut reconnects,
                            "session.create",
                            error,
                        )
                        .await?;
                    }
                }
            }
        })
        .await?;

    let mut reducer = HeadlessReducer::new(session_id.clone(), output);
    connection.worker_generation = created_generation;
    before_acceptance_deadline(
        timeout_deadline,
        "session.attach",
        attach_with_recovery(
            profile,
            &ensure,
            &mut connection,
            &mut reducer,
            &mut reconnects,
        ),
    )
    .await?;

    // This body is immutable across response-loss retries. In particular, its
    // original generation remains part of the durable command identity even if
    // reconnecting observes a newer worker generation.
    let submit_body = headless_submit_body(
        request.trust_hooks,
        submit_command_id,
        session_id.clone(),
        connection.worker_generation,
        request.prompt,
        submit_attachments,
    );
    let mut buffered = Vec::new();
    let mut submit_timeout_grace = None;
    let run_id = loop {
        let pending_response = match connection.client.begin_request(submit_body.clone()).await {
            Ok(pending_response) => pending_response,
            Err(error) => {
                buffered.clear();
                reconnect_for_submit(
                    profile,
                    &ensure,
                    &mut connection,
                    &mut reducer,
                    &mut reconnects,
                    &mut buffered,
                    "turn.submit",
                    error,
                )
                .await?;
                continue;
            }
        };
        let wait = pending_response.wait();
        tokio::pin!(wait);
        let response = loop {
            let response_deadline = submit_timeout_grace.or(timeout_deadline);
            tokio::select! {
                // W9b review fix: BIASED — a response that resolved in the
                // same wake as the peer's close must win the tie, or an
                // answers-then-closes daemon costs a spurious reconnect
                // (and the test double's listener may be gone).
                biased;
                response = &mut wait => break response,
                frame = connection.events.recv() => {
                    let Some(frame) = frame else {
                        break Err(ClientError::Disconnected(connection_reason(&connection.client)));
                    };
                    if frame_matches_attachment(&frame, connection.attachment_id.as_ref()) {
                        buffered.push(frame);
                    }
                }
                () = wait_until(response_deadline) => {
                    if submit_timeout_grace.is_some() {
                        return Err(cancellation_unconfirmed("turn.submit recovery"));
                    }
                    submit_timeout_grace = Some(Instant::now() + request.terminal_grace);
                    connection.client.close();
                    break Err(ClientError::Disconnected(DisconnectReason::Closed));
                }
            }
        };
        match response {
            Ok(ResponseBody::TurnSubmit {
                session_id: accepted_session,
                run_id,
                worker_generation,
                ..
            }) if accepted_session == session_id => {
                connection.worker_generation = worker_generation;
                break run_id;
            }
            Ok(ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            }) => return Err(rpc_error("turn.submit", code, message, retryable)),
            Ok(_) => {
                return Err(protocol_error(
                    "turn.submit",
                    "response coordinates did not match the created session",
                ));
            }
            Err(error) => {
                buffered.clear();
                let reconnect = reconnect_for_submit(
                    profile,
                    &ensure,
                    &mut connection,
                    &mut reducer,
                    &mut reconnects,
                    &mut buffered,
                    "turn.submit",
                    error,
                );
                if let Some(deadline) = submit_timeout_grace {
                    tokio::time::timeout_at(deadline, reconnect)
                        .await
                        .map_err(|_| cancellation_unconfirmed("turn.submit recovery"))??;
                } else {
                    reconnect.await?;
                }
            }
        }
    };
    reducer.run_id = Some(run_id.clone());

    let mut forced = submit_timeout_grace.map(|_| ForcedOutcome::Timeout);
    let mut grace_deadline = submit_timeout_grace;
    let mut cancel_command = submit_timeout_grace.map(|_| CancelCommand {
        command_id: CommandId::new(command_id("headless-cancel")),
    });
    if let Some(cancel) = cancel_command.clone()
        && let Some(deadline) = grace_deadline
    {
        send_cancel_before(
            deadline,
            profile,
            &ensure,
            &mut connection,
            &mut reducer,
            &mut reconnects,
            &run_id,
            &cancel,
        )
        .await?;
    }
    for frame in std::mem::take(&mut buffered) {
        if process_frame(&connection, &mut reducer, frame).await {
            let recovered = attach_for_run_before_deadline(
                profile,
                &ensure,
                &mut connection,
                &mut reducer,
                &mut reconnects,
                &run_id,
                &mut forced,
                &mut grace_deadline,
                &mut cancel_command,
                timeout_deadline,
                request.terminal_grace,
                "buffered cursor recovery",
            )
            .await?;
            if !recovered {
                break;
            }
            break;
        }
        handle_reducer_actions(
            profile,
            &ensure,
            &mut connection,
            &mut reducer,
            &mut reconnects,
            &run_id,
            &mut forced,
            &mut grace_deadline,
            &mut cancel_command,
            timeout_deadline,
            request.terminal_grace,
        )
        .await?;
    }
    // A gap/lag in the buffered pre-response stream can make the recovery
    // attach replay a permission or blocking event and then stop at CaughtUp.
    // Drain those actions before waiting for another live frame; the daemon
    // may be waiting on the answer we just reconstructed.
    handle_reducer_actions(
        profile,
        &ensure,
        &mut connection,
        &mut reducer,
        &mut reconnects,
        &run_id,
        &mut forced,
        &mut grace_deadline,
        &mut cancel_command,
        timeout_deadline,
        request.terminal_grace,
    )
    .await?;

    loop {
        if reducer.terminal.is_some()
            || grace_deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            break;
        }
        if connection.client.lost_events() != connection.observed_lost_events
            && connection.events.is_empty()
        {
            // W9b review fix: drain DELIVERED frames before recovering —
            // losses observed mid-burst would otherwise abandon deliverable
            // frames and turn bounded channel pressure into a spurious,
            // scheduler-timed reconnect.
            let recovered = attach_for_run_before_deadline(
                profile,
                &ensure,
                &mut connection,
                &mut reducer,
                &mut reconnects,
                &run_id,
                &mut forced,
                &mut grace_deadline,
                &mut cancel_command,
                timeout_deadline,
                request.terminal_grace,
                "lost-event cursor recovery",
            )
            .await?;
            if !recovered {
                break;
            }
            handle_reducer_actions(
                profile,
                &ensure,
                &mut connection,
                &mut reducer,
                &mut reconnects,
                &run_id,
                &mut forced,
                &mut grace_deadline,
                &mut cancel_command,
                timeout_deadline,
                request.terminal_grace,
            )
            .await?;
            continue;
        }

        let active_deadline = grace_deadline.or(timeout_deadline);
        tokio::select! {
            frame = connection.events.recv() => {
                let Some(frame) = frame else {
                    let reason = connection_reason(&connection.client);
                    let recovered = reconnect_after_disconnect_for_run_before_deadline(
                        profile,
                        &ensure,
                        &mut connection,
                        &mut reducer,
                        &mut reconnects,
                        &run_id,
                        &mut forced,
                        &mut grace_deadline,
                        &mut cancel_command,
                        timeout_deadline,
                        request.terminal_grace,
                        "event stream",
                        reason,
                    ).await?;
                    if !recovered {
                        break;
                    }
                    handle_reducer_actions(
                        profile,
                        &ensure,
                        &mut connection,
                        &mut reducer,
                        &mut reconnects,
                        &run_id,
                        &mut forced,
                        &mut grace_deadline,
                        &mut cancel_command,
                        timeout_deadline,
                        request.terminal_grace,
                    ).await?;
                    continue;
                };
                if process_frame(&connection, &mut reducer, frame).await {
                    let recovered = attach_for_run_before_deadline(
                        profile,
                        &ensure,
                        &mut connection,
                        &mut reducer,
                        &mut reconnects,
                        &run_id,
                        &mut forced,
                        &mut grace_deadline,
                        &mut cancel_command,
                        timeout_deadline,
                        request.terminal_grace,
                        "gap cursor recovery",
                    ).await?;
                    if !recovered {
                        break;
                    }
                }
                handle_reducer_actions(
                    profile,
                    &ensure,
                    &mut connection,
                    &mut reducer,
                    &mut reconnects,
                    &run_id,
                    &mut forced,
                    &mut grace_deadline,
                    &mut cancel_command,
                    timeout_deadline,
                    request.terminal_grace,
                ).await?;
            }
            () = wait_until(active_deadline) => {
                if forced.is_some() {
                    break;
                }
                forced = Some(ForcedOutcome::Timeout);
                grace_deadline = Some(Instant::now() + request.terminal_grace);
                if cancel_command.is_none() {
                    cancel_command = Some(CancelCommand {
                        command_id: CommandId::new(command_id("headless-cancel")),
                    });
                    if let Some(cancel) = cancel_command.clone()
                        && let Some(deadline) = grace_deadline
                    {
                        send_cancel_before(
                            deadline,
                            profile,
                            &ensure,
                            &mut connection,
                            &mut reducer,
                            &mut reconnects,
                            &run_id,
                            &cancel,
                        )
                        .await?;
                    }
                }
            }
        }
    }

    Ok(finalize(
        reducer,
        run_id,
        provider,
        model,
        attachment_refs,
        forced,
    ))
}

async fn upload_attachments(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reconnects: &mut ReconnectBudget,
    attachments: &[HeadlessAttachment],
) -> Result<(Vec<AttachmentBlock>, Vec<ArtifactRef>), HeadlessRunError> {
    let mut blocks = Vec::with_capacity(attachments.len());
    let mut refs = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let expected = ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(attachment.bytes()).to_hex()
        ));
        let body = RequestBody::ArtifactPut {
            data_base64: encode_base64(attachment.bytes()),
        };
        loop {
            match connection.client.request(body.clone()).await {
                Ok(ResponseBody::ArtifactPut { artifact, bytes }) => {
                    let expected_bytes =
                        u64::try_from(attachment.bytes().len()).unwrap_or(u64::MAX);
                    if artifact != expected || bytes != expected_bytes {
                        return Err(protocol_error(
                            "artifact.put",
                            "response content address or byte count did not match the upload",
                        ));
                    }
                    blocks.push(match attachment {
                        HeadlessAttachment::Image(image) => AttachmentBlock::Image {
                            artifact: artifact.clone(),
                            mime: image.mime.clone(),
                            width: None,
                            height: None,
                        },
                        HeadlessAttachment::File(file) => AttachmentBlock::File {
                            artifact: artifact.clone(),
                            name: file.name.clone(),
                            lines: file.lines,
                        },
                    });
                    refs.push(artifact);
                    break;
                }
                Ok(ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    ..
                }) => return Err(rpc_error("artifact.put", code, message, retryable)),
                Ok(_) => {
                    return Err(protocol_error(
                        "artifact.put",
                        "response method did not match request",
                    ));
                }
                Err(error) => {
                    reconnect_before_session(
                        profile,
                        ensure,
                        connection,
                        reconnects,
                        "artifact.put",
                        error,
                    )
                    .await?;
                }
            }
        }
    }
    Ok((blocks, refs))
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))],
            ));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[usize::from(third & 0x3f)]));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

async fn resolve_run_identity(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reconnects: &mut ReconnectBudget,
    explicit_provider: Option<String>,
    explicit_model: Option<String>,
) -> Result<(String, String), HeadlessRunError> {
    let provider_is_explicit = explicit_provider.is_some();
    let provider = match explicit_provider {
        Some(provider) => provider,
        None => active_account_provider(profile, ensure, connection, reconnects).await?,
    };
    if let Some(model) = explicit_model {
        return Ok((provider, model));
    }

    let summary = provider_summary(profile, ensure, connection, reconnects, &provider).await?;
    match summary {
        Some(summary) => summary
            .default_model
            .or_else(|| summary.models.into_iter().next())
            .map(|model| (provider.clone(), model))
            .ok_or_else(|| HeadlessRunError::Bootstrap {
                stage: "provider.list",
                code: ERROR_CODE_NO_DEFAULT_MODEL,
                message: format!(
                    "provider `{provider}` publishes neither a default model nor a model catalog"
                ),
                retryable: false,
            }),
        // An explicit unknown provider must reach session.create so the
        // daemon remains the provider-name authority. Its provider check
        // precedes the non-empty-model check, preserving the typed refusal.
        None if provider_is_explicit => Ok((provider, String::new())),
        None => Err(HeadlessRunError::Bootstrap {
            stage: "provider.list",
            code: ERROR_CODE_NO_DEFAULT_MODEL,
            message: format!("active provider `{provider}` is absent from provider.list"),
            retryable: false,
        }),
    }
}

async fn active_account_provider(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reconnects: &mut ReconnectBudget,
) -> Result<String, HeadlessRunError> {
    loop {
        match connection
            .client
            .request(RequestBody::AccountList { provider: None })
            .await
        {
            Ok(ResponseBody::AccountList { descriptors, .. }) => {
                return descriptors
                    .into_iter()
                    .find(|descriptor| descriptor.active)
                    .map(|descriptor| descriptor.provider)
                    .ok_or_else(|| HeadlessRunError::Bootstrap {
                        stage: "account.list",
                        code: ERROR_CODE_NO_ACTIVE_ACCOUNT,
                        message: "no active daemon account is configured".into(),
                        retryable: false,
                    });
            }
            Ok(ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            }) => return Err(rpc_error("account.list", code, message, retryable)),
            Ok(_) => {
                return Err(protocol_error(
                    "account.list",
                    "response method did not match request",
                ));
            }
            Err(error) => {
                reconnect_before_session(
                    profile,
                    ensure,
                    connection,
                    reconnects,
                    "account.list",
                    error,
                )
                .await?;
            }
        }
    }
}

async fn provider_summary(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reconnects: &mut ReconnectBudget,
    provider: &str,
) -> Result<Option<haider_rpc::ProviderSummaryWire>, HeadlessRunError> {
    loop {
        match connection
            .client
            .request(RequestBody::ProviderList {
                provider: Some(provider.to_owned()),
            })
            .await
        {
            Ok(ResponseBody::ProviderList { providers, .. }) => {
                return Ok(providers
                    .into_iter()
                    .find(|summary| summary.provider == provider));
            }
            Ok(ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            }) => return Err(rpc_error("provider.list", code, message, retryable)),
            Ok(_) => {
                return Err(protocol_error(
                    "provider.list",
                    "response method did not match request",
                ));
            }
            Err(error) => {
                reconnect_before_session(
                    profile,
                    ensure,
                    connection,
                    reconnects,
                    "provider.list",
                    error,
                )
                .await?;
            }
        }
    }
}

fn normalize_ensure_options(
    options: &mut EnsureOptions,
    permission_overrides: SessionPermissionOverridesV1,
    has_attachments: bool,
    trust_hooks: bool,
) {
    options.required_features.extend(required_live_features());
    if !permission_overrides.is_empty() {
        options
            .required_features
            .insert(FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned());
    }
    if has_attachments {
        options
            .required_features
            .insert(FEATURE_ARTIFACT_PUT_V1.to_owned());
    }
    if trust_hooks {
        options
            .required_features
            .insert(haider_rpc::FEATURE_HOOKS_V1.to_owned());
    }
    options.client = ClientConfig {
        client_name: "haider-headless".into(),
        client_instance_id: command_id("headless-client"),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        ..options.client.clone()
    };
}

async fn reconnect_before_session(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reconnects: &mut ReconnectBudget,
    stage: &'static str,
    error: ClientError,
) -> Result<(), HeadlessRunError> {
    let reason = client_error(stage, error)?;
    reconnects.spend(stage, reason)?;
    *connection = HeadlessConnection::open(profile, ensure.clone()).await?;
    Ok(())
}

async fn reconnect_attached(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    stage: &'static str,
    error: ClientError,
) -> Result<(), HeadlessRunError> {
    let reason = client_error(stage, error)?;
    reconnect_after_disconnect(
        profile, ensure, connection, reducer, reconnects, stage, reason,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn reconnect_for_submit(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    buffered: &mut Vec<WireFrame>,
    stage: &'static str,
    error: ClientError,
) -> Result<(), HeadlessRunError> {
    let reason = client_error(stage, error)?;
    reconnects.spend(stage, reason)?;
    *connection = HeadlessConnection::open(profile, ensure.clone()).await?;
    attach_buffered_with_recovery(profile, ensure, connection, reducer, reconnects, buffered).await
}

async fn reconnect_after_disconnect(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    stage: &'static str,
    reason: DisconnectReason,
) -> Result<(), HeadlessRunError> {
    reconnects.spend(stage, reason)?;
    *connection = HeadlessConnection::open(profile, ensure.clone()).await?;
    attach_with_recovery(profile, ensure, connection, reducer, reconnects).await
}

async fn attach_with_recovery(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
) -> Result<(), HeadlessRunError> {
    loop {
        match attach_once(connection, reducer).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(HeadlessRunError::Transport { stage, reason }) => {
                reconnects.spend(stage, reason)?;
                *connection = HeadlessConnection::open(profile, ensure.clone()).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn attach_buffered_with_recovery(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    buffered: &mut Vec<WireFrame>,
) -> Result<(), HeadlessRunError> {
    loop {
        buffered.clear();
        match attach_buffered_once(connection, reducer, buffered).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(HeadlessRunError::Transport { stage, reason }) => {
                reconnects.spend(stage, reason)?;
                *connection = HeadlessConnection::open(profile, ensure.clone()).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Establishes the Control barrier after a response-losing reconnect without
/// reducing run-scoped facts before the idempotent submit reply identifies
/// the run. The replay is buffered and reduced only after correlation.
async fn attach_buffered_once(
    connection: &mut HeadlessConnection,
    reducer: &HeadlessReducer,
    buffered: &mut Vec<WireFrame>,
) -> Result<bool, HeadlessRunError> {
    detach_existing(connection, "session.detach before submit retry").await?;
    let attach_loss_baseline = connection.client.lost_events();
    let response = connection
        .client
        .request(RequestBody::SessionAttach {
            session_id: reducer.session_id.clone(),
            after_seq: reducer.last_applied,
            mode: AttachMode::Control,
        })
        .await
        .map_err(|error| client_error_as_headless("session.attach before submit retry", error))?;
    let (attachment_id, attach_state) = match response {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } if attach_state.session_id == reducer.session_id => (attachment_id, attach_state),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => {
            return Err(rpc_error(
                "session.attach before submit retry",
                code,
                message,
                retryable,
            ));
        }
        _ => {
            return Err(protocol_error(
                "session.attach before submit retry",
                "response coordinates did not match the session",
            ));
        }
    };
    connection.worker_generation = attach_state.worker_generation;
    connection.attachment_id = Some(attachment_id.clone());
    connection.observed_lost_events = attach_loss_baseline;

    let mut health = tokio::time::interval(ATTACH_HEALTH_POLL);
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let frame = tokio::select! {
            frame = connection.events.recv() => frame,
            _ = health.tick() => {
                if connection.client.lost_events() != attach_loss_baseline {
                    return Ok(false);
                }
                if let ConnectionState::Disconnected(reason) = connection.client.state() {
                    return Err(HeadlessRunError::Transport {
                        stage: "session.attach before submit retry",
                        reason,
                    });
                }
                continue;
            }
        };
        let Some(frame) = frame else {
            return Err(HeadlessRunError::Transport {
                stage: "session.attach before submit retry",
                reason: connection_reason(&connection.client),
            });
        };
        match &frame {
            WireFrame::AttachCaughtUp {
                attachment_id: caught_up,
                ..
            } if caught_up == &attachment_id => {
                return Ok(connection.client.lost_events() == connection.observed_lost_events);
            }
            WireFrame::Event {
                attachment_id: event_attachment,
                session_id,
                ..
            } if event_attachment == &attachment_id && session_id == &reducer.session_id => {
                buffered.push(frame);
            }
            WireFrame::Lagged {
                attachment_id: lagged,
                ..
            } if lagged == &attachment_id => return Ok(false),
            WireFrame::ProtocolError(error) if error.fatal => {
                return Err(protocol_error(
                    "session.attach before submit retry",
                    &error.to_string(),
                ));
            }
            _ => {}
        }
    }
}

/// Returns true after a caught-up barrier, false when the cursor must reattach
/// on the same live connection.
async fn attach_once(
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
) -> Result<bool, HeadlessRunError> {
    detach_existing(connection, "session.detach").await?;
    let attach_loss_baseline = connection.client.lost_events();
    let response = connection
        .client
        .request(RequestBody::SessionAttach {
            session_id: reducer.session_id.clone(),
            after_seq: reducer.last_applied,
            mode: AttachMode::Control,
        })
        .await
        .map_err(|error| client_error_as_headless("session.attach", error))?;
    let (attachment_id, attach_state) = match response {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } if attach_state.session_id == reducer.session_id => (attachment_id, attach_state),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => return Err(rpc_error("session.attach", code, message, retryable)),
        _ => {
            return Err(protocol_error(
                "session.attach",
                "response coordinates did not match the session",
            ));
        }
    };
    connection.worker_generation = attach_state.worker_generation;
    connection.attachment_id = Some(attachment_id.clone());
    connection.observed_lost_events = attach_loss_baseline;

    let mut health = tokio::time::interval(ATTACH_HEALTH_POLL);
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let frame = tokio::select! {
            frame = connection.events.recv() => frame,
            _ = health.tick() => {
                if connection.client.lost_events() != attach_loss_baseline {
                    return Ok(false);
                }
                if let ConnectionState::Disconnected(reason) = connection.client.state() {
                    return Err(HeadlessRunError::Transport {
                        stage: "session.attach replay",
                        reason,
                    });
                }
                continue;
            }
        };
        let Some(frame) = frame else {
            return Err(HeadlessRunError::Transport {
                stage: "session.attach replay",
                reason: connection_reason(&connection.client),
            });
        };
        match frame {
            WireFrame::AttachCaughtUp {
                attachment_id: caught_up,
                ..
            } if caught_up == attachment_id => {
                return Ok(connection.client.lost_events() == connection.observed_lost_events);
            }
            WireFrame::Event {
                attachment_id: event_attachment,
                session_id,
                envelope,
            } if event_attachment == attachment_id && session_id == reducer.session_id => {
                match reducer.apply(envelope).await {
                    ApplyStatus::Gap => return Ok(false),
                    ApplyStatus::Duplicate | ApplyStatus::Applied => {}
                }
            }
            WireFrame::Lagged {
                attachment_id: lagged,
                ..
            } if lagged == attachment_id => return Ok(false),
            WireFrame::ProtocolError(error) if error.fatal => {
                return Err(protocol_error("session.attach replay", &error.to_string()));
            }
            _ => {}
        }
    }
}

async fn detach_existing(
    connection: &mut HeadlessConnection,
    stage: &'static str,
) -> Result<(), HeadlessRunError> {
    let Some(attachment_id) = connection.attachment_id.take() else {
        return Ok(());
    };
    match connection
        .client
        .request(RequestBody::SessionDetach { attachment_id })
        .await
    {
        Ok(ResponseBody::SessionDetach { .. }) | Ok(ResponseBody::Error { .. }) => Ok(()),
        Ok(_) => Err(protocol_error(
            stage,
            "response method did not match request",
        )),
        Err(error) => Err(client_error_as_headless(stage, error)),
    }
}

async fn process_frame(
    connection: &HeadlessConnection,
    reducer: &mut HeadlessReducer,
    frame: WireFrame,
) -> bool {
    match frame {
        WireFrame::Event {
            attachment_id,
            session_id,
            envelope,
        } if connection.attachment_id.as_ref() == Some(&attachment_id)
            && session_id == reducer.session_id =>
        {
            reducer.apply(envelope).await == ApplyStatus::Gap
        }
        WireFrame::Lagged { attachment_id, .. }
            if connection.attachment_id.as_ref() == Some(&attachment_id) =>
        {
            true
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn activate_timeout_after_stalled_recovery(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    run_id: &RunId,
    forced: &mut Option<ForcedOutcome>,
    grace_deadline: &mut Option<Instant>,
    cancel_command: &mut Option<CancelCommand>,
    terminal_grace: Duration,
    stage: &'static str,
) -> Result<(), HeadlessRunError> {
    *forced = Some(ForcedOutcome::Timeout);
    let deadline = Instant::now() + terminal_grace;
    *grace_deadline = Some(deadline);
    connection.client.close();
    tokio::time::timeout_at(
        deadline,
        reconnect_after_disconnect(
            profile,
            ensure,
            connection,
            reducer,
            reconnects,
            stage,
            DisconnectReason::Closed,
        ),
    )
    .await
    .map_err(|_| cancellation_unconfirmed(stage))??;
    if cancel_command.is_none() {
        *cancel_command = Some(CancelCommand {
            command_id: CommandId::new(command_id("headless-cancel")),
        });
    }
    if let Some(cancel) = cancel_command.clone() {
        send_cancel_before(
            deadline, profile, ensure, connection, reducer, reconnects, run_id, &cancel,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn attach_for_run_before_deadline(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    run_id: &RunId,
    forced: &mut Option<ForcedOutcome>,
    grace_deadline: &mut Option<Instant>,
    cancel_command: &mut Option<CancelCommand>,
    timeout_deadline: Option<Instant>,
    terminal_grace: Duration,
    stage: &'static str,
) -> Result<bool, HeadlessRunError> {
    if let Some(deadline) = *grace_deadline {
        return match tokio::time::timeout_at(
            deadline,
            attach_with_recovery(profile, ensure, connection, reducer, reconnects),
        )
        .await
        {
            Ok(result) => result.map(|()| true),
            Err(_) => Ok(false),
        };
    }
    if let Some(deadline) = timeout_deadline {
        match tokio::time::timeout_at(
            deadline,
            attach_with_recovery(profile, ensure, connection, reducer, reconnects),
        )
        .await
        {
            Ok(result) => {
                result?;
                return Ok(true);
            }
            Err(_) => {
                activate_timeout_after_stalled_recovery(
                    profile,
                    ensure,
                    connection,
                    reducer,
                    reconnects,
                    run_id,
                    forced,
                    grace_deadline,
                    cancel_command,
                    terminal_grace,
                    stage,
                )
                .await?;
                return Ok(true);
            }
        }
    }
    attach_with_recovery(profile, ensure, connection, reducer, reconnects).await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn reconnect_attached_for_run_before_deadline(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    run_id: &RunId,
    forced: &mut Option<ForcedOutcome>,
    grace_deadline: &mut Option<Instant>,
    cancel_command: &mut Option<CancelCommand>,
    timeout_deadline: Option<Instant>,
    terminal_grace: Duration,
    stage: &'static str,
    error: ClientError,
) -> Result<(), HeadlessRunError> {
    if let Some(deadline) = timeout_deadline {
        match tokio::time::timeout_at(
            deadline,
            reconnect_attached(
                profile, ensure, connection, reducer, reconnects, stage, error,
            ),
        )
        .await
        {
            Ok(result) => return result,
            Err(_) => {
                return activate_timeout_after_stalled_recovery(
                    profile,
                    ensure,
                    connection,
                    reducer,
                    reconnects,
                    run_id,
                    forced,
                    grace_deadline,
                    cancel_command,
                    terminal_grace,
                    stage,
                )
                .await;
            }
        }
    }
    reconnect_attached(
        profile, ensure, connection, reducer, reconnects, stage, error,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn reconnect_after_disconnect_for_run_before_deadline(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    run_id: &RunId,
    forced: &mut Option<ForcedOutcome>,
    grace_deadline: &mut Option<Instant>,
    cancel_command: &mut Option<CancelCommand>,
    timeout_deadline: Option<Instant>,
    terminal_grace: Duration,
    stage: &'static str,
    reason: DisconnectReason,
) -> Result<bool, HeadlessRunError> {
    if let Some(deadline) = *grace_deadline {
        return match tokio::time::timeout_at(
            deadline,
            reconnect_after_disconnect(
                profile, ensure, connection, reducer, reconnects, stage, reason,
            ),
        )
        .await
        {
            Ok(result) => result.map(|()| true),
            Err(_) => Ok(false),
        };
    }
    if let Some(deadline) = timeout_deadline {
        match tokio::time::timeout_at(
            deadline,
            reconnect_after_disconnect(
                profile, ensure, connection, reducer, reconnects, stage, reason,
            ),
        )
        .await
        {
            Ok(result) => {
                result?;
                return Ok(true);
            }
            Err(_) => {
                activate_timeout_after_stalled_recovery(
                    profile,
                    ensure,
                    connection,
                    reducer,
                    reconnects,
                    run_id,
                    forced,
                    grace_deadline,
                    cancel_command,
                    terminal_grace,
                    stage,
                )
                .await?;
                return Ok(true);
            }
        }
    }
    reconnect_after_disconnect(
        profile, ensure, connection, reducer, reconnects, stage, reason,
    )
    .await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn handle_reducer_actions(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    run_id: &RunId,
    forced: &mut Option<ForcedOutcome>,
    grace_deadline: &mut Option<Instant>,
    cancel_command: &mut Option<CancelCommand>,
    timeout_deadline: Option<Instant>,
    terminal_grace: Duration,
) -> Result<(), HeadlessRunError> {
    while let Some(action) = reducer.actions.front().cloned() {
        match action {
            ReducerAction::RejectPermission { .. } if forced.is_some() => {
                reducer.actions.pop_front();
            }
            ReducerAction::RejectPermission {
                command_id: answer_command_id,
                menu_id,
                request_seq,
                option_key,
                option_index,
            } => loop {
                if forced.is_some() {
                    reducer.actions.pop_front();
                    break;
                }
                if let Some(resolution) = reducer.menu_resolutions.get(menu_id.as_str()) {
                    reducer.actions.pop_front();
                    if permission_resolution_matches(resolution, &option_key, option_index) {
                        break;
                    }
                    reducer.actions.push_front(ReducerAction::Block(
                        HeadlessBlockingReason::PermissionResolutionConflict,
                    ));
                    break;
                }
                let answer_wait = {
                    let frame_command_id = answer_command_id.clone();
                    let frame_session_id = reducer.session_id.clone();
                    let frame_menu_id = menu_id.clone();
                    let frame_option_key = option_key.clone();
                    let worker_generation = connection.worker_generation;
                    let answer = async {
                        let pending = connection
                            .client
                            .begin_correlated_frame(move |request_id| WireFrame::MenuAnswer {
                                request_id: Some(request_id),
                                command_id: frame_command_id,
                                session_id: frame_session_id,
                                menu_id: frame_menu_id,
                                request_seq,
                                worker_generation,
                                option_key: frame_option_key,
                                option_index,
                                input: None,
                            })
                            .await?;
                        pending.wait().await
                    };
                    if let Some(deadline) = timeout_deadline {
                        tokio::select! {
                            response = answer => Some(response),
                            () = tokio::time::sleep_until(deadline) => None,
                        }
                    } else {
                        Some(answer.await)
                    }
                };
                let Some(response) = answer_wait else {
                    reducer.actions.pop_front();
                    *forced = Some(ForcedOutcome::Timeout);
                    *grace_deadline = Some(Instant::now() + terminal_grace);
                    if cancel_command.is_none() {
                        *cancel_command = Some(CancelCommand {
                            command_id: CommandId::new(command_id("headless-cancel")),
                        });
                    }
                    if let Some(cancel) = cancel_command.clone()
                        && let Some(deadline) = *grace_deadline
                    {
                        send_cancel_before(
                            deadline, profile, ensure, connection, reducer, reconnects, run_id,
                            &cancel,
                        )
                        .await?;
                    }
                    break;
                };
                match response {
                    Ok(ResponseBody::MenuAnswer { .. }) => {
                        reducer.actions.pop_front();
                        break;
                    }
                    Ok(ResponseBody::Error { code, .. }) if code == ERROR_CODE_ALREADY_RESOLVED => {
                        if !attach_for_run_before_deadline(
                            profile,
                            ensure,
                            connection,
                            reducer,
                            reconnects,
                            run_id,
                            forced,
                            grace_deadline,
                            cancel_command,
                            timeout_deadline,
                            terminal_grace,
                            "menu resolution replay",
                        )
                        .await?
                        {
                            break;
                        }
                        if !reducer.menu_resolutions.contains_key(menu_id.as_str()) {
                            return Err(protocol_error(
                                "menu.answer",
                                "daemon reported an already-resolved menu without its durable resolution",
                            ));
                        }
                    }
                    Ok(ResponseBody::Error {
                        code,
                        message,
                        retryable,
                        ..
                    }) => return Err(rpc_error("menu.answer", code, message, retryable)),
                    Ok(_) => {
                        return Err(protocol_error(
                            "menu.answer",
                            "response method did not match request",
                        ));
                    }
                    Err(error) => {
                        reconnect_attached_for_run_before_deadline(
                            profile,
                            ensure,
                            connection,
                            reducer,
                            reconnects,
                            run_id,
                            forced,
                            grace_deadline,
                            cancel_command,
                            timeout_deadline,
                            terminal_grace,
                            "menu.answer",
                            error,
                        )
                        .await?;
                    }
                }
            },
            ReducerAction::Block(reason) if forced.is_none() => {
                reducer.actions.pop_front();
                *forced = Some(ForcedOutcome::Blocked(reason));
                *grace_deadline = Some(Instant::now() + terminal_grace);
                if cancel_command.is_none() {
                    *cancel_command = Some(CancelCommand {
                        command_id: CommandId::new(command_id("headless-cancel")),
                    });
                    if let Some(cancel) = cancel_command.clone()
                        && let Some(deadline) = *grace_deadline
                    {
                        send_cancel_before(
                            deadline, profile, ensure, connection, reducer, reconnects, run_id,
                            &cancel,
                        )
                        .await?;
                    }
                }
            }
            ReducerAction::Block(_) => {
                reducer.actions.pop_front();
            }
        }
    }
    Ok(())
}

fn permission_resolution_matches(
    resolution: &DurableMenuResolution,
    option_key: &str,
    option_index: u32,
) -> bool {
    matches!(
        resolution,
        DurableMenuResolution::Answer {
            option_key: Some(actual_key),
            option_index: actual_index,
        } if actual_key == option_key && *actual_index == option_index
    )
}

#[allow(clippy::too_many_arguments)]
async fn send_cancel_before(
    deadline: Instant,
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    run_id: &RunId,
    cancel: &CancelCommand,
) -> Result<(), HeadlessRunError> {
    tokio::time::timeout_at(
        deadline,
        send_cancel(
            profile, ensure, connection, reducer, reconnects, run_id, cancel,
        ),
    )
    .await
    .map_err(|_| HeadlessRunError::Rpc {
        stage: "turn.cancel",
        code: "cancellation_unconfirmed".into(),
        message: "durable cancellation was not confirmed before terminal grace expired".into(),
        retryable: true,
    })?
}

async fn send_cancel(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reducer: &mut HeadlessReducer,
    reconnects: &mut ReconnectBudget,
    run_id: &RunId,
    cancel: &CancelCommand,
) -> Result<(), HeadlessRunError> {
    loop {
        if reducer.cancel_observed || reducer.terminal.is_some() {
            return Ok(());
        }
        let body = RequestBody::TurnCancel {
            command_id: cancel.command_id.clone(),
            session_id: reducer.session_id.clone(),
            worker_generation: connection.worker_generation,
            run_id: run_id.clone(),
        };
        match connection.client.request(body).await {
            Ok(ResponseBody::TurnCancel { .. }) => return Ok(()),
            Ok(ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            }) => return Err(rpc_error("turn.cancel", code, message, retryable)),
            Ok(_) => {
                return Err(protocol_error(
                    "turn.cancel",
                    "response method did not match request",
                ));
            }
            Err(error) => {
                reconnect_attached(
                    profile,
                    ensure,
                    connection,
                    reducer,
                    reconnects,
                    "turn.cancel",
                    error,
                )
                .await?;
            }
        }
    }
}

fn frame_matches_attachment(frame: &WireFrame, attachment_id: Option<&AttachmentId>) -> bool {
    match frame {
        WireFrame::Event {
            attachment_id: actual,
            ..
        }
        | WireFrame::Lagged {
            attachment_id: actual,
            ..
        }
        | WireFrame::AttachCaughtUp {
            attachment_id: actual,
            ..
        } => attachment_id == Some(actual),
        WireFrame::ProtocolError(_) | WireFrame::ServerDraining { .. } => true,
        _ => false,
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending::<()>().await,
    }
}

async fn before_acceptance_deadline<T>(
    deadline: Option<Instant>,
    stage: &'static str,
    future: impl Future<Output = Result<T, HeadlessRunError>>,
) -> Result<T, HeadlessRunError> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| HeadlessRunError::Rpc {
                stage,
                code: "timeout_before_acceptance".into(),
                message: "wall-clock timeout expired before a run was accepted".into(),
                retryable: true,
            })?,
        None => future.await,
    }
}

fn cancellation_unconfirmed(stage: &'static str) -> HeadlessRunError {
    HeadlessRunError::Rpc {
        stage,
        code: "cancellation_unconfirmed".into(),
        message: "durable cancellation could not be confirmed within terminal grace".into(),
        retryable: true,
    }
}

fn finalize(
    reducer: HeadlessReducer,
    run_id: RunId,
    provider: String,
    model: String,
    attachments: Vec<ArtifactRef>,
    forced: Option<ForcedOutcome>,
) -> HeadlessRunResult {
    let (outcome, failure, terminal_seq) = match forced {
        Some(ForcedOutcome::Timeout) => (
            HeadlessOutcome::Timeout,
            Some(HeadlessRunFailure {
                code: HeadlessFailureCode::Timeout,
                message: "run exceeded its wall-clock timeout".into(),
                retryable: false,
            }),
            reducer.terminal.as_ref().map(|terminal| terminal.seq),
        ),
        Some(ForcedOutcome::Blocked(reason)) => (
            HeadlessOutcome::InputRequired,
            Some(HeadlessRunFailure {
                code: HeadlessFailureCode::Blocked(reason),
                message: reason.message().into(),
                retryable: false,
            }),
            reducer.terminal.as_ref().map(|terminal| terminal.seq),
        ),
        None => reducer.terminal.as_ref().map_or_else(
            || {
                (
                    HeadlessOutcome::Errored,
                    Some(HeadlessRunFailure {
                        code: HeadlessFailureCode::Internal,
                        message: "event stream ended without a correlated terminal".into(),
                        retryable: false,
                    }),
                    None,
                )
            },
            |terminal| {
                (
                    terminal.outcome,
                    terminal.failure.clone(),
                    Some(terminal.seq),
                )
            },
        ),
    };
    let background_tasks_running = reducer
        .background_tasks
        .iter()
        .filter(|(_, (_, running))| *running)
        .map(|(task_id, (name, _))| HeadlessBackgroundTask {
            task_id: task_id.clone(),
            name: name.clone(),
        })
        .collect();
    HeadlessRunResult {
        session_id: reducer.session_id,
        run_id,
        provider,
        model,
        attachments,
        outcome,
        response: reducer.response,
        usage: reducer.usage,
        permission_denials: reducer.permission_denials,
        failure,
        terminal_seq,
        background_tasks_running,
    }
}

fn client_error(
    stage: &'static str,
    error: ClientError,
) -> Result<DisconnectReason, HeadlessRunError> {
    match error {
        ClientError::Disconnected(reason) => Ok(reason),
        ClientError::Encode(error) => Err(HeadlessRunError::Encode {
            stage,
            message: error.to_string(),
        }),
    }
}

fn client_error_as_headless(stage: &'static str, error: ClientError) -> HeadlessRunError {
    match error {
        ClientError::Disconnected(reason) => HeadlessRunError::Transport { stage, reason },
        ClientError::Encode(error) => HeadlessRunError::Encode {
            stage,
            message: error.to_string(),
        },
    }
}

fn connection_reason(client: &RpcClient) -> DisconnectReason {
    match client.state() {
        ConnectionState::Connected => DisconnectReason::PeerClosed,
        ConnectionState::Disconnected(reason) => reason,
    }
}

fn rpc_error(
    stage: &'static str,
    code: String,
    message: String,
    retryable: bool,
) -> HeadlessRunError {
    HeadlessRunError::Rpc {
        stage,
        code,
        message,
        retryable,
    }
}

fn protocol_error(stage: &'static str, message: &str) -> HeadlessRunError {
    HeadlessRunError::Protocol {
        stage,
        message: message.into(),
    }
}

fn command_id(prefix: &str) -> String {
    let sequence = COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{prefix}-{}-{nanos}-{sequence}", std::process::id())
}

fn error_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidArgument => "invalid_argument",
        ErrorCode::UnknownMethod => "unknown_method",
        ErrorCode::ProtocolMismatch => "protocol_mismatch",
        ErrorCode::Unauthorized => "unauthorized",
        ErrorCode::CredentialMissing => "credential_missing",
        ErrorCode::CredentialLimited => "credential_limited",
        ErrorCode::SessionNotFound => "session_not_found",
        ErrorCode::RunNotActive => "run_not_active",
        ErrorCode::MenuNotFound => "menu_not_found",
        ErrorCode::MenuAlreadyAnswered => "menu_already_answered",
        ErrorCode::SingleWriterViolation => "single_writer_violation",
        ErrorCode::Busy => "busy",
        ErrorCode::RevisionConflict => "revision_conflict",
        ErrorCode::LoopLimit => "loop_limit",
        ErrorCode::ProviderError => "provider_error",
        ErrorCode::ProviderTimeout => "provider_timeout",
        ErrorCode::VisionUnsupported => "vision_unsupported",
        ErrorCode::StoreCorrupt => "store_corrupt",
        ErrorCode::StoreLocked => "store_locked",
        ErrorCode::PermissionDenied => "permission_denied",
        ErrorCode::EffectUnknownOutcome => "effect_unknown_outcome",
        ErrorCode::Internal => "internal",
        ErrorCode::Unknown => "unknown",
    }
}

/// The required feature set after normalizing a headless request. Exposed for
/// front-end diagnostics and feature-refusal tests.
#[must_use]
pub fn required_headless_features(
    permission_overrides: SessionPermissionOverridesV1,
) -> BTreeSet<String> {
    let mut features = required_live_features();
    if !permission_overrides.is_empty() {
        features.insert(FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned());
    }
    features
}

/// Required daemon features for a headless run that will upload attachments.
#[must_use]
pub fn required_headless_features_with_attachments(
    permission_overrides: SessionPermissionOverridesV1,
) -> BTreeSet<String> {
    let mut features = required_headless_features(permission_overrides);
    features.insert(FEATURE_ARTIFACT_PUT_V1.to_owned());
    features
}

/// Required daemon features for an explicitly hook-trusted headless run.
#[must_use]
pub fn required_headless_features_with_hook_trust(
    permission_overrides: SessionPermissionOverridesV1,
) -> BTreeSet<String> {
    let mut features = required_headless_features(permission_overrides);
    features.insert(haider_rpc::FEATURE_HOOKS_V1.to_owned());
    features
}
