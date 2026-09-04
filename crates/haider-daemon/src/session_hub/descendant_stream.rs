//! Reconnectable descendant-tree attachment.
//!
//! Durable delegation rows are the sole lineage authority; each child
//! journal independently owns its raw replay cursor and run-state truth. The
//! stream keeps those domains separate and never assigns one global sequence
//! to the tree.

use super::*;
use haider_protocol::ids::AgentId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_rpc::{
    DescendantChangeKindWire, DescendantFanoutWire, DescendantParentAnchorsWire,
    DescendantReplayCursorWire, DescendantStreamNodeWire, DescendantTruncationWire,
    SessionDescendantBaselineWire, SessionDescendantStreamEventWire,
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{MissedTickBehavior, interval};

const DESCENDANT_RECONCILE_PERIOD: Duration = Duration::from_millis(250);

pub(super) enum PrepareDescendantStreamError {
    Hub(SessionHubError),
    Invalid(String),
    CursorAhead {
        cursor: DescendantReplayCursorWire,
        head: u64,
    },
}

impl From<SessionHubError> for PrepareDescendantStreamError {
    fn from(error: SessionHubError) -> Self {
        Self::Hub(error)
    }
}

impl From<HaiderError> for PrepareDescendantStreamError {
    fn from(error: HaiderError) -> Self {
        Self::Hub(SessionHubError::Store(error))
    }
}

pub(super) struct PreparedDescendantStream {
    pub(super) baseline: SessionDescendantBaselineWire,
    root_session_id: SessionId,
    accepted_children: usize,
    children: Vec<TrackedChild>,
    truncation: DescendantTruncationWire,
    lineage_publications: watch::Receiver<u64>,
    roster_publications: broadcast::Receiver<SessionId>,
}

impl PreparedDescendantStream {
    pub(super) fn streamed_session_ids(&self) -> HashSet<SessionId> {
        self.children
            .iter()
            .map(|child| child.record.child_session_id.clone())
            .collect()
    }

    pub(super) fn repair_identities(&self) -> Vec<haider_rpc::DescendantIdentityWire> {
        self.children
            .iter()
            .map(|child| haider_rpc::DescendantIdentityWire {
                session_id: child.record.child_session_id.clone(),
                agent_id: child.record.agent_id.clone(),
            })
            .collect()
    }

    fn watches_session(&self, session_id: &SessionId) -> bool {
        self.root_session_id == *session_id
            || self.children.iter().any(|child| {
                child.record.child_session_id == *session_id
                    || child.record.parent_session_id == *session_id
            })
    }
}

#[derive(Clone)]
struct TrackedChild {
    record: haider_core::DelegationRecord,
    wire: DescendantStreamNodeWire,
    cursor: u64,
    parent_head: u64,
}

pub(super) async fn prepare_descendant_stream(
    hub: &SessionHub,
    root_session_id: SessionId,
    cursors: Vec<DescendantReplayCursorWire>,
    requested_children: u32,
) -> Result<PreparedDescendantStream, PrepareDescendantStreamError> {
    if requested_children == 0 {
        return Err(PrepareDescendantStreamError::Invalid(
            "session.descendants.attach max_children must be greater than zero".into(),
        ));
    }
    if cursors.len() > haider_rpc::FLEET_MAX_NODES as usize {
        return Err(PrepareDescendantStreamError::Invalid(format!(
            "session.descendants.attach accepts at most {} child cursors",
            haider_rpc::FLEET_MAX_NODES
        )));
    }

    // Subscribe before the baseline reads. A lineage mutation or child
    // commit racing those reads is therefore queued for the first reconcile.
    let lineage_publications = hub.inner.descendant_lineage_publications.subscribe();
    let roster_publications = hub.inner.roster_publications.subscribe();
    let accepted_children = negotiated_child_limit(requested_children);
    let descendants = hub
        .delegation_descendants(
            root_session_id.clone(),
            haider_rpc::FLEET_MAX_NODES as usize,
            haider_rpc::FLEET_MAX_DEPTH,
        )
        .await?;

    let mut requested = HashMap::<(SessionId, AgentId), u64>::new();
    for cursor in &cursors {
        if requested
            .insert(
                (cursor.session_id.clone(), cursor.agent_id.clone()),
                cursor.after_seq,
            )
            .is_some()
        {
            return Err(PrepareDescendantStreamError::Invalid(format!(
                "duplicate descendant cursor for session {} and agent {}",
                cursor.session_id, cursor.agent_id
            )));
        }
    }
    let mut selected = Vec::<haider_core::DelegationRecord>::new();
    for cursor in &cursors {
        let Some(chain) =
            descendant_chain_to_root(hub, &root_session_id, &cursor.session_id, &cursor.agent_id)
                .await?
        else {
            return Err(PrepareDescendantStreamError::Invalid(format!(
                "descendant cursor for session {} and agent {} is not durable lineage beneath the requested root",
                cursor.session_id, cursor.agent_id
            )));
        };
        for record in chain {
            if !selected.iter().any(|selected| {
                selected.child_session_id == record.child_session_id
                    && selected.agent_id == record.agent_id
            }) {
                selected.push(record);
            }
        }
    }
    if selected.len() > accepted_children {
        return Err(PrepareDescendantStreamError::Invalid(
            "session.descendants.attach max_children is too small to preserve the requested cursor ancestry"
                .into(),
        ));
    }
    for descendant in &descendants.descendants {
        if selected.len() >= accepted_children {
            break;
        }
        if !selected.iter().any(|selected| {
            selected.child_session_id == descendant.record.child_session_id
                && selected.agent_id == descendant.record.agent_id
        }) {
            selected.push(descendant.record.clone());
        }
    }

    let mut children = Vec::with_capacity(selected.len());
    for record in selected {
        let after_seq = requested
            .get(&(record.child_session_id.clone(), record.agent_id.clone()))
            .copied()
            .unwrap_or(0);
        let tracked = hydrate_child(hub, record, after_seq).await?;
        if after_seq > tracked.wire.replay_through_seq {
            return Err(PrepareDescendantStreamError::CursorAhead {
                cursor: DescendantReplayCursorWire {
                    session_id: tracked.wire.session_id,
                    agent_id: tracked.wire.agent_id,
                    after_seq,
                },
                head: tracked.wire.replay_through_seq,
            });
        }
        children.push(tracked);
    }

    let truncation = descendant_truncation(&descendants, &children);
    let baseline = SessionDescendantBaselineWire {
        session_id: root_session_id.clone(),
        generated_at_ms: descendant_now_ms(),
        fanout: DescendantFanoutWire {
            requested_children,
            accepted_children: u32::try_from(accepted_children).unwrap_or(u32::MAX),
            hard_limit: haider_rpc::DESCENDANT_STREAM_MAX_CHILDREN,
        },
        truncation: truncation.clone(),
        roots: nested_nodes(&root_session_id, &children)?,
    };
    Ok(PreparedDescendantStream {
        baseline,
        root_session_id,
        accepted_children,
        children,
        truncation,
        lineage_publications,
        roster_publications,
    })
}

fn negotiated_child_limit(requested_children: u32) -> usize {
    requested_children.min(haider_rpc::DESCENDANT_STREAM_MAX_CHILDREN) as usize
}

fn descendant_truncation(
    descendants: &haider_core::DelegationDescendants,
    streamed: &[TrackedChild],
) -> DescendantTruncationWire {
    let observed_streamed = descendants
        .descendants
        .iter()
        .filter(|descendant| {
            streamed.iter().any(|child| {
                child.record.child_session_id == descendant.record.child_session_id
                    && child.record.agent_id == descendant.record.agent_id
            })
        })
        .count();
    let known_omitted = known_omitted_children(
        descendants.descendants.len(),
        observed_streamed,
        streamed.len(),
        descendants.truncated,
    );
    DescendantTruncationWire {
        truncated: known_omitted != 0,
        streamed_children: u32::try_from(streamed.len()).unwrap_or(u32::MAX),
        omitted_children: u32::try_from(known_omitted).unwrap_or(u32::MAX),
        count_complete: !descendants.truncated,
    }
}

fn known_omitted_children(
    observed_children: usize,
    observed_streamed: usize,
    streamed_children: usize,
    scan_truncated: bool,
) -> usize {
    let unstreamed_observed = observed_children.saturating_sub(observed_streamed);
    let omitted_witness = usize::from(scan_truncated && streamed_children == observed_streamed);
    unstreamed_observed.saturating_add(omitted_witness)
}

async fn descendant_chain_to_root(
    hub: &SessionHub,
    root_session_id: &SessionId,
    child_session_id: &SessionId,
    agent_id: &AgentId,
) -> Result<Option<Vec<haider_core::DelegationRecord>>, SessionHubError> {
    let Some(record) = hub.delegation(agent_id.clone()).await? else {
        return Ok(None);
    };
    if record.child_session_id != *child_session_id {
        return Ok(None);
    }
    let mut seen = HashSet::new();
    seen.insert(record.child_session_id.clone());
    let mut parent_session_id = record.parent_session_id.clone();
    let mut reverse_chain = vec![record];
    for _ in 0..haider_rpc::FLEET_MAX_DEPTH {
        if parent_session_id == *root_session_id {
            reverse_chain.reverse();
            return Ok(Some(reverse_chain));
        }
        if !seen.insert(parent_session_id.clone()) {
            return Ok(None);
        }
        let Some(parent) = hub
            .delegation_for_child_session(parent_session_id.clone())
            .await?
        else {
            return Ok(None);
        };
        parent_session_id = parent.parent_session_id.clone();
        reverse_chain.push(parent);
    }
    Ok(None)
}

fn descendant_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn hydrate_child(
    hub: &SessionHub,
    record: haider_core::DelegationRecord,
    after_seq: u64,
) -> Result<TrackedChild, SessionHubError> {
    let head = hub.inner.store.latest_seq(&record.child_session_id).await?;
    let state = descendant_child_state(&hub.inner.store, &record, head).await?;
    let parent_head = hub
        .inner
        .store
        .latest_seq(&record.parent_session_id)
        .await?;
    let parent_anchors = descendant_parent_anchors(
        &hub.inner.store,
        &record,
        0,
        parent_head,
        DescendantParentAnchorsWire::default(),
    )
    .await?;
    Ok(TrackedChild {
        wire: child_wire(&record, state, after_seq, head, parent_anchors),
        record,
        cursor: after_seq,
        parent_head,
    })
}

fn child_wire(
    record: &haider_core::DelegationRecord,
    state: haider_rpc::FleetAgentStateWire,
    requested_after_seq: u64,
    replay_through_seq: u64,
    parent_anchors: DescendantParentAnchorsWire,
) -> DescendantStreamNodeWire {
    let identity = fleet_node_identity(&record.manifest);
    DescendantStreamNodeWire {
        session_id: record.child_session_id.clone(),
        agent_id: record.agent_id.clone(),
        child_run_id: record.child_run_id.clone(),
        parent_session_id: record.parent_session_id.clone(),
        parent_run_id: record.parent_run_id.clone(),
        parent_branch_id: record.parent_branch_id.clone(),
        parent_agent_id: record.parent_agent_id.clone(),
        depth: record.depth,
        callsign: identity.callsign,
        model: identity.model,
        provider: identity.provider,
        task: record.task.clone(),
        state,
        requested_after_seq,
        replay_through_seq,
        parent_anchors,
        children: Vec::new(),
    }
}

fn nested_nodes(
    root_session_id: &SessionId,
    children: &[TrackedChild],
) -> Result<Vec<DescendantStreamNodeWire>, SessionHubError> {
    let mut children_by_parent = HashMap::<SessionId, Vec<usize>>::new();
    let mut nodes = children
        .iter()
        .enumerate()
        .map(|(index, child)| {
            children_by_parent
                .entry(child.record.parent_session_id.clone())
                .or_default()
                .push(index);
            Some(child.wire.clone())
        })
        .collect::<Vec<_>>();

    fn take_node(
        index: usize,
        nodes: &mut [Option<DescendantStreamNodeWire>],
        children_by_parent: &HashMap<SessionId, Vec<usize>>,
    ) -> Result<DescendantStreamNodeWire, SessionHubError> {
        let mut node = nodes.get_mut(index).and_then(Option::take).ok_or_else(|| {
            SessionHubError::Store(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "descendant stream graph contains a duplicate node",
                false,
            ))
        })?;
        if let Some(indices) = children_by_parent.get(&node.session_id) {
            for child in indices {
                node.children
                    .push(take_node(*child, nodes, children_by_parent)?);
            }
        }
        Ok(node)
    }

    let roots = children_by_parent
        .get(root_session_id)
        .cloned()
        .unwrap_or_default();
    let mut nested = Vec::with_capacity(roots.len());
    for root in roots {
        nested.push(take_node(root, &mut nodes, &children_by_parent)?);
    }
    Ok(nested)
}

