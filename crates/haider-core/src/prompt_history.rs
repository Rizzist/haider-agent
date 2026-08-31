//! Deterministic reconstruction of provider messages from the durable history
//! tree and its byte-preserving journal sidecars.

use crate::actor::model_tool_result_preview;
use crate::{SessionProjectionCheckpoint, StoreHandle};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_protocol::EventPayload;
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::context::{CONTEXT_SAVINGS_EXTENSION_KIND, ContextSavingsEvent};
use haider_protocol::envelope::{PromptRender, RawEnvelope, envelope_weight_bytes};
use haider_protocol::error::{ErrorAction, ErrorCode, HaiderError};
use haider_protocol::history::{CompactionIntent, CompactionResume, NodeKind, TreeNode};
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, EventId, ItemId, MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, TurnItem, UserCommandOriginV1};
use haider_protocol::menu::{ErrorRecoveryCardKind, MenuKind};
use haider_protocol::pipe::TranscriptProjector;
use haider_protocol::provider::{Block, PROVIDER_OPAQUE_EXTENSION_KIND};
use haider_protocol::state::RunState;
use haider_protocol::task::{TASK_TAIL_BYTES, TaskEventPayload, TaskTerminalState};
use haider_protocol::tool::{AttachmentBlock, BoundedResult};
use haider_provider::{Message, MessageRole, UserCommandRecord};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

#[cfg(test)]
#[path = "prompt_history_cache_tests.rs"]
mod prompt_history_cache_tests;

const HISTORY_PAGE: usize = 256;
const PROMPT_CACHE_SESSION_LIMIT: usize = 8;
/// Estimated retained heap across all prompt-cache sessions. In particular,
/// `RawEnvelope` JSON value trees commonly retain about 3–4× their serialized
/// body for short deltas; `envelope_weight_bytes` walks the actual owned IDs,
/// arrays, objects, and strings instead of applying a wire-size multiplier.
/// Durable journals remain authoritative; over-cap LRU entries keep only
/// replay/checkpoint cursors and rebuild their bodies on the next touch.
const PROMPT_CACHE_RETAINED_BYTES_LIMIT: usize = 32 * 1024 * 1024;
const PROMPT_CHECKPOINT_PROJECTION: &str = "prompt_history";
const PROMPT_CHECKPOINT_SHAPE_VERSION: u32 = 1;
/// Semantic compatibility of the prompt-history reducer.
///
/// Bump this only when projection/folding logic changes such that a prefix
/// produced by the previous reducer can differ from a full replay under the
/// new reducer. Ordinary package releases do not invalidate checkpoints;
/// payload layout changes are governed separately by
/// `PROMPT_CHECKPOINT_SHAPE_VERSION`.
const PROMPT_CHECKPOINT_REDUCER_VERSION: &str = "prompt-history-v1";
pub const USER_COMMAND_OUTPUT_PREVIEW_BYTES: usize = 8 * 1024;
/// Recent provider-message budget retained verbatim across model summaries.
/// The unit is the same honest provider-neutral bytes/4 estimate used by the
/// conversation savings ledger, not an exact model tokenizer.
pub const COMPACTION_RECENT_ESTIMATED_TOKENS: u64 = 24_000;
const COMPACTION_MIN_RECENT_PRIOR_TURNS: usize = 2;

/// Read-only CAS port used only when a durable compaction node is projected.
///
/// Keeping this separate from [`StoreHandle`] prevents ordinary event-store
/// implementations from acquiring an unrelated artifact-storage obligation.
#[async_trait]
pub trait ArtifactReader: Send + Sync {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError>;
}

/// Branch/agent-scoped committed-history compiler.
pub struct PromptHistoryCompiler;

#[derive(Debug)]
pub struct PromptCompactionPlanRequest<'a> {
    pub session_id: &'a SessionId,
    pub branch_id: Option<&'a BranchId>,
    pub agent_id: Option<&'a AgentId>,
    pub current_run: &'a RunId,
    pub operation_id: String,
    pub resume_cause: CompactionResume,
}

/// Daemon-lifetime prompt cache.
///
/// Durable journal bytes remain the only authority: the cache first samples
/// the session head, reads only the missing suffix, and keys a compiled
/// projection by that head plus its compaction epoch and complete branch,
/// agent, and current-run scope. Resident journals are capped across sessions,
/// and each retained session owns one incrementally updated tree/fact index.
/// On restart the cache may seed one exact timeline from a validated terminal
/// compaction-boundary checkpoint; any absent or unreadable checkpoint falls
/// back to the same journal fold from zero.
#[derive(Default)]
pub struct PromptHistoryCache {
    sessions: Mutex<HashMap<SessionId, CachedPromptSession>>,
    touch_clock: AtomicU64,
}

#[derive(Default)]
struct CachedPromptSession {
    last_touched: u64,
    bodies_evicted: bool,
    retained_envelope_bytes: usize,
    head_seq: u64,
    compaction_epochs: HashMap<PromptTimelineKey, u64>,
    envelopes: Vec<RawEnvelope>,
    projections: HashMap<PromptProjectionKey, CachedExactProjection>,
    append_prefixes: HashMap<PromptProjectionScope, CachedCompiledPrefix>,
    render_facts: JournalFactsIndex,
    tree_index: TreeProjection,
    lineage_scopes: HashMap<Option<BranchId>, LineageIndex>,
    checkpoint_base: Option<PromptCheckpointBase>,
    checkpoint_node_collision: bool,
    boundary_projector: TranscriptProjector,
    boundaries: HashMap<PromptTimelineKey, PromptBoundary>,
    saved_boundaries: HashMap<PromptTimelineKey, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PromptProjectionKey {
    head_seq: u64,
    compaction_epoch: u64,
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
    current_run: RunId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PromptProjectionScope {
    compaction_epoch: u64,
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PromptTimelineKey {
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
}

struct PromptCheckpointBase {
    timeline: PromptTimelineKey,
    prefix_node_ids: Vec<NodeId>,
    prefix_run_ids: Vec<RunId>,
}

#[derive(Clone)]
struct PromptBoundary {
    through_seq: u64,
    event_id: EventId,
    run_id: RunId,
}

#[derive(Clone)]
struct CachedCompiledPrefix {
    head_seq: u64,
    current_run: RunId,
    current_run_terminal: bool,
    projection: Option<Arc<CompiledPromptProjection>>,
    body_bytes: usize,
    cursor: PromptProjectionCursor,
}

#[derive(Clone)]
struct CachedExactProjection {
    projection: Option<Arc<CompiledPromptProjection>>,
    body_bytes: usize,
    cursor: PromptProjectionCursor,
}

#[derive(Clone)]
enum PromptProjectionCursor {
    Journal,
    Tree { head_node: NodeId },
}

#[derive(Serialize, Deserialize)]
struct DurablePromptCheckpoint {
    shape_version: u32,
    reducer_version: String,
    through_seq: u64,
    boundary_event_id: EventId,
    boundary_run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<AgentId>,
    compaction_epoch: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prefix_node_ids: Vec<NodeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prefix_run_ids: Vec<RunId>,
    messages: Vec<serde_json::Value>,
    stable_history_end: usize,
    current_user_start: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_compaction_summary_end: Option<usize>,
}

/// Provider-facing prompt projection plus ephemeral cache boundaries.
///
/// The message vector is byte-for-byte the same projection returned by the
/// legacy compiler entry points. Boundary metadata is deliberately separate
/// from the journal so request adapters can annotate cacheable prefixes
/// without changing replay or compaction semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledPromptProjection {
    pub messages: Vec<Message>,
    /// Exclusive end of immutable completed history.
    pub stable_history_end: usize,
    /// Index of the accepted current-user message, or `messages.len()` for an
    /// idle projection.
    pub current_user_start: usize,
    /// Exclusive end of the latest active compaction-summary message.
    pub latest_compaction_summary_end: Option<usize>,
}

impl CompiledPromptProjection {
    fn from_rendered(rendered: RenderedJournal) -> Self {
        let current_user_start = rendered
            .current_user_start
            .unwrap_or(rendered.messages.len());
        Self {
            stable_history_end: current_user_start,
            current_user_start,
            latest_compaction_summary_end: None,
            messages: rendered.messages,
        }
    }
}

impl PromptHistoryCompiler {
    /// Reduces the newest durable conversation-savings event. This journal
    /// fallback heals the narrow crash window between the authoritative event
    /// append and its redundant `sessions.meta_json` projection update.
    pub async fn latest_context_economy(
        store: &dyn StoreHandle,
        session_id: &SessionId,
    ) -> Result<Option<haider_protocol::context::ContextEconomy>, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let mut latest: Option<haider_protocol::context::ContextSavingsEvent> = None;
        for envelope in envelopes {
            let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            let Some(event) = ContextSavingsEvent::try_from_extension_item(&item)
                .map_err(|error| corrupt(format!("context-savings event is malformed: {error}")))?
            else {
                continue;
            };
            if let Some(existing) = &latest {
                if event.session_operation_count < existing.session_operation_count {
                    return Err(corrupt("context-savings operation count moved backwards"));
                }
                if event.session_operation_count == existing.session_operation_count
                    && event != *existing
                {
                    return Err(corrupt("equal context-savings coordinates disagree"));
                }
            }
            latest = Some(event);
        }
        Ok(
            latest.map(|event| haider_protocol::context::ContextEconomy {
                cumulative_estimated_tokens_saved: event.session_cumulative_estimated_tokens_saved,
                operation_count: event.session_operation_count,
                last_event: Some(event),
            }),
        )
    }

    /// Compiles the active durable tree. This entry point is sufficient for an
    /// uncompacted tree; encountering a committed compaction without a CAS
    /// reader is store corruption, never permission to resurrect its prefix.
    pub async fn compile(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<Vec<Message>, HaiderError> {
        Self::compile_provider_projection(store, session_id, branch_id, agent_id, current_run)
            .await
            .map(|projection| projection.messages)
    }

    /// Compiles the active durable tree with provider-neutral cache boundary
    /// metadata. This is additive; [`Self::compile`] retains its exact return
    /// type and message bytes for existing consumers.
    pub async fn compile_provider_projection(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<CompiledPromptProjection, HaiderError> {
        Self::compile_projection(store, None, session_id, branch_id, agent_id, current_run).await
    }

    /// Compiles the active durable tree and resolves committed compaction
    /// summaries through `artifacts`.
    pub async fn compile_with_artifacts(
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<Vec<Message>, HaiderError> {
        Self::compile_provider_projection_with_artifacts(
            store,
            artifacts,
            session_id,
            branch_id,
            agent_id,
            current_run,
        )
        .await
        .map(|projection| projection.messages)
    }

    /// Artifact-resolving compiler entry point with ephemeral cache boundary
    /// metadata for the live provider projection.
    pub async fn compile_provider_projection_with_artifacts(
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<CompiledPromptProjection, HaiderError> {
        Self::compile_projection(
            store,
            Some(artifacts),
            session_id,
            branch_id,
            agent_id,
            current_run,
        )
        .await
    }

    /// Legacy journal renderer retained as an explicit equivalence oracle.
    /// Production calls [`Self::compile`] or [`Self::compile_with_artifacts`].
    #[doc(hidden)]
    pub async fn compile_journal(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<Vec<Message>, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        render_journal(
            &envelopes,
            &envelopes,
            branch_id,
            agent_id,
            Some(current_run),
            true,
        )
        .map(|rendered| rendered.messages)
    }

    async fn compile_projection(
        store: &dyn StoreHandle,
        artifacts: Option<&dyn ArtifactReader>,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<CompiledPromptProjection, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        compile_projection_from_envelopes(
            store,
            artifacts,
            session_id,
            branch_id,
            agent_id,
            current_run,
            &envelopes,
        )
        .await
    }

    /// Creates an empty daemon-lifetime incremental cache.
    #[must_use]
    pub fn cache() -> PromptHistoryCache {
        PromptHistoryCache::default()
    }

    /// Compiles through a daemon-lifetime incremental journal/projection
    /// cache. The returned value is always equivalent to a fresh compile at
    /// the sampled durable head.
    pub async fn compile_cached_provider_projection_with_artifacts(
        cache: &PromptHistoryCache,
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<CompiledPromptProjection, HaiderError> {
        cache
            .compile_provider_projection_with_artifacts(
                store,
                artifacts,
                session_id,
                branch_id,
                agent_id,
                current_run,
            )
            .await
    }

    /// Compiles the terminal active head without inventing a current user
    /// turn. Used by the idle-only manual compaction operation.
    pub async fn compile_idle_with_artifacts(
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
    ) -> Result<Vec<Message>, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes);
        let ancestry = tree
            .latest_ancestry(&envelopes, &lineage, agent_id, lineage.head.as_ref())?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "there is no durable history to compact",
                    false,
                )
            })?;
        let tree_head_seq = ancestry.last().map_or(0, |entry| entry.seq);
        let mut projection =
            compile_ancestry(&envelopes, &ancestry, None, Some(artifacts), agent_id, None).await?;
        let tail = envelopes
            .iter()
            .filter(|envelope| {
                envelope.seq > tree_head_seq && scoped(envelope, branch_id, agent_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let rendered = render_journal(&tail, &envelopes, branch_id, agent_id, None, false)?;
        projection.messages.extend(rendered.messages);
        Ok(projection.messages)
    }

    /// Round 5 — the idle compile plus its latest prior-compaction boundary,
    /// so a MANUAL compaction's replay request marks the same breakpoint the
    /// live lane would (metadata parity, not a fresh cache epoch claim).
    pub async fn compile_idle_with_artifacts_and_boundary(
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
    ) -> Result<(Vec<Message>, Option<usize>), HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes);
        let ancestry = tree
            .latest_ancestry(&envelopes, &lineage, agent_id, lineage.head.as_ref())?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "there is no durable history to compact",
                    false,
                )
            })?;
        let tree_head_seq = ancestry.last().map_or(0, |entry| entry.seq);
        let mut projection =
            compile_ancestry(&envelopes, &ancestry, None, Some(artifacts), agent_id, None).await?;
        let boundary = projection.latest_compaction_summary_end;
        let tail = envelopes
            .iter()
            .filter(|envelope| {
                envelope.seq > tree_head_seq && scoped(envelope, branch_id, agent_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let rendered = render_journal(&tail, &envelopes, branch_id, agent_id, None, false)?;
        projection.messages.extend(rendered.messages);
        Ok((projection.messages, boundary))
    }

    /// Returns the latest committed tree head in one branch/agent scope.
    #[doc(hidden)]
    pub async fn latest_head(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
    ) -> Result<Option<NodeId>, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        if legacy_journal_only(&envelopes, branch_id, agent_id) {
            return Ok(None);
        }
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes);
        Ok(tree
            .latest_ancestry(&envelopes, &lineage, agent_id, lineage.head.as_ref())?
            .and_then(|ancestry| ancestry.last().map(|entry| entry.node.node.clone())))
    }

