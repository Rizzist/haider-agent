//! Reusable daemon-backed one-shot transaction for non-interactive clients.
//!
//! This module owns execution ordering, durable command retries, cursor replay,
//! fail-closed permission handling, and terminal reduction. A lossless hybrid
//! event ledger retains small runs in memory and spills larger runs without
//! coupling control-plane progress to presentation formatting.

#[cfg(test)]
#[path = "headless_tests.rs"]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::future::{Future, pending};
use std::io::{BufRead as _, BufReader, BufWriter, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use haider_rpc::haider_protocol::EventPayload;
use haider_rpc::haider_protocol::effect::{AuthorizationVerdict, EffectPhase};
use haider_rpc::haider_protocol::envelope::{RawEnvelope, envelope_weight_bytes};
use haider_rpc::haider_protocol::error::{ErrorCode, ErrorPresentation};
use haider_rpc::haider_protocol::headless::{
    HeadlessRunEventPayload, HeadlessRunSpecV1, ReplayDivergenceV1, RunBudgetExhaustedV1,
    RunBudgetV1,
};
use haider_rpc::haider_protocol::ids::{ArtifactRef, MenuId, RunId, SessionId};
use haider_rpc::haider_protocol::item::{ItemEvent, TurnItem};
use haider_rpc::haider_protocol::menu::{DecisionKind, MenuKind};
use haider_rpc::haider_protocol::provider::Usage;
use haider_rpc::haider_protocol::session::{
    SessionInteractionModeV1, SessionPermissionOverridesV1,
};
use haider_rpc::haider_protocol::state::RunState;
use haider_rpc::haider_protocol::tool::AttachmentBlock;
use haider_rpc::{
    AttachMode, AttachmentId, CancelStatus, Capability, CapabilitySet, ClientKind, CommandId,
    ERROR_CODE_ALREADY_RESOLVED, FEATURE_ARTIFACT_PUT_V1, FEATURE_AUTONOMOUS_INTERACTION_V1,
    FEATURE_SESSION_PERMISSION_OVERRIDES_V1, RequestBody, ResponseBody, SeqRange, WireFrame,
};
use serde::ser::{Error as _, SerializeSeq as _};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;
use zeroize::Zeroizing;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;

use crate::client::{
    ClientConfig, ClientError, ClientHealthWait, ConnectionState, DisconnectReason, RpcClient,
    connect,
};
use crate::profile::ResolvedProfile;
#[cfg(unix)]
use crate::profile::effective_uid;
#[cfg(unix)]
use crate::spawn::signal_authenticated_peer;
use crate::spawn::{
    DaemonLifetime, DaemonOwnershipToken, EnsureError, EnsureOptions, ensure_daemon,
    required_live_features,
};

/// Default time allowed for a durable cancellation to reach a correlated
/// terminal after timeout or blocked-input detection.
///
/// Supervisor contract: an outer process hard-kill deadline must be strictly
/// later than Haider's internal `--timeout` plus this terminal grace. Killing
/// at the same instant as `--timeout` prevents the client from cancelling and
/// observing the durable terminal fact, turning a truthful timeout into an
/// ambiguous process disappearance.
pub const DEFAULT_TERMINAL_GRACE: Duration = Duration::from_secs(2);

/// No daemon account descriptor is currently active for headless bootstrap.
pub const ERROR_CODE_NO_ACTIVE_ACCOUNT: &str = "no_active_account";
/// The selected provider publishes neither a default nor a fallback model.
pub const ERROR_CODE_NO_DEFAULT_MODEL: &str = "no_default_model";

const MAX_RECONNECTS: u8 = 3;
const ATTACH_HEALTH_REPAIR_INTERVAL: Duration = Duration::from_secs(30);
const EPHEMERAL_DRAIN_ALLOWANCE: Duration = Duration::from_secs(5);
const EPHEMERAL_REAP_GRACE: Duration = Duration::from_millis(250);
static COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Correlated event bytes retained in memory before the ledger switches once
/// to its private disk spool. The estimate deliberately counts retained JSON
/// tree/string bytes without serializing each ordinary event.
pub const HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES: usize = 256 * 1024;
/// A spilled ledger is flushed in bounded batches and once more at terminal.
const HEADLESS_EVENT_SPOOL_FLUSH_BYTES: usize = 64 * 1024;

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

/// One daemon-bound PDF. The client inspects only admission metadata; text
/// extraction and provider encoding remain daemon-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessPdfAttachment {
    pub bytes: Vec<u8>,
    pub name: String,
    pub pages: u32,
}

/// One loaded attachment of either supported kind (G2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessAttachment {
    Image(HeadlessImageAttachment),
    File(HeadlessFileAttachment),
    Pdf(HeadlessPdfAttachment),
}

impl HeadlessAttachment {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Image(image) => &image.bytes,
            Self::File(file) => &file.bytes,
            Self::Pdf(pdf) => &pdf.bytes,
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

impl From<HeadlessPdfAttachment> for HeadlessAttachment {
    fn from(pdf: HeadlessPdfAttachment) -> Self {
        Self::Pdf(pdf)
    }
}

fn attachment_error(code: &str, title: &str, message: impl Into<String>) -> HeadlessRunError {
    let message = message.into();
    HeadlessRunError::Attachment {
        code: code.into(),
        message: message.clone(),
        presentation: haider_rpc::haider_protocol::error::ErrorPresentation::new(
            code,
            title,
            &message,
            haider_rpc::haider_protocol::error::ErrorScope::Turn,
            [haider_rpc::haider_protocol::error::ErrorAction::None],
        ),
    }
}

/// Reads at most the accepted image size plus one byte and identifies the
/// format from magic bytes rather than the path extension.
pub fn load_image_attachment(path: &Path) -> Result<HeadlessImageAttachment, HeadlessRunError> {
    let file = std::fs::File::open(path).map_err(|error| {
        attachment_error(
            "attachment_io",
            "Attachment could not be opened",
            format!("cannot open attachment {}: {error}", path.display()),
        )
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_HEADLESS_ATTACHMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            attachment_error(
                "attachment_io",
                "Attachment could not be read",
                format!("cannot read attachment {}: {error}", path.display()),
            )
        })?;
    if bytes.len() > MAX_HEADLESS_ATTACHMENT_BYTES {
        return Err(attachment_error(
            "attachment_too_large",
            "Attachment is too large",
            format!(
                "attachment {} exceeds the 5 MiB per-attachment limit",
                path.display()
            ),
        ));
    }
    let mime = sniff_image_mime(&bytes).ok_or_else(|| {
        attachment_error(
            "unsupported_attachment_type",
            "Unsupported attachment type",
            format!(
                "attachment {} is not a JPEG, PNG, GIF, or WebP image",
                path.display()
            ),
        )
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
    let file = std::fs::File::open(path).map_err(|error| {
        attachment_error(
            "attachment_io",
            "Attachment could not be opened",
            format!("cannot open attachment {}: {error}", path.display()),
        )
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_HEADLESS_ATTACHMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            attachment_error(
                "attachment_io",
                "Attachment could not be read",
                format!("cannot read attachment {}: {error}", path.display()),
            )
        })?;
    if bytes.len() > MAX_HEADLESS_ATTACHMENT_BYTES {
        return Err(attachment_error(
            "attachment_too_large",
            "Attachment is too large",
            format!(
                "attachment {} exceeds the 5 MiB per-attachment limit",
                path.display()
            ),
        ));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        attachment_error(
            "unsupported_attachment_encoding",
            "Attachment is not UTF-8 text",
            format!("attachment {} is not UTF-8 text", path.display()),
        )
    })?;
    let lines = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
    let name = sanitize_attachment_name(
        &path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
    );
    Ok(HeadlessFileAttachment { bytes, name, lines })
}

/// Loads a PDF under the PDF-specific byte and page caps. Only the page tree
/// is inspected here; the daemon performs text extraction/provider shaping.
pub fn load_pdf_attachment(path: &Path) -> Result<HeadlessPdfAttachment, HeadlessRunError> {
    let file = std::fs::File::open(path).map_err(|error| {
        attachment_error(
            "attachment_io",
            "PDF could not be opened",
            format!("cannot open attachment {}: {error}", path.display()),
        )
    })?;
    let mut bytes = Vec::new();
    file.take((haider_pdf::MAX_PDF_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            attachment_error(
                "attachment_io",
                "PDF could not be read",
                format!("cannot read attachment {}: {error}", path.display()),
            )
        })?;
    if bytes.len() > haider_pdf::MAX_PDF_BYTES {
        return Err(attachment_error(
            "pdf-too-large",
            "PDF is too large",
            format!("{} exceeds the 32 MiB PDF attachment limit", path.display()),
        ));
    }
    let metadata = haider_pdf::inspect_pdf(&bytes).map_err(|error| {
        attachment_error(
            "pdf-malformed",
            "PDF could not be read",
            format!("{}: {error}", path.display()),
        )
    })?;
    if metadata.pages > haider_pdf::MAX_PDF_PAGES {
        return Err(attachment_error(
            "pdf-too-many-pages",
            "PDF has too many pages",
            format!(
                "{} has {} pages; the PDF attachment limit is {} pages",
                path.display(),
                metadata.pages,
                haider_pdf::MAX_PDF_PAGES
            ),
        ));
    }
    let name = sanitize_attachment_name(
        &path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
    );
    Ok(HeadlessPdfAttachment {
        bytes,
        name,
        pages: metadata.pages,
    })
}

/// Shared ingress order for `haider run --attach` and TUI `/attach`.
pub fn load_attachment(path: &Path) -> Result<HeadlessAttachment, HeadlessRunError> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        // Preserve image-magic precedence without forcing a legitimate PDF
        // through the image lane's much smaller 5 MiB read cap.
        let mut prefix = [0_u8; 16];
        let mut file = std::fs::File::open(path).map_err(|error| {
            attachment_error(
                "attachment_io",
                "Attachment could not be opened",
                format!("cannot open attachment {}: {error}", path.display()),
            )
        })?;
        let read = file.read(&mut prefix).map_err(|error| {
            attachment_error(
                "attachment_io",
                "Attachment could not be read",
                format!("cannot read attachment {}: {error}", path.display()),
            )
        })?;
        if sniff_image_mime(&prefix[..read]).is_some() {
            return load_image_attachment(path).map(HeadlessAttachment::Image);
        }
        return load_pdf_attachment(path).map(HeadlessAttachment::Pdf);
    }
    match load_image_attachment(path) {
        Ok(image) => Ok(HeadlessAttachment::Image(image)),
        Err(HeadlessRunError::Attachment { ref code, .. })
            if code == "unsupported_attachment_type" =>
        {
            load_text_attachment(path).map(HeadlessAttachment::File)
        }
        Err(error) => Err(error),
    }
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
    /// Already-durable attachment blocks reused by replay without copying
    /// bytes through a second upload path.
    pub durable_attachments: Vec<AttachmentBlock>,
    /// Explicit provider override. `None` follows the daemon's active account.
    pub provider: Option<String>,
    /// Explicit model override. `None` follows the selected provider summary.
    pub model: Option<String>,
    pub max_tokens: u64,
    pub budget: RunBudgetV1,
    pub seed: Option<u64>,
    pub replay_of: Option<RunId>,
    /// Record the resolved run inputs through the headless v1 journal
    /// contract even when the caller waits for completion.
    pub journal_pin: bool,
    /// Return immediately after durable acceptance. The daemon and journal
    /// remain the owners of the running turn.
    pub detached: bool,
    pub permission_overrides: SessionPermissionOverridesV1,
    /// Trust discovered hooks for this run only. The daemon journals the
    /// grant in the same atomic acceptance transaction as the turn.
    pub trust_hooks: bool,
    pub timeout: Option<Duration>,
    pub terminal_grace: Duration,
}

