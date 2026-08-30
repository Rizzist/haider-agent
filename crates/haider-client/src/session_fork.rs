//! Feature-negotiated typed client helpers for prompt-oriented session forks.
//!
//! Two halves, deliberately narrow:
//!
//! - [`forkable_prompts`] reads the committed journal and reports the user
//!   prompts that can serve as a fork cut, each with the DURABLE sequence
//!   the daemon resolves. A prompt without a sequence is not reported: the
//!   cut coordinate is journal truth and is never invented client-side.
//! - [`fork_at_prompt`] performs the exclusive prompt-oriented
//!   `session.fork` and returns the daemon's receipt plus the editable,
//!   unsent [`SessionForkDraft`].
//!
//! The fork NEVER mutates the source. The daemon mints a new session id and
//! copies history up to the boundary before the selected prompt; the source
//! session, its transcript, and its attachment stream are untouched. The
//! returned draft is the source prompt with its complete typed attachment
//! blocks, so a resubmission keeps image dimensions, filenames, and PDF
//! delivery mode instead of re-deriving a lossy approximation.

use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{BranchId, SessionId};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::session_fork::{
    SessionForkDraft, SessionForkInvalidCutReason, SessionForkPromptSelector, SessionForkProvenance,
};
use haider_rpc::{
    CommandId, ErrorData, FEATURE_SESSION_FORK_V1, FEATURE_SESSION_PROMPT_FORK_V1, RequestBody,
    ResponseBody, SeqRange, Welcome,
};

use crate::client::{ClientError, RpcClient};

/// The daemon's hard cap on one `session.read` range. Paging honors it
/// rather than discovering it as a refusal.
pub const FORKABLE_PROMPT_PAGE: u64 = 1_024;

/// One committed user prompt usable as an exclusive fork cut.
///
/// `seq` is the durable journal coordinate the daemon resolves to the
/// history boundary BEFORE the prompt. Nothing here is derived or guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkablePrompt {
    pub seq: u64,
    pub text: String,
    /// The branch the prompt was committed on; `None` is legacy/main.
    pub branch_id: Option<BranchId>,
}

/// Stable receipt of a completed prompt-oriented fork.
///
/// `forked_from` and `draft` are present exactly because the request named a
/// prompt selector; a response missing either is an unexpected body, never a
/// fork silently downgraded to the legacy exact-node shape.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptFork {
    /// The daemon-minted CHILD session. The source is `source_session_id`.
    pub session_id: SessionId,
    pub source_session_id: SessionId,
    pub source_branch_id: Option<BranchId>,
    pub worker_generation: u64,
    pub created_seq: u64,
    pub metadata: SessionMetadataV1,
    pub forked_from: SessionForkProvenance,
    /// Editable and UNSENT. Submitting it is the caller's next act.
    pub draft: SessionForkDraft,
}

#[derive(Debug)]
pub enum SessionForkClientError {
    Client(ClientError),
    MissingFeature(&'static str),
    /// The named event is not a forkable user prompt on the requested source
    /// branch. Typed so a caller can point at the exact row it offered.
    InvalidCut {
        session_id: SessionId,
        seq: u64,
        reason: SessionForkInvalidCutReason,
    },
    Daemon {
        code: String,
        message: String,
        retryable: bool,
        data: Option<ErrorData>,
    },
    UnexpectedResponse(&'static str),
}

impl std::fmt::Display for SessionForkClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "session-fork RPC failed: {error}"),
            Self::MissingFeature(feature) => write!(
                formatter,
                "daemon does not advertise required feature `{feature}`"
            ),
            Self::InvalidCut { seq, reason, .. } => write!(
                formatter,
                "sequence {seq} is not a forkable user prompt ({reason:?})"
            ),
            Self::Daemon {
                code,
                message,
                retryable,
                ..
            } => write!(
                formatter,
                "session-fork RPC was rejected ({code}, retryable={retryable}): {message}"
            ),
            Self::UnexpectedResponse(expected) => write!(
                formatter,
                "session-fork RPC returned an unexpected response; expected {expected}"
            ),
        }
    }
}

impl std::error::Error for SessionForkClientError {}

impl From<ClientError> for SessionForkClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

/// Whether the connected daemon serves the exclusive prompt-cut fork.
///
/// BOTH tokens are required: `session_fork_v1` carries the method and
/// `session_prompt_fork_v1` the additive prompt-selector request shape. A
/// daemon advertising only the former honors the legacy exact-node fork and
/// would see the prompt request as a fork with no coordinates at all.
#[must_use]
pub fn prompt_fork_available(welcome: &Welcome) -> bool {
    welcome.features.contains(FEATURE_SESSION_FORK_V1)
        && welcome.features.contains(FEATURE_SESSION_PROMPT_FORK_V1)
}

