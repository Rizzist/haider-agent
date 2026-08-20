//! Startup reconciliation for durable dispatch/outcome crash windows.
//!
//! The live [`haider_tools::EffectBroker`] owns terminal arbitration inside one
//! process. After process death its private lifecycle map is gone, so startup
//! reduces the authoritative journal instead: every `Dispatched` effect with
//! no later `Outcome` receives one durable `Unknown` outcome. Re-running the
//! scan is idempotent because the appended outcome becomes part of that same
//! reduction.
//!
//! W3b1 seam (additive, d1 report R16): `haider-daemon` runs this inside its
//! reconcile-before-ready gate — after the profile lock and generation bump,
//! before any listener binds. Ambiguous effects are never retried here; they
//! are only marked `Unknown` so a human or later policy can decide.

use crate::{SqliteStoreHandle, StoreHandle};
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EffectId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::{MenuAnswer, MenuKind, effect_recovery_menu};
use haider_protocol::state::RunState;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const RECOVERY_PAGE_SIZE: usize = 1_024;
const RECOVERY_EVIDENCE_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Default)]
struct RecoveryEvidence {
    call_id: Option<String>,
    agent_id: Option<AgentId>,
    dispatch_seq: Option<u64>,
    pid: Option<i32>,
    recorded_revision: Option<String>,
    result_committed: bool,
    artifact_committed: bool,
    artifact_seq: Option<u64>,
    latest_workspace_mutation_seq: Option<u64>,
}

#[derive(Default)]
struct CommittedCallEvidence {
    result_seq: Option<u64>,
    artifact_seq: Option<u64>,
}

/// Performs one bounded, read-only journal/process-liveness sweep for the
/// effects about to be parked. Failure and deadline expiry degrade to a terse
/// unavailable line; neither can prevent the durable recovery menu opening.
pub async fn effect_recovery_evidence<S: StoreHandle + ?Sized>(
    store: &S,
    session_id: &SessionId,
    run_id: Option<&RunId>,
    branch_id: Option<&BranchId>,
    effects: &[EffectId],
) -> HashMap<EffectId, String> {
    match tokio::time::timeout(
        RECOVERY_EVIDENCE_DEADLINE,
        gather_effect_recovery_evidence(store, session_id, run_id, branch_id, effects),
    )
    .await
    {
        Ok(Ok(evidence)) => evidence,
        Ok(Err(_)) | Err(_) => unavailable_evidence(effects),
    }
}

fn unavailable_evidence(effects: &[EffectId]) -> HashMap<EffectId, String> {
    effects
        .iter()
        .cloned()
        .map(|effect| (effect, "probe: evidence unavailable".to_owned()))
        .collect()
}