    /// Plans the largest safe prefix preceding `current_run`. The caller must
    /// durably append the returned intent before private summarization.
    pub async fn plan_compaction(
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        request: PromptCompactionPlanRequest<'_>,
    ) -> Result<crate::PlannedContextCompaction, HaiderError> {
        let PromptCompactionPlanRequest {
            session_id,
            branch_id,
            agent_id,
            current_run,
            operation_id,
            resume_cause,
        } = request;
        let envelopes = read_all(store, session_id).await?;
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes);
        let ancestry = tree
            .ancestry_for_run(&envelopes, &lineage, agent_id, current_run)?
            .ok_or_else(|| {
                corrupt(format!(
                    "cannot compact run {current_run} without a durable tree head"
                ))
            })?;
        let current_start = ancestry
            .iter()
            .position(|entry| entry.run_id.as_ref() == Some(current_run))
            .ok_or_else(|| corrupt(format!("tree ancestry omits current run {current_run}")))?;
        let prior_user_turns = ancestry[..current_start]
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                matches!(entry.node.kind, NodeKind::UserTurn { .. }).then_some(index)
            })
            .collect::<Vec<_>>();
        if prior_user_turns.len() < COMPACTION_MIN_RECENT_PRIOR_TURNS.saturating_add(1) {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "there is not enough older clean-turn history to compact",
                false,
            ));
        }
        let full_projection = compile_ancestry(
            &envelopes,
            &ancestry,
            None,
            Some(artifacts),
            agent_id,
            Some(current_run),
        )
        .await?;
        let mut retained_turns = 0_usize;
        let mut retain_from = current_start;
        for candidate in prior_user_turns.iter().rev().copied() {
            retained_turns = retained_turns.saturating_add(1);
            retain_from = candidate;
            let covered_projection = compile_ancestry(
                &envelopes,
                &ancestry[..candidate],
                None,
                Some(artifacts),
                agent_id,
                None,
            )
            .await?;
            let retained = full_projection
                .messages
                .get(covered_projection.messages.len()..full_projection.current_user_start)
                .ok_or_else(|| corrupt("clean compaction boundary exceeds active projection"))?;
            let retained_tokens = serde_json::to_vec(retained)
                .map(|bytes| {
                    u64::try_from(bytes.len())
                        .unwrap_or(u64::MAX)
                        .saturating_add(3)
                        / 4
                })
                .unwrap_or(u64::MAX);
            if retained_turns >= COMPACTION_MIN_RECENT_PRIOR_TURNS
                && retained_tokens >= COMPACTION_RECENT_ESTIMATED_TOKENS
            {
                break;
            }
        }

        let protected_runs = envelopes
            .iter()
            .filter_map(|envelope| {
                let EventPayload::ToolResult { result, .. } =
                    serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?
                else {
                    return None;
                };
                (!result.images.is_empty())
                    .then(|| envelope.run_id.clone())
                    .flatten()
            })
            .collect::<HashSet<_>>();
        let protected_from = ancestry[..current_start]
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let NodeKind::UserTurn { attachments, .. } = &entry.node.kind else {
                    return None;
                };
                let protected_attachment = attachments.iter().any(|attachment| {
                    matches!(
                        attachment,
                        AttachmentBlock::Image { .. } | AttachmentBlock::Skill { .. }
                    )
                });
                (protected_attachment
                    || entry
                        .run_id
                        .as_ref()
                        .is_some_and(|run_id| protected_runs.contains(run_id)))
                .then_some(index)
            })
            .min();
        if let Some(protected_from) = protected_from {
            retain_from = retain_from.min(protected_from);
        }

        let latest_prior_compaction = ancestry[..current_start]
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                matches!(entry.node.kind, NodeKind::Compaction { .. }).then_some(index)
            })
            .next_back();
        if latest_prior_compaction.is_some_and(|index| retain_from <= index) {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "protected or recent history reaches an active prior compaction boundary",
                false,
            ));
        }
        let covers_to = retain_from.checked_sub(1).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "there is no prior history prefix to compact",
                false,
            )
        })?;
        let covered_projection = compile_ancestry(
            &envelopes,
            &ancestry[..retain_from],
            None,
            Some(artifacts),
            agent_id,
            None,
        )
        .await?;
        if covered_projection.messages.is_empty()
            || covered_projection.messages.len() > full_projection.current_user_start
        {
            return Err(corrupt(
                "compaction planner produced an invalid message boundary",
            ));
        }
        Ok(crate::PlannedContextCompaction {
            intent: CompactionIntent {
                operation_id,
                covers_from: ancestry[0].node.node.clone(),
                covers_to: ancestry[covers_to].node.node.clone(),
                resume_cause,
            },
            covered_message_count: covered_projection.messages.len(),
        })
    }

    /// Rebuilds the exact original messages named by a compaction intent.
    /// Active summary artifacts are deliberately ignored, so a replacement
    /// summary never receives an older summary as input.
    pub async fn compile_compaction_source(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
        intent: &CompactionIntent,
    ) -> Result<Vec<Message>, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes);
        let ancestry = match tree.ancestry_for_run(&envelopes, &lineage, agent_id, current_run)? {
            Some(ancestry) => ancestry,
            None => tree
                .latest_ancestry(&envelopes, &lineage, agent_id, lineage.head.as_ref())?
                .ok_or_else(|| corrupt("compaction source has no active ancestry"))?,
        };
        let from = ancestry
            .iter()
            .position(|entry| entry.node.node == intent.covers_from)
            .ok_or_else(|| corrupt("compaction source is missing covers_from"))?;
        let to = ancestry
            .iter()
            .position(|entry| entry.node.node == intent.covers_to)
            .ok_or_else(|| corrupt("compaction source is missing covers_to"))?;
        if from > to {
            return Err(corrupt("compaction source coverage is reversed"));
        }
        let indexed_facts = JournalFactsIndex::build(&envelopes);
        let mut fragments = HashMap::<Option<BranchId>, Vec<&RawEnvelope>>::new();
        for envelope in &envelopes {
            if envelope.agent_id.as_ref() == agent_id {
                fragments
                    .entry(envelope.branch_id.clone())
                    .or_default()
                    .push(envelope);
            }
        }
        let mut messages = Vec::new();
        let mut selected = Vec::new();
        let mut owner = None::<Option<BranchId>>;
        let mut current_user_seen = false;
        let mut current_user_start = None;
        for entry in &ancestry[from..=to] {
            if owner.as_ref() != Some(&entry.owner_branch) {
                if let Some(previous_owner) = owner.take() {
                    flush_verbatim(
                        &mut selected,
                        &indexed_facts,
                        previous_owner.as_ref(),
                        agent_id,
                        None,
                        &mut current_user_seen,
                        &mut current_user_start,
                        &mut messages,
                    )?;
                }
                owner = Some(entry.owner_branch.clone());
            }
            if let Some(scoped) = fragments.get(&entry.owner_branch) {
                let start = scoped.partition_point(|envelope| envelope.seq <= entry.fragment_after);
                let end = scoped.partition_point(|envelope| envelope.seq <= entry.seq);
                selected.extend(
                    scoped[start..end]
                        .iter()
                        .map(|envelope| (*envelope).clone()),
                );
            }
        }
        if let Some(owner) = owner {
            flush_verbatim(
                &mut selected,
                &indexed_facts,
                owner.as_ref(),
                agent_id,
                None,
                &mut current_user_seen,
                &mut current_user_start,
                &mut messages,
            )?;
        }
        let boundary = messages.len();
        let mut projection = CompiledPromptProjection {
            messages,
            stable_history_end: boundary,
            current_user_start: boundary,
            latest_compaction_summary_end: None,
        };
        apply_structural_trim_events(&envelopes, &ancestry, agent_id, None, &mut projection)?;
        Ok(projection.messages)
    }

    /// Plans an idle compaction while retaining the same protected recent
    /// suffix as automatic summarization.
    pub async fn plan_idle_compaction(
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        operation_id: String,
    ) -> Result<crate::PlannedContextCompaction, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes);
        let ancestry = tree
            .latest_ancestry(&envelopes, &lineage, agent_id, lineage.head.as_ref())?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "there is no durable history to compact",
                    false,
                )
            })?;
        let full_projection =
            compile_ancestry(&envelopes, &ancestry, None, Some(artifacts), agent_id, None).await?;
        let mut retain_from = ancestry.len();
        let protected_runs = envelopes
            .iter()
            .filter_map(|envelope| {
                let EventPayload::ToolResult { result, .. } =
                    serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?
                else {
                    return None;
                };
                (!result.images.is_empty())
                    .then(|| envelope.run_id.clone())
                    .flatten()
            })
            .collect::<HashSet<_>>();
        if let Some(protected_from) = ancestry
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let NodeKind::UserTurn { attachments, .. } = &entry.node.kind else {
                    return None;
                };
                let protected_attachment = attachments.iter().any(|attachment| {
                    matches!(
                        attachment,
                        AttachmentBlock::Image { .. } | AttachmentBlock::Skill { .. }
                    )
                });
                (protected_attachment
                    || entry
                        .run_id
                        .as_ref()
                        .is_some_and(|run_id| protected_runs.contains(run_id)))
                .then_some(index)
            })
            .min()
        {
            retain_from = retain_from.min(protected_from);
        }
        let latest_prior_compaction = ancestry
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                matches!(entry.node.kind, NodeKind::Compaction { .. }).then_some(index)
            })
            .next_back();
        if latest_prior_compaction.is_some_and(|index| retain_from <= index) {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "protected history reaches an active prior compaction boundary",
                false,
            ));
        }
        let covers_to = retain_from.checked_sub(1).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "there is no clean history prefix to compact",
                false,
            )
        })?;
        let covered_projection = compile_ancestry(
            &envelopes,
            &ancestry[..retain_from],
            None,
            Some(artifacts),
            agent_id,
            None,
        )
        .await?;
        if covered_projection.messages.is_empty()
            || covered_projection.messages.len() > full_projection.messages.len()
        {
            return Err(corrupt(
                "idle compaction planner produced an invalid message boundary",
            ));
        }
        Ok(crate::PlannedContextCompaction {
            intent: CompactionIntent {
                operation_id,
                covers_from: ancestry[0].node.node.clone(),
                covers_to: ancestry[covers_to].node.node.clone(),
                resume_cause: CompactionResume::ManualIdle,
            },
            covered_message_count: covered_projection.messages.len(),
        })
    }
}

impl PromptHistoryCache {
    async fn compile_provider_projection_with_artifacts(
        &self,
        store: &dyn StoreHandle,
        artifacts: &dyn ArtifactReader,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<CompiledPromptProjection, HaiderError> {
        let head_seq = store.latest_seq(session_id).await?;
        let timeline = PromptTimelineKey {
            branch_id: branch_id.cloned(),
            agent_id: agent_id.cloned(),
        };
        let mut cached = self
            .sessions
            .lock()
            .await
            .remove(session_id)
            .unwrap_or_default();
        if cached.head_seq > head_seq {
            cached = CachedPromptSession::default();
        } else if cached.bodies_evicted {
            let saved_boundaries = std::mem::take(&mut cached.saved_boundaries);
            let retained_projections = std::mem::take(&mut cached.projections);
            let retained_append_prefixes = std::mem::take(&mut cached.append_prefixes);
            cached = replay_cached_session(store, session_id, head_seq).await?;
            cached.saved_boundaries = saved_boundaries;
            cached.projections = retained_projections;
            cached.append_prefixes = retained_append_prefixes;
        }

        // A checkpoint truncates only ONE exact branch+agent timeline. A
        // later request for another timeline must not inherit that prefix.
        if cached
            .checkpoint_base
            .as_ref()
            .is_some_and(|base| base.timeline != timeline)
        {
            cached = CachedPromptSession::default();
        }
        if cached.head_seq == 0
            && let Some(loaded) =
                load_prompt_checkpoint(store, session_id, &timeline, head_seq, current_run).await
        {
            let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
            let scope = PromptProjectionScope {
                compaction_epoch: loaded.compaction_epoch,
                branch_id: timeline.branch_id.clone(),
                agent_id: timeline.agent_id.clone(),
            };
            cached.head_seq = loaded.prefix.head_seq;
            cached
                .compaction_epochs
                .insert(timeline.clone(), loaded.compaction_epoch);
            cached.checkpoint_base = Some(PromptCheckpointBase {
                timeline: timeline.clone(),
                prefix_node_ids: loaded.prefix_node_ids,
                prefix_run_ids: loaded.prefix_run_ids,
            });
            cached
                .lineage_scopes
                .insert(timeline.branch_id.clone(), lineage.index);
            cached.append_prefixes.insert(scope, loaded.prefix);
        }

        let previous_compaction_epochs = cached.compaction_epochs.clone();
        let mut cursor = cached.head_seq;
        let mut compaction_after_checkpoint = false;
        while cursor < head_seq {
            let page = store.read(session_id, cursor, HISTORY_PAGE).await?;
            let before = cached.envelopes.len();
            for envelope in page {
                if envelope.seq > head_seq {
                    break;
                }
                cursor = envelope.seq;
                let affects_checkpoint_timeline = envelope.branch_id == timeline.branch_id
                    && envelope.agent_id == timeline.agent_id;
                if cached.push_envelope(envelope) {
                    compaction_after_checkpoint |=
                        cached.checkpoint_base.is_some() && affects_checkpoint_timeline;
                }
            }
            if cached.envelopes.len() == before {
                return Err(corrupt(format!(
                    "prompt cache could not read durable head {head_seq} after sequence {cursor}"
                )));
            }
        }
        cached.flush_boundary_rows();

        // A later compaction resets the model context again. The prior
        // checkpoint cannot derive the new overlay without its omitted tree;
        // replay once from zero, then install the newer boundary checkpoint.
        if compaction_after_checkpoint || cached.checkpoint_node_collision {
            cached = replay_cached_session(store, session_id, head_seq).await?;
        }
        if cached.head_seq < head_seq {
            cached.head_seq = head_seq;
            let changed_timelines = cached
                .compaction_epochs
                .iter()
                .filter(|(timeline, epoch)| {
                    previous_compaction_epochs.get(*timeline) != Some(*epoch)
                })
                .map(|(timeline, _)| timeline.clone())
                .collect::<HashSet<_>>();
            if !changed_timelines.is_empty() {
                cached.append_prefixes.retain(|scope, _| {
                    !changed_timelines.contains(&PromptTimelineKey {
                        branch_id: scope.branch_id.clone(),
                        agent_id: scope.agent_id.clone(),
                    })
                });
                cached.projections.retain(|key, _| {
                    !changed_timelines.contains(&PromptTimelineKey {
                        branch_id: key.branch_id.clone(),
                        agent_id: key.agent_id.clone(),
                    })
                });
            }
        }

        let compaction_epoch = cached.compaction_epoch(&timeline);
        let scope = PromptProjectionScope {
            compaction_epoch,
            branch_id: branch_id.cloned(),
            agent_id: agent_id.cloned(),
        };
        if cached.checkpoint_base.is_some() && !cached.append_prefixes.contains_key(&scope) {
            // A decoded checkpoint that cannot enter the existing suffix
            // extension seam proves nothing; rebuild with the oracle.
            cached = replay_cached_session(store, session_id, head_seq).await?;
        }
        let compaction_epoch = cached.compaction_epoch(&timeline);
        let scope = PromptProjectionScope {
            compaction_epoch,
            branch_id: branch_id.cloned(),
            agent_id: agent_id.cloned(),
        };
        let key = PromptProjectionKey {
            head_seq,
            compaction_epoch,
            branch_id: branch_id.cloned(),
            agent_id: agent_id.cloned(),
            current_run: current_run.clone(),
        };
        if let Some(projection) = cached
            .projections
            .get(&key)
            .and_then(|cached| cached.projection.as_ref())
            .cloned()
        {
            let projection = projection.as_ref().clone();
            self.install(session_id.clone(), cached).await;
            return Ok(projection);
        }
        let previous_exact = cached
            .projections
            .iter()
            .filter(|(candidate, _)| {
                candidate.head_seq < head_seq
                    && candidate.compaction_epoch == compaction_epoch
                    && candidate.branch_id == key.branch_id
                    && candidate.agent_id == key.agent_id
                    && candidate.current_run == *current_run
            })
            .filter(|(_, projection)| projection.projection.is_some())
            .max_by_key(|(candidate, _)| candidate.head_seq)
            .map(|(candidate, projection)| (candidate.head_seq, projection.clone()));
        let append_prefix = cached
            .append_prefixes
            .get(&scope)
            .filter(|prefix| {
                prefix.projection.is_some()
                    && prefix.head_seq < head_seq
                    && prefix.current_run != *current_run
            })
            .cloned();
        let (projection, projection_cursor) = if let Some(prefix) = append_prefix {
            if let Some(extended) =
                extend_compiled_projection(&prefix, &cached, branch_id, agent_id, current_run)?
            {
                extended
            } else {
                if cached.checkpoint_base.is_some() {
                    cached = replay_cached_session(store, session_id, head_seq).await?;
                }
                compile_projection_from_cache(
                    &mut cached,
                    store,
                    Some(artifacts),
                    session_id,
                    branch_id,
                    agent_id,
                    current_run,
                )
                .await?
            }
        } else if previous_exact.is_some() {
            let (prefix_head_seq, prefix) = previous_exact
                .as_ref()
                .ok_or_else(|| corrupt("exact prompt prefix disappeared"))?;
            if let Some(extended) = extend_exact_projection(
                *prefix_head_seq,
                prefix,
                &cached,
                branch_id,
                agent_id,
                current_run,
            )? {
                extended
            } else {
                if cached.checkpoint_base.is_some() {
                    cached = replay_cached_session(store, session_id, head_seq).await?;
                }
                compile_projection_from_cache(
                    &mut cached,
                    store,
                    Some(artifacts),
                    session_id,
                    branch_id,
                    agent_id,
                    current_run,
                )
                .await?
            }
        } else {
            compile_projection_from_cache(
                &mut cached,
                store,
                Some(artifacts),
                session_id,
                branch_id,
                agent_id,
                current_run,
            )
            .await?
        };
        let projection_body_bytes = serialized_body_bytes(&projection.messages);
        let projection = Arc::new(projection);
        // Keep the earliest request boundary for a live run. Later tool-round
        // recompiles intentionally suppress that run's assistant/tool output;
        // replacing this prefix at their later head would make those facts
        // fall before the next run's suffix and disappear permanently.
        if cached
            .append_prefixes
            .get(&scope)
            .is_none_or(|prefix| prefix.projection.is_none() || prefix.current_run != *current_run)
            && cached.can_seed_append_prefix(branch_id, agent_id, current_run, head_seq)
        {
            let current_run_terminal = cached
                .render_facts
                .facts(branch_id, agent_id)?
                .and_then(|facts| facts.terminal.get(current_run))
                .is_some_and(RunState::is_terminal);
            cached.append_prefixes.insert(
                scope,
                CachedCompiledPrefix {
                    head_seq,
                    current_run: current_run.clone(),
                    current_run_terminal,
                    projection: Some(Arc::clone(&projection)),
                    body_bytes: projection_body_bytes,
                    cursor: projection_cursor.clone(),
                },
            );
        }
        // One exact projection per branch+agent timeline is sufficient. The
        // append prefix above keeps the earlier live-run boundary when it is
        // semantically distinct; both maps share the same Arc at installation
        // instead of cloning the complete message vector.
        cached.projections.retain(|candidate, _| {
            candidate.branch_id != key.branch_id || candidate.agent_id != key.agent_id
        });
        cached.projections.insert(
            key,
            CachedExactProjection {
                projection: Some(Arc::clone(&projection)),
                body_bytes: projection_body_bytes,
                cursor: projection_cursor,
            },
        );

        if cached.checkpoint_base.is_none()
            && let Some(boundary) = cached.boundaries.get(&timeline).cloned()
            && cached
                .saved_boundaries
                .get(&timeline)
                .is_none_or(|saved| *saved < boundary.through_seq)
        {
            match build_prompt_checkpoint(
                store,
                artifacts,
                session_id,
                &timeline,
                &boundary,
                &cached.envelopes,
            )
            .await
            {
                Ok(checkpoint) => match store.put_projection_checkpoint(checkpoint).await {
                    Ok(()) => {
                        cached
                            .saved_boundaries
                            .insert(timeline.clone(), boundary.through_seq);
                    }
                    Err(error) => tracing::debug!(
                        session_id = %session_id,
                        boundary_seq = boundary.through_seq,
                        ?error,
                        "prompt projection checkpoint write failed; journal replay remains authoritative"
                    ),
                },
                Err(error) => tracing::debug!(
                    session_id = %session_id,
                    boundary_seq = boundary.through_seq,
                    ?error,
                    "prompt projection checkpoint build failed; journal replay remains authoritative"
                ),
            }
        }
        self.install(session_id.clone(), cached).await;
        Ok(projection.as_ref().clone())
    }

    async fn install(&self, session_id: SessionId, mut cached: CachedPromptSession) {
        let last_touched = self
            .touch_clock
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        cached.last_touched = last_touched;
        let mut sessions = self.sessions.lock().await;
        if let Some(current) = sessions.get_mut(&session_id)
            && current.head_seq > cached.head_seq
        {
            current.last_touched = last_touched;
            return;
        }
        if sessions
            .get(&session_id)
            .is_none_or(|current| current.head_seq <= cached.head_seq)
        {
            if !sessions.contains_key(&session_id)
                && sessions.len() >= PROMPT_CACHE_SESSION_LIMIT
                && let Some(evicted) = sessions
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_touched)
                    .map(|(session_id, _)| session_id.clone())
            {
                sessions.remove(&evicted);
            }
            sessions.insert(session_id, cached);
            while sessions
                .values()
                .map(CachedPromptSession::retained_heap_bytes)
                .fold(0_usize, usize::saturating_add)
                > PROMPT_CACHE_RETAINED_BYTES_LIMIT
            {
                let Some(evicted) = sessions
                    .iter()
                    .filter(|(_, entry)| !entry.bodies_evicted)
                    .min_by_key(|(_, entry)| entry.last_touched)
                    .map(|(session_id, _)| session_id.clone())
                else {
                    break;
                };
                if let Some(entry) = sessions.get_mut(&evicted) {
                    entry.evict_bodies();
                }
            }
        }
    }
}