/// Optional durable session configuration applied after creation and control
/// attachment, before the first turn is submitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadlessSessionConfig {
    /// A model id or `provider/model` selector. A slash is treated as a
    /// provider separator only when its prefix is a registered provider.
    pub model: Option<String>,
    /// Provider-vocabulary effort level, validated by the daemon against the
    /// selected pair's current catalog.
    pub effort: Option<String>,
    /// `Some(true)` selects fast; `Some(false)` durably selects normal.
    pub fast: Option<bool>,
    /// Exact account alias for this session. When no model is supplied, the
    /// daemon atomically selects this account's provider default.
    pub account: Option<String>,
    /// Launch-time model visibility for saved SSH profiles. `None` omits the
    /// additive field and therefore means the daemon's `All` default.
    pub ssh_scope: Option<haider_rpc::SshScopeWire>,
}

/// Incremental facts exposed to output adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum HeadlessEvent {
    /// Durable turn acceptance, emitted before any replayed/model envelope.
    Accepted {
        session_id: SessionId,
        head_seq: u64,
    },
    /// One fully applied durable envelope. Duplicates and gap-crossing frames
    /// are never emitted.
    Envelope(Box<RawEnvelope>),
    /// The run's one durable terminal envelope plus its stable automation
    /// discriminator. This replaces, rather than duplicates, the ordinary
    /// envelope event at the same cursor.
    Terminal(HeadlessTerminalEvent),
    /// Observable fail-closed decision for a permission-gated effect.
    PermissionDenied(HeadlessPermissionDenial),
}

/// Process-interrupt intent supplied by a headless surface.
///
/// The first interrupt is converted into one idempotent durable
/// `turn.cancel`. A subsequent interrupt stops waiting for the terminal as
/// soon as the first cancellation receipt is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessInterrupt {
    CancelAndDrain,
    ExitImmediately,
}

/// Stable terminal vocabulary for attached automation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadlessTerminalKind {
    Success,
    Failure,
    Budget,
    Cancellation,
    Timeout,
    ProviderError,
}

/// One typed terminal carrying the original durable cursor and payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadlessTerminalEvent {
    pub envelope: Box<RawEnvelope>,
    pub kind: HeadlessTerminalKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Delivery/retention policy for a headless output adapter.
///
/// Retaining modes keep the correlated ledger. `FullRecordSet` starts that
/// ledger on disk for single-JSON output, while ordinary runs retain small
/// ledgers in memory and spill once at the documented threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessEventMode {
    /// Stream every envelope (for JSONL and general API consumers).
    Stream,
    /// Stream every envelope without cloning it into the returned result.
    /// Intended for adapters, such as CLI JSONL, whose output is itself the
    /// lossless record and which never consume `HeadlessRunResult::events`.
    StreamWithoutResultLedger,
    /// Stream announcements/denials only; retain the ledger for the result.
    Summary,
    /// Retain the complete ledger on disk for one final JSON document.
    FullRecordSet,
}

impl HeadlessEventMode {
    fn streams_envelopes(self) -> bool {
        matches!(self, Self::Stream | Self::StreamWithoutResultLedger)
    }

    fn spools_immediately(self) -> bool {
        self == Self::FullRecordSet
    }

    fn retains_result_ledger(self) -> bool {
        self != Self::StreamWithoutResultLedger
    }
}

/// Machine-readable permission denial made by the headless default policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessPermissionDenial {
    /// Permission-menu id for the interactive fallback, or the effect id when
    /// autonomous policy denied the effect before opening a menu.
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
    Started,
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
    /// Safe typed presentation copied from the durable protocol event/card.
    pub presentation: Option<ErrorPresentation>,
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
            Self::Run(code) => code.as_str(),
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
    /// Lossless correlated journal stream. Event payloads are the shared
    /// client contract, including typed tool results and normalized cache
    /// read/write usage; unknown future payloads remain intact.
    pub events: HeadlessRunEvents,
    pub budget_exhausted: Option<RunBudgetExhaustedV1>,
    pub replay: Option<ReplayDivergenceV1>,
    pub permission_denials: Vec<HeadlessPermissionDenial>,
    pub failure: Option<HeadlessRunFailure>,
    pub terminal_seq: Option<u64>,
    /// Background tasks with a durable started fact and no completed fact
    /// when the run's terminal was observed (W-A decision 8).
    pub background_tasks_running: Vec<HeadlessBackgroundTask>,
}

/// Lossless correlated journal stream for a headless run.
///
/// Small ledgers remain in memory. Once the retained-byte estimate crosses
/// [`HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES`], the complete prefix is written
/// to one private spool and all later records stay there; memory and disk
/// records therefore never interleave. The spool is deleted after the last
/// clone is dropped.
#[derive(Clone)]
pub struct HeadlessRunEvents {
    run_id: RunId,
    len: usize,
    storage: HeadlessRunEventStorage,
}

#[derive(Clone)]
enum HeadlessRunEventStorage {
    Empty,
    Memory(Arc<Vec<RawEnvelope>>),
    Spool(Arc<HeadlessEventSpoolCleanup>),
}

impl HeadlessRunEvents {
    #[must_use]
    pub fn empty(run_id: RunId) -> Self {
        Self {
            run_id,
            len: 0,
            storage: HeadlessRunEventStorage::Empty,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Opens an independent streaming reader at the start of the ledger.
    pub fn iter(&self) -> std::io::Result<HeadlessRunEventReader> {
        let storage = match &self.storage {
            HeadlessRunEventStorage::Empty => HeadlessRunEventReaderStorage::Empty,
            HeadlessRunEventStorage::Memory(events) => HeadlessRunEventReaderStorage::Memory {
                events: Arc::clone(events),
                index: 0,
            },
            HeadlessRunEventStorage::Spool(spool) => {
                HeadlessRunEventReaderStorage::Spool(BufReader::new(File::open(&spool.path)?))
            }
        };
        Ok(HeadlessRunEventReader {
            storage,
            run_id: self.run_id.clone(),
            expected: self.len,
            yielded: 0,
            failed: false,
        })
    }

    /// Visits every envelope without materializing the ledger.
    pub fn try_for_each(
        &self,
        mut visit: impl FnMut(RawEnvelope) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        for envelope in self.iter()? {
            visit(envelope?)?;
        }
        Ok(())
    }

    /// Builds a hybrid ledger from an already bounded fixture batch.
    pub fn from_envelopes(
        run_id: RunId,
        envelopes: impl IntoIterator<Item = RawEnvelope>,
    ) -> Result<Self, HeadlessRunError> {
        let mut writer = HeadlessEventLedgerWriter::new(false);
        let mut len = 0_usize;
        for envelope in envelopes {
            if envelope.run_id.as_ref() != Some(&run_id) {
                return Err(protocol_error(
                    "event spool",
                    "fixture envelope does not match the ledger run id",
                ));
            }
            writer.record_owned(envelope);
            len = len.saturating_add(1);
        }
        writer.finish(run_id, len)
    }
}

impl std::fmt::Debug for HeadlessRunEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HeadlessRunEvents")
            .field("run_id", &self.run_id)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl PartialEq for HeadlessRunEvents {
    fn eq(&self, other: &Self) -> bool {
        if self.run_id != other.run_id || self.len != other.len {
            return false;
        }
        let (Ok(mut left), Ok(mut right)) = (self.iter(), other.iter()) else {
            return false;
        };
        loop {
            match (left.next(), right.next()) {
                (None, None) => return true,
                (Some(Ok(left)), Some(Ok(right))) if left == right => {}
                _ => return false,
            }
        }
    }
}

impl Serialize for HeadlessRunEvents {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.len))?;
        if let HeadlessRunEventStorage::Memory(events) = &self.storage {
            for envelope in events.iter() {
                sequence.serialize_element(envelope)?;
            }
        } else {
            let events = self.iter().map_err(S::Error::custom)?;
            for envelope in events {
                sequence.serialize_element(&envelope.map_err(S::Error::custom)?)?;
            }
        }
        sequence.end()
    }
}

/// Streaming reader for [`HeadlessRunEvents`].
pub struct HeadlessRunEventReader {
    storage: HeadlessRunEventReaderStorage,
    run_id: RunId,
    expected: usize,
    yielded: usize,
    failed: bool,
}

enum HeadlessRunEventReaderStorage {
    Empty,
    Memory {
        events: Arc<Vec<RawEnvelope>>,
        index: usize,
    },
    Spool(BufReader<File>),
}

impl Iterator for HeadlessRunEventReader {
    type Item = std::io::Result<RawEnvelope>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let event = match &mut self.storage {
            HeadlessRunEventReaderStorage::Empty => {
                if self.yielded == self.expected {
                    return None;
                }
                self.failed = true;
                return Some(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "headless event ledger is absent",
                )));
            }
            HeadlessRunEventReaderStorage::Memory { events, index } => {
                let Some(event) = events.get(*index).cloned() else {
                    if self.yielded == self.expected {
                        return None;
                    }
                    self.failed = true;
                    return Some(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "headless event ledger ended after {} of {} envelopes",
                            self.yielded, self.expected
                        ),
                    )));
                };
                *index = index.saturating_add(1);
                event
            }
            HeadlessRunEventReaderStorage::Spool(file) => {
                let mut record = Vec::new();
                let read = match file.read_until(b'\n', &mut record) {
                    Ok(read) => read,
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(error));
                    }
                };
                if read == 0 {
                    if self.yielded == self.expected {
                        return None;
                    }
                    self.failed = true;
                    return Some(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "headless event ledger ended after {} of {} envelopes",
                            self.yielded, self.expected
                        ),
                    )));
                }
                if record.last() != Some(&b'\n') {
                    self.failed = true;
                    return Some(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "headless event ledger has a partial final record",
                    )));
                }
                match serde_json::from_slice::<RawEnvelope>(&record[..record.len() - 1]) {
                    Ok(event) => event,
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("cannot decode headless event ledger: {error}"),
                        )));
                    }
                }
            }
        };
        if event.run_id.as_ref() != Some(&self.run_id) {
            self.failed = true;
            return Some(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headless event ledger contains a mismatched run id",
            )));
        }
        if self.yielded == self.expected {
            self.failed = true;
            return Some(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headless event ledger contains more envelopes than recorded",
            )));
        }
        self.yielded = self.yielded.saturating_add(1);
        Some(Ok(event))
    }
}

/// Durable lifecycle snapshot resolved by run id, independent of a live
/// attachment or the process which originally started the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadlessRunStatus {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub worker_generation: u64,
    pub state: RunState,
    pub head_seq: u64,
    pub terminal_seq: Option<u64>,
    pub budget_exhausted: Option<RunBudgetExhaustedV1>,
    pub spec: HeadlessRunSpecV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessRunStopResult {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub status: CancelStatus,
    pub terminal_seq: Option<u64>,
}

