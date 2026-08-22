//! Deterministic reconstruction of provider messages from the durable history
//! tree and its byte-preserving journal sidecars.

use crate::{SessionProjectionCheckpoint, StoreHandle};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_protocol::EventPayload;
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::envelope::{PromptRender, RawEnvelope};
use haider_protocol::error::{ErrorAction, ErrorCode, HaiderError};
use haider_protocol::history::{CompactionIntent, CompactionResume, NodeKind, TreeNode};
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, EventId, MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, TurnItem, UserCommandOriginV1};
use haider_protocol::menu::{ErrorRecoveryCardKind, MenuKind};
use haider_protocol::pipe::TranscriptProjector;
use haider_protocol::provider::{Block, PROVIDER_OPAQUE_EXTENSION_KIND};
use haider_protocol::state::RunState;
use haider_protocol::task::{TASK_TAIL_BYTES, TaskEventPayload, TaskTerminalState};
use haider_protocol::tool::BoundedResult;
use haider_provider::{Message, MessageRole, UserCommandRecord};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex;

const HISTORY_PAGE: usize = 256;
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

/// Daemon-lifetime prompt cache.
///
/// Durable journal bytes remain the only authority: the cache first samples
/// the session head, reads only the missing suffix, and keys a compiled
/// projection by that head plus its compaction epoch and complete branch,
/// agent, and current-run scope. On restart it may seed one exact timeline
/// from a validated terminal compaction-boundary checkpoint; any absent or
/// unreadable checkpoint falls back to the same journal fold from zero.
#[derive(Default)]
pub struct PromptHistoryCache {
    sessions: Mutex<HashMap<SessionId, CachedPromptSession>>,
}