async fn replay_cached_session(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    head_seq: u64,
) -> Result<CachedPromptSession, HaiderError> {
    let mut cached = CachedPromptSession::default();
    let mut cursor = 0;
    while cursor < head_seq {
        let page = store.read(session_id, cursor, HISTORY_PAGE).await?;
        let before = cached.envelopes.len();
        for envelope in page {
            if envelope.seq > head_seq {
                break;
            }
            cursor = envelope.seq;
            cached.push_envelope(envelope);
        }
        if cached.envelopes.len() == before {
            return Err(corrupt(format!(
                "prompt cache could not read durable head {head_seq} after sequence {cursor}"
            )));
        }
    }
    cached.flush_boundary_rows();
    cached.head_seq = head_seq;
    Ok(cached)
}

impl CachedPromptSession {
    fn retained_heap_bytes(&self) -> usize {
        if self.bodies_evicted {
            return 0;
        }
        // `envelope_weight_bytes` charges each initialized vector slot. Add
        // the live Vec's spare slots so eviction decisions reflect its actual
        // retained allocation rather than only its logical length.
        let envelope_capacity_slack = self
            .envelopes
            .capacity()
            .saturating_sub(self.envelopes.len())
            .saturating_mul(std::mem::size_of::<RawEnvelope>());
        let mut total = self
            .retained_envelope_bytes
            .saturating_add(envelope_capacity_slack);
        let mut projections = HashSet::new();
        for prefix in self.append_prefixes.values() {
            if let Some(projection) = prefix.projection.as_ref()
                && projections.insert(Arc::as_ptr(projection) as usize)
            {
                total = total.saturating_add(prefix.body_bytes);
            }
        }
        for exact in self.projections.values() {
            if let Some(projection) = exact.projection.as_ref()
                && projections.insert(Arc::as_ptr(projection) as usize)
            {
                total = total.saturating_add(exact.body_bytes);
            }
        }
        total
    }

    fn evict_bodies(&mut self) {
        self.retained_envelope_bytes = 0;
        self.bodies_evicted = true;
        // `clear` would drop the values but retain the body-owning allocation.
        self.envelopes = Vec::new();
        for prefix in self.append_prefixes.values_mut() {
            prefix.projection = None;
            prefix.body_bytes = 0;
        }
        for exact in self.projections.values_mut() {
            exact.projection = None;
            exact.body_bytes = 0;
        }
        self.render_facts = JournalFactsIndex::default();
        self.tree_index = TreeProjection::default();
        self.lineage_scopes.clear();
        self.checkpoint_base = None;
        self.checkpoint_node_collision = false;
        self.boundary_projector = TranscriptProjector::default();
        self.boundaries.clear();
    }

    fn note_compaction(&mut self, envelope: &RawEnvelope) {
        self.compaction_epochs.insert(
            PromptTimelineKey {
                branch_id: envelope.branch_id.clone(),
                agent_id: envelope.agent_id.clone(),
            },
            envelope.seq,
        );
    }

    fn compaction_epoch(&self, timeline: &PromptTimelineKey) -> u64 {
        self.compaction_epochs.get(timeline).copied().unwrap_or(0)
    }

    /// Advances every session-wide index from one decoded journal envelope.
    /// Returns whether the envelope starts a new compaction epoch.
    fn push_envelope(&mut self, envelope: RawEnvelope) -> bool {
        self.retained_envelope_bytes = self
            .retained_envelope_bytes
            .saturating_add(envelope_weight_bytes(&envelope));
        let envelope_index = self.envelopes.len();
        let payload = EventPayload::deserialize(&envelope.payload).ok();
        let checkpoint_node_collision = self
            .checkpoint_base
            .as_ref()
            .zip(payload.as_ref())
            .is_some_and(|(base, payload)| {
                let EventPayload::NodeCommitted(node) = payload else {
                    return false;
                };
                envelope.agent_id == base.timeline.agent_id
                    && self
                        .lineage_scopes
                        .get(&base.timeline.branch_id)
                        .is_some_and(|lineage| {
                            lineage.admits(envelope.branch_id.as_ref(), envelope.seq)
                        })
                    && base
                        .prefix_node_ids
                        .binary_search_by(|candidate| candidate.as_str().cmp(node.node.as_str()))
                        .is_ok()
            });
        self.checkpoint_node_collision |= checkpoint_node_collision;
        let is_compaction = matches!(
            payload.as_ref(),
            Some(EventPayload::NodeCommitted(TreeNode {
                kind: NodeKind::Compaction { .. },
                ..
            }))
        );
        if is_compaction {
            self.note_compaction(&envelope);
        }
        let is_context_savings = payload.as_ref().is_some_and(|payload| {
            let EventPayload::Item(ItemEvent::Completed { item, .. }) = payload else {
                return false;
            };
            matches!(item, TurnItem::Extension { kind, .. } if kind == CONTEXT_SAVINGS_EXTENSION_KIND)
        });
        if is_context_savings {
            let timeline = PromptTimelineKey {
                branch_id: envelope.branch_id.clone(),
                agent_id: envelope.agent_id.clone(),
            };
            self.append_prefixes.retain(|scope, _| {
                scope.branch_id != timeline.branch_id || scope.agent_id != timeline.agent_id
            });
            self.projections.retain(|key, _| {
                key.branch_id != timeline.branch_id || key.agent_id != timeline.agent_id
            });
        }
        self.render_facts.push_decoded(&envelope, payload.as_ref());
        self.tree_index
            .push(envelope_index, &envelope, payload.as_ref());
        let rows = self.boundary_projector.push(&envelope);
        self.envelopes.push(envelope);
        self.note_boundary_rows(rows);
        is_compaction || is_context_savings
    }

    fn indexed_ancestry_for_run(
        &self,
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        self.tree_index
            .ancestry_for_run(&self.envelopes, lineage, agent_id, current_run)
    }

    fn indexed_extension_for_run(
        &self,
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
        stop_node: &NodeId,
    ) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        self.tree_index.descendant_extension_for_run(
            &self.envelopes,
            lineage,
            agent_id,
            current_run,
            stop_node,
        )
    }

    /// A projection captured after current-run items have already committed
    /// cannot become the next run's prefix: those items are deliberately
    /// hidden while their run is current, but must appear once it is prior
    /// history. Keep an earlier safe prefix when one exists, or skip seeding
    /// so the next run performs the indexed full fold.
    fn can_seed_append_prefix(
        &self,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
        head_seq: u64,
    ) -> bool {
        // Tree-backed histories can jump directly to this run's first node,
        // keeping the one-time safety check proportional to the live run
        // rather than to all retained ancestry. Legacy node-less journals use
        // zero because they have no equivalent durable boundary.
        let run_key = (agent_id.cloned(), current_run.clone());
        let start_after = self.tree_index.by_run.get(&run_key).and_then(|indices| {
            indices.iter().find_map(|index| {
                let indexed = self.tree_index.ordered.get(*index)?;
                let envelope = self.envelopes.get(indexed.envelope_index)?;
                (envelope.branch_id.as_ref() == branch_id).then_some(indexed.fragment_after)
            })
        });
        let start = start_after.map_or(0, |seq| {
            self.envelopes
                .partition_point(|envelope| envelope.seq <= seq)
        });
        let end = self
            .envelopes
            .partition_point(|envelope| envelope.seq <= head_seq);
        !self.envelopes[start..end].iter().any(|envelope| {
            envelope.seq <= head_seq
                && envelope.run_id.as_ref() == Some(current_run)
                && scoped(envelope, branch_id, agent_id)
                && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::Item(_) | EventPayload::ToolResult { .. }
                        )
                    },
                )
        })
    }

    fn flush_boundary_rows(&mut self) {
        let rows = self.boundary_projector.flush_unresolved_tools();
        self.note_boundary_rows(rows);
    }

    fn note_boundary_rows(
        &mut self,
        rows: impl IntoIterator<Item = haider_protocol::pipe::SidecarRow>,
    ) {
        for row in rows {
            if !row.is_compaction_boundary() {
                continue;
            }
            let Some(boundary) = self
                .envelopes
                .iter()
                .rev()
                .find(|envelope| envelope.seq == row.seq())
            else {
                continue;
            };
            let Some(run_id) = boundary.run_id.clone() else {
                continue;
            };
            let timeline = PromptTimelineKey {
                branch_id: boundary.branch_id.clone(),
                agent_id: boundary.agent_id.clone(),
            };
            let candidate = PromptBoundary {
                through_seq: boundary.seq,
                event_id: boundary.event_id.clone(),
                run_id,
            };
            if self
                .boundaries
                .get(&timeline)
                .is_none_or(|current| current.through_seq < candidate.through_seq)
            {
                self.boundaries.insert(timeline, candidate);
            }
        }
    }
}

struct LoadedPromptCheckpoint {
    compaction_epoch: u64,
    prefix_node_ids: Vec<NodeId>,
    prefix_run_ids: Vec<RunId>,
    prefix: CachedCompiledPrefix,
}

fn prompt_timeline_key(timeline: &PromptTimelineKey) -> String {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "branch_id": timeline.branch_id,
        "agent_id": timeline.agent_id,
    }))
    .unwrap_or_default();
    format!("v1:{}", blake3::hash(&bytes).to_hex())
}

fn prompt_checkpoint_reducer_version() -> String {
    PROMPT_CHECKPOINT_REDUCER_VERSION.to_owned()
}