/// Failure before a correlated final result could be produced.
#[derive(Debug)]
pub enum HeadlessRunError {
    Attachment {
        code: String,
        message: String,
        presentation: haider_rpc::haider_protocol::error::ErrorPresentation,
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
            Self::Attachment { code, message, .. } => {
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
    daemon_ownership: Arc<Mutex<Option<DaemonOwnershipToken>>>,
    attachment_id: Option<AttachmentId>,
    worker_generation: u64,
    observed_lost_events: u64,
}

fn lock_daemon_ownership(
    ownership: &Mutex<Option<DaemonOwnershipToken>>,
) -> std::sync::MutexGuard<'_, Option<DaemonOwnershipToken>> {
    match ownership.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
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

fn headless_submit_body_with_spec(
    trust_hooks: bool,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    text: String,
    attachments: Vec<AttachmentBlock>,
    spec: Option<HeadlessRunSpecV1>,
) -> RequestBody {
    match spec {
        Some(spec) => RequestBody::HeadlessRunStart {
            command_id,
            session_id,
            worker_generation,
            text,
            attachments,
            spec,
            trust_hooks,
        },
        None => headless_submit_body(
            trust_hooks,
            command_id,
            session_id,
            worker_generation,
            text,
            attachments,
        ),
    }
}

impl HeadlessConnection {
    /// Boxed on purpose: this future carries the whole `ensure_daemon` →
    /// `try_attach` spawn subtree, whose debug-build frames are large enough
    /// that inlining them into every reconnect caller overflows the test
    /// thread stack now that the E5-E8 protocol types grew (bisected via the
    /// competing-permission law). Heap-pinning detaches that subtree from
    /// every caller's stack frame.
    fn open<'a>(
        profile: &'a ResolvedProfile,
        options: EnsureOptions,
        daemon_ownership: Arc<Mutex<Option<DaemonOwnershipToken>>>,
    ) -> Pin<Box<dyn Future<Output = Result<Self, HeadlessRunError>> + Send + 'a>> {
        Box::pin(Self::open_inner(profile, options, daemon_ownership))
    }

    async fn open_inner(
        profile: &ResolvedProfile,
        options: EnsureOptions,
        daemon_ownership: Arc<Mutex<Option<DaemonOwnershipToken>>>,
    ) -> Result<Self, HeadlessRunError> {
        let mut ensured = ensure_daemon(profile, options)
            .await
            .map_err(HeadlessRunError::Ensure)?;
        if let Some(ownership) = ensured.ownership.take() {
            let mut slot = lock_daemon_ownership(&daemon_ownership);
            let replace = match slot.as_mut() {
                None => true,
                Some(existing) => existing.child.try_wait().ok().flatten().is_some(),
            };
            if replace {
                *slot = Some(ownership);
            }
        }
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
            daemon_ownership,
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

struct HeadlessEventSpoolCleanup {
    path: PathBuf,
}

impl Drop for HeadlessEventSpoolCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct BufferedWireFrames {
    file: Option<BufWriter<File>>,
    len: usize,
    cleanup: Option<Arc<HeadlessEventSpoolCleanup>>,
}

impl BufferedWireFrames {
    fn new() -> Self {
        Self {
            file: None,
            len: 0,
            cleanup: None,
        }
    }

    fn ensure_file(&mut self) -> Result<&mut BufWriter<File>, HeadlessRunError> {
        if self.file.is_none() {
            let (file, cleanup) = create_private_spool("haider-replay", "submit replay spool")?;
            self.file = Some(file);
            self.cleanup = Some(cleanup);
        }
        self.file
            .as_mut()
            .ok_or_else(|| protocol_error("submit replay spool", "replay spool was not created"))
    }

    fn clear(&mut self) -> Result<(), HeadlessRunError> {
        if let Some(file) = self.file.as_mut() {
            file.flush()
                .and_then(|()| file.get_mut().set_len(0))
                .and_then(|()| file.get_mut().seek(std::io::SeekFrom::Start(0)))
                .map_err(|error| HeadlessRunError::Protocol {
                    stage: "submit replay spool",
                    message: format!("cannot reset replay spool: {error}"),
                })?;
        }
        self.len = 0;
        Ok(())
    }

    fn push(&mut self, frame: &WireFrame) -> Result<(), HeadlessRunError> {
        let file = self.ensure_file()?;
        serde_json::to_writer(&mut *file, frame)
            .map_err(std::io::Error::other)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush())
            .map_err(|error| HeadlessRunError::Protocol {
                stage: "submit replay spool",
                message: format!("cannot write replay spool: {error}"),
            })?;
        self.len = self.len.saturating_add(1);
        Ok(())
    }

    fn reader(&mut self) -> Result<BufferedWireFrameReader, HeadlessRunError> {
        if let Some(file) = self.file.as_mut() {
            file.flush().map_err(|error| HeadlessRunError::Protocol {
                stage: "submit replay spool",
                message: format!("cannot flush replay spool: {error}"),
            })?;
        }
        let file = self
            .cleanup
            .as_ref()
            .map(|cleanup| File::open(&cleanup.path).map(BufReader::new))
            .transpose()
            .map_err(|error| HeadlessRunError::Protocol {
                stage: "submit replay spool",
                message: format!("cannot open replay spool: {error}"),
            })?;
        Ok(BufferedWireFrameReader {
            file,
            expected: self.len,
            yielded: 0,
            failed: false,
            _cleanup: self.cleanup.clone(),
        })
    }
}

struct BufferedWireFrameReader {
    file: Option<BufReader<File>>,
    expected: usize,
    yielded: usize,
    failed: bool,
    _cleanup: Option<Arc<HeadlessEventSpoolCleanup>>,
}

impl Iterator for BufferedWireFrameReader {
    type Item = Result<WireFrame, HeadlessRunError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let Some(file) = self.file.as_mut() else {
            if self.yielded == self.expected {
                return None;
            }
            self.failed = true;
            return Some(Err(protocol_error(
                "submit replay spool",
                "replay spool is absent before its recorded frame count",
            )));
        };
        let mut record = Vec::new();
        let read = match file.read_until(b'\n', &mut record) {
            Ok(read) => read,
            Err(error) => {
                self.failed = true;
                return Some(Err(protocol_error(
                    "submit replay spool",
                    &format!("cannot read replay spool: {error}"),
                )));
            }
        };
        if read == 0 {
            if self.yielded == self.expected {
                return None;
            }
            self.failed = true;
            return Some(Err(protocol_error(
                "submit replay spool",
                "replay spool ended before its recorded frame count",
            )));
        }
        if record.last() != Some(&b'\n') {
            self.failed = true;
            return Some(Err(protocol_error(
                "submit replay spool",
                "replay spool has a partial final record",
            )));
        }
        if self.yielded == self.expected {
            self.failed = true;
            return Some(Err(protocol_error(
                "submit replay spool",
                "replay spool contains more frames than recorded",
            )));
        }
        let frame = serde_json::from_slice(&record[..record.len() - 1]).map_err(|error| {
            protocol_error(
                "submit replay spool",
                &format!("cannot decode replay spool: {error}"),
            )
        });
        if frame.is_err() {
            self.failed = true;
        } else {
            self.yielded = self.yielded.saturating_add(1);
        }
        Some(frame)
    }
}

fn create_private_spool(
    prefix: &str,
    stage: &'static str,
) -> Result<(BufWriter<File>, Arc<HeadlessEventSpoolCleanup>), HeadlessRunError> {
    let path = std::env::temp_dir().join(format!("{}.jsonl", command_id(prefix)));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(&path)
        .map_err(|error| HeadlessRunError::Protocol {
            stage,
            message: format!("cannot create private spool: {error}"),
        })?;
    Ok((
        BufWriter::new(file),
        Arc::new(HeadlessEventSpoolCleanup { path }),
    ))
}

struct SpilledEventLedger {
    file: BufWriter<File>,
    cleanup: Arc<HeadlessEventSpoolCleanup>,
    len: usize,
    unflushed_bytes: usize,
}

enum HeadlessEventLedgerState {
    Memory {
        events: Vec<RawEnvelope>,
        estimated_bytes: usize,
    },
    Spool(SpilledEventLedger),
}

struct HeadlessEventLedgerWriter {
    state: HeadlessEventLedgerState,
    spool_immediately: bool,
    error: Option<String>,
}

impl HeadlessEventLedgerWriter {
    fn new(spool_immediately: bool) -> Self {
        Self {
            state: HeadlessEventLedgerState::Memory {
                events: Vec::new(),
                estimated_bytes: 0,
            },
            spool_immediately,
            error: None,
        }
    }

    fn record(&mut self, envelope: &RawEnvelope) {
        if self.error.is_some() {
            return;
        }
        let estimate = envelope_weight_bytes(envelope);
        let should_spill = match &self.state {
            HeadlessEventLedgerState::Memory {
                estimated_bytes, ..
            } => {
                self.spool_immediately
                    || estimated_bytes.saturating_add(estimate)
                        > HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES
            }
            HeadlessEventLedgerState::Spool(_) => false,
        };
        if should_spill {
            self.spill_and_record(envelope, estimate);
            return;
        }
        match &mut self.state {
            HeadlessEventLedgerState::Memory {
                events,
                estimated_bytes,
            } => {
                events.push(envelope.clone());
                *estimated_bytes = estimated_bytes.saturating_add(estimate);
            }
            HeadlessEventLedgerState::Spool(spool) => {
                if let Err(error) = write_event_spool_record(spool, envelope, estimate) {
                    self.error = Some(error.to_string());
                }
            }
        }
    }

    fn record_owned(&mut self, envelope: RawEnvelope) {
        if self.error.is_some() {
            return;
        }
        let estimate = envelope_weight_bytes(&envelope);
        let should_spill = match &self.state {
            HeadlessEventLedgerState::Memory {
                estimated_bytes, ..
            } => {
                self.spool_immediately
                    || estimated_bytes.saturating_add(estimate)
                        > HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES
            }
            HeadlessEventLedgerState::Spool(_) => false,
        };
        if should_spill {
            self.spill_and_record(&envelope, estimate);
            return;
        }
        match &mut self.state {
            HeadlessEventLedgerState::Memory {
                events,
                estimated_bytes,
            } => {
                events.push(envelope);
                *estimated_bytes = estimated_bytes.saturating_add(estimate);
            }
            HeadlessEventLedgerState::Spool(spool) => {
                if let Err(error) = write_event_spool_record(spool, &envelope, estimate) {
                    self.error = Some(error.to_string());
                }
            }
        }
    }

    fn spill_and_record(&mut self, envelope: &RawEnvelope, estimate: usize) {
        let previous = match std::mem::replace(
            &mut self.state,
            HeadlessEventLedgerState::Memory {
                events: Vec::new(),
                estimated_bytes: 0,
            },
        ) {
            HeadlessEventLedgerState::Memory { events, .. } => events,
            HeadlessEventLedgerState::Spool(spool) => {
                self.state = HeadlessEventLedgerState::Spool(spool);
                return;
            }
        };
        let opened = create_private_spool("haider-events", "event spool").map(|(file, cleanup)| {
            SpilledEventLedger {
                file,
                cleanup,
                len: 0,
                unflushed_bytes: 0,
            }
        });
        let mut spool = match opened {
            Ok(spool) => spool,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        for event in &previous {
            if let Err(error) =
                write_event_spool_record(&mut spool, event, envelope_weight_bytes(event))
            {
                self.error = Some(error.to_string());
                return;
            }
        }
        if let Err(error) = write_event_spool_record(&mut spool, envelope, estimate) {
            self.error = Some(error.to_string());
            return;
        }
        self.state = HeadlessEventLedgerState::Spool(spool);
    }

    fn finish(
        mut self,
        run_id: RunId,
        expected_len: usize,
    ) -> Result<HeadlessRunEvents, HeadlessRunError> {
        if let Some(message) = self.error.take() {
            return Err(HeadlessRunError::Protocol {
                stage: "event spool",
                message,
            });
        }
        let (len, storage) = match self.state {
            HeadlessEventLedgerState::Memory { events, .. } => {
                let len = events.len();
                let storage = if events.is_empty() {
                    HeadlessRunEventStorage::Empty
                } else {
                    HeadlessRunEventStorage::Memory(Arc::new(events))
                };
                (len, storage)
            }
            HeadlessEventLedgerState::Spool(mut spool) => {
                spool
                    .file
                    .flush()
                    .map_err(|error| HeadlessRunError::Protocol {
                        stage: "event spool",
                        message: format!("cannot flush event ledger at terminal: {error}"),
                    })?;
                (spool.len, HeadlessRunEventStorage::Spool(spool.cleanup))
            }
        };
        if len != expected_len {
            return Err(protocol_error(
                "event spool",
                "correlated event count did not match the retained ledger",
            ));
        }
        Ok(HeadlessRunEvents {
            run_id,
            len,
            storage,
        })
    }
}

fn write_event_spool_record(
    spool: &mut SpilledEventLedger,
    envelope: &RawEnvelope,
    estimate: usize,
) -> std::io::Result<()> {
    serde_json::to_writer(&mut spool.file, envelope).map_err(std::io::Error::other)?;
    spool.file.write_all(b"\n")?;
    spool.len = spool.len.saturating_add(1);
    spool.unflushed_bytes = spool.unflushed_bytes.saturating_add(estimate);
    if spool.unflushed_bytes >= HEADLESS_EVENT_SPOOL_FLUSH_BYTES {
        spool.file.flush()?;
        spool.unflushed_bytes = 0;
    }
    Ok(())
}

struct HeadlessEventOutput {
    sender: Option<mpsc::UnboundedSender<HeadlessEvent>>,
    stream_envelopes: bool,
    ledger: Option<HeadlessEventLedgerWriter>,
}

impl HeadlessEventOutput {
    fn new(sender: mpsc::UnboundedSender<HeadlessEvent>, mode: HeadlessEventMode) -> Self {
        Self {
            sender: Some(sender),
            stream_envelopes: mode.streams_envelopes(),
            ledger: mode
                .retains_result_ledger()
                .then(|| HeadlessEventLedgerWriter::new(mode.spools_immediately())),
        }
    }