async fn gather_effect_recovery_evidence<S: StoreHandle + ?Sized>(
    store: &S,
    session_id: &SessionId,
    run_id: Option<&RunId>,
    branch_id: Option<&BranchId>,
    effects: &[EffectId],
) -> Result<HashMap<EffectId, String>, HaiderError> {
    let targets = effects.iter().map(EffectId::as_str).collect::<HashSet<_>>();
    let mut evidence = effects
        .iter()
        .cloned()
        .map(|effect| (effect, RecoveryEvidence::default()))
        .collect::<HashMap<_, _>>();
    let mut open_calls = Vec::<(Option<AgentId>, ItemId, String)>::new();
    let mut call_evidence = HashMap::<(Option<AgentId>, String), CommittedCallEvidence>::new();
    let mut current_revision = Some("workspace-revision:0".to_owned());
    let mut cursor = 0;
    loop {
        let page = store.read(session_id, cursor, RECOVERY_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            let mutation_event = matches!(
                (
                    envelope
                        .payload
                        .get("type")
                        .and_then(serde_json::Value::as_str),
                    envelope
                        .payload
                        .get("phase")
                        .and_then(serde_json::Value::as_str),
                ),
                (Some("effect"), Some("outcome")) | (Some("task_completed"), _)
            );
            if let Some(revision) = mutation_event
                .then(|| {
                    envelope
                        .payload
                        .get("workspace_mutation")
                        .and_then(|mutation| mutation.get("workspace_revision"))
                        .and_then(serde_json::Value::as_str)
                })
                .flatten()
            {
                current_revision = Some(revision.to_owned());
                for probe in evidence.values_mut() {
                    probe.latest_workspace_mutation_seq = Some(envelope.seq);
                }
            }
            if envelope.run_id.as_ref() != run_id || envelope.branch_id.as_ref() != branch_id {
                continue;
            }
            let seq = envelope.seq;
            let agent_id = envelope.agent_id.clone();
            let raw_pid = effect_dispatch_pid(&envelope.payload);
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::Item(ItemEvent::Started { item_id, item }) => {
                    if let Some(call_id) = item_call_id(&item) {
                        open_calls.push((agent_id, item_id, call_id.to_owned()));
                    }
                }
                EventPayload::Item(ItemEvent::Completed { item_id, .. }) => {
                    open_calls.retain(|(_, candidate, _)| candidate != &item_id);
                }
                EventPayload::Effect(EffectPhase::Intent(intent))
                    if targets.contains(intent.effect.as_str()) =>
                {
                    if let Some(probe) = evidence.get_mut(&intent.effect) {
                        probe.agent_id.clone_from(&agent_id);
                        probe.call_id =
                            sole_open_call(&open_calls, agent_id.as_ref()).map(ToOwned::to_owned);
                        probe.recorded_revision = intent
                            .workspace_revision
                            .map(|revision| revision.as_str().to_owned());
                    }
                }
                EventPayload::Effect(EffectPhase::Dispatched { effect })
                    if targets.contains(effect.as_str()) =>
                {
                    if let Some(probe) = evidence.get_mut(&effect) {
                        probe.agent_id.clone_from(&agent_id);
                        probe.dispatch_seq = Some(seq);
                        probe.pid = raw_pid;
                        if probe.call_id.is_none() {
                            probe.call_id = sole_open_call(&open_calls, agent_id.as_ref())
                                .map(ToOwned::to_owned);
                        }
                    }
                }
                EventPayload::ToolResult { call_id, result } => {
                    let committed = call_evidence.entry((agent_id, call_id)).or_default();
                    committed.result_seq = Some(seq);
                    if result.artifact.is_some() || !result.images.is_empty() {
                        committed.artifact_seq = Some(seq);
                    }
                }
                EventPayload::ProcessSignalRecorded(signal)
                    if targets.contains(signal.effect_id.as_str()) =>
                {
                    if let Some(probe) = evidence.get_mut(&signal.effect_id) {
                        probe.agent_id.clone_from(&agent_id);
                        probe.call_id = Some(signal.call_id);
                        if signal.artifact.is_some() {
                            probe.artifact_seq = Some(seq);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut rendered = HashMap::with_capacity(evidence.len());
    for (effect, mut probe) in evidence {
        if let (Some(call_id), Some(dispatch)) = (probe.call_id.as_deref(), probe.dispatch_seq)
            && let Some(committed) =
                call_evidence.get(&(probe.agent_id.clone(), call_id.to_owned()))
        {
            probe.result_committed = committed.result_seq.is_some_and(|seq| seq > dispatch);
            probe.artifact_committed = committed.artifact_seq.is_some_and(|seq| seq > dispatch);
        }
        if let (Some(dispatch), Some(artifact)) = (probe.dispatch_seq, probe.artifact_seq) {
            probe.artifact_committed |= artifact > dispatch;
        }
        let line = render_recovery_evidence(&probe, current_revision.as_deref()).await;
        rendered.insert(effect, line);
    }
    Ok(rendered)
}

fn item_call_id(item: &TurnItem) -> Option<&str> {
    match item {
        TurnItem::ToolCall { call_id, .. } | TurnItem::CommandExecution { call_id, .. } => {
            Some(call_id)
        }
        _ => None,
    }
}

fn sole_open_call<'a>(
    open_calls: &'a [(Option<AgentId>, ItemId, String)],
    agent_id: Option<&AgentId>,
) -> Option<&'a str> {
    let mut matching = open_calls
        .iter()
        .filter(|(candidate_agent, _, _)| candidate_agent.as_ref() == agent_id)
        .map(|(_, _, call_id)| call_id.as_str());
    let call_id = matching.next()?;
    matching.next().is_none().then_some(call_id)
}

fn effect_dispatch_pid(payload: &serde_json::Value) -> Option<i32> {
    if payload.get("phase").and_then(serde_json::Value::as_str) != Some("dispatched") {
        return None;
    }
    ["pid", "process_group", "process_group_id", "pgid"]
        .into_iter()
        .find_map(|field| payload.get(field).and_then(serde_json::Value::as_i64))
        .and_then(|pid| i32::try_from(pid).ok())
}

async fn render_recovery_evidence(
    probe: &RecoveryEvidence,
    current_revision: Option<&str>,
) -> String {
    let process = if let Some(pid) = probe.pid {
        tokio::task::spawn_blocking(move || haider_tools::probe_group_liveness_evidence(pid))
            .await
            .ok()
            .map(|liveness| match liveness {
                haider_tools::EvidencePidLiveness::Alive => "process alive",
                haider_tools::EvidencePidLiveness::Dead => "process dead",
                haider_tools::EvidencePidLiveness::Unknown => "process unknown",
            })
    } else {
        None
    };
    let result = probe.call_id.as_ref().map(|_| {
        if probe.result_committed {
            "result committed"
        } else {
            "no result committed"
        }
    });
    let artifact = probe.artifact_committed.then_some("artifact present");
    let workspace = match (
        probe.dispatch_seq,
        probe.latest_workspace_mutation_seq,
        probe.recorded_revision.as_deref(),
        current_revision,
    ) {
        (Some(dispatch), Some(mutation), _, _) if mutation > dispatch => {
            Some("workspace ADVANCED past dispatch")
        }
        (_, _, Some(recorded), Some(current)) if current == recorded => {
            Some("workspace revision unchanged")
        }
        (_, _, Some(recorded), Some(current))
            if matches!(
                (workspace_revision_seq(recorded), workspace_revision_seq(current)),
                (Some(recorded), Some(current)) if current > recorded
            ) =>
        {
            Some("workspace ADVANCED since effect record")
        }
        _ => None,
    };
    let findings = [process, result, artifact, workspace]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if findings.is_empty() {
        return "probe: evidence unavailable".to_owned();
    }
    let caution = process == Some("process alive")
        || result == Some("result committed")
        || artifact == Some("artifact present")
        || workspace.is_some_and(|finding| finding.starts_with("workspace ADVANCED"));
    let likely_safe = process == Some("process dead")
        && result == Some("no result committed")
        && artifact.is_none()
        && workspace == Some("workspace revision unchanged");
    let mut line = format!("probe: {}", findings.join(" · "));
    if caution {
        line.push_str(" — verify before Retry");
    } else if likely_safe {
        line.push_str(" — Retry is likely safe");
    }
    line
}

fn workspace_revision_seq(revision: &str) -> Option<u64> {
    revision
        .strip_prefix("workspace-revision:")
        .and_then(|seq| seq.parse().ok())
}

/// The dispatch coordinates needed to synthesize recovery envelopes.
///
/// Keeping only these fields avoids retaining (and cloning) the dispatched
/// event's raw JSON payload for the remainder of a session scan.
struct RecoveryDispatch {
    event_id: EventId,
    session_id: SessionId,
    branch_id: Option<BranchId>,
    run_id: Option<RunId>,
    agent_id: Option<AgentId>,
    authority_epoch: u64,
}

/// Durable effects terminalized during one startup recovery pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub reconciled_effects: Vec<EffectId>,
    pub opened_recovery_menus: Vec<EffectId>,
}

/// Reconciles every prior dispatched-without-outcome effect before readiness.
///
/// Appends `EffectOutcome::Unknown` exactly once per orphaned dispatch:
/// the deterministic `recovery-*` event id makes reruns of the same
/// generation collide harmlessly, and a completed rerun finds the appended
/// outcome terminal. Effects are processed in stable (sorted) order so two
/// interrupted passes cannot interleave differently.
pub async fn reconcile_dispatched_effects(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
) -> Result<RecoveryReport, HaiderError> {
    let mut report = RecoveryReport::default();
    let worker_generation = store.worker_generation();
    for session_id in store.session_ids().await? {
        let mut cursor = 0;
        let mut dispatched = HashMap::<EffectId, RecoveryDispatch>::new();
        let mut outcomes = HashMap::<EffectId, bool>::new();
        let mut recovery_menus = HashMap::<MenuId, EffectId>::new();
        loop {
            let page = store.read(&session_id, cursor, RECOVERY_PAGE_SIZE).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            reduce_page(page, &mut dispatched, &mut outcomes, &mut recovery_menus)?;
        }

        let open_recovery_effects = recovery_menus
            .values()
            .map(EffectId::as_str)
            .collect::<HashSet<_>>();
        let mut pending = dispatched
            .into_iter()
            .filter(|(effect, dispatch)| match outcomes.get(effect) {
                None => true,
                Some(true) => {
                    dispatch.run_id.is_some() && !open_recovery_effects.contains(effect.as_str())
                }
                Some(false) => false,
            })
            .collect::<Vec<_>>();
        pending.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        if pending.is_empty() {
            continue;
        }

        let mut evidence_groups = Vec::<(RunId, Option<BranchId>, Vec<EffectId>)>::new();
        for (effect, dispatch) in &pending {
            let Some(run_id) = dispatch.run_id.as_ref() else {
                continue;
            };
            if let Some((_, _, effects)) = evidence_groups.iter_mut().find(|(run, branch, _)| {
                run == run_id && branch.as_ref() == dispatch.branch_id.as_ref()
            }) {
                effects.push(effect.clone());
            } else {
                evidence_groups.push((
                    run_id.clone(),
                    dispatch.branch_id.clone(),
                    vec![effect.clone()],
                ));
            }
        }
        let evidence_effects = evidence_groups
            .iter()
            .flat_map(|(_, _, effects)| effects.iter().cloned())
            .collect::<Vec<_>>();
        let evidence = tokio::time::timeout(RECOVERY_EVIDENCE_DEADLINE, async {
            let mut evidence = HashMap::new();
            for (run_id, branch_id, effects) in evidence_groups {
                evidence.extend(
                    effect_recovery_evidence(
                        store,
                        &session_id,
                        Some(&run_id),
                        branch_id.as_ref(),
                        &effects,
                    )
                    .await,
                );
            }
            evidence
        })
        .await
        .unwrap_or_else(|_| unavailable_evidence(&evidence_effects));

        let mut recovery = Vec::with_capacity(pending.len().saturating_mul(3));
        for (effect, dispatch) in pending {
            if !outcomes.contains_key(&effect) {
                recovery.push(recovery_envelope(
                    worker_generation,
                    device_id,
                    &dispatch,
                    recovery_event_id(session_id.as_str(), worker_generation, effect.as_str()),
                    EventPayload::Effect(EffectPhase::Outcome {
                        effect: effect.clone(),
                        outcome: EffectOutcome::Unknown,
                        freshness: None,
                        workspace_mutation: None,
                    }),
                )?);
                report.reconciled_effects.push(effect.clone());
            }
            // Legacy/standalone effects without a run have no UI owner. They
            // retain the historical outcome-only reconciliation shape.
            if dispatch.run_id.is_none() {
                continue;
            }
            let mut menu = effect_recovery_menu(
                recovery_menu_id(session_id.as_str(), effect.as_str()),
                effect.clone(),
                format!("effect {}", effect.as_str()),
            );
            menu.body.push(
                evidence
                    .get(&effect)
                    .cloned()
                    .unwrap_or_else(|| "probe: evidence unavailable".into()),
            );
            recovery.push(recovery_envelope(
                worker_generation,
                device_id,
                &dispatch,
                recovery_event_id_for(
                    session_id.as_str(),
                    worker_generation,
                    effect.as_str(),
                    "menu",
                ),
                EventPayload::MenuOpened(menu),
            )?);
            recovery.push(recovery_envelope(
                worker_generation,
                device_id,
                &dispatch,
                recovery_event_id_for(
                    session_id.as_str(),
                    worker_generation,
                    effect.as_str(),
                    "state",
                ),
                EventPayload::RunState(RunState::EffectOutcomeUnknown),
            )?);
            report.opened_recovery_menus.push(effect);
        }
        store.append(&mut recovery).await?;
    }
    Ok(report)
}

fn reduce_page(
    page: Vec<RawEnvelope>,
    dispatched: &mut HashMap<EffectId, RecoveryDispatch>,
    outcomes: &mut HashMap<EffectId, bool>,
    recovery_menus: &mut HashMap<MenuId, EffectId>,
) -> Result<(), HaiderError> {
    for envelope in page {
        if !matches!(
            envelope
                .payload
                .get("type")
                .and_then(|value| value.as_str()),
            Some("effect" | "menu_opened" | "menu_answered" | "menu_closed")
        ) {
            continue;
        }
        let payload: EventPayload = serde_json::from_value(envelope.payload).map_err(|error| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "invalid effect payload in session {}, seq {}: {error}",
                    envelope.session_id, envelope.seq
                ),
                false,
            )
        })?;
        match payload {
            EventPayload::Effect(EffectPhase::Dispatched { effect }) => {
                dispatched.insert(
                    effect,
                    RecoveryDispatch {
                        event_id: envelope.event_id,
                        session_id: envelope.session_id,
                        branch_id: envelope.branch_id,
                        run_id: envelope.run_id,
                        agent_id: envelope.agent_id,
                        authority_epoch: envelope.authority_epoch,
                    },
                );
            }
            EventPayload::Effect(EffectPhase::Outcome {
                effect, outcome, ..
            }) => {
                outcomes.insert(effect, matches!(outcome, EffectOutcome::Unknown));
            }
            EventPayload::MenuOpened(menu) => {
                if let MenuKind::Recovery { effect, .. } = menu.kind {
                    recovery_menus.insert(menu.id, effect);
                }
            }
            EventPayload::MenuAnswered(MenuAnswer { menu, .. })
            | EventPayload::MenuClosed { menu, .. } => {
                recovery_menus.remove(&menu);
            }
            _ => {}
        }
    }
    Ok(())
}