async fn descendant_child_state(
    store: &dyn StoreHandle,
    record: &haider_core::DelegationRecord,
    through_seq: u64,
) -> Result<haider_rpc::FleetAgentStateWire, HaiderError> {
    let mut latest_state = None;
    let mut cursor = 0;
    while cursor < through_seq {
        let page = store
            .read(&record.child_session_id, cursor, REPLAY_PAGE_SIZE)
            .await?;
        if page.is_empty() {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "child journal {} ended before sealed head {through_seq}",
                    record.child_session_id
                ),
                false,
            ));
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                break;
            }
            if envelope.run_id.as_ref() == Some(&record.child_run_id)
                && let Ok(EventPayload::RunState(state)) = envelope.payload.decode_event()
            {
                latest_state = Some(state);
            }
            cursor = envelope.seq;
            advanced = true;
        }
        if !advanced {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "child journal {} did not advance toward sealed head {through_seq}",
                    record.child_session_id
                ),
                false,
            ));
        }
    }
    Ok(descendant_agent_state(record, latest_state.as_ref()))
}

fn descendant_agent_state(
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

async fn descendant_parent_anchors(
    store: &dyn StoreHandle,
    record: &haider_core::DelegationRecord,
    after_seq: u64,
    through_seq: u64,
    mut anchors: DescendantParentAnchorsWire,
) -> Result<DescendantParentAnchorsWire, HaiderError> {
    let mut cursor = after_seq;
    while cursor < through_seq {
        let page = store
            .read(&record.parent_session_id, cursor, REPLAY_PAGE_SIZE)
            .await?;
        if page.is_empty() {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "parent journal {} ended before sealed head {through_seq}",
                    record.parent_session_id
                ),
                false,
            ));
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > through_seq {
                break;
            }
            let same_parent_turn = envelope.run_id.as_ref() == Some(&record.parent_run_id)
                && envelope.branch_id.as_ref() == record.parent_branch_id.as_ref();
            if let Ok(payload) = envelope.payload.decode_event() {
                match payload {
                    EventPayload::AgentSpawned(manifest)
                        if same_parent_turn
                            && manifest.agent == record.agent_id
                            && anchors.spawn_seq.is_none() =>
                    {
                        anchors.spawn_seq = Some(envelope.seq);
                    }
                    EventPayload::AgentReport(report)
                        if same_parent_turn
                            && report.agent == record.agent_id
                            && anchors.result_seq.is_none() =>
                    {
                        anchors.result_seq = Some(envelope.seq);
                    }
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::ChildSpawn { agent },
                        ..
                    }) if same_parent_turn
                        && agent == record.agent_id
                        && anchors.spawn_item_seq.is_none() =>
                    {
                        anchors.spawn_item_seq = Some(envelope.seq);
                    }
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::ChildResult { report },
                        ..
                    }) if same_parent_turn
                        && report.agent == record.agent_id
                        && anchors.result_item_seq.is_none() =>
                    {
                        anchors.result_item_seq = Some(envelope.seq);
                    }
                    _ => {}
                }
            }
            cursor = envelope.seq;
            advanced = true;
        }
        if !advanced {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "parent journal {} did not advance toward sealed head {through_seq}",
                    record.parent_session_id
                ),
                false,
            ));
        }
    }
    Ok(anchors)
}