    fn emit(&mut self, event: HeadlessEvent) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        if sender.send(event).is_err() {
            self.sender = None;
        }
    }

    fn emit_envelope(&mut self, envelope: RawEnvelope, correlated: bool) {
        if correlated && !self.stream_envelopes {
            if let Some(ledger) = self.ledger.as_mut() {
                ledger.record_owned(envelope);
            }
            return;
        }
        if correlated && let Some(ledger) = self.ledger.as_mut() {
            ledger.record(&envelope);
        }
        if self.stream_envelopes {
            self.emit(HeadlessEvent::Envelope(Box::new(envelope)));
        }
    }

    fn retain_envelope(&mut self, envelope: &RawEnvelope) {
        // The ledger is `Some` only for modes that retain a result set
        // (`retains_result_ledger`). Without one, `finish` returns an empty
        // run, so recording is correctly a no-op rather than an error.
        if let Some(ledger) = self.ledger.as_mut() {
            ledger.record(envelope);
        }
    }

    fn finish(
        self,
        run_id: RunId,
        expected_len: usize,
    ) -> Result<HeadlessRunEvents, HeadlessRunError> {
        match self.ledger {
            Some(ledger) => ledger.finish(run_id, expected_len),
            None => Ok(HeadlessRunEvents::empty(run_id)),
        }
    }
}

struct HeadlessReducer {
    session_id: SessionId,
    run_id: Option<RunId>,
    last_applied: u64,
    response: Option<String>,
    usage: Option<Usage>,
    permission_denials: Vec<HeadlessPermissionDenial>,
    effect_summaries: BTreeMap<String, String>,
    pending_run_failure: Option<(u64, HeadlessRunFailure)>,
    blocking_presentation: Option<ErrorPresentation>,
    terminal: Option<NaturalTerminal>,
    terminal_envelope: Option<RawEnvelope>,
    menu_resolutions: BTreeMap<String, DurableMenuResolution>,
    cancel_observed: bool,
    actions: VecDeque<ReducerAction>,
    output: HeadlessEventOutput,
    /// W-A: task id → (name, running) from the additive task facts, in
    /// deterministic id order for the run summary.
    background_tasks: BTreeMap<String, (String, bool)>,
    event_count: usize,
    budget_exhausted: Option<RunBudgetExhaustedV1>,
    deadline_exceeded: bool,
}

impl HeadlessReducer {
    fn new(session_id: SessionId, output: HeadlessEventOutput) -> Self {
        Self {
            session_id,
            run_id: None,
            last_applied: 0,
            response: None,
            usage: None,
            permission_denials: Vec::new(),
            effect_summaries: BTreeMap::new(),
            pending_run_failure: None,
            blocking_presentation: None,
            terminal: None,
            terminal_envelope: None,
            menu_resolutions: BTreeMap::new(),
            cancel_observed: false,
            actions: VecDeque::new(),
            background_tasks: BTreeMap::new(),
            event_count: 0,
            budget_exhausted: None,
            deadline_exceeded: false,
            output,
        }
    }

    fn is_correlated(&self, envelope: &RawEnvelope) -> bool {
        self.run_id
            .as_ref()
            .is_some_and(|run_id| envelope.run_id.as_ref() == Some(run_id))
    }

    fn emit(&mut self, event: HeadlessEvent) {
        self.output.emit(event);
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

        self.last_applied = envelope.seq;
        let correlated = self.is_correlated(&envelope);
        if correlated {
            self.event_count = self.event_count.saturating_add(1);
        }
        let payload_type = envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str);
        // W-A: background task facts are SESSION-scoped (they outlive turns
        // by design) and ride the additive union outside `EventPayload` —
        // track them regardless of run correlation so the run summary can
        // name still-running tasks honestly at exit.
        if matches!(payload_type, Some("task_started" | "task_completed"))
            && let Some(fact) =
                haider_rpc::haider_protocol::task::TaskEventPayload::from_payload_value(
                    &envelope.payload,
                )
        {
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
        if correlated
            && payload_type == Some("run_budget_exhausted")
            && let Some(HeadlessRunEventPayload::RunBudgetExhausted(exhausted)) =
                HeadlessRunEventPayload::from_payload_value(&envelope.payload)
        {
            self.budget_exhausted = Some(exhausted);
        }
        if correlated
            && payload_type == Some("run_deadline_exceeded")
            && matches!(
                HeadlessRunEventPayload::from_payload_value(&envelope.payload),
                Some(HeadlessRunEventPayload::RunDeadlineExceeded(_))
            )
        {
            self.deadline_exceeded = true;
        }
        // Only payload families that change the headless projection need a
        // typed decode. Decode from the already-parsed JSON value by reference:
        // streamed/retained envelopes keep their original lossless payload,
        // while unrelated large tool/history payloads avoid a second walk.
        let reduce_core_payload = match payload_type {
            Some("item") => {
                envelope
                    .payload
                    .get("event")
                    .and_then(serde_json::Value::as_str)
                    == Some("completed")
                    && matches!(
                        envelope
                            .payload
                            .get("item")
                            .and_then(|item| item.get("item"))
                            .and_then(serde_json::Value::as_str),
                        Some("agent_message" | "incomplete_agent_message")
                    )
            }
            Some("effect") => matches!(
                envelope
                    .payload
                    .get("phase")
                    .and_then(serde_json::Value::as_str),
                Some("intent" | "authorized")
            ),
            Some(
                "usage" | "run_failed" | "run_state" | "menu_opened" | "menu_answered"
                | "menu_closed",
            ) => true,
            _ => false,
        };
        let mut denial_to_emit = None;
        let mut is_terminal_envelope = false;
        if correlated
            && reduce_core_payload
            && let Ok(payload) = EventPayload::deserialize(&envelope.payload)
        {
            match payload {
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::AgentMessage { text },
                    ..
                }) => self.response = Some(text.into_string()),
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::IncompleteAgentMessage { text, .. },
                    ..
                }) => self.response = Some(text),
                EventPayload::Usage(usage) => self.usage = Some(usage),
                EventPayload::Effect(EffectPhase::Intent(intent)) => {
                    self.effect_summaries
                        .insert(intent.effect.as_str().to_owned(), intent.summary);
                }
                EventPayload::Effect(EffectPhase::Authorized {
                    effect,
                    verdict: AuthorizationVerdict::Deny { .. },
                }) => {
                    let denial = HeadlessPermissionDenial {
                        menu_id: effect.as_str().to_owned(),
                        effect_summary: self
                            .effect_summaries
                            .remove(effect.as_str())
                            .unwrap_or_else(|| effect.as_str().to_owned()),
                        notice: "permission_denied_by_headless_default".into(),
                    };
                    self.permission_denials.push(denial.clone());
                    denial_to_emit = Some(denial);
                }
                EventPayload::Effect(EffectPhase::Authorized { effect, .. }) => {
                    self.effect_summaries.remove(effect.as_str());
                }
                EventPayload::RunFailed {
                    code,
                    message,
                    retryable,
                    presentation,
                } => {
                    let message = presentation
                        .as_ref()
                        .map_or(message, |safe| format!("{} — {}", safe.title, safe.detail));
                    self.pending_run_failure = Some((
                        envelope.seq,
                        HeadlessRunFailure {
                            code: HeadlessFailureCode::Run(code),
                            message,
                            retryable,
                            presentation,
                        },
                    ));
                }
                EventPayload::RunState(state) => {
                    is_terminal_envelope = self.reduce_run_state(state, envelope.seq);
                }
                EventPayload::MenuOpened(menu) => match menu.kind {
                    MenuKind::Permission { effect_summary } => {
                        let selected =
                            menu.options.iter().enumerate().find(|(_, option)| {
                                option.decision == Some(DecisionKind::RejectOnce)
                            });
                        if let Some((index, option)) = selected
                            && let Ok(option_index) = u32::try_from(index)
                        {
                            let denial = HeadlessPermissionDenial {
                                menu_id: menu.id.as_str().to_owned(),
                                effect_summary,
                                notice: "permission_denied_by_headless_default".into(),
                            };
                            self.permission_denials.push(denial.clone());
                            denial_to_emit = Some(denial);
                            self.actions.push_back(ReducerAction::RejectPermission {
                                command_id: CommandId::new(command_id("headless-menu")),
                                menu_id: menu.id,
                                request_seq: envelope.seq,
                                option_key: option.key.clone(),
                                option_index,
                            });
                        } else {
                            self.actions.push_back(ReducerAction::Block(
                                HeadlessBlockingReason::PermissionRejectUnavailable,
                            ));
                        }
                    }
                    MenuKind::ErrorRecovery { presentation, .. } if menu.blocking => {
                        self.blocking_presentation = Some(presentation);
                        self.actions
                            .push_back(ReducerAction::Block(HeadlessBlockingReason::InputRequired));
                    }
                    _ if menu.blocking => self
                        .actions
                        .push_back(ReducerAction::Block(HeadlessBlockingReason::InputRequired)),
                    _ => {}
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
        }
        if is_terminal_envelope {
            self.output.retain_envelope(&envelope);
            self.terminal_envelope = Some(envelope);
        } else {
            self.output.emit_envelope(envelope, correlated);
        }
        if let Some(denial) = denial_to_emit {
            self.emit(HeadlessEvent::PermissionDenied(denial));
        }
        ApplyStatus::Applied
    }

    fn reduce_run_state(&mut self, state: RunState, seq: u64) -> bool {
        match state {
            RunState::Done => {
                self.terminal = Some(NaturalTerminal {
                    outcome: HeadlessOutcome::Done,
                    failure: None,
                    seq,
                });
                true
            }
            RunState::Cancelled => {
                self.cancel_observed = true;
                self.terminal = Some(NaturalTerminal {
                    outcome: HeadlessOutcome::Cancelled,
                    failure: None,
                    seq,
                });
                true
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
                        presentation: None,
                    });
                if self.deadline_exceeded {
                    self.terminal = Some(NaturalTerminal {
                        outcome: HeadlessOutcome::Timeout,
                        failure: Some(HeadlessRunFailure {
                            code: HeadlessFailureCode::Timeout,
                            message: "run exceeded its wall-clock timeout".into(),
                            retryable: false,
                            presentation: None,
                        }),
                        seq,
                    });
                    return true;
                }
                self.terminal = Some(NaturalTerminal {
                    outcome: HeadlessOutcome::Errored,
                    failure: Some(failure),
                    seq,
                });
                true
            }
            RunState::InputRequired { .. } => {
                self.actions
                    .push_back(ReducerAction::Block(HeadlessBlockingReason::InputRequired));
                false
            }
            RunState::EffectOutcomeUnknown => {
                self.actions.push_back(ReducerAction::Block(
                    HeadlessBlockingReason::EffectOutcomeUnknown,
                ));
                false
            }
            RunState::Cancelling => {
                self.cancel_observed = true;
                false
            }
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
            | RunState::Concluding => false,
        }
    }
}