#[derive(Default)]
struct CachedPromptSession {
    head_seq: u64,
    compaction_epochs: HashMap<PromptTimelineKey, u64>,
    envelopes: Vec<RawEnvelope>,
    projections: HashMap<PromptProjectionKey, CompiledPromptProjection>,
    append_prefixes: HashMap<PromptProjectionScope, CachedCompiledPrefix>,
    checkpoint_base: Option<PromptCheckpointBase>,
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
    projection: CompiledPromptProjection,
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
        let tree = TreeProjection::build(&envelopes, &lineage, agent_id)?;
        let ancestry = tree
            .latest_ancestry(lineage.head.as_ref())?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "there is no durable history to compact",
                    false,
                )
            })?;
        let tree_head_seq = ancestry.last().map_or(0, |entry| entry.seq);
        let mut projection =
            compile_ancestry(&envelopes, &ancestry, Some(artifacts), agent_id, None).await?;
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
        let tree = TreeProjection::build(&envelopes, &lineage, agent_id)?;
        let ancestry = tree
            .latest_ancestry(lineage.head.as_ref())?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "there is no durable history to compact",
                    false,
                )
            })?;
        let tree_head_seq = ancestry.last().map_or(0, |entry| entry.seq);
        let mut projection =
            compile_ancestry(&envelopes, &ancestry, Some(artifacts), agent_id, None).await?;
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
        let tree = TreeProjection::build(&envelopes, &lineage, agent_id)?;
        Ok(tree
            .latest_ancestry(lineage.head.as_ref())?
            .and_then(|ancestry| ancestry.last().map(|entry| entry.node.node.clone())))
    }

    /// Plans the largest safe prefix preceding `current_run`. The caller must
    /// durably append the returned intent before private summarization.
    pub async fn plan_compaction(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
        operation_id: String,
        resume_cause: CompactionResume,
    ) -> Result<CompactionIntent, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes, &lineage, agent_id)?;
        let ancestry = tree.ancestry_for_run(current_run)?.ok_or_else(|| {
            corrupt(format!(
                "cannot compact run {current_run} without a durable tree head"
            ))
        })?;
        let current_start = ancestry
            .iter()
            .position(|entry| entry.run_id.as_ref() == Some(current_run))
            .ok_or_else(|| corrupt(format!("tree ancestry omits current run {current_run}")))?;
        let covers_to = current_start.checked_sub(1).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "there is no prior history prefix to compact",
                false,
            )
        })?;
        Ok(CompactionIntent {
            operation_id,
            covers_from: ancestry[0].node.node.clone(),
            covers_to: ancestry[covers_to].node.node.clone(),
            resume_cause,
        })
    }

    /// Plans an idle compaction over the complete active ancestry.
    pub async fn plan_idle_compaction(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        operation_id: String,
    ) -> Result<CompactionIntent, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let lineage = ResolvedLineage::load(store, session_id, branch_id).await?;
        let tree = TreeProjection::build(&envelopes, &lineage, agent_id)?;
        let ancestry = tree
            .latest_ancestry(lineage.head.as_ref())?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "there is no durable history to compact",
                    false,
                )
            })?;
        Ok(CompactionIntent {
            operation_id,
            covers_from: ancestry[0].node.node.clone(),
            covers_to: ancestry
                .last()
                .map(|entry| entry.node.node.clone())
                .ok_or_else(|| corrupt("history ancestry unexpectedly empty"))?,
            resume_cause: CompactionResume::ManualIdle,
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
            });
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
                if serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::NodeCommitted(TreeNode {
                                kind: NodeKind::Compaction { .. },
                                ..
                            })
                        )
                    },
                ) {
                    let affects_checkpoint_timeline = envelope.branch_id == timeline.branch_id
                        && envelope.agent_id == timeline.agent_id;
                    cached.note_compaction(&envelope);
                    compaction_after_checkpoint |=
                        cached.checkpoint_base.is_some() && affects_checkpoint_timeline;
                }
                cached.push_envelope(envelope);
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
        if compaction_after_checkpoint {
            cached = CachedPromptSession::default();
            let mut cursor = 0;
            while cursor < head_seq {
                let page = store.read(session_id, cursor, HISTORY_PAGE).await?;
                let before = cached.envelopes.len();
                for envelope in page {
                    if envelope.seq > head_seq {
                        break;
                    }
                    cursor = envelope.seq;
                    if serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                        |payload| {
                            matches!(
                                payload,
                                EventPayload::NodeCommitted(TreeNode {
                                    kind: NodeKind::Compaction { .. },
                                    ..
                                })
                            )
                        },
                    ) {
                        cached.note_compaction(&envelope);
                    }
                    cached.push_envelope(envelope);
                }
                if cached.envelopes.len() == before {
                    return Err(corrupt(format!(
                        "prompt cache could not read durable head {head_seq} after sequence {cursor}"
                    )));
                }
            }
            cached.flush_boundary_rows();
        }
        if cached.head_seq < head_seq {
            cached.head_seq = head_seq;
            cached.projections.clear();
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
            }
        }

        let compaction_epoch = cached.compaction_epoch(&timeline);
        let scope = PromptProjectionScope {
            compaction_epoch,
            branch_id: branch_id.cloned(),
            agent_id: agent_id.cloned(),
        };
        if cached.checkpoint_base.is_some()
            && cached.append_prefixes.get(&scope).is_none_or(|prefix| {
                prefix.head_seq >= head_seq || prefix.current_run == *current_run
            })
        {
            // A decoded checkpoint that cannot enter the existing suffix
            // extension seam proves nothing; rebuild with the oracle.
            cached = CachedPromptSession::default();
            let mut cursor = 0;
            while cursor < head_seq {
                let page = store.read(session_id, cursor, HISTORY_PAGE).await?;
                let before = cached.envelopes.len();
                for envelope in page {
                    if envelope.seq > head_seq {
                        break;
                    }
                    cursor = envelope.seq;
                    if serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                        |payload| {
                            matches!(
                                payload,
                                EventPayload::NodeCommitted(TreeNode {
                                    kind: NodeKind::Compaction { .. },
                                    ..
                                })
                            )
                        },
                    ) {
                        cached.note_compaction(&envelope);
                    }
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
        if let Some(projection) = cached.projections.get(&key).cloned() {
            self.install(session_id.clone(), cached).await;
            return Ok(projection);
        }
        let projection = match cached.append_prefixes.get(&scope) {
            Some(prefix) if prefix.head_seq < head_seq && prefix.current_run != *current_run => {
                extend_compiled_projection(
                    prefix,
                    &cached.envelopes,
                    branch_id,
                    agent_id,
                    current_run,
                )?
            }
            _ => {
                compile_projection_from_envelopes(
                    store,
                    Some(artifacts),
                    session_id,
                    branch_id,
                    agent_id,
                    current_run,
                    &cached.envelopes,
                )
                .await?
            }
        };
        // Keep the earliest request boundary for a live run. Later tool-round
        // recompiles intentionally suppress that run's assistant/tool output;
        // replacing this prefix at their later head would make those facts
        // fall before the next run's suffix and disappear permanently.
        if cached
            .append_prefixes
            .get(&scope)
            .is_none_or(|prefix| prefix.current_run != *current_run)
        {
            cached.append_prefixes.insert(
                scope,
                CachedCompiledPrefix {
                    head_seq,
                    current_run: current_run.clone(),
                    projection: projection.clone(),
                },
            );
        }
        cached.projections.insert(key, projection.clone());

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
        Ok(projection)
    }

    async fn install(&self, session_id: SessionId, cached: CachedPromptSession) {
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(&session_id)
            .is_none_or(|current| current.head_seq <= cached.head_seq)
        {
            sessions.insert(session_id, cached);
        }
    }
}