pub(super) async fn run_descendant_stream(
    hub: SessionHub,
    attachment_id: AttachmentId,
    mut prepared: PreparedDescendantStream,
    sink: Arc<dyn FrameSink>,
    mut cancel: watch::Receiver<bool>,
) -> ReplayCompletion {
    let (_lag_sender, mut lagged) = watch::channel(Option::<u64>::None);
    let initial_repair_identities = prepared.repair_identities();
    for child in &mut prepared.children {
        let high_water = child.wire.replay_through_seq;
        if !replay_child(
            &hub,
            &sink,
            &attachment_id,
            child,
            high_water,
            &mut lagged,
            &mut cancel,
        )
        .await
        {
            if *cancel.borrow() || hub.inner.draining.load(Ordering::Acquire) {
                let _ = hub.detach_descendant(&attachment_id);
            } else {
                hub.repair_and_detach_descendant(&sink, &attachment_id, initial_repair_identities);
            }
            return ReplayCompletion::Complete;
        }
    }

    let mut ticker = interval(DESCENDANT_RECONCILE_PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    'stream: loop {
        if *cancel.borrow() {
            break;
        }
        if !reconcile(
            &hub,
            &sink,
            &attachment_id,
            &mut prepared,
            &mut lagged,
            &mut cancel,
        )
        .await
        {
            if *cancel.borrow() || hub.inner.draining.load(Ordering::Acquire) {
                break;
            }
            hub.repair_and_detach_descendant(&sink, &attachment_id, prepared.repair_identities());
            return ReplayCompletion::Complete;
        }
        if hub.inner.draining.load(Ordering::Acquire) {
            break;
        }
        loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        break 'stream;
                    }
                }
                changed = prepared.lineage_publications.changed() => {
                    if changed.is_err() {
                        break 'stream;
                    }
                    break;
                }
                received = prepared.roster_publications.recv() => {
                    match received {
                        Ok(session_id) if prepared.watches_session(&session_id) => break,
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break 'stream,
                    }
                }
                _ = ticker.tick() => break,
            }
        }
    }
    let _ = hub.detach_descendant(&attachment_id);
    ReplayCompletion::Complete
}