fn recovery_envelope(
    worker_generation: u64,
    device_id: &DeviceId,
    dispatch: &RecoveryDispatch,
    event_id: EventId,
    payload: EventPayload,
) -> Result<RawEnvelope, HaiderError> {
    let payload = serde_json::to_value(payload).map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("recovery payload could not serialize: {error}"),
            false,
        )
    })?;
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id,
        seq: 0,
        session_id: dispatch.session_id.clone(),
        branch_id: dispatch.branch_id.clone(),
        run_id: dispatch.run_id.clone(),
        agent_id: dispatch.agent_id.clone(),
        device_id: device_id.clone(),
        authority_epoch: dispatch.authority_epoch,
        worker_generation,
        causation_id: Some(dispatch.event_id.clone()),
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Pruned,
        },
        payload,
    })
}

/// Deterministic id for one (session, generation, effect) recovery outcome.
///
/// Hashing each part separately (fixed-width digests) removes concatenation
/// ambiguity: `("a", "b-2-c")` and `("a-2-b", "c")` cannot collide even
/// though the ids are opaque strings.
fn recovery_event_id(session_id: &str, worker_generation: u64, effect_id: &str) -> EventId {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, session_id.as_bytes());
    hash_part(&mut hasher, &worker_generation.to_be_bytes());
    hash_part(&mut hasher, effect_id.as_bytes());
    EventId::new(format!("recovery-{}", hasher.finalize().to_hex()))
}

fn recovery_event_id_for(
    session_id: &str,
    worker_generation: u64,
    effect_id: &str,
    kind: &str,
) -> EventId {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, session_id.as_bytes());
    hash_part(&mut hasher, &worker_generation.to_be_bytes());
    hash_part(&mut hasher, effect_id.as_bytes());
    hash_part(&mut hasher, kind.as_bytes());
    EventId::new(format!("recovery-{}", hasher.finalize().to_hex()))
}

fn recovery_menu_id(session_id: &str, effect_id: &str) -> MenuId {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, session_id.as_bytes());
    hash_part(&mut hasher, effect_id.as_bytes());
    MenuId::new(format!("effect-recovery-{}", hasher.finalize().to_hex()))
}

fn hash_part(hasher: &mut blake3::Hasher, part: &[u8]) {
    hasher.update(blake3::hash(part).as_bytes());
}