async fn load_prompt_checkpoint(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    timeline: &PromptTimelineKey,
    head_seq: u64,
    current_run: &RunId,
) -> Option<LoadedPromptCheckpoint> {
    let timeline_key = prompt_timeline_key(timeline);
    let stored = match store
        .projection_checkpoint(session_id, PROMPT_CHECKPOINT_PROJECTION, &timeline_key)
        .await
    {
        Ok(stored) => stored?,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                ?error,
                "prompt projection checkpoint read failed; replaying journal"
            );
            return None;
        }
    };
    let mut decoded = match serde_json::from_slice::<DurablePromptCheckpoint>(&stored.payload) {
        Ok(decoded) => decoded,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                ?error,
                "prompt projection checkpoint payload is corrupt; replaying journal"
            );
            return None;
        }
    };
    if decoded.shape_version != PROMPT_CHECKPOINT_SHAPE_VERSION
        || decoded.reducer_version != prompt_checkpoint_reducer_version()
        || decoded.through_seq == 0
        || decoded.through_seq > head_seq
        || decoded.through_seq != stored.through_seq
        || decoded.boundary_event_id != stored.boundary_event_id
        || decoded.boundary_run_id == *current_run
        || decoded.branch_id != timeline.branch_id
        || decoded.agent_id != timeline.agent_id
        || decoded.compaction_epoch == 0
        || decoded.compaction_epoch > decoded.through_seq
        || decoded.stable_history_end != decoded.messages.len()
        || decoded.current_user_start != decoded.messages.len()
        || decoded
            .latest_compaction_summary_end
            .is_none_or(|boundary| boundary == 0 || boundary > decoded.messages.len())
    {
        return None;
    }
    let boundary = match store
        .read(session_id, decoded.through_seq.saturating_sub(1), 1)
        .await
    {
        Ok(events) => events.into_iter().next()?,
        Err(error) => {
            tracing::debug!(
                session_id = %session_id,
                ?error,
                "prompt checkpoint boundary could not be read; replaying journal"
            );
            return None;
        }
    };
    if boundary.seq != decoded.through_seq
        || boundary.event_id != decoded.boundary_event_id
        || boundary.run_id.as_ref() != Some(&decoded.boundary_run_id)
        || boundary.branch_id != decoded.branch_id
        || boundary.agent_id != decoded.agent_id
    {
        return None;
    }
    let Ok(EventPayload::RunState(boundary_state)) =
        serde_json::from_value::<EventPayload>(boundary.payload.clone())
    else {
        return None;
    };
    if !boundary_state.is_terminal() {
        return None;
    }
    // Recover only the compacted prefix's tree head. It is usually the
    // compaction node itself, but scanning through the terminal boundary also
    // covers a valid committed node between compaction and terminal state.
    let mut anchor_cursor = decoded.compaction_epoch.saturating_sub(1);
    let mut checkpoint_head_node = None;
    let mut saw_compaction = false;
    while anchor_cursor < decoded.through_seq {
        let page = match store.read(session_id, anchor_cursor, HISTORY_PAGE).await {
            Ok(page) => page,
            Err(error) => {
                tracing::debug!(
                    session_id = %session_id,
                    ?error,
                    "prompt checkpoint tree anchor could not be read; replaying journal"
                );
                return None;
            }
        };
        let before = anchor_cursor;
        for envelope in page {
            if envelope.seq > decoded.through_seq {
                break;
            }
            anchor_cursor = envelope.seq;
            if envelope.branch_id != decoded.branch_id || envelope.agent_id != decoded.agent_id {
                continue;
            }
            let Ok(EventPayload::NodeCommitted(node)) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            if envelope.seq == decoded.compaction_epoch {
                if !matches!(node.kind, NodeKind::Compaction { .. }) {
                    return None;
                }
                saw_compaction = true;
            }
            checkpoint_head_node = Some(node.node);
        }
        if anchor_cursor == before {
            return None;
        }
    }
    if !saw_compaction {
        return None;
    }
    let checkpoint_head_node = checkpoint_head_node?;
    let mut prefix_node_ids = std::mem::take(&mut decoded.prefix_node_ids);
    prefix_node_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    if prefix_node_ids.windows(2).any(|pair| pair[0] == pair[1])
        || prefix_node_ids
            .binary_search_by(|candidate| candidate.as_str().cmp(checkpoint_head_node.as_str()))
            .is_err()
    {
        // Older checkpoints did not persist the membership proof required to
        // distinguish a suffix node from a duplicate of compacted ancestry.
        return None;
    }
    let mut prefix_run_ids = std::mem::take(&mut decoded.prefix_run_ids);
    prefix_run_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    if prefix_run_ids.windows(2).any(|pair| pair[0] == pair[1])
        || prefix_run_ids
            .binary_search_by(|candidate| candidate.as_str().cmp(decoded.boundary_run_id.as_str()))
            .is_err()
        || prefix_run_ids
            .binary_search_by(|candidate| candidate.as_str().cmp(current_run.as_str()))
            .is_ok()
    {
        return None;
    }
    let messages = match decoded
        .messages
        .into_iter()
        .map(serde_json::from_value::<Message>)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(messages) => messages,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                ?error,
                "prompt projection checkpoint messages are corrupt; replaying journal"
            );
            return None;
        }
    };
    Some(LoadedPromptCheckpoint {
        compaction_epoch: decoded.compaction_epoch,
        prefix_node_ids,
        prefix_run_ids,
        prefix: CachedCompiledPrefix {
            head_seq: decoded.through_seq,
            current_run: decoded.boundary_run_id,
            current_run_terminal: true,
            body_bytes: serialized_body_bytes(&messages),
            projection: Some(Arc::new(CompiledPromptProjection {
                messages,
                stable_history_end: decoded.stable_history_end,
                current_user_start: decoded.current_user_start,
                latest_compaction_summary_end: decoded.latest_compaction_summary_end,
            })),
            cursor: PromptProjectionCursor::Tree {
                head_node: checkpoint_head_node,
            },
        },
    })
}

async fn build_prompt_checkpoint(
    store: &dyn StoreHandle,
    artifacts: &dyn ArtifactReader,
    session_id: &SessionId,
    timeline: &PromptTimelineKey,
    boundary: &PromptBoundary,
    envelopes: &[RawEnvelope],
) -> Result<SessionProjectionCheckpoint, HaiderError> {
    let prefix = envelopes
        .iter()
        .take_while(|envelope| envelope.seq <= boundary.through_seq)
        .cloned()
        .collect::<Vec<_>>();
    let boundary_envelope = prefix
        .last()
        .ok_or_else(|| corrupt("prompt checkpoint boundary is absent from the replayed prefix"))?;
    if boundary_envelope.seq != boundary.through_seq
        || boundary_envelope.event_id != boundary.event_id
        || boundary_envelope.run_id.as_ref() != Some(&boundary.run_id)
        || boundary_envelope.branch_id != timeline.branch_id
        || boundary_envelope.agent_id != timeline.agent_id
    {
        return Err(corrupt(
            "prompt checkpoint boundary identity disagrees with the journal",
        ));
    }
    let EventPayload::RunState(boundary_state) = serde_json::from_value::<EventPayload>(
        boundary_envelope.payload.clone(),
    )
    .map_err(|error| corrupt(format!("prompt checkpoint boundary is malformed: {error}")))?
    else {
        return Err(corrupt(
            "prompt checkpoint boundary is not a run-state event",
        ));
    };
    if !boundary_state.is_terminal() {
        return Err(corrupt(
            "prompt checkpoint boundary run-state is not terminal",
        ));
    }
    let compaction_epoch = prefix
        .iter()
        .filter(|envelope| {
            envelope.branch_id == timeline.branch_id && envelope.agent_id == timeline.agent_id
        })
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| {
                    matches!(
                        payload,
                        EventPayload::NodeCommitted(TreeNode {
                            kind: NodeKind::Compaction { .. },
                            ..
                        })
                    )
                })
                .then_some(envelope.seq)
        })
        .max()
        .ok_or_else(|| corrupt("compaction boundary has no preceding compaction node"))?;
    let lineage = ResolvedLineage::load(store, session_id, timeline.branch_id.as_ref()).await?;
    let mut seen_node_ids = HashSet::new();
    let mut prefix_node_ids = Vec::new();
    let mut seen_run_ids = HashSet::new();
    let mut prefix_run_ids = Vec::new();
    for envelope in &prefix {
        if envelope.agent_id != timeline.agent_id
            || !lineage.admits(envelope.branch_id.as_ref(), envelope.seq)
        {
            continue;
        }
        if let Some(run_id) = &envelope.run_id
            && seen_run_ids.insert(run_id.clone())
        {
            prefix_run_ids.push(run_id.clone());
        }
        let Ok(EventPayload::NodeCommitted(node)) =
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
        else {
            continue;
        };
        if !seen_node_ids.insert(node.node.clone()) {
            return Err(corrupt(format!(
                "history tree contains duplicate node {}",
                node.node
            )));
        }
        prefix_node_ids.push(node.node);
    }
    let projection = compile_idle_projection_at_prefix(
        store,
        artifacts,
        session_id,
        timeline.branch_id.as_ref(),
        timeline.agent_id.as_ref(),
        &prefix,
    )
    .await?;
    if projection.latest_compaction_summary_end.is_none() {
        return Err(corrupt(
            "compaction boundary produced no active summary in the prompt projection",
        ));
    }
    let messages = projection
        .messages
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt(format!("prompt checkpoint message encode failed: {error}")))?;
    let payload = serde_json::to_vec(&DurablePromptCheckpoint {
        shape_version: PROMPT_CHECKPOINT_SHAPE_VERSION,
        reducer_version: prompt_checkpoint_reducer_version(),
        through_seq: boundary.through_seq,
        boundary_event_id: boundary.event_id.clone(),
        boundary_run_id: boundary.run_id.clone(),
        branch_id: timeline.branch_id.clone(),
        agent_id: timeline.agent_id.clone(),
        compaction_epoch,
        prefix_node_ids,
        prefix_run_ids,
        messages,
        stable_history_end: projection.stable_history_end,
        current_user_start: projection.current_user_start,
        latest_compaction_summary_end: projection.latest_compaction_summary_end,
    })
    .map_err(|error| corrupt(format!("prompt checkpoint encode failed: {error}")))?;
    Ok(SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: PROMPT_CHECKPOINT_PROJECTION.to_owned(),
        timeline_key: prompt_timeline_key(timeline),
        through_seq: boundary.through_seq,
        boundary_event_id: boundary.event_id.clone(),
        payload,
    })
}

/// Extends a completed request projection along the selected journal or tree
/// path. The old request (including its user message) is now immutable history;
/// rendering the extension produces the preceding assistant/tool results
/// followed by the new current user. A compaction epoch change never reaches
/// this function and recompiles through the full oracle instead.
fn extend_compiled_projection(
    prefix: &CachedCompiledPrefix,
    cached: &CachedPromptSession,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: &RunId,
) -> Result<Option<(CompiledPromptProjection, PromptProjectionCursor)>, HaiderError> {
    let Some(prefix_projection) = prefix.projection.as_ref() else {
        return Ok(None);
    };
    let suffix_start = cached
        .envelopes
        .partition_point(|envelope| envelope.seq <= prefix.head_seq);
    let suffix = &cached.envelopes[suffix_start..];
    let (rendered, cursor) = match &prefix.cursor {
        PromptProjectionCursor::Journal => {
            // Surface the same scoped prepass corruption as a full render
            // even when the suffix has no newly visible messages.
            let _ = cached.render_facts.facts(branch_id, agent_id)?;
            if suffix_revises_prior_facts(
                &cached.envelopes[..suffix_start],
                suffix,
                branch_id,
                agent_id,
                &prefix.current_run,
                cached
                    .checkpoint_base
                    .as_ref()
                    .map(|base| base.prefix_run_ids.as_slice()),
                true,
                prefix.current_run_terminal,
            )? {
                return Ok(None);
            }
            let timeline = PromptTimelineKey {
                branch_id: branch_id.cloned(),
                agent_id: agent_id.cloned(),
            };
            if cached.tree_index.has_timeline(&timeline) {
                return Ok(None);
            }
            let rendered = render_journal_with_indexed_facts(
                suffix,
                &cached.render_facts,
                branch_id,
                agent_id,
                Some(current_run),
                true,
            )?;
            (rendered, PromptProjectionCursor::Journal)
        }
        PromptProjectionCursor::Tree { head_node } => {
            let branch_key = branch_id.cloned();
            let Some(index) = cached.lineage_scopes.get(&branch_key).cloned() else {
                return Ok(None);
            };
            if lineage_suffix_revises_prior_facts(
                cached,
                &index,
                &cached.envelopes[..suffix_start],
                suffix,
                agent_id,
                &prefix.current_run,
                true,
                prefix.current_run_terminal,
            )? {
                return Ok(None);
            }
            let lineage = ResolvedLineage { index, head: None };
            let Some(extension) =
                cached.indexed_extension_for_run(&lineage, agent_id, current_run, head_node)?
            else {
                return Ok(None);
            };
            if extension
                .iter()
                .any(|entry| matches!(entry.node.kind, NodeKind::Compaction { .. }))
            {
                return Ok(None);
            }
            let rendered = render_tree_extension(
                &extension,
                &cached.envelopes,
                &cached.render_facts,
                agent_id,
                current_run,
            )?;
            let head_node = extension
                .last()
                .map(|entry| entry.node.node.clone())
                .unwrap_or_else(|| head_node.clone());
            (rendered, PromptProjectionCursor::Tree { head_node })
        }
    };
    let mut messages = prefix_projection.messages.clone();
    let offset = messages.len();
    let current_user_start = rendered
        .current_user_start
        .map(|index| offset.saturating_add(index))
        .ok_or_else(|| {
            corrupt(format!(
                "accepted run {current_run} has no append-only committed user message"
            ))
        })?;
    messages.extend(rendered.messages);
    Ok(Some((
        CompiledPromptProjection {
            stable_history_end: current_user_start,
            current_user_start,
            latest_compaction_summary_end: prefix_projection.latest_compaction_summary_end,
            messages,
        },
        cursor,
    )))
}