async fn reconcile(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    prepared: &mut PreparedDescendantStream,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> bool {
    let descendants = match hub
        .delegation_descendants(
            prepared.root_session_id.clone(),
            haider_rpc::FLEET_MAX_NODES as usize,
            haider_rpc::FLEET_MAX_DEPTH,
        )
        .await
    {
        Ok(descendants) => descendants,
        Err(_) => return true,
    };
    // The negotiated cohort is stable. A later shallower child cannot evict
    // an already-streamed nested child merely by moving ahead in a fresh BFS
    // prefix; new lineage fills only genuinely unused fan-out slots.
    let mut selected = Vec::with_capacity(prepared.accepted_children);
    for child in &prepared.children {
        let record = if let Some(descendant) = descendants.descendants.iter().find(|descendant| {
            descendant.record.child_session_id == child.record.child_session_id
                && descendant.record.agent_id == child.record.agent_id
        }) {
            Some(descendant.record.clone())
        } else {
            match descendant_chain_to_root(
                hub,
                &prepared.root_session_id,
                &child.record.child_session_id,
                &child.record.agent_id,
            )
            .await
            {
                Ok(chain) => chain.and_then(|chain| chain.into_iter().last()),
                Err(_) => return true,
            }
        };
        let Some(record) = record else {
            let _ = send_repair(
                hub,
                sink,
                attachment_id,
                child,
                child.cursor.saturating_add(1),
                None,
                lagged,
                cancel,
            )
            .await;
            return false;
        };
        selected.push(record);
    }
    for descendant in &descendants.descendants {
        if selected.len() >= prepared.accepted_children {
            break;
        }
        if !selected.iter().any(|selected| {
            selected.child_session_id == descendant.record.child_session_id
                && selected.agent_id == descendant.record.agent_id
        }) {
            selected.push(descendant.record.clone());
        }
    }

    for record in selected {
        let found = prepared.children.iter().position(|child| {
            child.record.child_session_id == record.child_session_id
                && child.record.agent_id == record.agent_id
        });
        let Some(index) = found else {
            let Ok(mut child) = hydrate_child(hub, record, 0).await else {
                continue;
            };
            match hub.track_descendant_attachment_session(
                attachment_id,
                child.record.child_session_id.clone(),
            ) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => return false,
            }
            if !send_stream_event(
                hub,
                sink,
                attachment_id,
                SessionDescendantStreamEventWire::Delta {
                    change: DescendantChangeKindWire::Appeared,
                    child: child.wire.clone(),
                },
                lagged,
                cancel,
            )
            .await
            {
                return false;
            }
            let high_water = child.wire.replay_through_seq;
            if !replay_child(
                hub,
                sink,
                attachment_id,
                &mut child,
                high_water,
                lagged,
                cancel,
            )
            .await
            {
                return false;
            }
            prepared.children.push(child);
            continue;
        };

        let child = &mut prepared.children[index];
        let head = match hub.inner.store.latest_seq(&record.child_session_id).await {
            Ok(head) => head,
            Err(_) => continue,
        };
        if head < child.cursor {
            let _ = send_repair(
                hub,
                sink,
                attachment_id,
                child,
                child.cursor.saturating_add(1),
                None,
                lagged,
                cancel,
            )
            .await;
            return false;
        }
        if head > child.cursor
            && !replay_child(hub, sink, attachment_id, child, head, lagged, cancel).await
        {
            return false;
        }

        let state = if head == child.wire.replay_through_seq && record == child.record {
            child.wire.state
        } else {
            match descendant_child_state(&hub.inner.store, &record, head).await {
                Ok(state) => state,
                Err(_) => continue,
            }
        };
        let parent_head = match hub.inner.store.latest_seq(&record.parent_session_id).await {
            Ok(head) => head,
            Err(_) => continue,
        };
        let anchors = if parent_head == child.parent_head {
            child.wire.parent_anchors.clone()
        } else {
            match descendant_parent_anchors(
                &hub.inner.store,
                &record,
                child.parent_head,
                parent_head,
                child.wire.parent_anchors.clone(),
            )
            .await
            {
                Ok(anchors) => anchors,
                Err(_) => continue,
            }
        };
        let next = child_wire(
            &record,
            state,
            child.wire.requested_after_seq,
            head,
            anchors,
        );
        if next != child.wire || record != child.record {
            let terminal = !terminal_state(child.wire.state) && terminal_state(next.state);
            child.wire = next.clone();
            child.record = record;
            child.parent_head = parent_head;
            if !send_stream_event(
                hub,
                sink,
                attachment_id,
                SessionDescendantStreamEventWire::Delta {
                    change: if terminal {
                        DescendantChangeKindWire::Terminated
                    } else {
                        DescendantChangeKindWire::Updated
                    },
                    child: next,
                },
                lagged,
                cancel,
            )
            .await
            {
                return false;
            }
        }
    }

    let truncation = descendant_truncation(&descendants, &prepared.children);
    if truncation != prepared.truncation {
        if !send_stream_event(
            hub,
            sink,
            attachment_id,
            SessionDescendantStreamEventWire::Truncation {
                truncation: truncation.clone(),
            },
            lagged,
            cancel,
        )
        .await
        {
            return false;
        }
        prepared.truncation = truncation;
    }
    true
}

