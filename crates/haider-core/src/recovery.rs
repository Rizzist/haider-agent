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
use haider_protocol::ids::{DeviceId, EffectId, EventId, MenuId};
use haider_protocol::menu::{MenuKind, effect_recovery_menu};
use haider_protocol::state::RunState;
use std::collections::{HashMap, HashSet};

const RECOVERY_PAGE_SIZE: usize = 1_024;

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
    for session_id in store.session_ids().await? {
        let mut cursor = 0;
        let mut dispatched = HashMap::<EffectId, RawEnvelope>::new();
        let mut outcomes = HashMap::<EffectId, EffectOutcome>::new();
        let mut recovery_menus = HashSet::<EffectId>::new();
        loop {
            let page = store.read(&session_id, cursor, RECOVERY_PAGE_SIZE).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            reduce_page(&page, &mut dispatched, &mut outcomes, &mut recovery_menus)?;
        }

        let mut pending = dispatched
            .into_iter()
            .filter(|(effect, _)| {
                outcomes
                    .get(effect)
                    .is_none_or(|outcome| matches!(outcome, EffectOutcome::Unknown))
                    && !recovery_menus.contains(effect)
            })
            .collect::<Vec<_>>();
        pending.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        if pending.is_empty() {
            continue;
        }

        let mut recovery = Vec::with_capacity(pending.len().saturating_mul(3));
        for (effect, dispatch) in pending {
            if !outcomes.contains_key(&effect) {
                recovery.push(recovery_envelope(
                    store,
                    device_id,
                    &dispatch,
                    recovery_event_id(
                        session_id.as_str(),
                        store.worker_generation(),
                        effect.as_str(),
                    ),
                    EventPayload::Effect(EffectPhase::Outcome {
                        effect: effect.clone(),
                        outcome: EffectOutcome::Unknown,
                        freshness: None,
                    }),
                )?);
                report.reconciled_effects.push(effect.clone());
            }
            // Legacy/standalone effects without a run have no UI owner. They
            // retain the historical outcome-only reconciliation shape.
            if dispatch.run_id.is_none() {
                continue;
            }
            let menu = effect_recovery_menu(
                recovery_menu_id(session_id.as_str(), effect.as_str()),
                effect.clone(),
                format!("effect {}", effect.as_str()),
            );
            recovery.push(recovery_envelope(
                store,
                device_id,
                &dispatch,
                recovery_event_id_for(
                    session_id.as_str(),
                    store.worker_generation(),
                    effect.as_str(),
                    "menu",
                ),
                EventPayload::MenuOpened(menu),
            )?);
            recovery.push(recovery_envelope(
                store,
                device_id,
                &dispatch,
                recovery_event_id_for(
                    session_id.as_str(),
                    store.worker_generation(),
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
    page: &[RawEnvelope],
    dispatched: &mut HashMap<EffectId, RawEnvelope>,
    outcomes: &mut HashMap<EffectId, EffectOutcome>,
    recovery_menus: &mut HashSet<EffectId>,
) -> Result<(), HaiderError> {
    for envelope in page {
        if !matches!(
            envelope
                .payload
                .get("type")
                .and_then(|value| value.as_str()),
            Some("effect" | "menu_opened")
        ) {
            continue;
        }
        let payload: EventPayload =
            serde_json::from_value(envelope.payload.clone()).map_err(|error| {
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
                dispatched.insert(effect, envelope.clone());
            }
            EventPayload::Effect(EffectPhase::Outcome {
                effect, outcome, ..
            }) => {
                outcomes.insert(effect, outcome);
            }
            EventPayload::MenuOpened(menu) => {
                if let MenuKind::Recovery { effect, .. } = menu.kind {
                    recovery_menus.insert(effect);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn recovery_envelope(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
    dispatch: &RawEnvelope,
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
        worker_generation: store.worker_generation(),
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