#[derive(Debug, Clone)]
enum ForcedOutcome {
    Timeout,
    Blocked(HeadlessBlockingReason),
    Interrupted,
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
    run_headless_with_session_config(
        profile,
        ensure,
        request,
        HeadlessSessionConfig::default(),
        output,
    )
    .await
}

/// [`run_headless`] with an optional durable configuration phase between
/// session attachment and the first turn submission.
pub async fn run_headless_with_session_config(
    profile: &ResolvedProfile,
    ensure: EnsureOptions,
    request: HeadlessRunRequest,
    session_config: HeadlessSessionConfig,
    output: mpsc::Sender<HeadlessEvent>,
) -> Result<HeadlessRunResult, HeadlessRunError> {
    run_headless_with_session_config_and_event_mode(
        profile,
        ensure,
        request,
        session_config,
        output,
        HeadlessEventMode::Stream,
    )
    .await
}

/// [`run_headless_with_session_config`] with an explicit output-adapter event
/// policy. This keeps single-JSON output off the live envelope channel while
/// preserving its complete, byte-stable final ledger.
pub async fn run_headless_with_session_config_and_event_mode(
    profile: &ResolvedProfile,
    ensure: EnsureOptions,
    request: HeadlessRunRequest,
    session_config: HeadlessSessionConfig,
    output: mpsc::Sender<HeadlessEvent>,
    event_mode: HeadlessEventMode,
) -> Result<HeadlessRunResult, HeadlessRunError> {
    run_headless_with_session_config_event_mode_and_interrupts(
        profile,
        ensure,
        request,
        session_config,
        output,
        event_mode,
        None,
    )
    .await
}

/// [`run_headless_with_session_config_and_event_mode`] with a surface-owned
/// interrupt stream. This keeps OS signal registration out of the reusable
/// client while putting durable cancel/drain ordering beside the correlated
/// attachment that owns it.
pub async fn run_headless_with_session_config_event_mode_and_interrupts(
    profile: &ResolvedProfile,
    ensure: EnsureOptions,
    request: HeadlessRunRequest,
    session_config: HeadlessSessionConfig,
    output: mpsc::Sender<HeadlessEvent>,
    event_mode: HeadlessEventMode,
    interrupts: Option<mpsc::UnboundedReceiver<HeadlessInterrupt>>,
) -> Result<HeadlessRunResult, HeadlessRunError> {
    let daemon_lifetime = ensure.daemon_lifetime;
    let daemon_ownership = Arc::new(Mutex::new(None));
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let reducer_output = HeadlessEventOutput::new(event_sender, event_mode);
    let forwarder = tokio::spawn(forward_headless_events(event_receiver, output));
    let teardown_client = ensure.client.clone();
    let result = run_headless_inner(
        profile,
        ensure,
        request,
        session_config,
        reducer_output,
        Arc::clone(&daemon_ownership),
        interrupts,
    )
    .await;
    let forwarding = forwarder.await.map_err(|error| HeadlessRunError::Protocol {
        stage: "event forwarding",
        message: format!("in-memory event forwarder failed: {error}"),
    });
    let teardown = if daemon_lifetime == DaemonLifetime::EphemeralIfSpawned {
        teardown_owned_daemon(profile, &teardown_client, &daemon_ownership).await
    } else {
        Ok(())
    };
    match (result, forwarding, teardown) {
        (Ok(result), Ok(()), Ok(())) => Ok(result),
        (Err(error), _, _) | (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => Err(error),
    }
}

async fn forward_headless_events(
    mut events: mpsc::UnboundedReceiver<HeadlessEvent>,
    output: mpsc::Sender<HeadlessEvent>,
) {
    while let Some(event) = events.recv().await {
        if output.send(event).await.is_err() {
            break;
        }
    }
}

fn normalize_lifecycle_options(options: &mut EnsureOptions) {
    options.required_features.extend(required_live_features());
    options
        .required_features
        .insert(haider_rpc::FEATURE_HEADLESS_RUN_V1.to_owned());
    options.client = ClientConfig {
        client_name: "haider-headless-lifecycle".into(),
        client_instance_id: command_id("headless-lifecycle-client"),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        ..options.client.clone()
    };
}

async fn lifecycle_request(
    profile: &ResolvedProfile,
    mut ensure: EnsureOptions,
    body: RequestBody,
    stage: &'static str,
) -> Result<ResponseBody, HeadlessRunError> {
    normalize_lifecycle_options(&mut ensure);
    let lifetime = ensure.daemon_lifetime;
    let ownership = Arc::new(Mutex::new(None));
    let mut connection = HeadlessConnection::open(profile, ensure, Arc::clone(&ownership)).await?;
    let response = connection
        .client
        .request(body)
        .await
        .map_err(|error| client_error_as_headless(stage, error));
    let teardown = if lifetime == DaemonLifetime::EphemeralIfSpawned {
        teardown_owned_daemon_on_connection(profile, &mut connection, &ownership).await
    } else {
        Ok(())
    };
    match (response, teardown) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Reads detached lifecycle state by the daemon-owned global run index seam.
pub async fn headless_run_status(
    profile: &ResolvedProfile,
    ensure: EnsureOptions,
    run_id: RunId,
) -> Result<HeadlessRunStatus, HeadlessRunError> {
    match lifecycle_request(
        profile,
        ensure,
        RequestBody::HeadlessRunStatus { run_id },
        "headless.run.status",
    )
    .await?
    {
        ResponseBody::HeadlessRunStatus {
            session_id,
            run_id,
            worker_generation,
            state,
            head_seq,
            terminal_seq,
            budget_exhausted,
            spec,
        } => Ok(HeadlessRunStatus {
            session_id,
            run_id,
            worker_generation,
            state,
            head_seq,
            terminal_seq,
            budget_exhausted,
            spec,
        }),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(rpc_error("headless.run.status", code, message, retryable)),
        _ => Err(protocol_error(
            "headless.run.status",
            "response method did not match request",
        )),
    }
}

/// Idempotently requests durable cancellation by run id.
pub async fn stop_headless_run(
    profile: &ResolvedProfile,
    ensure: EnsureOptions,
    run_id: RunId,
) -> Result<HeadlessRunStopResult, HeadlessRunError> {
    match lifecycle_request(
        profile,
        ensure,
        RequestBody::HeadlessRunStop {
            command_id: CommandId::new(command_id("headless-stop")),
            run_id,
        },
        "headless.run.stop",
    )
    .await?
    {
        ResponseBody::HeadlessRunStop {
            session_id,
            run_id,
            status,
            terminal_seq,
        } => Ok(HeadlessRunStopResult {
            session_id,
            run_id,
            status,
            terminal_seq,
        }),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(rpc_error("headless.run.stop", code, message, retryable)),
        _ => Err(protocol_error(
            "headless.run.stop",
            "response method did not match request",
        )),
    }
}

/// Reads the exact typed source stream used to reconstruct replay inputs.
pub async fn headless_run_events(
    profile: &ResolvedProfile,
    ensure: EnsureOptions,
    status: &HeadlessRunStatus,
) -> Result<HeadlessRunEvents, HeadlessRunError> {
    let mut writer = HeadlessEventLedgerWriter::new(false);
    let mut event_count = 0_usize;
    let mut start_seq = 1_u64;
    while start_seq <= status.head_seq {
        let end_seq = start_seq.saturating_add(1023).min(status.head_seq);
        let response = lifecycle_request(
            profile,
            ensure.clone(),
            RequestBody::SessionRead {
                session_id: status.session_id.clone(),
                range: SeqRange { start_seq, end_seq },
            },
            "session.read for replay",
        )
        .await?;
        match response {
            ResponseBody::SessionRead { result } if result.session_id == status.session_id => {
                for envelope in result
                    .envelopes
                    .into_iter()
                    .filter(|envelope| envelope.run_id.as_ref() == Some(&status.run_id))
                {
                    writer.record_owned(envelope);
                    event_count = event_count.saturating_add(1);
                }
            }
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => {
                return Err(rpc_error(
                    "session.read for replay",
                    code,
                    message,
                    retryable,
                ));
            }
            _ => {
                return Err(protocol_error(
                    "session.read for replay",
                    "response method did not match request",
                ));
            }
        }
        start_seq = end_seq.saturating_add(1);
    }
    writer.finish(status.run_id.clone(), event_count)
}

async fn teardown_owned_daemon(
    profile: &ResolvedProfile,
    client: &ClientConfig,
    daemon_ownership: &Mutex<Option<DaemonOwnershipToken>>,
) -> Result<(), HeadlessRunError> {
    let Some(ownership) = take_live_daemon_ownership(daemon_ownership)? else {
        return Ok(());
    };
    let request_deadline = Instant::now() + EPHEMERAL_DRAIN_ALLOWANCE;
    let shutdown =
        reconnect_and_shutdown_owned_daemon_peer(profile, client, &ownership, request_deadline)
            .await;
    finish_owned_daemon_teardown(ownership, shutdown, request_deadline).await
}

async fn teardown_owned_daemon_on_connection(
    profile: &ResolvedProfile,
    connection: &mut HeadlessConnection,
    daemon_ownership: &Mutex<Option<DaemonOwnershipToken>>,
) -> Result<(), HeadlessRunError> {
    let Some(ownership) = take_live_daemon_ownership(daemon_ownership)? else {
        return Ok(());
    };
    let request_deadline = Instant::now() + EPHEMERAL_DRAIN_ALLOWANCE;
    let HeadlessConnection { client, events, .. } = connection;
    let shutdown = shutdown_owned_daemon_peer(
        &profile.profile_id,
        client,
        events,
        &ownership,
        request_deadline,
    )
    .await;
    finish_owned_daemon_teardown(ownership, shutdown, request_deadline).await
}

fn take_live_daemon_ownership(
    daemon_ownership: &Mutex<Option<DaemonOwnershipToken>>,
) -> Result<Option<DaemonOwnershipToken>, HeadlessRunError> {
    let Some(mut ownership) = lock_daemon_ownership(daemon_ownership).take() else {
        return Ok(None);
    };
    if ownership
        .child
        .try_wait()
        .map_err(|error| teardown_protocol(format!("could not inspect owned child: {error}")))?
        .is_some()
    {
        return Ok(None);
    }
    Ok(Some(ownership))
}

async fn reconnect_and_shutdown_owned_daemon_peer(
    profile: &ResolvedProfile,
    client_config: &ClientConfig,
    ownership: &DaemonOwnershipToken,
    request_deadline: Instant,
) -> Result<Instant, HeadlessRunError> {
    let connected = tokio::time::timeout_at(
        request_deadline,
        connect(&profile.endpoint_path, client_config.clone()),
    )
    .await
    .map_err(|_| teardown_protocol("timed out reconnecting to owned daemon"))?
    .map_err(|error| teardown_protocol(format!("could not reconnect to owned daemon: {error}")))?;
    let mut events = connected
        .client
        .take_events()
        .ok_or_else(|| teardown_protocol("one-shot teardown could not retain daemon events"))?;
    shutdown_owned_daemon_peer(
        &profile.profile_id,
        &connected.client,
        &mut events,
        ownership,
        request_deadline,
    )
    .await
}

async fn shutdown_owned_daemon_peer(
    expected_profile_id: &str,
    client: &RpcClient,
    events: &mut mpsc::Receiver<WireFrame>,
    ownership: &DaemonOwnershipToken,
    request_deadline: Instant,
) -> Result<Instant, HeadlessRunError> {
    let welcome = client.welcome();
    let peer_credentials = client.peer_credentials();
    #[cfg(unix)]
    let uid_changed = peer_credentials.uid != effective_uid();
    #[cfg(not(unix))]
    let uid_changed = false;
    if welcome.profile_id != expected_profile_id
        || welcome.instance_id != ownership.instance_id
        || welcome.daemon_generation != ownership.daemon_generation
        || uid_changed
        || peer_credentials.pid != Some(ownership.authenticated_pid)
    {
        let _ = client.close();
        return Err(teardown_protocol(
            "owned daemon identity changed before one-shot teardown",
        ));
    }

    let request = tokio::time::timeout_at(
        request_deadline,
        client.request(RequestBody::DaemonShutdown {}),
    )
    .await;
    let request_failure = match request {
        Ok(Ok(ResponseBody::DaemonShutdown {})) => None,
        Ok(Ok(ResponseBody::Error { code, message, .. }))
            if code == "unknown_method"
                || (code == "invalid_argument" && message == "unknown session method") =>
        {
            #[cfg(unix)]
            {
                signal_authenticated_peer(ownership.authenticated_pid).map_err(|error| {
                    teardown_protocol(format!(
                        "could not signal authenticated owned daemon: {error}"
                    ))
                })?;
                None
            }
            #[cfg(not(unix))]
            {
                Some("daemon does not support graceful shutdown RPC".to_owned())
            }
        }
        Ok(Ok(ResponseBody::Error { code, message, .. })) => Some(format!(
            "daemon refused graceful shutdown ({code}): {message}"
        )),
        Ok(Ok(_)) => Some("daemon returned the wrong shutdown response method".to_owned()),
        Ok(Err(error)) => Some(format!("shutdown RPC transport failed: {error}")),
        Err(_) => Some("shutdown RPC response timed out".to_owned()),
    };

    // The daemon's drain allowance begins only after it handles the RPC (or
    // signal), not when this client began reconnecting. Give publication of
    // ServerDraining the daemon's full legal window from that acknowledgment.
    let notice_deadline = Instant::now() + EPHEMERAL_DRAIN_ALLOWANCE + EPHEMERAL_REAP_GRACE;
    let notice = tokio::time::timeout_at(notice_deadline, async {
        while let Some(frame) = events.recv().await {
            if let WireFrame::ServerDraining {
                instance_id,
                daemon_generation,
                deadline_unix_ms,
                ..
            } = frame
            {
                return Some((instance_id, daemon_generation, deadline_unix_ms));
            }
        }
        None
    })
    .await
    .map_err(|_| {
        teardown_protocol(
            request_failure
                .as_deref()
                .unwrap_or("owned daemon drain notice timed out"),
        )
    })?
    .ok_or_else(|| {
        teardown_protocol(
            request_failure
                .as_deref()
                .unwrap_or("owned daemon disconnected without a drain notice"),
        )
    })?;
    if notice.0 != ownership.instance_id || notice.1 != ownership.daemon_generation {
        let _ = client.close();
        return Err(teardown_protocol(
            "owned daemon drain notice did not match its authenticated Welcome",
        ));
    }
    let drain_deadline = daemon_drain_deadline(notice.2);
    tokio::time::timeout_at(drain_deadline, client.disconnected())
        .await
        .map_err(|_| teardown_protocol("owned daemon disconnect timed out"))?;
    Ok(drain_deadline)
}

async fn finish_owned_daemon_teardown(
    ownership: DaemonOwnershipToken,
    shutdown: Result<Instant, HeadlessRunError>,
    request_deadline: Instant,
) -> Result<(), HeadlessRunError> {
    let reap_deadline = shutdown.as_ref().copied().unwrap_or(request_deadline);
    let reap = reap_owned_daemon(ownership, reap_deadline).await;
    match (shutdown, reap) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(_), Ok(())) => Ok(()),
    }
}