/// Every user prompt in `envelopes` that carries a durable sequence, in
/// journal order.
///
/// Subagent-authored messages are excluded: a fork cut names one of the
/// SESSION's own prompts. Pure, so callers can project a page they already
/// hold without another read.
#[must_use]
pub fn forkable_prompts_in(envelopes: &[RawEnvelope]) -> Vec<ForkablePrompt> {
    envelopes
        .iter()
        .filter(|envelope| envelope.agent_id.is_none())
        .filter_map(|envelope| {
            match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                Ok(EventPayload::UserMessage { text, .. }) => Some(ForkablePrompt {
                    seq: envelope.seq,
                    text,
                    branch_id: envelope.branch_id.clone(),
                }),
                _ => None,
            }
        })
        .collect()
}

/// The session's forkable prompts, NEWEST FIRST, bounded by `limit`.
///
/// The journal is paged backwards from the head in daemon-sized pages, so a
/// long session costs one page per `FORKABLE_PROMPT_PAGE` envelopes actually
/// walked and stops as soon as `limit` prompts are in hand. `limit == 0`
/// reads nothing beyond the head probe.
pub async fn forkable_prompts(
    client: &RpcClient,
    session_id: SessionId,
    limit: usize,
) -> Result<Vec<ForkablePrompt>, SessionForkClientError> {
    // The one-envelope probe is also the head-sequence read: every
    // `session.read` result carries `head_seq` regardless of its range.
    let (head_seq, _) = read_page(
        client,
        &session_id,
        SeqRange {
            start_seq: 1,
            end_seq: 1,
        },
    )
    .await?;
    let mut found: Vec<ForkablePrompt> = Vec::new();
    let mut end_seq = head_seq;
    while found.len() < limit && end_seq >= 1 {
        let start_seq = end_seq.saturating_sub(FORKABLE_PROMPT_PAGE - 1).max(1);
        let (_, envelopes) =
            read_page(client, &session_id, SeqRange { start_seq, end_seq }).await?;
        // The page is journal order; the roster is newest first.
        let mut page = forkable_prompts_in(&envelopes);
        page.reverse();
        found.extend(page);
        found.truncate(limit);
        if start_seq == 1 {
            break;
        }
        end_seq = start_seq - 1;
    }
    Ok(found)
}

/// Fork `session_id` at the user prompt committed at `seq`.
///
/// The source session is untouched: the daemon mints the child, copies the
/// history boundary before the prompt, and returns the prompt itself as an
/// editable, unsent draft. The command id makes this receipt-backed — a lost
/// response retried under the same id returns the original child rather than
/// minting a second one.
pub async fn fork_at_prompt(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    source_branch_id: Option<BranchId>,
    seq: u64,
    name: Option<String>,
) -> Result<PromptFork, SessionForkClientError> {
    if !prompt_fork_available(client.welcome()) {
        return Err(SessionForkClientError::MissingFeature(
            if client.welcome().features.contains(FEATURE_SESSION_FORK_V1) {
                FEATURE_SESSION_PROMPT_FORK_V1
            } else {
                FEATURE_SESSION_FORK_V1
            },
        ));
    }
    let response = client
        .request(RequestBody::SessionFork {
            command_id,
            session_id,
            worker_generation,
            source_branch_id,
            // Both legacy exact-node fields stay absent: their presence is
            // what distinguishes a legacy fork from this exclusive cut.
            fork_node_id: None,
            fork_seq: None,
            prompt: Some(SessionForkPromptSelector { seq }),
            name,
        })
        .await?;
    prompt_fork_response(response)
}

/// Project one `session.fork` response into the typed prompt-fork receipt.
///
/// Exposed so a client that decodes [`ResponseBody`] itself — the TUI's link
/// layer does — shares this crate's absence discipline instead of restating
/// it.
pub fn prompt_fork_response(body: ResponseBody) -> Result<PromptFork, SessionForkClientError> {
    match refuse(body)? {
        ResponseBody::SessionFork {
            session_id,
            source_session_id,
            source_branch_id,
            created_seq,
            worker_generation,
            metadata,
            forked_from: Some(forked_from),
            draft: Some(draft),
            ..
        } => Ok(PromptFork {
            session_id,
            source_session_id,
            source_branch_id,
            worker_generation,
            created_seq,
            metadata,
            forked_from,
            draft,
        }),
        _ => Err(SessionForkClientError::UnexpectedResponse(
            "session.fork carrying prompt provenance and an editable draft",
        )),
    }
}

async fn read_page(
    client: &RpcClient,
    session_id: &SessionId,
    range: SeqRange,
) -> Result<(u64, Vec<RawEnvelope>), SessionForkClientError> {
    match refuse(
        client
            .request(RequestBody::SessionRead {
                session_id: session_id.clone(),
                range,
            })
            .await?,
    )? {
        ResponseBody::SessionRead { result } if &result.session_id == session_id => {
            Ok((result.head_seq, result.envelopes))
        }
        _ => Err(SessionForkClientError::UnexpectedResponse(
            "session.read for the requested session",
        )),
    }
}

fn refuse(body: ResponseBody) -> Result<ResponseBody, SessionForkClientError> {
    match body {
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(match data {
            Some(ErrorData::SessionForkInvalidCut {
                session_id,
                seq,
                reason,
            }) => SessionForkClientError::InvalidCut {
                session_id,
                seq,
                reason,
            },
            data => SessionForkClientError::Daemon {
                code,
                message,
                retryable,
                data,
            },
        }),
        body => Ok(body),
    }
}
