//! Deterministic reconstruction of provider messages from the durable history
//! tree and its byte-preserving journal sidecars.

use crate::StoreHandle;
use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{PromptRender, RawEnvelope};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::history::{CompactionIntent, CompactionResume, NodeKind, TreeNode};
use haider_protocol::ids::{AgentId, ArtifactRef, BranchId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{Block, PROVIDER_OPAQUE_EXTENSION_KIND};
use haider_protocol::state::RunState;
use haider_protocol::tool::BoundedResult;
use haider_provider::{Message, MessageRole};
use std::collections::{HashMap, HashSet};

const HISTORY_PAGE: usize = 256;

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
    ) -> Result<Vec<Message>, HaiderError> {
        let envelopes = read_all(store, session_id).await?;
        let tree = TreeProjection::build(&envelopes, branch_id, agent_id)?;
        let Some(ancestry) = tree.ancestry_for_run(current_run)? else {
            // Upgrade compatibility: a session written before tree activation
            // keeps the exact journal projection until its first node exists.
            return render_journal(
                &envelopes,
                &envelopes,
                branch_id,
                agent_id,
                Some(current_run),
                true,
            )
            .map(|rendered| rendered.messages);
        };
        compile_ancestry(
            &envelopes,
            &ancestry,
            artifacts,
            branch_id,
            agent_id,
            Some(current_run),
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
        let tree = TreeProjection::build(&envelopes, branch_id, agent_id)?;
        let ancestry = tree.latest_ancestry()?.ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "there is no durable history to compact",
                false,
            )
        })?;
        compile_ancestry(
            &envelopes,
            &ancestry,
            Some(artifacts),
            branch_id,
            agent_id,
            None,
        )
        .await
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
        Ok(envelopes.iter().rev().find_map(|envelope| {
            if !scoped(envelope, branch_id, agent_id) {
                return None;
            }
            match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                Ok(EventPayload::NodeCommitted(node)) => Some(node.node),
                _ => None,
            }
        }))
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
        let tree = TreeProjection::build(&envelopes, branch_id, agent_id)?;
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
        let tree = TreeProjection::build(&envelopes, branch_id, agent_id)?;
        let ancestry = tree.latest_ancestry()?.ok_or_else(|| {
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

#[derive(Clone)]
struct TreeEntry {
    node: TreeNode,
    seq: u64,
    fragment_after: u64,
    run_id: Option<RunId>,
}

struct TreeProjection {
    ordered: Vec<TreeEntry>,
    by_id: HashMap<NodeId, usize>,
}

impl TreeProjection {
    fn build(
        envelopes: &[RawEnvelope],
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
    ) -> Result<Self, HaiderError> {
        let mut ordered = Vec::new();
        let mut by_id = HashMap::new();
        let mut previous_node_seq = 0;
        for envelope in envelopes {
            if !scoped(envelope, branch_id, agent_id) {
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
            ordered.push(TreeEntry {
                node,
                seq: envelope.seq,
                fragment_after: previous_node_seq,
                run_id: envelope.run_id.clone(),
            });
            previous_node_seq = envelope.seq;
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

    fn latest_ancestry(&self) -> Result<Option<Vec<TreeEntry>>, HaiderError> {
        let Some(index) = self.ordered.len().checked_sub(1) else {
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
}

async fn compile_ancestry(
    envelopes: &[RawEnvelope],
    ancestry: &[TreeEntry],
    artifacts: Option<&dyn ArtifactReader>,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
) -> Result<Vec<Message>, HaiderError> {
    let plan = ProjectionPlan::build(ancestry)?;
    let mut messages = Vec::new();
    let mut current_user_seen = false;
    let mut verbatim = Vec::new();

    for (index, entry) in ancestry.iter().enumerate() {
        if let Some(compaction) = plan.summary_at.get(&index) {
            flush_verbatim(
                &mut verbatim,
                envelopes,
                branch_id,
                agent_id,
                current_run,
                &mut current_user_seen,
                &mut messages,
            )?;
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
        }

        if plan.covered.contains(&index) || matches!(entry.node.kind, NodeKind::Compaction { .. }) {
            continue;
        }
        verbatim.extend(
            envelopes
                .iter()
                .filter(|envelope| {
                    scoped(envelope, branch_id, agent_id)
                        && envelope.seq > entry.fragment_after
                        && envelope.seq <= entry.seq
                })
                .cloned(),
        );
    }
    flush_verbatim(
        &mut verbatim,
        envelopes,
        branch_id,
        agent_id,
        current_run,
        &mut current_user_seen,
        &mut messages,
    )?;
    if let Some(current_run) = current_run
        && !current_user_seen
    {
        return Err(corrupt(format!(
            "accepted run {current_run} has no tree-selected committed user message"
        )));
    }
    Ok(messages)
}

fn flush_verbatim(
    verbatim: &mut Vec<RawEnvelope>,
    all_envelopes: &[RawEnvelope],
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    current_run: Option<&RunId>,
    current_user_seen: &mut bool,
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
    for envelope in all_envelopes {
        if !scoped(envelope, branch_id, agent_id) {
            continue;
        }
        let Some(run_id) = envelope.run_id.clone() else {
            continue;
        };
        if let Ok(EventPayload::RunState(state)) =
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
        {
            terminal.insert(run_id, state);
        }
    }

    let mut messages = Vec::new();
    let mut pending_tool_results = HashMap::<String, BoundedResult>::new();
    let mut current_user_seen = false;
    for envelope in selected {
        if !scoped(envelope, branch_id, agent_id) {
            continue;
        }
        let Some(run_id) = envelope.run_id.clone() else {
            continue;
        };
        let is_current = current_run.is_some_and(|current| run_id == *current);
        if !is_current
            && !terminal
                .get(&run_id)
                .is_some_and(|state| *state == RunState::Done)
        {
            continue;
        }
        if envelope.render.prompt == PromptRender::Omit {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        match payload {
            EventPayload::UserMessage {
                text, attachments, ..
            } if !is_current || !current_user_seen => {
                let mut blocks = vec![Block::Text { text }];
                blocks.extend(attachments.into_iter().map(Block::Attachment));
                messages.push(Message {
                    role: MessageRole::User,
                    blocks,
                });
                if is_current {
                    current_user_seen = true;
                }
            }
            EventPayload::Item(ItemEvent::Completed { item, .. }) if !is_current => match item {
                TurnItem::AgentMessage { text } => {
                    messages.push(Message::assistant(vec![Block::Text { text }]));
                }
                TurnItem::ToolCall {
                    call_id,
                    name,
                    args,
                    status: haider_protocol::item::ToolStatus::Completed,
                } => {
                    messages.push(Message::assistant(vec![Block::ToolCall {
                        call_id: call_id.clone(),
                        name,
                        args,
                    }]));
                    if let Some(result) = pending_tool_results.remove(&call_id) {
                        messages.push(Message::tool_result(
                            call_id,
                            result.preview,
                            result.truncated,
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

fn scoped(
    envelope: &RawEnvelope,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
) -> bool {
    envelope.branch_id.as_ref() == branch_id && envelope.agent_id.as_ref() == agent_id
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