impl CachedPromptSession {
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

    fn push_envelope(&mut self, envelope: RawEnvelope) {
        let rows = self.boundary_projector.push(&envelope);
        self.envelopes.push(envelope);
        self.note_boundary_rows(rows);
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
    let decoded = match serde_json::from_slice::<DurablePromptCheckpoint>(&stored.payload) {
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
        prefix: CachedCompiledPrefix {
            head_seq: decoded.through_seq,
            current_run: decoded.boundary_run_id,
            projection: CompiledPromptProjection {
                messages,
                stable_history_end: decoded.stable_history_end,
                current_user_start: decoded.current_user_start,
                latest_compaction_summary_end: decoded.latest_compaction_summary_end,
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
    let compaction_epoch = prefix
        .iter()
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

/// Extends a completed request projection with only the journal suffix that
/// accumulated before the next accepted run. The old request (including its
/// user message) is now immutable history; rendering the suffix produces the
/// preceding assistant/tool results followed by the new current user. A
/// compaction epoch change never reaches this function and recompiles through
/// the full oracle instead.
fn extend_compiled_projection(
    prefix: &CachedCompiledPrefix,
    envelopes: &[RawEnvelope],
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: &RunId,
) -> Result<CompiledPromptProjection, HaiderError> {
    let suffix = envelopes
        .iter()
        .filter(|envelope| envelope.seq > prefix.head_seq)
        .cloned()
        .collect::<Vec<_>>();
    let rendered = render_journal(
        &suffix,
        envelopes,
        branch_id,
        agent_id,
        Some(current_run),
        true,
    )?;
    let mut messages = prefix.projection.messages.clone();
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
    Ok(CompiledPromptProjection {
        stable_history_end: current_user_start,
        current_user_start,
        latest_compaction_summary_end: prefix.projection.latest_compaction_summary_end,
        messages,
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
    let tree = TreeProjection::build(envelopes, &lineage, agent_id)?;
    let ancestry = tree
        .latest_ancestry(lineage.head.as_ref())?
        .ok_or_else(|| corrupt("compaction boundary has no durable history ancestry"))?;
    let tree_head_seq = ancestry.last().map_or(0, |entry| entry.seq);
    let mut projection =
        compile_ancestry(envelopes, &ancestry, Some(artifacts), agent_id, None).await?;
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
    let tree = TreeProjection::build(envelopes, &lineage, agent_id)?;
    let Some(ancestry) = tree.ancestry_for_run(current_run)? else {
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
    compile_ancestry(envelopes, &ancestry, artifacts, agent_id, Some(current_run)).await
}

struct LineageScope {
    branch_id: Option<BranchId>,
    through_seq: u64,
}

struct ResolvedLineage {
    scopes: Vec<LineageScope>,
    head: Option<(NodeId, u64)>,
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
                scopes: vec![LineageScope {
                    branch_id: None,
                    through_seq: u64::MAX,
                }],
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
        let mut scopes = Vec::with_capacity(descriptors.len() + 1);
        let mut ceiling = u64::MAX;
        let mut concrete = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors.iter().rev() {
            concrete.push(LineageScope {
                branch_id: Some(descriptor.branch_id.clone()),
                through_seq: ceiling,
            });
            ceiling = ceiling.min(descriptor.fork_seq);
        }
        scopes.push(LineageScope {
            branch_id: None,
            through_seq: ceiling,
        });
        concrete.reverse();
        scopes.extend(concrete);
        Ok(Self {
            scopes,
            head: Some((leaf.head_node_id.clone(), leaf.head_seq)),
        })
    }

    fn admits(&self, branch_id: Option<&BranchId>, seq: u64) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.branch_id.as_ref() == branch_id && seq <= scope.through_seq)
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

struct TreeProjection {
    ordered: Vec<TreeEntry>,
    by_id: HashMap<NodeId, usize>,
}

impl TreeProjection {
    fn build(
        envelopes: &[RawEnvelope],
        lineage: &ResolvedLineage,
        agent_id: Option<&AgentId>,
    ) -> Result<Self, HaiderError> {
        let mut ordered = Vec::new();
        let mut by_id = HashMap::new();
        let mut previous_node_seq = HashMap::<Option<BranchId>, u64>::new();
        for envelope in envelopes {
            if envelope.agent_id.as_ref() != agent_id
                || !lineage.admits(envelope.branch_id.as_ref(), envelope.seq)
            {
                continue;
            }
            let Ok(EventPayload::NodeCommitted(node)) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            if by_id.contains_key(&node.node) {
                return Err(corrupt(format!(
                    "history tree contains duplicate node {}",
                    node.node
                )));
            }
            let index = ordered.len();
            by_id.insert(node.node.clone(), index);
            let owner_branch = envelope.branch_id.clone();
            let fragment_after = previous_node_seq.get(&owner_branch).copied().unwrap_or(0);
            ordered.push(TreeEntry {
                node,
                seq: envelope.seq,
                fragment_after,
                run_id: envelope.run_id.clone(),
                owner_branch: owner_branch.clone(),
            });
            previous_node_seq.insert(owner_branch, envelope.seq);
        }
        Ok(Self { ordered, by_id })
    }

    fn ancestry_for_run(&self, run_id: &RunId) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        let Some(index) = self
            .ordered
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| (entry.run_id.as_ref() == Some(run_id)).then_some(index))
        else {
            return Ok(None);
        };
        self.ancestry_from(index).map(Some)
    }

    fn latest_ancestry(
        &self,
        head: Option<&(NodeId, u64)>,
    ) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        let index = if let Some((head_node, head_seq)) = head {
            let index = *self.by_id.get(head_node).ok_or_else(|| {
                corrupt(format!("branch head references missing node {head_node}"))
            })?;
            if self.ordered[index].seq != *head_seq {
                return Err(corrupt(format!(
                    "branch head node {head_node} disagrees with sequence {head_seq}"
                )));
            }
            index
        } else if let Some(index) = self.ordered.len().checked_sub(1) {
            index
        } else {
            return Ok(None);
        };
        self.ancestry_from(index).map(Some)
    }

    fn ancestry_from(&self, mut index: usize) -> Result<Vec<TreeEntry>, HaiderError> {
        let mut reverse = Vec::new();
        let mut visited = HashSet::new();
        loop {
            let entry = self.ordered[index].clone();
            if !visited.insert(entry.node.node.clone()) {
                return Err(corrupt(format!(
                    "history tree contains a cycle at node {}",
                    entry.node.node
                )));
            }
            let parent = entry.node.parent.clone();
            reverse.push(entry);
            let Some(parent) = parent else {
                break;
            };
            index = *self.by_id.get(&parent).ok_or_else(|| {
                corrupt(format!("history tree references missing parent {parent}"))
            })?;
        }
        reverse.reverse();
        Ok(reverse)
    }
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

    fn finish(self) -> (String, bool, bool, u64) {
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
        (preview, truncated, lossy_utf8, self.total_bytes)
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
    artifacts: Option<&dyn ArtifactReader>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
) -> Result<CompiledPromptProjection, HaiderError> {
    let plan = ProjectionPlan::build(ancestry)?;
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
                    envelopes,
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
                    envelopes,
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
            envelopes,
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
    Ok(CompiledPromptProjection {
        stable_history_end: current_user_start,
        current_user_start,
        latest_compaction_summary_end,
        messages,
    })
}

#[allow(clippy::too_many_arguments)]
fn flush_verbatim(
    verbatim: &mut Vec<RawEnvelope>,
    all_envelopes: &[RawEnvelope],
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
    let rendered = render_journal(
        verbatim,
        all_envelopes,
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

fn render_journal(
    selected: &[RawEnvelope],
    all_envelopes: &[RawEnvelope],
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
    require_current_user: bool,
) -> Result<RenderedJournal, HaiderError> {
    let mut terminal = HashMap::<RunId, RunState>::new();
    let mut partial_menus = HashMap::<MenuId, (haider_protocol::ids::ItemId, String, u32)>::new();
    let mut continued_partial_items = HashSet::new();
    let mut user_command_origins =
        HashMap::<haider_protocol::ids::ItemId, UserCommandOriginV1>::new();
    for envelope in all_envelopes {
        if !scoped(envelope, branch_id, agent_id) {
            continue;
        }
        let Some(run_id) = envelope.run_id.clone() else {
            continue;
        };
        if let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
            match payload {
                EventPayload::RunState(state) => {
                    terminal.insert(run_id, state);
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
                        partial_menus.insert(
                            menu.id,
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
                        partial_menus.get(&answer.menu)
                        && answer
                            .option_key
                            .as_deref()
                            .map_or(answer.option_index == *continue_index, |key| {
                                key == continue_key
                            })
                    {
                        continued_partial_items.insert(item_id.clone());
                    }
                }
                EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                    let origin =
                        UserCommandOriginV1::try_from_extension_item(&item).map_err(|error| {
                            corrupt(format!("malformed user-command origin marker: {error}"))
                        })?;
                    if let Some(origin) = origin
                        && user_command_origins
                            .insert(origin.command_item_id.clone(), origin)
                            .is_some()
                    {
                        return Err(corrupt("duplicate user-command origin marker"));
                    }
                }
                _ => {}
            }
        }
    }

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
        let prior_state = terminal.get(&run_id);
        // Invariant since the terminal-history flip (f53cb2c): every run that
        // was previously visible only through the narrow terminal
        // user-command lane (`!is_current && prior_state is terminal`) is now
        // ordinarily visible — `ordinary_visible` is a strict superset of the
        // retired `terminal_user_command_visible`, so it alone gates below.
        let ordinary_visible = is_current || prior_state.is_some_and(RunState::is_terminal);
        if !ordinary_visible {
            continue;
        }
        if envelope.render.prompt == PromptRender::Omit {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        if let EventPayload::Item(ItemEvent::Delta {
            item_id,
            delta: ItemDelta::CommandOutput { stream, chunk_b64 },
        }) = &payload
            && user_command_origins.contains_key(item_id)
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
            EventPayload::Item(ItemEvent::Completed { item_id, item }) if !is_current => match item
            {
                TurnItem::AgentMessage { text } => {
                    messages.push(Message::assistant(vec![Block::Text { text }]));
                }
                TurnItem::IncompleteAgentMessage { text, .. }
                    if continued_partial_items.contains(&item_id) =>
                {
                    messages.push(Message::assistant(vec![Block::Text { text }]));
                }
                TurnItem::CommandExecution {
                    call_id,
                    command,
                    status,
                    exit_code,
                } if user_command_origins
                    .get(&item_id)
                    .is_some_and(|origin| origin.call_id == call_id) =>
                {
                    let output = user_command_outputs.remove(&item_id).unwrap_or_default();
                    let (output_preview, output_truncated, output_lossy_utf8, output_bytes) =
                        output.finish();
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
                    messages.push(Message::assistant(vec![Block::ToolCall {
                        call_id: call_id.clone(),
                        name,
                        args,
                    }]));
                    if let Some(result) = pending_tool_results.remove(&call_id) {
                        messages.push(Message::tool_result_with_images(
                            call_id,
                            result.preview,
                            result.truncated,
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