fn extend_exact_projection(
    prefix_head_seq: u64,
    prefix: &CachedExactProjection,
    cached: &CachedPromptSession,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: &RunId,
) -> Result<Option<(CompiledPromptProjection, PromptProjectionCursor)>, HaiderError> {
    let Some(prefix_projection) = prefix.projection.as_ref() else {
        return Ok(None);
    };
    let suffix_start = cached
        .envelopes
        .partition_point(|envelope| envelope.seq <= prefix_head_seq);
    let suffix = &cached.envelopes[suffix_start..];
    match &prefix.cursor {
        PromptProjectionCursor::Journal => {
            // Surface the same scoped prepass corruption as a full render
            // even when the appended suffix has no visible messages.
            let _ = cached.render_facts.facts(branch_id, agent_id)?;
            if suffix_revises_prior_facts(
                &cached.envelopes[..suffix_start],
                suffix,
                branch_id,
                agent_id,
                current_run,
                cached
                    .checkpoint_base
                    .as_ref()
                    .map(|base| base.prefix_run_ids.as_slice()),
                false,
                false,
            )? {
                return Ok(None);
            }
            let timeline = PromptTimelineKey {
                branch_id: branch_id.cloned(),
                agent_id: agent_id.cloned(),
            };
            if cached.tree_index.has_timeline(&timeline) {
                return Ok(None);
            }
            let rendered = render_journal_with_indexed_facts(
                suffix,
                &cached.render_facts,
                branch_id,
                agent_id,
                Some(current_run),
                false,
            )?;
            let mut messages = prefix_projection.messages.clone();
            messages.extend(rendered.messages);
            Ok(Some((
                CompiledPromptProjection {
                    messages,
                    stable_history_end: prefix_projection.stable_history_end,
                    current_user_start: prefix_projection.current_user_start,
                    latest_compaction_summary_end: prefix_projection.latest_compaction_summary_end,
                },
                PromptProjectionCursor::Journal,
            )))
        }
        PromptProjectionCursor::Tree { head_node } => {
            let branch_key = branch_id.cloned();
            let Some(index) = cached.lineage_scopes.get(&branch_key).cloned() else {
                return Ok(None);
            };
            if lineage_suffix_revises_prior_facts(
                cached,
                &index,
                &cached.envelopes[..suffix_start],
                suffix,
                agent_id,
                current_run,
                false,
                false,
            )? {
                return Ok(None);
            }
            let lineage = ResolvedLineage { index, head: None };
            let Some(extension) =
                cached.indexed_extension_for_run(&lineage, agent_id, current_run, head_node)?
            else {
                return Ok(None);
            };
            if extension
                .iter()
                .any(|entry| matches!(entry.node.kind, NodeKind::Compaction { .. }))
            {
                return Ok(None);
            }
            let projection = extend_tree_projection(
                prefix_projection,
                &extension,
                &cached.envelopes,
                &cached.render_facts,
                agent_id,
                current_run,
            )?;
            let head_node = extension
                .last()
                .map(|entry| entry.node.node.clone())
                .unwrap_or_else(|| head_node.clone());
            Ok(Some((
                projection,
                PromptProjectionCursor::Tree { head_node },
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn lineage_suffix_revises_prior_facts(
    cached: &CachedPromptSession,
    lineage: &LineageIndex,
    prior: &[RawEnvelope],
    suffix: &[RawEnvelope],
    agent_id: Option<&AgentId>,
    prefix_run: &RunId,
    prefix_becomes_prior: bool,
    prefix_was_terminal: bool,
) -> Result<bool, HaiderError> {
    let checkpoint_prefix_runs = cached
        .checkpoint_base
        .as_ref()
        .map(|base| base.prefix_run_ids.as_slice());
    let checkpoint_owns_prefix_run = checkpoint_prefix_runs.is_some_and(|runs| {
        runs.binary_search_by(|candidate| candidate.as_str().cmp(prefix_run.as_str()))
            .is_ok()
    });
    let mut prefix_owner_scopes = HashSet::<Option<BranchId>>::new();
    if prefix_becomes_prior {
        if let Some(indices) = cached
            .tree_index
            .by_run
            .get(&(agent_id.cloned(), prefix_run.clone()))
        {
            for index in indices {
                let Some(indexed) = cached.tree_index.ordered.get(*index) else {
                    return Err(corrupt("prompt tree run index is out of bounds"));
                };
                if indexed.envelope_index >= prior.len() {
                    continue;
                }
                let Some(envelope) = cached.envelopes.get(indexed.envelope_index) else {
                    return Err(corrupt("prompt tree envelope index is out of bounds"));
                };
                if lineage.admits(envelope.branch_id.as_ref(), envelope.seq) {
                    prefix_owner_scopes.insert(envelope.branch_id.clone());
                }
            }
        }
        if checkpoint_owns_prefix_run && let Some(base) = &cached.checkpoint_base {
            prefix_owner_scopes.insert(base.timeline.branch_id.clone());
        }
        if prefix_owner_scopes.is_empty() {
            // A tree prefix should identify its run-owning node. If truncated
            // state cannot prove that owner, let the complete fold decide.
            return Ok(true);
        }
    }
    let mut affected_scopes = suffix
        .iter()
        .filter(|envelope| {
            envelope.agent_id.as_ref() == agent_id
                && envelope
                    .branch_id
                    .as_ref()
                    .is_none_or(|branch| lineage.branches.contains_key(branch))
        })
        .map(|envelope| envelope.branch_id.clone())
        .collect::<HashSet<_>>();
    affected_scopes.extend(prefix_owner_scopes.iter().cloned());
    let check_scope = |branch_id: Option<&BranchId>| -> Result<bool, HaiderError> {
        // Only suffix owners and the prior run's indexed node owners can
        // change an already-compiled projection. This keeps same-run warm
        // steers proportional to the appended suffix instead of rescanning
        // every branch and the complete journal.
        let check = || -> Result<bool, HaiderError> {
            let facts = cached.render_facts.facts(branch_id, agent_id)?;
            let owns_prefix_run = prefix_owner_scopes.contains(&branch_id.cloned());
            let prefix_scope_was_terminal = facts
                .and_then(|facts| facts.terminal.get(prefix_run))
                .is_some_and(RunState::is_terminal)
                || (owns_prefix_run && checkpoint_owns_prefix_run && prefix_was_terminal);
            suffix_revises_prior_facts(
                prior,
                suffix,
                branch_id,
                agent_id,
                prefix_run,
                checkpoint_prefix_runs,
                prefix_becomes_prior && owns_prefix_run,
                prefix_scope_was_terminal,
            )
        };
        match check() {
            result @ Ok(_) => result,
            // This prepass includes scopes that selected ancestry may not
            // render (including an irrelevant requested leaf). Rebuild
            // through the oracle so it alone decides whether an error is live.
            Err(_) => Ok(true),
        }
    };
    let mut affected_scopes = affected_scopes.into_iter().collect::<Vec<_>>();
    affected_scopes.sort_by(|left, right| match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => left.as_str().cmp(right.as_str()),
    });
    for branch in &affected_scopes {
        if check_scope(branch.as_ref())? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn suffix_revises_prior_facts(
    prior: &[RawEnvelope],
    suffix: &[RawEnvelope],
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    prefix_run: &RunId,
    checkpoint_prefix_runs: Option<&[RunId]>,
    require_prefix_terminal: bool,
    prefix_was_terminal: bool,
) -> Result<bool, HaiderError> {
    // Durable checkpoints intentionally retain the compiled projection rather
    // than all pre-boundary render facts. Any later envelope for a run that is
    // represented by that projection can depend on those omitted facts (for
    // example, a completed assistant item after the run became terminal).
    // Let the complete fold adjudicate every such continuation.
    if checkpoint_prefix_runs.is_some_and(|runs| {
        suffix.iter().any(|envelope| {
            scoped(envelope, branch_id, agent_id)
                && envelope.run_id.as_ref().is_some_and(|run_id| {
                    runs.binary_search_by(|candidate| candidate.as_str().cmp(run_id.as_str()))
                        .is_ok()
                })
        })
    }) {
        return Ok(true);
    }
    let mut terminal_other_runs = HashSet::new();
    let mut prefix_final_state = None;
    let mut suffix_partial_menus = HashMap::<MenuId, ItemId>::new();
    let mut suffix_completed_items = HashSet::new();
    for envelope in suffix {
        if !scoped(envelope, branch_id, agent_id) {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        match payload {
            EventPayload::RunState(state) => {
                if let Some(run_id) = &envelope.run_id {
                    if run_id == prefix_run {
                        prefix_final_state = Some(state);
                    } else {
                        terminal_other_runs.insert(run_id.clone());
                    }
                }
            }
            EventPayload::MenuOpened(menu) => {
                if let MenuKind::ErrorRecovery {
                    card: ErrorRecoveryCardKind::PartialStream,
                    source_item: Some(source_item),
                    ..
                } = menu.kind
                {
                    suffix_partial_menus.insert(menu.id, source_item);
                }
            }
            EventPayload::Item(ItemEvent::Completed { item_id, .. }) => {
                suffix_completed_items.insert(item_id);
            }
            _ => {}
        }
    }
    if require_prefix_terminal
        && prefix_final_state
            .as_ref()
            .map_or(!prefix_was_terminal, |state| !state.is_terminal())
    {
        return Ok(true);
    }
    // A non-current run whose complete history begins in the suffix is local
    // to that suffix (the normal checkpoint-resume case). If any of its facts
    // precede the seam, the terminal transition retroactively changes prefix
    // visibility and requires the indexed full fold.
    if !terminal_other_runs.is_empty()
        && (prior.iter().any(|envelope| {
            scoped(envelope, branch_id, agent_id)
                && envelope
                    .run_id
                    .as_ref()
                    .is_some_and(|run_id| terminal_other_runs.contains(run_id))
        }) || checkpoint_prefix_runs.is_some_and(|runs| {
            terminal_other_runs.iter().any(|run_id| {
                runs.binary_search_by(|candidate| candidate.as_str().cmp(run_id.as_str()))
                    .is_ok()
            })
        }))
    {
        return Ok(true);
    }
    for envelope in suffix {
        if !scoped(envelope, branch_id, agent_id) {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        match payload {
            EventPayload::MenuAnswered(answer) => {
                let Some(source_item) = suffix_partial_menus.get(&answer.menu) else {
                    return Ok(true);
                };
                if checkpoint_prefix_runs.is_some()
                    || !suffix_completed_items.contains(source_item)
                    || contains_scoped_item(prior, branch_id, agent_id, source_item)
                {
                    return Ok(true);
                }
            }
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                if let Some(origin) =
                    UserCommandOriginV1::try_from_extension_item(&item).map_err(|error| {
                        corrupt(format!("malformed user-command origin marker: {error}"))
                    })?
                    && (checkpoint_prefix_runs.is_some()
                        || !suffix_completed_items.contains(&origin.command_item_id)
                        || contains_scoped_item(
                            prior,
                            branch_id,
                            agent_id,
                            &origin.command_item_id,
                        ))
                {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn contains_scoped_item(
    envelopes: &[RawEnvelope],
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    item_id: &ItemId,
) -> bool {
    envelopes.iter().any(|envelope| {
        if !scoped(envelope, branch_id, agent_id) {
            return false;
        }
        serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(|payload| {
            matches!(
                payload,
                EventPayload::Item(ItemEvent::Delta { item_id: candidate, .. })
                    | EventPayload::Item(ItemEvent::Completed { item_id: candidate, .. })
                    if &candidate == item_id
            )
        })
    })
}

fn extend_tree_projection(
    prefix: &CompiledPromptProjection,
    extension: &[TreeEntry],
    envelopes: &[RawEnvelope],
    indexed_facts: &JournalFactsIndex,
    agent_id: Option<&AgentId>,
    current_run: &RunId,
) -> Result<CompiledPromptProjection, HaiderError> {
    let rendered =
        render_tree_extension(extension, envelopes, indexed_facts, agent_id, current_run)?;
    let mut messages = prefix.messages.clone();
    messages.extend(rendered.messages);
    Ok(CompiledPromptProjection {
        messages,
        stable_history_end: prefix.stable_history_end,
        current_user_start: prefix.current_user_start,
        latest_compaction_summary_end: prefix.latest_compaction_summary_end,
    })
}

fn render_tree_extension(
    extension: &[TreeEntry],
    envelopes: &[RawEnvelope],
    indexed_facts: &JournalFactsIndex,
    agent_id: Option<&AgentId>,
    current_run: &RunId,
) -> Result<RenderedJournal, HaiderError> {
    let mut messages = Vec::new();
    let mut current_user_seen = false;
    let mut current_user_start = None;
    let mut verbatim = Vec::new();
    let mut owner = None::<Option<BranchId>>;
    for entry in extension {
        if owner.as_ref() != Some(&entry.owner_branch) {
            if let Some(previous_owner) = owner.take() {
                flush_verbatim(
                    &mut verbatim,
                    indexed_facts,
                    previous_owner.as_ref(),
                    agent_id,
                    Some(current_run),
                    &mut current_user_seen,
                    &mut current_user_start,
                    &mut messages,
                )?;
            }
            owner = Some(entry.owner_branch.clone());
        }
        let start = envelopes.partition_point(|envelope| envelope.seq <= entry.fragment_after);
        let end = envelopes.partition_point(|envelope| envelope.seq <= entry.seq);
        verbatim.extend(
            envelopes[start..end]
                .iter()
                .filter(|envelope| {
                    envelope.branch_id.as_ref() == entry.owner_branch.as_ref()
                        && envelope.agent_id.as_ref() == agent_id
                })
                .cloned(),
        );
    }
    if let Some(owner) = owner {
        flush_verbatim(
            &mut verbatim,
            indexed_facts,
            owner.as_ref(),
            agent_id,
            Some(current_run),
            &mut current_user_seen,
            &mut current_user_start,
            &mut messages,
        )?;
    }
    Ok(RenderedJournal {
        messages,
        current_user_seen,
        current_user_start,
    })
}

async fn compile_idle_projection_at_prefix(
    store: &dyn StoreHandle,
    artifacts: &dyn ArtifactReader,
    session_id: &SessionId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    envelopes: &[RawEnvelope],
) -> Result<CompiledPromptProjection, HaiderError> {
    let mut lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
    if branch_id.is_some() {
        lineage.head = envelopes.iter().rev().find_map(|envelope| {
            if envelope.branch_id.as_ref() != branch_id || envelope.agent_id.as_ref() != agent_id {
                return None;
            }
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, envelope.seq))
        });
    }
    let tree = TreeProjection::build(envelopes);
    let ancestry = tree
        .latest_ancestry(envelopes, &lineage, agent_id, lineage.head.as_ref())?
        .ok_or_else(|| corrupt("compaction boundary has no durable history ancestry"))?;
    let tree_head_seq = ancestry.last().map_or(0, |entry| entry.seq);
    let mut projection =
        compile_ancestry(envelopes, &ancestry, None, Some(artifacts), agent_id, None).await?;
    let tail = envelopes
        .iter()
        .filter(|envelope| envelope.seq > tree_head_seq && scoped(envelope, branch_id, agent_id))
        .cloned()
        .collect::<Vec<_>>();
    let rendered = render_journal(&tail, envelopes, branch_id, agent_id, None, false)?;
    projection.messages.extend(rendered.messages);
    projection.stable_history_end = projection.messages.len();
    projection.current_user_start = projection.messages.len();
    Ok(projection)
}

async fn compile_projection_from_envelopes(
    store: &dyn StoreHandle,
    artifacts: Option<&dyn ArtifactReader>,
    session_id: &SessionId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: &RunId,
    envelopes: &[RawEnvelope],
) -> Result<CompiledPromptProjection, HaiderError> {
    if legacy_journal_only(envelopes, branch_id, agent_id) {
        return render_journal(
            envelopes,
            envelopes,
            branch_id,
            agent_id,
            Some(current_run),
            true,
        )
        .map(CompiledPromptProjection::from_rendered);
    }
    let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
    let tree = TreeProjection::build(envelopes);
    let Some(ancestry) = tree.ancestry_for_run(envelopes, &lineage, agent_id, current_run)? else {
        return render_journal(
            envelopes,
            envelopes,
            branch_id,
            agent_id,
            Some(current_run),
            true,
        )
        .map(CompiledPromptProjection::from_rendered);
    };
    compile_ancestry(
        envelopes,
        &ancestry,
        None,
        artifacts,
        agent_id,
        Some(current_run),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn compile_projection_from_cache(
    cached: &mut CachedPromptSession,
    store: &dyn StoreHandle,
    artifacts: Option<&dyn ArtifactReader>,
    session_id: &SessionId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: &RunId,
) -> Result<(CompiledPromptProjection, PromptProjectionCursor), HaiderError> {
    if legacy_journal_only(&cached.envelopes, branch_id, agent_id) {
        let projection = render_journal_with_indexed_facts(
            &cached.envelopes,
            &cached.render_facts,
            branch_id,
            agent_id,
            Some(current_run),
            true,
        )
        .map(CompiledPromptProjection::from_rendered)?;
        return Ok((projection, PromptProjectionCursor::Journal));
    }
    let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
    cached
        .lineage_scopes
        .insert(branch_id.cloned(), lineage.index.clone());
    let Some(ancestry) = cached.indexed_ancestry_for_run(&lineage, agent_id, current_run)? else {
        let projection = render_journal_with_indexed_facts(
            &cached.envelopes,
            &cached.render_facts,
            branch_id,
            agent_id,
            Some(current_run),
            true,
        )
        .map(CompiledPromptProjection::from_rendered)?;
        return Ok((projection, PromptProjectionCursor::Journal));
    };
    let head_node = ancestry
        .last()
        .map(|entry| entry.node.node.clone())
        .ok_or_else(|| corrupt("indexed prompt ancestry is unexpectedly empty"))?;
    let projection = compile_ancestry(
        &cached.envelopes,
        &ancestry,
        Some(&cached.render_facts),
        artifacts,
        agent_id,
        Some(current_run),
    )
    .await?;
    Ok((projection, PromptProjectionCursor::Tree { head_node }))
}

#[derive(Clone)]
struct LineageIndex {
    main_through_seq: u64,
    branches: HashMap<BranchId, u64>,
}

struct ResolvedLineage {
    index: LineageIndex,
    head: Option<(NodeId, u64)>,
}

impl LineageIndex {
    fn admits(&self, branch_id: Option<&BranchId>, seq: u64) -> bool {
        branch_id.map_or(seq <= self.main_through_seq, |branch| {
            self.branches
                .get(branch)
                .is_some_and(|through_seq| seq <= *through_seq)
        })
    }
}

impl ResolvedLineage {
    async fn load(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Self, HaiderError> {
        let descriptors = store.branch_lineage(session_id, branch_id).await?;
        let Some(requested) = branch_id else {
            if !descriptors.is_empty() {
                return Err(corrupt(
                    "implicit main branch returned concrete lineage rows",
                ));
            }
            return Ok(Self {
                index: LineageIndex {
                    main_through_seq: u64::MAX,
                    branches: HashMap::new(),
                },
                head: None,
            });
        };
        let leaf = descriptors.last().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("branch {requested} does not exist"),
                false,
            )
        })?;
        if leaf.branch_id != *requested {
            return Err(corrupt(format!(
                "branch lineage leaf {} does not match requested {requested}",
                leaf.branch_id
            )));
        }
        validate_descriptor_chain(&descriptors)?;
        let mut branches = HashMap::with_capacity(descriptors.len());
        let mut ceiling = u64::MAX;
        for descriptor in descriptors.iter().rev() {
            branches.insert(descriptor.branch_id.clone(), ceiling);
            ceiling = ceiling.min(descriptor.fork_seq);
        }
        Ok(Self {
            index: LineageIndex {
                main_through_seq: ceiling,
                branches,
            },
            head: Some((leaf.head_node_id.clone(), leaf.head_seq)),
        })
    }

    fn admits(&self, branch_id: Option<&BranchId>, seq: u64) -> bool {
        self.index.admits(branch_id, seq)
    }
}

fn validate_descriptor_chain(descriptors: &[BranchDescriptor]) -> Result<(), HaiderError> {
    let mut expected_source: Option<&BranchId> = None;
    let mut seen = HashSet::new();
    for descriptor in descriptors {
        if descriptor.source_branch_id.as_ref() != expected_source
            || descriptor.fork_seq == 0
            || descriptor.created_seq == 0
            || descriptor.head_seq == 0
            || descriptor.fork_node_id.as_str().is_empty()
            || descriptor.head_node_id.as_str().is_empty()
            || !seen.insert(descriptor.branch_id.clone())
        {
            return Err(corrupt("branch registry contains an invalid lineage chain"));
        }
        expected_source = Some(&descriptor.branch_id);
    }
    Ok(())
}

#[derive(Clone)]
struct TreeEntry {
    node: TreeNode,
    seq: u64,
    fragment_after: u64,
    run_id: Option<RunId>,
    owner_branch: Option<BranchId>,
}

#[derive(Default)]
struct TreeProjection {
    // One session-wide node index. Lineage and agent scope are applied while
    // resolving candidates, so touched branches never duplicate their common
    // ancestry and each appended node is indexed exactly once.
    ordered: Vec<IndexedTreeEntry>,
    by_id: HashMap<(Option<AgentId>, NodeId), NodeIndices>,
    by_run: HashMap<(Option<AgentId>, RunId), Vec<usize>>,
    duplicate_ids: HashSet<(Option<AgentId>, NodeId)>,
    latest_by_timeline: HashMap<PromptTimelineKey, usize>,
    previous_node_seq: HashMap<PromptTimelineKey, u64>,
}

struct IndexedTreeEntry {
    envelope_index: usize,
    parent: Option<NodeId>,
    fragment_after: u64,
}

enum NodeIndices {
    One(usize),
    Many(Vec<usize>),
}

impl NodeIndices {
    fn push(&mut self, index: usize) {
        match self {
            Self::One(first) => *self = Self::Many(vec![*first, index]),
            Self::Many(indices) => indices.push(index),
        }
    }

    fn find(&self, mut predicate: impl FnMut(usize) -> bool) -> Option<usize> {
        match self {
            Self::One(index) => predicate(*index).then_some(*index),
            Self::Many(indices) => indices.iter().copied().find(|index| predicate(*index)),
        }
    }

    fn matching_count(&self, mut predicate: impl FnMut(usize) -> bool) -> usize {
        match self {
            Self::One(index) => {
                if predicate(*index) {
                    1
                } else {
                    0
                }
            }
            Self::Many(indices) => indices
                .iter()
                .copied()
                .filter(|index| predicate(*index))
                .take(2)
                .count(),
        }
    }
}

impl TreeProjection {
    fn build(envelopes: &[RawEnvelope]) -> Self {
        let mut tree = Self::default();
        for (envelope_index, envelope) in envelopes.iter().enumerate() {
            let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok();
            tree.push(envelope_index, envelope, payload.as_ref());
        }
        tree
    }

    fn push(
        &mut self,
        envelope_index: usize,
        envelope: &RawEnvelope,
        payload: Option<&EventPayload>,
    ) {
        let Some(EventPayload::NodeCommitted(node)) = payload else {
            return;
        };
        let index = self.ordered.len();
        let timeline = PromptTimelineKey {
            branch_id: envelope.branch_id.clone(),
            agent_id: envelope.agent_id.clone(),
        };
        let fragment_after = self.previous_node_seq.get(&timeline).copied().unwrap_or(0);
        self.ordered.push(IndexedTreeEntry {
            envelope_index,
            parent: node.parent.clone(),
            fragment_after,
        });
        let node_key = (envelope.agent_id.clone(), node.node.clone());
        if let Some(candidates) = self.by_id.get_mut(&node_key) {
            candidates.push(index);
            self.duplicate_ids.insert(node_key);
        } else {
            self.by_id.insert(node_key, NodeIndices::One(index));
        }
        if let Some(run_id) = &envelope.run_id {
            self.by_run
                .entry((envelope.agent_id.clone(), run_id.clone()))
                .or_default()
                .push(index);
        }
        self.latest_by_timeline.insert(timeline.clone(), index);
        self.previous_node_seq.insert(timeline, envelope.seq);
    }

    fn has_timeline(&self, timeline: &PromptTimelineKey) -> bool {
        self.latest_by_timeline.contains_key(timeline)
    }

    fn ancestry_for_run(
        &self,
        envelopes: &[RawEnvelope],
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
        run_id: &RunId,
    ) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        self.validate_duplicates(envelopes, lineage, agent_id)?;
        let key = (agent_id.cloned(), run_id.clone());
        let Some(index) = self.by_run.get(&key).and_then(|candidates| {
            candidates
                .iter()
                .rev()
                .copied()
                .find(|index| self.admitted(envelopes, *index, lineage, agent_id))
        }) else {
            return Ok(None);
        };
        self.ancestry_from(envelopes, lineage, agent_id, index)
            .map(Some)
    }

    /// Resolves only the selected descendants after `stop_node`. Unlike a cold
    /// ancestry compile, warm extension cost is proportional to the appended
    /// path and a checkpoint anchor need not remain resident in the journal.
    fn descendant_extension_for_run(
        &self,
        envelopes: &[RawEnvelope],
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
        run_id: &RunId,
        stop_node: &NodeId,
    ) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        self.validate_duplicates(envelopes, lineage, agent_id)?;
        let key = (agent_id.cloned(), run_id.clone());
        let Some(mut index) = self.by_run.get(&key).and_then(|candidates| {
            candidates
                .iter()
                .rev()
                .copied()
                .find(|index| self.admitted(envelopes, *index, lineage, agent_id))
        }) else {
            return Ok(None);
        };
        let mut reverse = Vec::new();
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(index) {
                let envelope = self.envelope(envelopes, index)?;
                return Err(corrupt(format!(
                    "history tree contains a cycle at node {}",
                    decode_tree_node(envelope)?.node
                )));
            }
            let indexed = &self.ordered[index];
            let envelope = self.envelope(envelopes, index)?;
            let node = decode_tree_node(envelope)?;
            if node.node == *stop_node {
                reverse.reverse();
                return Ok(Some(reverse));
            }
            let parent = indexed.parent.clone();
            reverse.push(TreeEntry {
                node,
                seq: envelope.seq,
                fragment_after: indexed.fragment_after,
                run_id: envelope.run_id.clone(),
                owner_branch: envelope.branch_id.clone(),
            });
            let Some(parent) = parent else {
                return Ok(None);
            };
            if parent == *stop_node {
                reverse.reverse();
                return Ok(Some(reverse));
            }
            let Some(parent_index) = self.find_node(envelopes, lineage, agent_id, &parent) else {
                // A checkpoint deliberately omits pre-boundary nodes, so a
                // selected rewind can leave the resident index without being
                // corrupt. The caller's full/replay fallback adjudicates the
                // complete tree and still surfaces a genuinely absent parent.
                return Ok(None);
            };
            index = parent_index;
        }
    }

    fn latest_ancestry(
        &self,
        envelopes: &[RawEnvelope],
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
        head: Option<&(NodeId, u64)>,
    ) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        self.validate_duplicates(envelopes, lineage, agent_id)?;
        let index = if let Some((head_node, head_seq)) = head {
            let index = self
                .find_node(envelopes, lineage, agent_id, head_node)
                .ok_or_else(|| {
                    corrupt(format!("branch head references missing node {head_node}"))
                })?;
            let envelope = self.envelope(envelopes, index)?;
            if envelope.seq != *head_seq {
                return Err(corrupt(format!(
                    "branch head node {head_node} disagrees with sequence {head_seq}"
                )));
            }
            index
        } else {
            let timeline = PromptTimelineKey {
                branch_id: None,
                agent_id: agent_id.cloned(),
            };
            let Some(index) = self.latest_by_timeline.get(&timeline).copied() else {
                return Ok(None);
            };
            index
        };
        self.ancestry_from(envelopes, lineage, agent_id, index)
            .map(Some)
    }

    fn ancestry_from(
        &self,
        envelopes: &[RawEnvelope],
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
        mut index: usize,
    ) -> Result<Vec<TreeEntry>, HaiderError> {
        let mut reverse = Vec::new();
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(index) {
                let envelope = self.envelope(envelopes, index)?;
                return Err(corrupt(format!(
                    "history tree contains a cycle at node {}",
                    decode_tree_node(envelope)?.node
                )));
            }
            let indexed = &self.ordered[index];
            let envelope = self.envelope(envelopes, index)?;
            let node = decode_tree_node(envelope)?;
            reverse.push(TreeEntry {
                node,
                seq: envelope.seq,
                fragment_after: indexed.fragment_after,
                run_id: envelope.run_id.clone(),
                owner_branch: envelope.branch_id.clone(),
            });
            let Some(parent) = &indexed.parent else {
                break;
            };
            index = self
                .find_node(envelopes, lineage, agent_id, parent)
                .ok_or_else(|| {
                    corrupt(format!("history tree references missing parent {parent}"))
                })?;
        }
        reverse.reverse();
        Ok(reverse)
    }

    fn validate_duplicates(
        &self,
        envelopes: &[RawEnvelope],
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
    ) -> Result<(), HaiderError> {
        for (candidate_agent, node_id) in &self.duplicate_ids {
            if candidate_agent.as_ref() != agent_id {
                continue;
            }
            let admitted = self
                .by_id
                .get(&(candidate_agent.clone(), node_id.clone()))
                .map_or(0, |indices| {
                    indices
                        .matching_count(|index| self.admitted(envelopes, index, lineage, agent_id))
                });
            if admitted > 1 {
                return Err(corrupt(format!(
                    "history tree contains duplicate node {node_id}"
                )));
            }
        }
        Ok(())
    }

    fn find_node(
        &self,
        envelopes: &[RawEnvelope],
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
        node_id: &NodeId,
    ) -> Option<usize> {
        self.by_id
            .get(&(agent_id.cloned(), node_id.clone()))
            .and_then(|indices| {
                indices.find(|index| self.admitted(envelopes, index, lineage, agent_id))
            })
    }

    fn admitted(
        &self,
        envelopes: &[RawEnvelope],
        index: usize,
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
    ) -> bool {
        self.envelope(envelopes, index).is_ok_and(|envelope| {
            envelope.agent_id.as_ref() == agent_id
                && lineage.admits(envelope.branch_id.as_ref(), envelope.seq)
        })
    }

    fn envelope<'a>(
        &self,
        envelopes: &'a [RawEnvelope],
        index: usize,
    ) -> Result<&'a RawEnvelope, HaiderError> {
        envelopes
            .get(self.ordered[index].envelope_index)
            .ok_or_else(|| corrupt("history tree index points outside the journal"))
    }
}

fn decode_tree_node(envelope: &RawEnvelope) -> Result<TreeNode, HaiderError> {
    let EventPayload::NodeCommitted(node) =
        serde_json::from_value::<EventPayload>(envelope.payload.clone())
            .map_err(|error| corrupt(format!("indexed history node is malformed: {error}")))?
    else {
        return Err(corrupt("history tree index points to a non-node event"));
    };
    Ok(node)
}

struct SummaryInsertion {
    node: NodeId,
    summary_artifact: ArtifactRef,
}

struct ProjectionPlan {
    covered: HashSet<usize>,
    summary_at: HashMap<usize, SummaryInsertion>,
}

impl ProjectionPlan {
    fn build(ancestry: &[TreeEntry]) -> Result<Self, HaiderError> {
        let positions = ancestry
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.node.node.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut ranges = Vec::<(usize, usize, usize, SummaryInsertion)>::new();
        for (compaction_index, entry) in ancestry.iter().enumerate() {
            let NodeKind::Compaction {
                covers_from,
                covers_to,
                summary_artifact,
                ..
            } = &entry.node.kind
            else {
                continue;
            };
            let from = *positions.get(covers_from).ok_or_else(|| {
                corrupt(format!(
                    "compaction node {} covers missing node {covers_from}",
                    entry.node.node
                ))
            })?;
            let to = *positions.get(covers_to).ok_or_else(|| {
                corrupt(format!(
                    "compaction node {} covers missing node {covers_to}",
                    entry.node.node
                ))
            })?;
            if from > to || to >= compaction_index {
                return Err(corrupt(format!(
                    "compaction node {} has an invalid inclusive coverage range",
                    entry.node.node
                )));
            }
            ranges.push((
                from,
                to,
                compaction_index,
                SummaryInsertion {
                    node: entry.node.node.clone(),
                    summary_artifact: summary_artifact.clone(),
                },
            ));
        }

        for left in 0..ranges.len() {
            for right in (left + 1)..ranges.len() {
                let (a_from, a_to, _, _) = &ranges[left];
                let (b_from, b_to, _, _) = &ranges[right];
                let overlaps = a_from <= b_to && b_from <= a_to;
                let nested =
                    (a_from <= b_from && b_to <= a_to) || (b_from <= a_from && a_to <= b_to);
                if overlaps && !nested {
                    return Err(corrupt("history tree contains crossing compaction ranges"));
                }
            }
        }

        let active = ranges
            .iter()
            .enumerate()
            .filter_map(|(candidate, range)| {
                let covered_by_later = ranges.iter().enumerate().any(|(other, later)| {
                    other != candidate
                        && later.2 > range.2
                        && later.0 <= range.2
                        && range.2 <= later.1
                });
                (!covered_by_later).then_some(range)
            })
            .collect::<Vec<_>>();
        let mut covered = HashSet::new();
        let mut summary_at = HashMap::new();
        for (from, to, _, insertion) in active {
            covered.extend(*from..=*to);
            if summary_at
                .insert(
                    *from,
                    SummaryInsertion {
                        node: insertion.node.clone(),
                        summary_artifact: insertion.summary_artifact.clone(),
                    },
                )
                .is_some()
            {
                return Err(corrupt(
                    "history tree contains ambiguous compaction insertions",
                ));
            }
        }
        Ok(Self {
            covered,
            summary_at,
        })
    }
}

struct RenderedJournal {
    messages: Vec<Message>,
    current_user_seen: bool,
    current_user_start: Option<usize>,
}

#[derive(Default)]
struct UserCommandOutput {
    chunks: Vec<UserCommandOutputChunk>,
    retained_bytes: usize,
    total_bytes: u64,
}

struct UserCommandOutputChunk {
    stream: OutputStream,
    bytes: Vec<u8>,
}

impl UserCommandOutput {
    fn push(&mut self, stream: OutputStream, bytes: &[u8]) {
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let remaining = USER_COMMAND_OUTPUT_PREVIEW_BYTES.saturating_sub(self.retained_bytes);
        let retained = bytes.len().min(remaining);
        if retained == 0 {
            return;
        }
        if let Some(last) = self.chunks.last_mut()
            && last.stream == stream
        {
            last.bytes.extend_from_slice(&bytes[..retained]);
        } else {
            self.chunks.push(UserCommandOutputChunk {
                stream,
                bytes: bytes[..retained].to_vec(),
            });
        }
        self.retained_bytes = self.retained_bytes.saturating_add(retained);
    }

    fn finish(self, failed: bool) -> (String, bool, bool, u64) {
        let mut preview = String::new();
        let mut truncated = self.total_bytes > self.retained_bytes as u64;
        let mut lossy_utf8 = false;
        for (index, chunk) in self.chunks.into_iter().enumerate() {
            let label = match (index, chunk.stream) {
                (0, OutputStream::Stdout) => "[stdout]\n",
                (0, OutputStream::Stderr) => "[stderr]\n",
                (_, OutputStream::Stdout) => "\n[stdout]\n",
                (_, OutputStream::Stderr) => "\n[stderr]\n",
            };
            truncated |= !append_bounded_utf8(&mut preview, label);
            let decoded = String::from_utf8_lossy(&chunk.bytes);
            lossy_utf8 |= matches!(decoded, std::borrow::Cow::Owned(_));
            truncated |= !append_bounded_utf8(&mut preview, &decoded);
        }
        // E4 content law: the journal remains the raw command transcript.
        // Deterministic output adapters apply only while constructing the
        // provider-facing record from those raw bytes.
        let reduced = haider_tools::reduce_tool_output("shell_exec", &preview, failed);
        truncated |= reduced.text != preview;
        (reduced.text, truncated, lossy_utf8, self.total_bytes)
    }
}

fn append_bounded_utf8(target: &mut String, value: &str) -> bool {
    let remaining = USER_COMMAND_OUTPUT_PREVIEW_BYTES.saturating_sub(target.len());
    if value.len() <= remaining {
        target.push_str(value);
        return true;
    }
    let mut end = remaining.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
    false
}

async fn compile_ancestry(
    envelopes: &[RawEnvelope],
    ancestry: &[TreeEntry],
    indexed_facts: Option<&JournalFactsIndex>,
    artifacts: Option<&dyn ArtifactReader>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
) -> Result<CompiledPromptProjection, HaiderError> {
    let plan = ProjectionPlan::build(ancestry)?;
    // Rendering may flush once per owning branch and compaction boundary.
    // Fold the journal-wide terminal/menu/origin facts once so those flushes
    // do not each rescan the complete envelope vector.
    let owned_facts = indexed_facts
        .is_none()
        .then(|| JournalFactsIndex::build(envelopes));
    let Some(indexed_facts) = indexed_facts.or(owned_facts.as_ref()) else {
        return Err(corrupt("prompt journal fact index was not installed"));
    };
    // Index once by the already-fixed agent and owning branch. An ancestry
    // node can then select its `(fragment_after, seq]` slice with two binary
    // searches instead of rescanning the complete journal for every node.
    let mut fragments = HashMap::<Option<BranchId>, Vec<&RawEnvelope>>::new();
    for envelope in envelopes {
        if envelope.agent_id.as_ref() == agent_id {
            fragments
                .entry(envelope.branch_id.clone())
                .or_default()
                .push(envelope);
        }
    }
    let mut messages = Vec::new();
    let mut current_user_seen = false;
    let mut current_user_start = None;
    let mut latest_compaction_summary_end = None;
    let mut verbatim = Vec::new();
    let mut verbatim_owner = None::<Option<BranchId>>;

    for (index, entry) in ancestry.iter().enumerate() {
        if let Some(compaction) = plan.summary_at.get(&index) {
            if let Some(owner) = verbatim_owner.take() {
                flush_verbatim(
                    &mut verbatim,
                    indexed_facts,
                    owner.as_ref(),
                    agent_id,
                    current_run,
                    &mut current_user_seen,
                    &mut current_user_start,
                    &mut messages,
                )?;
            }
            let reader = artifacts.ok_or_else(|| {
                corrupt(format!(
                    "compaction node {} requires artifact {} but no artifact reader was supplied",
                    compaction.node, compaction.summary_artifact
                ))
            })?;
            let bytes = reader
                .read_artifact(&compaction.summary_artifact)
                .await
                .map_err(|error| {
                    corrupt(format!(
                        "compaction node {} summary {} is unavailable: {}",
                        compaction.node, compaction.summary_artifact, error.message
                    ))
                })?;
            let summary = String::from_utf8(bytes).map_err(|error| {
                corrupt(format!(
                    "compaction node {} summary {} is not UTF-8: {error}",
                    compaction.node, compaction.summary_artifact
                ))
            })?;
            messages.push(Message::user_text(summary));
            latest_compaction_summary_end = Some(messages.len());
        }

        if plan.covered.contains(&index) || matches!(entry.node.kind, NodeKind::Compaction { .. }) {
            continue;
        }
        if verbatim_owner.as_ref() != Some(&entry.owner_branch) {
            if let Some(owner) = verbatim_owner.take() {
                flush_verbatim(
                    &mut verbatim,
                    indexed_facts,
                    owner.as_ref(),
                    agent_id,
                    current_run,
                    &mut current_user_seen,
                    &mut current_user_start,
                    &mut messages,
                )?;
            }
            verbatim_owner = Some(entry.owner_branch.clone());
        }
        if let Some(scoped) = fragments.get(&entry.owner_branch) {
            let start = scoped.partition_point(|envelope| envelope.seq <= entry.fragment_after);
            let end = scoped.partition_point(|envelope| envelope.seq <= entry.seq);
            verbatim.extend(
                scoped[start..end]
                    .iter()
                    .map(|envelope| (*envelope).clone()),
            );
        }
    }
    if let Some(owner) = verbatim_owner {
        flush_verbatim(
            &mut verbatim,
            indexed_facts,
            owner.as_ref(),
            agent_id,
            current_run,
            &mut current_user_seen,
            &mut current_user_start,
            &mut messages,
        )?;
    }
    if let Some(current_run) = current_run
        && !current_user_seen
    {
        return Err(corrupt(format!(
            "accepted run {current_run} has no tree-selected committed user message"
        )));
    }
    let current_user_start = current_user_start.unwrap_or(messages.len());
    let mut projection = CompiledPromptProjection {
        stable_history_end: current_user_start,
        current_user_start,
        latest_compaction_summary_end,
        messages,
    };
    apply_structural_trim_events(envelopes, ancestry, agent_id, current_run, &mut projection)?;
    Ok(projection)
}

fn apply_structural_trim_events(
    envelopes: &[RawEnvelope],
    ancestry: &[TreeEntry],
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
    projection: &mut CompiledPromptProjection,
) -> Result<(), HaiderError> {
    let mut fragments = HashMap::<Option<BranchId>, Vec<&RawEnvelope>>::new();
    for envelope in envelopes {
        if envelope.agent_id.as_ref() == agent_id {
            fragments
                .entry(envelope.branch_id.clone())
                .or_default()
                .push(envelope);
        }
    }
    let mut selections = Vec::<(u64, String)>::new();
    let mut seen_savings_events = HashSet::new();
    for entry in ancestry {
        let Some(scoped) = fragments.get(&entry.owner_branch) else {
            continue;
        };
        let start = scoped.partition_point(|envelope| envelope.seq <= entry.fragment_after);
        let end = scoped.partition_point(|envelope| envelope.seq <= entry.seq);
        for envelope in &scoped[start..end] {
            if !seen_savings_events.insert(envelope.seq) {
                continue;
            }
            let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            if let Some(event) = ContextSavingsEvent::try_from_extension_item(&item)
                .map_err(|error| corrupt(format!("context-savings event is malformed: {error}")))?
            {
                selections.extend(
                    event
                        .removed_tool_call_ids
                        .into_iter()
                        .map(|call_id| (envelope.seq, call_id)),
                );
            }
        }
    }
    // A structural trim commits before the provider request that will create
    // the next assistant tree node. If the daemon restarts in that interval,
    // the event is newer than the ancestry head but remains authoritative for
    // this accepted run's provider view.
    if let Some(current_run) = current_run {
        for envelope in envelopes.iter().filter(|envelope| {
            envelope.agent_id.as_ref() == agent_id && envelope.run_id.as_ref() == Some(current_run)
        }) {
            if !seen_savings_events.insert(envelope.seq) {
                continue;
            }
            let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            if let Some(event) = ContextSavingsEvent::try_from_extension_item(&item)
                .map_err(|error| corrupt(format!("context-savings event is malformed: {error}")))?
            {
                selections.extend(
                    event
                        .removed_tool_call_ids
                        .into_iter()
                        .map(|call_id| (envelope.seq, call_id)),
                );
            }
        }
    }
    selections.sort_by_key(|(seq, _)| *seq);
    for (_, call_id) in selections {
        remove_oldest_complete_tool_pair(projection, &call_id);
    }
    Ok(())
}

fn remove_oldest_complete_tool_pair(
    projection: &mut CompiledPromptProjection,
    removed_call_id: &str,
) {
    let mut pending_call = None;
    let mut pair = None;
    'messages: for (message_index, message) in projection.messages.iter().enumerate() {
        for (block_index, block) in message.blocks.iter().enumerate() {
            let coordinate = (message_index, block_index);
            match block {
                Block::ToolCall { call_id, .. } if call_id == removed_call_id => {
                    if pending_call.is_some() {
                        // Live trimming never records an ambiguous pair, so
                        // restart replay stays conservative instead of
                        // guessing between two unmatched calls.
                        return;
                    }
                    pending_call = Some(coordinate);
                }
                Block::ToolResult {
                    call_id, images, ..
                } if call_id == removed_call_id => {
                    let Some(call) = pending_call else {
                        continue;
                    };
                    if call.0 >= coordinate.0 || !images.is_empty() {
                        return;
                    }
                    pair = Some((call, coordinate));
                    break 'messages;
                }
                _ => {}
            }
        }
    }
    let Some((call, result)) = pair else {
        return;
    };
    let stable_history_end = projection.stable_history_end;
    let current_user_start = projection.current_user_start;
    let latest_summary_end = projection.latest_compaction_summary_end;
    let mut removed_before_stable = 0_usize;
    let mut removed_before_current = 0_usize;
    let mut removed_before_summary = 0_usize;
    let mut message_index = 0_usize;
    projection.messages.retain_mut(|message| {
        let original_index = message_index;
        message_index = message_index.saturating_add(1);
        let mut block_index = 0_usize;
        message.blocks.retain(|_| {
            let coordinate = (original_index, block_index);
            block_index = block_index.saturating_add(1);
            coordinate != call && coordinate != result
        });
        let retain = !message.blocks.is_empty();
        if !retain {
            removed_before_stable += usize::from(original_index < stable_history_end);
            removed_before_current += usize::from(original_index < current_user_start);
            removed_before_summary +=
                usize::from(latest_summary_end.is_some_and(|end| original_index < end));
        }
        retain
    });
    projection.stable_history_end = stable_history_end.saturating_sub(removed_before_stable);
    projection.current_user_start = current_user_start.saturating_sub(removed_before_current);
    projection.latest_compaction_summary_end =
        latest_summary_end.map(|end| end.saturating_sub(removed_before_summary));
}

#[allow(clippy::too_many_arguments)]
fn flush_verbatim(
    verbatim: &mut Vec<RawEnvelope>,
    indexed_facts: &JournalFactsIndex,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
    current_user_seen: &mut bool,
    current_user_start: &mut Option<usize>,
    messages: &mut Vec<Message>,
) -> Result<(), HaiderError> {
    if verbatim.is_empty() {
        return Ok(());
    }
    let rendered = render_journal_with_indexed_facts(
        verbatim,
        indexed_facts,
        branch_id,
        agent_id,
        current_run,
        false,
    )?;
    if current_user_start.is_none()
        && let Some(relative) = rendered.current_user_start
    {
        *current_user_start = Some(messages.len().saturating_add(relative));
    }
    *current_user_seen |= rendered.current_user_seen;
    messages.extend(rendered.messages);
    verbatim.clear();
    Ok(())
}

#[derive(Default)]
struct JournalFactsIndex {
    timelines: HashMap<PromptTimelineKey, JournalFactsState>,
}

#[derive(Default)]
struct JournalFactsState {
    facts: JournalFacts,
    error: Option<HaiderError>,
}

#[derive(Default)]
struct JournalFacts {
    terminal: HashMap<RunId, RunState>,
    partial_menus: HashMap<MenuId, (haider_protocol::ids::ItemId, String, u32)>,
    continued_partial_items: HashSet<haider_protocol::ids::ItemId>,
    user_command_origins: HashMap<haider_protocol::ids::ItemId, UserCommandOriginV1>,
}

impl JournalFactsIndex {
    fn build(envelopes: &[RawEnvelope]) -> Self {
        let mut index = Self::default();
        for envelope in envelopes {
            index.push(envelope);
        }
        index
    }

    fn push(&mut self, envelope: &RawEnvelope) {
        let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok();
        self.push_decoded(envelope, payload.as_ref());
    }

    fn push_decoded(&mut self, envelope: &RawEnvelope, payload: Option<&EventPayload>) {
        let timeline = PromptTimelineKey {
            branch_id: envelope.branch_id.clone(),
            agent_id: envelope.agent_id.clone(),
        };
        let state = self.timelines.entry(timeline).or_default();
        if state.error.is_none()
            && let Err(error) = state.facts.push(envelope, payload)
        {
            state.error = Some(error);
        }
    }

    fn facts(
        &self,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
    ) -> Result<Option<&JournalFacts>, HaiderError> {
        let timeline = PromptTimelineKey {
            branch_id: branch_id.cloned(),
            agent_id: agent_id.cloned(),
        };
        let Some(state) = self.timelines.get(&timeline) else {
            return Ok(None);
        };
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        Ok(Some(&state.facts))
    }
}

impl JournalFacts {
    fn build(
        envelopes: &[RawEnvelope],
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
    ) -> Result<Self, HaiderError> {
        let mut facts = Self::default();
        for envelope in envelopes {
            if scoped(envelope, branch_id, agent_id) {
                let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok();
                facts.push(envelope, payload.as_ref())?;
            }
        }
        Ok(facts)
    }

    fn push(
        &mut self,
        envelope: &RawEnvelope,
        payload: Option<&EventPayload>,
    ) -> Result<(), HaiderError> {
        let Some(run_id) = envelope.run_id.clone() else {
            return Ok(());
        };
        let Some(payload) = payload else {
            return Ok(());
        };
        match payload {
            EventPayload::RunState(state) => {
                self.terminal.insert(run_id, state.clone());
            }
            EventPayload::MenuOpened(menu) => {
                if let MenuKind::ErrorRecovery {
                    card: ErrorRecoveryCardKind::PartialStream,
                    option_actions,
                    source_item: Some(item_id),
                    ..
                } = &menu.kind
                    && let Some(action_index) = option_actions
                        .iter()
                        .position(|action| *action == ErrorAction::ContinuePartial)
                    && let Some(option) = menu.options.get(action_index)
                {
                    self.partial_menus.insert(
                        menu.id.clone(),
                        (
                            item_id.clone(),
                            option.key.clone(),
                            u32::try_from(action_index).unwrap_or(u32::MAX),
                        ),
                    );
                }
            }
            EventPayload::MenuAnswered(answer) => {
                if let Some((item_id, continue_key, continue_index)) =
                    self.partial_menus.get(&answer.menu)
                    && answer
                        .option_key
                        .as_deref()
                        .map_or(answer.option_index == *continue_index, |key| {
                            key == continue_key
                        })
                {
                    self.continued_partial_items.insert(item_id.clone());
                }
            }
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                let origin =
                    UserCommandOriginV1::try_from_extension_item(item).map_err(|error| {
                        corrupt(format!("malformed user-command origin marker: {error}"))
                    })?;
                if let Some(origin) = origin
                    && self
                        .user_command_origins
                        .insert(origin.command_item_id.clone(), origin)
                        .is_some()
                {
                    return Err(corrupt("duplicate user-command origin marker"));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn render_journal(
    selected: &[RawEnvelope],
    all_envelopes: &[RawEnvelope],
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
    require_current_user: bool,
) -> Result<RenderedJournal, HaiderError> {
    let facts = JournalFacts::build(all_envelopes, branch_id, agent_id)?;
    render_journal_with_facts(
        selected,
        &facts,
        branch_id,
        agent_id,
        current_run,
        require_current_user,
    )
}

fn render_journal_with_indexed_facts(
    selected: &[RawEnvelope],
    indexed_facts: &JournalFactsIndex,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
    require_current_user: bool,
) -> Result<RenderedJournal, HaiderError> {
    let empty = JournalFacts::default();
    let facts = indexed_facts.facts(branch_id, agent_id)?.unwrap_or(&empty);
    render_journal_with_facts(
        selected,
        facts,
        branch_id,
        agent_id,
        current_run,
        require_current_user,
    )
}

fn render_journal_with_facts(
    selected: &[RawEnvelope],
    facts: &JournalFacts,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
    require_current_user: bool,
) -> Result<RenderedJournal, HaiderError> {
    let mut messages = Vec::new();
    let mut pending_tool_results = HashMap::<String, BoundedResult>::new();
    let mut user_command_outputs =
        HashMap::<haider_protocol::ids::ItemId, UserCommandOutput>::new();
    let mut current_user_seen = false;
    let mut current_user_start = None;
    for envelope in selected {
        if !scoped(envelope, branch_id, agent_id) {
            continue;
        }
        // W-A background task facts render as bounded user-role notices
        // BEFORE the run-terminal gate: a task outlives its spawning run by
        // design, so its completion must reach the next prompt even when
        // that run was cancelled. `render.prompt` stays the one off switch —
        // a steer-delivered completion journals with `Omit` because the
        // durable steer user message already carries the prompt copy.
        if envelope.render.prompt != PromptRender::Omit
            && let Some(event) = TaskEventPayload::from_payload_value(&envelope.payload)
        {
            messages.push(Message::user_text(task_event_notice(&event)));
            continue;
        }
        let Some(run_id) = envelope.run_id.clone() else {
            continue;
        };
        let is_current = current_run.is_some_and(|current| run_id == *current);
        let prior_state = facts.terminal.get(&run_id);
        // Invariant since the terminal-history flip (f53cb2c): every run that
        // was previously visible only through the narrow terminal
        // user-command lane (`!is_current && prior_state is terminal`) is now
        // ordinarily visible — `ordinary_visible` is a strict superset of the
        // retired `terminal_user_command_visible`, so it alone gates below.
        let ordinary_visible = is_current || prior_state.is_some_and(RunState::is_terminal);
        if !ordinary_visible {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        // Direct-shell chunks are raw durable/UI facts with prompt=omit. The
        // completed command item below is the sole prompt record, and adapts
        // this accumulated raw transcript exactly once for the model.
        if let EventPayload::Item(ItemEvent::Delta {
            item_id,
            delta: ItemDelta::CommandOutput { stream, chunk_b64 },
        }) = &payload
            && facts.user_command_origins.contains_key(item_id)
        {
            let bytes = BASE64.decode(chunk_b64).map_err(|error| {
                corrupt(format!(
                    "user command output for item {item_id} is not valid base64: {error}"
                ))
            })?;
            user_command_outputs
                .entry(item_id.clone())
                .or_default()
                .push(*stream, &bytes);
            continue;
        }
        if envelope.render.prompt == PromptRender::Omit {
            continue;
        }
        match payload {
            EventPayload::UserMessage {
                text, attachments, ..
            } => {
                let mut blocks = vec![Block::Text { text }];
                blocks.extend(attachments.into_iter().map(Block::Attachment));
                messages.push(Message {
                    role: MessageRole::User,
                    blocks,
                });
                if is_current {
                    current_user_seen = true;
                    current_user_start.get_or_insert(messages.len().saturating_sub(1));
                }
            }
            EventPayload::PeerMessage(message) => {
                messages.push(Message::user_text(message.render_for_prompt()));
                if is_current {
                    current_user_seen = true;
                    current_user_start.get_or_insert(messages.len().saturating_sub(1));
                }
            }
            EventPayload::Item(ItemEvent::Completed { item_id, item }) if !is_current => match item
            {
                TurnItem::AgentMessage { text } => {
                    messages.push(Message::assistant(vec![Block::Text { text }]));
                }
                TurnItem::IncompleteAgentMessage { text, .. }
                    if facts.continued_partial_items.contains(&item_id) =>
                {
                    messages.push(Message::assistant(vec![Block::Text { text }]));
                }
                TurnItem::CommandExecution {
                    call_id,
                    command,
                    status,
                    exit_code,
                } if facts
                    .user_command_origins
                    .get(&item_id)
                    .is_some_and(|origin| origin.call_id == call_id) =>
                {
                    let output = user_command_outputs.remove(&item_id).unwrap_or_default();
                    let failed = status != haider_protocol::item::ToolStatus::Completed;
                    let (output_preview, output_truncated, output_lossy_utf8, output_bytes) =
                        output.finish(failed);
                    messages.push(Message::user_command(UserCommandRecord {
                        call_id,
                        command,
                        status,
                        exit_code,
                        output_preview,
                        output_bytes,
                        output_truncated,
                        output_lossy_utf8,
                    }));
                }
                TurnItem::ToolCall {
                    call_id,
                    name,
                    args,
                    status:
                        haider_protocol::item::ToolStatus::Completed
                        | haider_protocol::item::ToolStatus::Rejected
                        | haider_protocol::item::ToolStatus::Conflict
                        | haider_protocol::item::ToolStatus::Failed
                        | haider_protocol::item::ToolStatus::Unknown,
                } => {
                    let model_result = pending_tool_results.remove(&call_id).map(|result| {
                        let (preview, truncated) = model_tool_result_preview(&name, &result);
                        (result, preview, truncated)
                    });
                    messages.push(Message::assistant(vec![Block::ToolCall {
                        call_id: call_id.clone(),
                        name,
                        args,
                    }]));
                    if let Some((result, preview, truncated)) = model_result {
                        messages.push(Message::tool_result_with_images(
                            call_id,
                            preview,
                            truncated,
                            result.images,
                        ));
                    }
                }
                TurnItem::Extension { kind, data } if kind == PROVIDER_OPAQUE_EXTENSION_KIND => {
                    if let Some(block) = provider_opaque_extension(data) {
                        messages.push(Message::assistant(vec![block]));
                    }
                }
                _ => {}
            },
            EventPayload::ToolResult { call_id, result } if !is_current => {
                pending_tool_results.insert(call_id, result);
            }
            _ => {}
        }
    }
    if require_current_user && !current_user_seen {
        let current_run = current_run.map_or("<missing>", RunId::as_str);
        return Err(corrupt(format!(
            "accepted run {current_run} has no committed user message"
        )));
    }
    Ok(RenderedJournal {
        messages,
        current_user_seen,
        current_user_start,
    })
}

async fn read_all(
    store: &dyn StoreHandle,
    session_id: &SessionId,
) -> Result<Vec<RawEnvelope>, HaiderError> {
    let mut envelopes = Vec::new();
    let mut cursor = 0;
    loop {
        let page = store.read(session_id, cursor, HISTORY_PAGE).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        envelopes.extend(page);
    }
    Ok(envelopes)
}

fn legacy_journal_only(
    envelopes: &[RawEnvelope],
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
) -> bool {
    let mut has_scoped_node = false;
    let mut has_registry_fact = branch_id.is_none();
    for envelope in envelopes {
        if BranchCreated::from_payload_value(&envelope.payload)
            .is_some_and(|created| Some(created.branch.branch_id) == branch_id.cloned())
        {
            has_registry_fact = true;
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        match payload {
            EventPayload::NodeCommitted(_)
                if envelope.branch_id.as_ref() == branch_id
                    && envelope.agent_id.as_ref() == agent_id =>
            {
                has_scoped_node = true;
            }
            _ => {}
        }
    }
    !has_scoped_node && !has_registry_fact
}

fn scoped(
    envelope: &RawEnvelope,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
) -> bool {
    envelope.branch_id.as_ref() == branch_id && envelope.agent_id.as_ref() == agent_id
}

/// One bounded user-role prompt notice for a background task fact. Every
/// interpolated field is already bounded at journal time; the tail is
/// re-clamped here defensively so the notice can never outgrow the fact.
///
/// Public because the daemon's steer delivery reuses the SAME text: the
/// mid-turn injection and the next-turn prompt notice must never diverge.
pub fn task_event_notice(event: &TaskEventPayload) -> String {
    match event {
        TaskEventPayload::TaskStarted(started) => format!(
            "[background task started] {} ({}) — {}",
            started.name, started.task, started.command
        ),
        TaskEventPayload::TaskCompleted(completed) => {
            let disposition = match &completed.state {
                TaskTerminalState::Completed {
                    exit_code: Some(code),
                } => format!("exited with code {code}"),
                TaskTerminalState::Completed { exit_code: None } => {
                    "exited (ended by signal)".into()
                }
                TaskTerminalState::Failed { reason } => format!("failed: {reason}"),
                TaskTerminalState::Killed => "was killed".into(),
            };
            let truncation = if completed.full_output_unavailable {
                " (full output unavailable; bounded tail retained below)"
            } else if completed.truncated {
                " (truncated; full retained output in the task artifact)"
            } else {
                ""
            };
            let mut tail = completed.tail.as_str();
            if tail.len() > TASK_TAIL_BYTES {
                let mut end = TASK_TAIL_BYTES;
                while !tail.is_char_boundary(end) {
                    end -= 1;
                }
                tail = &tail[..end];
            }
            let tail_section = if tail.trim().is_empty() {
                String::new()
            } else {
                format!("\noutput tail:\n{tail}")
            };
            format!(
                "[background task finished] {} ({}) {} after {}s — {} output bytes{}{}",
                completed.name,
                completed.task,
                disposition,
                completed.elapsed_ms / 1000,
                completed.output_bytes,
                truncation,
                tail_section,
            )
        }
    }
}

fn provider_opaque_extension(data: serde_json::Value) -> Option<Block> {
    let object = data.as_object()?;
    let provider = object.get("provider")?.as_str()?.to_owned();
    let data = object.get("data")?.clone();
    Some(Block::ProviderOpaque { provider, data })
}

fn corrupt(message: impl Into<String>) -> HaiderError {
    HaiderError::new(ErrorCode::StoreCorrupt, message, false)
}

fn serialized_body_bytes(value: &(impl Serialize + ?Sized)) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod cache_bound_tests {
    use super::*;

    fn resident(head_seq: u64) -> CachedPromptSession {
        CachedPromptSession {
            head_seq,
            ..CachedPromptSession::default()
        }
    }

    #[tokio::test]
    async fn session_limit_evicts_the_least_recently_touched_entry() {
        let cache = PromptHistoryCache::default();
        for index in 0..PROMPT_CACHE_SESSION_LIMIT {
            cache
                .install(SessionId::new(format!("session-{index}")), resident(1))
                .await;
        }
        let touched = cache
            .sessions
            .lock()
            .await
            .remove(&SessionId::new("session-0"))
            .expect("oldest session remains resident before touch");
        cache.install(SessionId::new("session-0"), touched).await;
        cache
            .install(SessionId::new("session-new"), resident(1))
            .await;

        let sessions = cache.sessions.lock().await;
        assert!(sessions.contains_key(&SessionId::new("session-0")));
        assert!(!sessions.contains_key(&SessionId::new("session-1")));
        assert!(sessions.contains_key(&SessionId::new("session-new")));
    }

    #[tokio::test]
    async fn body_cap_retains_replay_cursors_while_dropping_lru_bodies() {
        let cache = PromptHistoryCache::default();
        let timeline = PromptTimelineKey {
            branch_id: None,
            agent_id: None,
        };
        let mut cached = resident(42);
        cached.retained_envelope_bytes = PROMPT_CACHE_RETAINED_BYTES_LIMIT.saturating_add(1);
        cached.compaction_epochs.insert(timeline.clone(), 17);
        cached.saved_boundaries.insert(timeline.clone(), 31);
        let run_id = RunId::new("oversized-run");
        let projection = Arc::new(CompiledPromptProjection {
            messages: vec![Message::user_text("cached body")],
            stable_history_end: 0,
            current_user_start: 0,
            latest_compaction_summary_end: None,
        });
        let projection_key = PromptProjectionKey {
            head_seq: 42,
            compaction_epoch: 17,
            branch_id: None,
            agent_id: None,
            current_run: run_id.clone(),
        };
        cached.projections.insert(
            projection_key.clone(),
            CachedExactProjection {
                projection: Some(Arc::clone(&projection)),
                body_bytes: serialized_body_bytes(&projection.messages),
                cursor: PromptProjectionCursor::Journal,
            },
        );
        let projection_scope = PromptProjectionScope {
            compaction_epoch: 17,
            branch_id: None,
            agent_id: None,
        };
        cached.append_prefixes.insert(
            projection_scope.clone(),
            CachedCompiledPrefix {
                head_seq: 42,
                current_run: run_id,
                current_run_terminal: true,
                projection: Some(projection),
                body_bytes: 1,
                cursor: PromptProjectionCursor::Tree {
                    head_node: NodeId::new("cached-head"),
                },
            },
        );
        cache
            .install(SessionId::new("oversized-session"), cached)
            .await;

        let sessions = cache.sessions.lock().await;
        let cached = sessions
            .get(&SessionId::new("oversized-session"))
            .expect("cursor shell remains resident");
        assert!(cached.bodies_evicted);
        assert_eq!(cached.retained_heap_bytes(), 0);
        assert_eq!(cached.head_seq, 42);
        assert_eq!(cached.compaction_epochs.get(&timeline), Some(&17));
        assert_eq!(cached.saved_boundaries.get(&timeline), Some(&31));
        let exact = cached
            .projections
            .get(&projection_key)
            .expect("projection hash and cursor remain resident");
        assert!(exact.projection.is_none());
        assert!(matches!(exact.cursor, PromptProjectionCursor::Journal));
        let prefix = cached
            .append_prefixes
            .get(&projection_scope)
            .expect("append hash and cursor remain resident");
        assert!(prefix.projection.is_none());
        assert!(matches!(prefix.cursor, PromptProjectionCursor::Tree { .. }));
    }
}