fn daemon_drain_deadline(deadline_unix_ms: u64) -> Instant {
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let remaining = Duration::from_millis(deadline_unix_ms.saturating_sub(now_unix_ms))
        .min(EPHEMERAL_DRAIN_ALLOWANCE);
    Instant::now() + remaining + EPHEMERAL_REAP_GRACE
}

async fn reap_owned_daemon(
    ownership: DaemonOwnershipToken,
    deadline: Instant,
) -> Result<(), HeadlessRunError> {
    let authenticated_pid = ownership.authenticated_pid;
    let child = ownership.child;
    // `std::process::Child::wait` blocks on the OS process-exit notification
    // (waitpid on Unix, the process handle on Windows). Keep this exact waiter
    // alive across the deadline: escalation addresses the authenticated PID,
    // then the retained child handle supplies the unconditional final reap.
    let mut wait = Box::pin(haider_platform::wait_for_child_exit(child));
    match tokio::time::timeout_at(deadline, &mut wait).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(teardown_protocol(format!(
            "could not reap owned daemon: {error}"
        ))),
        Err(_) => {
            let mut escalation_errors = Vec::new();
            if let Err(error) = haider_platform::signal_process(
                authenticated_pid,
                haider_platform::ProcessSignal::Terminate,
            ) && !haider_platform::process_error_is_missing(&error)
            {
                escalation_errors.push(format!("second shutdown signal failed: {error}"));
            }

            let forced_deadline = Instant::now() + EPHEMERAL_REAP_GRACE;
            match tokio::time::timeout_at(forced_deadline, &mut wait).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    return Err(teardown_protocol(format!(
                        "could not reap overdue owned daemon: {error}"
                    )));
                }
                Err(_) => {
                    if let Err(error) = haider_platform::kill_process_tree(authenticated_pid, true)
                        && !haider_platform::process_error_is_missing(&error)
                    {
                        escalation_errors.push(format!("forced process-tree kill failed: {error}"));
                    }
                    wait.await.map_err(|error| {
                        teardown_protocol(format!(
                            "could not reap force-terminated owned daemon: {error}"
                        ))
                    })?;
                }
            }

            let detail = if escalation_errors.is_empty() {
                String::new()
            } else {
                format!(" ({})", escalation_errors.join("; "))
            };
            Err(teardown_protocol(format!(
                "owned daemon exceeded its drain deadline and was terminated before final reap{detail}"
            )))
        }
    }
}

fn teardown_protocol(message: impl Into<String>) -> HeadlessRunError {
    HeadlessRunError::Protocol {
        stage: "daemon teardown",
        message: message.into(),
    }
}