fn terminal_state(state: haider_rpc::FleetAgentStateWire) -> bool {
    matches!(
        state,
        haider_rpc::FleetAgentStateWire::Done
            | haider_rpc::FleetAgentStateWire::Failed
            | haider_rpc::FleetAgentStateWire::Cancelled
    )
}

async fn replay_child(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    child: &mut TrackedChild,
    high_water: u64,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> bool {
    while child.cursor < high_water {
        let page = match hub
            .inner
            .store
            .read(
                &child.record.child_session_id,
                child.cursor,
                REPLAY_PAGE_SIZE,
            )
            .await
        {
            Ok(page) => page,
            Err(_) => return true,
        };
        let expected = child.cursor.saturating_add(1);
        let observed = page.first().map(|envelope| envelope.seq);
        if !is_next_sequence(child.cursor, observed) {
            let _ = send_repair(
                hub,
                sink,
                attachment_id,
                child,
                expected,
                observed,
                lagged,
                cancel,
            )
            .await;
            return false;
        }
        let mut advanced = false;
        for envelope in page {
            if envelope.seq > high_water {
                break;
            }
            if !is_next_sequence(child.cursor, Some(envelope.seq)) {
                let _ = send_repair(
                    hub,
                    sink,
                    attachment_id,
                    child,
                    child.cursor.saturating_add(1),
                    Some(envelope.seq),
                    lagged,
                    cancel,
                )
                .await;
                return false;
            }
            let seq = envelope.seq;
            if !send_stream_event(
                hub,
                sink,
                attachment_id,
                SessionDescendantStreamEventWire::Envelope {
                    session_id: child.record.child_session_id.clone(),
                    agent_id: child.record.agent_id.clone(),
                    envelope,
                },
                lagged,
                cancel,
            )
            .await
            {
                return false;
            }
            child.cursor = seq;
            advanced = true;
        }
        if !advanced {
            let _ = send_repair(
                hub,
                sink,
                attachment_id,
                child,
                expected,
                observed,
                lagged,
                cancel,
            )
            .await;
            return false;
        }
    }
    send_stream_event(
        hub,
        sink,
        attachment_id,
        SessionDescendantStreamEventWire::ChildCaughtUp {
            session_id: child.record.child_session_id.clone(),
            agent_id: child.record.agent_id.clone(),
            high_water_seq: high_water,
        },
        lagged,
        cancel,
    )
    .await
}

fn is_next_sequence(cursor: u64, observed: Option<u64>) -> bool {
    observed == Some(cursor.saturating_add(1))
}

#[allow(clippy::too_many_arguments)]
async fn send_repair(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    child: &TrackedChild,
    expected_seq: u64,
    observed_seq: Option<u64>,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> bool {
    send_stream_event(
        hub,
        sink,
        attachment_id,
        SessionDescendantStreamEventWire::RepairRequired {
            session_id: child.record.child_session_id.clone(),
            agent_id: child.record.agent_id.clone(),
            resume_after_seq: child.cursor,
            expected_seq,
            observed_seq,
        },
        lagged,
        cancel,
    )
    .await
}

async fn send_stream_event(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    event: SessionDescendantStreamEventWire,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> bool {
    let frame = WireFrame::SessionDescendantStream {
        attachment_id: attachment_id.clone(),
        event,
    };
    matches!(
        super::replay::deliver_frame(hub, sink, attachment_id, &frame, lagged, cancel).await,
        super::replay::FrameDelivery::Delivered
    )
}

#[cfg(test)]
#[path = "descendant_stream_tests.rs"]
mod descendant_stream_tests;