async fn run_headless_inner(
    profile: &ResolvedProfile,
    mut ensure: EnsureOptions,
    request: HeadlessRunRequest,
    session_config: HeadlessSessionConfig,
    output: HeadlessEventOutput,
    daemon_ownership: Arc<Mutex<Option<DaemonOwnershipToken>>>,
    mut interrupts: Option<mpsc::UnboundedReceiver<HeadlessInterrupt>>,
) -> Result<HeadlessRunResult, HeadlessRunError> {
    if request.attachments.len() > MAX_HEADLESS_ATTACHMENTS {
        return Err(attachment_error(
            "too_many_attachments",
            "Too many attachments",
            format!(
                "headless run carries {} attachments; the limit is {MAX_HEADLESS_ATTACHMENTS}",
                request.attachments.len()
            ),
        ));
    }
    normalize_ensure_options(
        &mut ensure,
        request.permission_overrides,
        !request.attachments.is_empty(),
        request.trust_hooks,
    );
    let pinned_headless = request.journal_pin
        || request.detached
        || request.seed.is_some()
        || request.replay_of.is_some()
        || !request.budget.is_empty();
    if pinned_headless {
        ensure
            .required_features
            .insert(haider_rpc::FEATURE_HEADLESS_RUN_V1.to_owned());
    }
    if !request.budget.is_empty() {
        ensure
            .required_features
            .insert(haider_rpc::FEATURE_RUN_BUDGET_V1.to_owned());
    }
    normalize_session_config_features(&mut ensure, &session_config)?;
    let timeout_deadline = request.timeout.map(|timeout| Instant::now() + timeout);
    let submit_command_id = CommandId::new(command_id("headless-submit"));
    let mut reconnects = ReconnectBudget::new();
    let mut connection = before_acceptance_deadline(
        timeout_deadline,
        "connect",
        HeadlessConnection::open(profile, ensure.clone(), Arc::clone(&daemon_ownership)),
    )
    .await?;
    let (create_provider, create_model, resolve_provider, resolve_model) =
        before_acceptance_deadline(
            timeout_deadline,
            "model selector bootstrap",
            resolve_create_identity(
                profile,
                &ensure,
                &mut connection,
                &mut reconnects,
                request.provider.clone(),
                request.model.clone(),
                session_config.model.as_deref().or(request.model.as_deref()),
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
    let mut submit_attachments = submit_attachments;
    let mut attachment_refs = attachment_refs;
    for attachment in &request.durable_attachments {
        let artifact = match attachment {
            AttachmentBlock::Image { artifact, .. }
            | AttachmentBlock::PastedText { artifact, .. }
            | AttachmentBlock::File { artifact, .. }
            | AttachmentBlock::Pdf { artifact, .. } => Some(artifact.clone()),
            AttachmentBlock::Skill { .. } => None,
        };
        if let Some(artifact) = artifact {
            attachment_refs.push(artifact);
        }
    }
    submit_attachments.extend(request.durable_attachments.clone());
    let create_body = RequestBody::SessionCreateWithPermissionOverrides {
        command_id: CommandId::new(command_id("headless-create")),
        cwd: request.cwd.clone(),
        provider: create_provider.clone(),
        model: create_model.clone(),
        max_tokens: request.max_tokens,
        permission_overrides: (!request.permission_overrides.is_empty())
            .then_some(request.permission_overrides),
        cache_policy: None,
        interaction_mode: SessionInteractionModeV1::Autonomous,
        ssh_scope: session_config.ssh_scope.clone(),
        account_alias: session_config
            .account
            .as_deref()
            .map(haider_rpc::haider_protocol::ids::CredentialAlias::new),
        resolve_provider,
        resolve_model,
        effort: session_config.effort.clone(),
        fast: session_config.fast,
    };

    let (session_id, created_generation, created_seq, created_metadata) =
        before_acceptance_deadline(timeout_deadline, "session.create", async {
            loop {
                match connection.client.request(create_body.clone()).await {
                    Ok(ResponseBody::SessionCreate {
                        session_id,
                        created_seq,
                        worker_generation,
                        metadata,
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
                        if metadata.interaction_mode != SessionInteractionModeV1::Autonomous {
                            return Err(HeadlessRunError::Protocol {
                                stage: "session.create",
                                message: "daemon did not persist autonomous interaction mode"
                                    .into(),
                            });
                        }
                        if (!resolve_provider && metadata.provider != create_provider)
                            || (!resolve_model && metadata.model != create_model)
                            || metadata.account_alias != session_config.account
                            || metadata.effort != session_config.effort
                            || metadata.fast != session_config.fast.unwrap_or(false)
                        {
                            return Err(HeadlessRunError::Protocol {
                                stage: "session.create",
                                message: "daemon did not persist the requested headless identity and tuning"
                                    .into(),
                            });
                        }
                        break Ok((session_id, worker_generation, created_seq, metadata));
                    }
                    Ok(ResponseBody::Error {
                        code,
                        message,
                        retryable,
                        ..
                    }) => {
                        break Err(session_create_error(code, message, retryable));
                    }
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
    // The announcement fires at session RESOLUTION — before the attach —
    // because replay envelopes legitimately race (and win against) the
    // turn.submit response: consumers need the session identity as the
    // FIRST event, ahead of any envelope. The adapter dedupes the later
    // acceptance-time emission, whose head_seq refines this baseline.
    reducer.emit(HeadlessEvent::Accepted {
        session_id: session_id.clone(),
        head_seq: created_seq,
    });
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

    let provider = created_metadata.provider.clone();
    let model = created_metadata.model.clone();

    // This body is immutable across response-loss retries. In particular, its
    // original generation remains part of the durable command identity even if
    // reconnecting observes a newer worker generation.
    let submit_spec = pinned_headless.then(|| HeadlessRunSpecV1 {
        cwd: created_metadata.cwd.clone(),
        provider: provider.clone(),
        model: model.clone(),
        max_output_tokens: request.max_tokens,
        effort: session_config.effort.clone(),
        fast: session_config.fast.unwrap_or(false),
        seed: request.seed,
        permission_overrides: created_metadata.permission_overrides.unwrap_or_default(),
        trust_hooks: request.trust_hooks,
        budget: request.budget.clone(),
        request_deadline_unix_ms: timeout_deadline.map(deadline_unix_ms),
        replay_of: request.replay_of.clone(),
    });
    let submit_body = headless_submit_body_with_spec(
        request.trust_hooks,
        submit_command_id,
        session_id.clone(),
        connection.worker_generation,
        request.prompt,
        submit_attachments,
        submit_spec,
    );
    let mut buffered = BufferedWireFrames::new();
    let mut submit_timeout_grace = None;
    let run_id = loop {
        let pending_response = match connection.client.begin_request(submit_body.clone()).await {
            Ok(pending_response) => pending_response,
            Err(error) => {
                buffered.clear()?;
                reconnect_for_submit(SubmitReconnectInput {
                    profile,
                    ensure: &ensure,
                    connection: &mut connection,
                    reducer: &mut reducer,
                    reconnects: &mut reconnects,
                    buffered: &mut buffered,
                    stage: "turn.submit",
                    error,
                })
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
                        buffered.push(&frame)?;
                    }
                }
                () = wait_until(response_deadline) => {
                    if submit_timeout_grace.is_some() {
                        return Err(cancellation_unconfirmed("turn.submit recovery"));
                    }
                    submit_timeout_grace = Some(Instant::now() + request.terminal_grace);
                    let _ = connection.client.close();
                    break Err(ClientError::Disconnected(DisconnectReason::Closed));
                }
            }
        };
        match response {
            Ok(
                ResponseBody::TurnSubmit {
                    session_id: accepted_session,
                    run_id,
                    accepted_seq,
                    worker_generation,
                    ..
                }
                | ResponseBody::HeadlessRunStart {
                    session_id: accepted_session,
                    run_id,
                    accepted_seq,
                    worker_generation,
                    ..
                },
            ) if accepted_session == session_id => {
                connection.worker_generation = worker_generation;
                reducer.emit(HeadlessEvent::Accepted {
                    session_id: accepted_session,
                    head_seq: accepted_seq,
                });
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
                buffered.clear()?;
                let reconnect = reconnect_for_submit(SubmitReconnectInput {
                    profile,
                    ensure: &ensure,
                    connection: &mut connection,
                    reducer: &mut reducer,
                    reconnects: &mut reconnects,
                    buffered: &mut buffered,
                    stage: "turn.submit",
                    error,
                });
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
    let budget_deadline = request
        .budget
        .max_time_ms
        .map(|millis| Instant::now() + Duration::from_millis(millis));
    let caller_run_deadline = match (timeout_deadline, budget_deadline) {
        (Some(timeout), Some(budget)) => Some(timeout.min(budget)),
        (Some(timeout), None) => Some(timeout),
        (None, Some(budget)) => Some(budget),
        (None, None) => None,
    };
    if request.detached {
        let events = HeadlessRunEvents::empty(run_id.clone());
        let result = HeadlessRunResult {
            session_id,
            run_id,
            provider,
            model,
            attachments: attachment_refs,
            outcome: HeadlessOutcome::Started,
            response: None,
            usage: None,
            events,
            budget_exhausted: None,
            replay: None,
            permission_denials: Vec::new(),
            failure: None,
            terminal_seq: None,
            background_tasks_running: Vec::new(),
        };
        if ensure.daemon_lifetime == DaemonLifetime::EphemeralIfSpawned {
            teardown_owned_daemon_on_connection(profile, &mut connection, &daemon_ownership)
                .await?;
        }
        return Ok(result);
    }

    let mut forced = submit_timeout_grace.map(|_| ForcedOutcome::Timeout);
    let mut grace_deadline = submit_timeout_grace;
    let mut cancel_command = submit_timeout_grace.map(|_| CancelCommand {
        command_id: CommandId::new(command_id("headless-cancel")),
    });
    let mut immediate_interrupt = false;
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
    for frame in buffered.reader()? {
        let frame = frame?;
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
        if matches!(forced, Some(ForcedOutcome::Interrupted))
            && try_take_pending_interrupt(&mut interrupts)
        {
            immediate_interrupt = true;
            break;
        }
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
            interrupt = receive_interrupt(&mut interrupts) => {
                match interrupt {
                    Some(HeadlessInterrupt::CancelAndDrain)
                        if !matches!(forced, Some(ForcedOutcome::Interrupted)) =>
                    {
                        forced = Some(ForcedOutcome::Interrupted);
                        let deadline = caller_run_deadline
                            .unwrap_or_else(|| Instant::now() + request.terminal_grace);
                        grace_deadline = Some(deadline);
                        if cancel_command.is_none() {
                            cancel_command = Some(CancelCommand {
                                command_id: CommandId::new(command_id("headless-sigint-cancel")),
                            });
                        }
                        if let Some(cancel) = cancel_command.clone() {
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
                    Some(HeadlessInterrupt::CancelAndDrain | HeadlessInterrupt::ExitImmediately)
                        if matches!(forced, Some(ForcedOutcome::Interrupted)) =>
                    {
                        // `send_cancel_before` returned only after the
                        // idempotent durable receipt, so this immediate exit
                        // cannot outrun the first SIGINT's journal fact.
                        immediate_interrupt = true;
                        break;
                    }
                    Some(HeadlessInterrupt::ExitImmediately) => {
                        // Treat an out-of-order surface signal as the first
                        // interrupt so no immediate exit can precede durable
                        // cancellation.
                        forced = Some(ForcedOutcome::Interrupted);
                        let deadline = caller_run_deadline
                            .unwrap_or_else(|| Instant::now() + request.terminal_grace);
                        grace_deadline = Some(deadline);
                        if cancel_command.is_none() {
                            cancel_command = Some(CancelCommand {
                                command_id: CommandId::new(command_id("headless-sigint-cancel")),
                            });
                        }
                        if let Some(cancel) = cancel_command.clone() {
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
                    None => interrupts = None,
                    Some(HeadlessInterrupt::CancelAndDrain) => {}
                }
            }
        }
    }

    let result = finalize(reducer, run_id, provider, model, attachment_refs, forced);
    if immediate_interrupt {
        // The durable cancellation receipt is already in the daemon. Relinquish
        // one-shot lifecycle ownership so the second-interrupt fast exit does
        // not wait for daemon drain in the outer teardown fallback.
        lock_daemon_ownership(&daemon_ownership).take();
    }
    let teardown =
        if ensure.daemon_lifetime == DaemonLifetime::EphemeralIfSpawned && !immediate_interrupt {
            teardown_owned_daemon_on_connection(profile, &mut connection, &daemon_ownership).await
        } else {
            Ok(())
        };
    match (result, teardown) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
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
        // One immutable private snapshot is the retry authority. A reconnect
        // never reopens the pathname and every attempt borrows these exact
        // bytes into the segmented RPC frame.
        let data_base64 = Arc::new(Zeroizing::new(encode_base64(attachment.bytes())));
        loop {
            match connection
                .client
                .request_artifact_put(Arc::clone(&data_base64))
                .await
            {
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
                        HeadlessAttachment::Pdf(pdf) => AttachmentBlock::Pdf {
                            artifact: artifact.clone(),
                            name: pdf.name.clone(),
                            pages: pdf.pages,
                            delivery:
                                haider_rpc::haider_protocol::tool::PdfDeliveryMode::ExtractedText,
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

async fn resolve_create_identity(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
    reconnects: &mut ReconnectBudget,
    explicit_provider: Option<String>,
    legacy_model: Option<String>,
    selector: Option<&str>,
) -> Result<(String, String, bool, bool), HeadlessRunError> {
    let provider = explicit_provider;
    let mut model = legacy_model;
    let Some(selector) = selector else {
        let resolve_provider = provider.is_none();
        let resolve_model = model.is_none();
        return Ok((
            provider.unwrap_or_default(),
            model.unwrap_or_default(),
            resolve_provider,
            resolve_model,
        ));
    };
    if let Some((candidate, model)) = selector.split_once('/') {
        let registered = provider_summary(profile, ensure, connection, reconnects, candidate)
            .await?
            .is_some();
        if registered {
            if model.is_empty() {
                return Err(HeadlessRunError::Bootstrap {
                    stage: "model selector bootstrap",
                    code: haider_rpc::ERROR_CODE_INVALID_ARGUMENT,
                    message: "provider/model selector has an empty model".into(),
                    retryable: false,
                });
            }
            if let Some(explicit_provider) = provider.as_ref()
                && explicit_provider != candidate
            {
                return Err(HeadlessRunError::Bootstrap {
                    stage: "model selector bootstrap",
                    code: haider_rpc::ERROR_CODE_INVALID_ARGUMENT,
                    message: format!(
                        "model selector names provider `{candidate}`, which conflicts with explicit provider `{explicit_provider}`"
                    ),
                    retryable: false,
                });
            }
            let resolve_provider = false;
            let resolve_model = false;
            return Ok((
                candidate.to_owned(),
                model.to_owned(),
                resolve_provider,
                resolve_model,
            ));
        }
    }
    model = Some(selector.to_owned());
    let resolve_provider = provider.is_none();
    Ok((
        provider.unwrap_or_default(),
        model.unwrap_or_default(),
        resolve_provider,
        false,
    ))
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
    options
        .required_features
        .insert(FEATURE_AUTONOMOUS_INTERACTION_V1.to_owned());
    options
        .required_features
        .insert(haider_rpc::FEATURE_SESSION_CREATE_ADMISSION_V1.to_owned());
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

fn normalize_session_config_features(
    options: &mut EnsureOptions,
    config: &HeadlessSessionConfig,
) -> Result<(), HeadlessRunError> {
    if config.account.is_some() {
        options
            .required_features
            .insert(haider_rpc::FEATURE_SESSION_ACCOUNT_SELECT_V1.to_owned());
    }
    if config.model.is_some() || config.effort.is_some() || config.fast.is_some() {
        options
            .required_features
            .insert(haider_rpc::FEATURE_SESSION_CONFIG_V1.to_owned());
    }
    if config.model.is_some() {
        options
            .required_features
            .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    }
    if config.effort.is_some() {
        options
            .required_features
            .insert(haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1.to_owned());
    }
    if config.fast.is_some() {
        options
            .required_features
            .insert(haider_rpc::FEATURE_SESSION_FAST_SELECT_V1.to_owned());
    }
    if config.ssh_scope.is_some() {
        options
            .required_features
            .insert(haider_rpc::FEATURE_SSH_PROFILES_V1.to_owned());
    }
    Ok(())
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
    reopen_headless_connection(profile, ensure, connection).await?;
    Ok(())
}

async fn reopen_headless_connection(
    profile: &ResolvedProfile,
    ensure: &EnsureOptions,
    connection: &mut HeadlessConnection,
) -> Result<(), HeadlessRunError> {
    let daemon_ownership = Arc::clone(&connection.daemon_ownership);
    *connection = HeadlessConnection::open(profile, ensure.clone(), daemon_ownership).await?;
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

struct SubmitReconnectInput<'a> {
    profile: &'a ResolvedProfile,
    ensure: &'a EnsureOptions,
    connection: &'a mut HeadlessConnection,
    reducer: &'a mut HeadlessReducer,
    reconnects: &'a mut ReconnectBudget,
    buffered: &'a mut BufferedWireFrames,
    stage: &'static str,
    error: ClientError,
}

async fn reconnect_for_submit(input: SubmitReconnectInput<'_>) -> Result<(), HeadlessRunError> {
    let SubmitReconnectInput {
        profile,
        ensure,
        connection,
        reducer,
        reconnects,
        buffered,
        stage,
        error,
    } = input;
    let reason = client_error(stage, error)?;
    reconnects.spend(stage, reason)?;
    reopen_headless_connection(profile, ensure, connection).await?;
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
    reopen_headless_connection(profile, ensure, connection).await?;
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
                reopen_headless_connection(profile, ensure, connection).await?;
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
    buffered: &mut BufferedWireFrames,
) -> Result<(), HeadlessRunError> {
    loop {
        buffered.clear()?;
        match attach_buffered_once(connection, reducer, buffered).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(HeadlessRunError::Transport { stage, reason }) => {
                reconnects.spend(stage, reason)?;
                reopen_headless_connection(profile, ensure, connection).await?;
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
    buffered: &mut BufferedWireFrames,
) -> Result<bool, HeadlessRunError> {
    detach_existing(connection, "session.detach before submit retry").await?;
    let attach_loss_baseline = connection.client.lost_events();
    let response = connection
        .client
        .request(RequestBody::SessionAttach {
            session_id: reducer.session_id.clone(),
            after_seq: reducer.last_applied,
            mode: AttachMode::Control,
            sealed_replay: false,
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

    let health = connection.client.health_watch();
    let mut health_wait = Box::pin(health.wait(
        "headless buffered attach health",
        ATTACH_HEALTH_REPAIR_INTERVAL,
    ));
    loop {
        let frame = tokio::select! {
            biased;
            frame = connection.events.recv() => frame,
            outcome = &mut health_wait => {
                let (health, outcome) = outcome;
                if !attached_health_is_healthy(
                    &outcome,
                    attach_loss_baseline,
                    "session.attach before submit retry",
                )? {
                    return Ok(false);
                }
                health_wait = Box::pin(health.wait(
                    "headless buffered attach health",
                    ATTACH_HEALTH_REPAIR_INTERVAL,
                ));
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
                buffered.push(&frame)?;
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
        // Frames stay ahead of health so a disconnect cannot hide a buffered
        // terminal fact. A saturated frame lane can otherwise starve the
        // health future itself, so probe the retained loss generation after
        // every consumed frame and repair the gap immediately.
        if connection.client.lost_events() != attach_loss_baseline {
            return Ok(false);
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
            sealed_replay: false,
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

    let health = connection.client.health_watch();
    let mut health_wait =
        Box::pin(health.wait("headless attach health", ATTACH_HEALTH_REPAIR_INTERVAL));
    loop {
        let frame = tokio::select! {
            biased;
            frame = connection.events.recv() => frame,
            outcome = &mut health_wait => {
                let (health, outcome) = outcome;
                if !attached_health_is_healthy(
                    &outcome,
                    attach_loss_baseline,
                    "session.attach replay",
                )? {
                    return Ok(false);
                }
                health_wait = Box::pin(
                    health.wait("headless attach health", ATTACH_HEALTH_REPAIR_INTERVAL),
                );
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
        // Preserve queued-frame-first semantics while making overload
        // bounded: even if `events.recv()` remains continuously ready, a
        // dropped frame is observed after the next frame we do consume.
        if connection.client.lost_events() != attach_loss_baseline {
            return Ok(false);
        }
    }
}

fn attached_health_is_healthy(
    outcome: &ClientHealthWait,
    lost_events_baseline: u64,
    stage: &'static str,
) -> Result<bool, HeadlessRunError> {
    // `RepairDue` carries the typed WaitTimeout that makes a lost watch wake a
    // bounded condition. The same latest-value probes are authoritative for
    // both a notification and the 30 s repair path.
    let _repair_timeout = outcome.repair_timeout();
    let health = outcome.snapshot();
    if health.lost_events != lost_events_baseline {
        return Ok(false);
    }
    match &health.state {
        ConnectionState::Disconnected(reason) => Err(HeadlessRunError::Transport {
            stage,
            reason: reason.clone(),
        }),
        ConnectionState::Connected if outcome.channel_closed() => {
            Err(HeadlessRunError::Transport {
                stage,
                reason: DisconnectReason::Closed,
            })
        }
        ConnectionState::Connected => Ok(true),
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
    let _ = connection.client.close();
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
                        // Boxed for the same stack-size reason as
                        // `HeadlessConnection::open`: this is the deepest
                        // reconnect subtree and it hangs off the per-event
                        // reducer loop.
                        Box::pin(reconnect_attached_for_run_before_deadline(
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
                        ))
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

async fn receive_interrupt(
    interrupts: &mut Option<mpsc::UnboundedReceiver<HeadlessInterrupt>>,
) -> Option<HeadlessInterrupt> {
    match interrupts {
        Some(interrupts) => interrupts.recv().await,
        None => pending::<Option<HeadlessInterrupt>>().await,
    }
}

fn try_take_pending_interrupt(
    interrupts: &mut Option<mpsc::UnboundedReceiver<HeadlessInterrupt>>,
) -> bool {
    let Some(interrupts_rx) = interrupts.as_mut() else {
        return false;
    };
    match interrupts_rx.try_recv() {
        Ok(HeadlessInterrupt::CancelAndDrain | HeadlessInterrupt::ExitImmediately) => true,
        Err(mpsc::error::TryRecvError::Empty) => false,
        Err(mpsc::error::TryRecvError::Disconnected) => {
            *interrupts = None;
            false
        }
    }
}

fn deadline_unix_ms(deadline: Instant) -> u64 {
    let remaining_ms = u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    now_ms.saturating_add(remaining_ms)
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
) -> Result<HeadlessRunResult, HeadlessRunError> {
    let HeadlessReducer {
        session_id,
        response,
        usage,
        permission_denials,
        blocking_presentation,
        terminal,
        terminal_envelope,
        background_tasks,
        event_count,
        budget_exhausted,
        mut output,
        ..
    } = reducer;
    let (outcome, failure, terminal_seq) = match forced {
        Some(ForcedOutcome::Timeout) => (
            HeadlessOutcome::Timeout,
            Some(HeadlessRunFailure {
                code: HeadlessFailureCode::Timeout,
                message: "run exceeded its wall-clock timeout".into(),
                retryable: false,
                presentation: None,
            }),
            terminal.as_ref().map(|terminal| terminal.seq),
        ),
        Some(ForcedOutcome::Blocked(reason)) => {
            let message = blocking_presentation.as_ref().map_or_else(
                || reason.message().into(),
                |safe| format!("{} — {}", safe.title, safe.detail),
            );
            (
                HeadlessOutcome::InputRequired,
                Some(HeadlessRunFailure {
                    code: HeadlessFailureCode::Blocked(reason),
                    message,
                    retryable: false,
                    presentation: blocking_presentation,
                }),
                terminal.as_ref().map(|terminal| terminal.seq),
            )
        }
        Some(ForcedOutcome::Interrupted) => (
            HeadlessOutcome::Cancelled,
            terminal
                .as_ref()
                .and_then(|terminal| terminal.failure.clone()),
            terminal.as_ref().map(|terminal| terminal.seq),
        ),
        None => terminal.map_or_else(
            || {
                (
                    HeadlessOutcome::Errored,
                    Some(HeadlessRunFailure {
                        code: HeadlessFailureCode::Internal,
                        message: "event stream ended without a correlated terminal".into(),
                        retryable: false,
                        presentation: None,
                    }),
                    None,
                )
            },
            |terminal| (terminal.outcome, terminal.failure, Some(terminal.seq)),
        ),
    };
    if let Some(envelope) = terminal_envelope {
        let kind = terminal_kind(outcome, failure.as_ref());
        let error_code = failure
            .as_ref()
            .map(|failure| failure.code.as_str().to_owned());
        output.emit(HeadlessEvent::Terminal(HeadlessTerminalEvent {
            envelope: Box::new(envelope),
            kind,
            error_code,
        }));
    }
    let events = output.finish(run_id.clone(), event_count)?;
    let background_tasks_running = background_tasks
        .iter()
        .filter(|(_, (_, running))| *running)
        .map(|(task_id, (name, _))| HeadlessBackgroundTask {
            task_id: task_id.clone(),
            name: name.clone(),
        })
        .collect();
    Ok(HeadlessRunResult {
        session_id,
        run_id,
        provider,
        model,
        attachments,
        outcome,
        response,
        usage,
        events,
        budget_exhausted,
        replay: None,
        permission_denials,
        failure,
        terminal_seq,
        background_tasks_running,
    })
}

fn terminal_kind(
    outcome: HeadlessOutcome,
    failure: Option<&HeadlessRunFailure>,
) -> HeadlessTerminalKind {
    match outcome {
        HeadlessOutcome::Done => HeadlessTerminalKind::Success,
        HeadlessOutcome::Cancelled => HeadlessTerminalKind::Cancellation,
        HeadlessOutcome::Timeout => HeadlessTerminalKind::Timeout,
        HeadlessOutcome::Errored | HeadlessOutcome::InputRequired | HeadlessOutcome::Started => {
            match failure.map(|failure| &failure.code) {
                Some(HeadlessFailureCode::Run(ErrorCode::BudgetExhausted)) => {
                    HeadlessTerminalKind::Budget
                }
                Some(HeadlessFailureCode::Run(
                    ErrorCode::ProviderError | ErrorCode::ProviderTimeout,
                )) => HeadlessTerminalKind::ProviderError,
                _ => HeadlessTerminalKind::Failure,
            }
        }
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
        ClientError::MissingFeature(feature) => Err(HeadlessRunError::Protocol {
            stage,
            message: format!("missing_feature: {feature}"),
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
        ClientError::MissingFeature(feature) => HeadlessRunError::Protocol {
            stage,
            message: format!("missing_feature: {feature}"),
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

fn session_create_error(code: String, message: String, retryable: bool) -> HeadlessRunError {
    let bootstrap_code = match code.as_str() {
        ERROR_CODE_NO_ACTIVE_ACCOUNT => Some(ERROR_CODE_NO_ACTIVE_ACCOUNT),
        ERROR_CODE_NO_DEFAULT_MODEL => Some(ERROR_CODE_NO_DEFAULT_MODEL),
        _ => None,
    };
    if let Some(code) = bootstrap_code {
        HeadlessRunError::Bootstrap {
            stage: "session.create",
            code,
            message,
            retryable,
        }
    } else {
        rpc_error("session.create", code, message, retryable)
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

/// The required feature set after normalizing a headless request. Exposed for
/// front-end diagnostics and feature-refusal tests.
#[must_use]
pub fn required_headless_features(
    permission_overrides: SessionPermissionOverridesV1,
) -> BTreeSet<String> {
    let mut features = required_live_features();
    features.insert(FEATURE_AUTONOMOUS_INTERACTION_V1.to_owned());
    features.insert(haider_rpc::FEATURE_SESSION_CREATE_ADMISSION_V1.to_owned());
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
