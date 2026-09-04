#![allow(clippy::expect_used)] // test diagnostics name the failed recovery boundary

use haider_core::{SqliteStoreHandle, StoreHandle, reconcile_dispatched_effects};
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EffectId, EventId, RunId, SessionId};
use haider_protocol::menu::MenuKind;
use haider_protocol::state::RunState;
use haider_store::{EventStore, Store};
use std::collections::HashSet;

fn envelope(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    payload: EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("before-crash"),
        authority_epoch: 5,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Pruned,
        },
        payload: serde_json::to_value(payload)
            .expect("payload serializes")
            .into(),
    }
}

#[tokio::test]
async fn startup_reconciliation_is_durable_and_idempotent() {
    let root = tempfile::tempdir().expect("temporary profile");
    let session = SessionId::new("recovered-session");
    let effect = EffectId::new("ambiguous-effect");
    {
        let store = Store::open(root.path()).expect("seed store");
        let mut events = vec![envelope(
            &session,
            "dispatched-before-crash",
            store.worker_generation(),
            EventPayload::Effect(EffectPhase::Dispatched {
                effect: effect.clone(),
            }),
        )];
        store.append(&mut events).expect("seed dispatched effect");
    }

    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("open recovery generation");
    let first = reconcile_dispatched_effects(&store, &DeviceId::new("recovery-daemon"))
        .await
        .expect("first reconciliation");
    let second = reconcile_dispatched_effects(&store, &DeviceId::new("recovery-daemon"))
        .await
        .expect("second reconciliation");
    assert_eq!(first.reconciled_effects, vec![effect.clone()]);
    assert!(second.reconciled_effects.is_empty());
    let events = store.read(&session, 0, 100).await.expect("read journal");
    let unknowns = events
        .iter()
        .filter_map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone().into()).ok()
        })
        .filter(|payload| {
            matches!(
                payload,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect: found,
                    outcome: EffectOutcome::Unknown,
                    ..
                }) if found == &effect
            )
        })
        .count();
    assert_eq!(unknowns, 1);
    assert!(
        events
            .last()
            .is_some_and(|event| event.worker_generation == store.worker_generation())
    );
    store.close().await.expect("close recovered store");
}

#[tokio::test]
async fn recovery_event_ids_are_unambiguous_for_opaque_id_tuples() {
    let root = tempfile::tempdir().expect("temporary profile");
    let cases = [
        (SessionId::new("a"), EffectId::new("b-2-c")),
        (SessionId::new("a-2-b"), EffectId::new("c")),
    ];
    {
        let store = Store::open(root.path()).expect("seed store");
        assert_eq!(store.worker_generation(), 1);
        for (index, (session, effect)) in cases.iter().enumerate() {
            let mut events = vec![envelope(
                session,
                &format!("dispatch-{index}"),
                store.worker_generation(),
                EventPayload::Effect(EffectPhase::Dispatched {
                    effect: effect.clone(),
                }),
            )];
            store.append(&mut events).expect("seed dispatch");
        }
    }

    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("open recovery generation");
    assert_eq!(store.worker_generation(), 2);
    let report = reconcile_dispatched_effects(&store, &DeviceId::new("recovery-daemon"))
        .await
        .expect("reconcile ambiguous legacy tuples");
    assert_eq!(report.reconciled_effects.len(), 2);

    let mut recovery_ids = HashSet::new();
    for (session, _) in &cases {
        let events = store.read(session, 0, 10).await.expect("read recovery");
        let recovery = events.last().expect("unknown outcome appended");
        assert!(recovery.event_id.as_str().starts_with("recovery-"));
        recovery_ids.insert(recovery.event_id.clone());
    }
    assert_eq!(recovery_ids.len(), 2);
    store.close().await.expect("close recovered store");
}

/// E6a mutation law: omitting Unknown, the menu, or the parked state breaks
/// one of these exact counts; treating Unknown as terminal makes the card
/// disappear entirely.
#[tokio::test]
async fn e6a_crash_mid_effect_journals_unknown_and_exact_four_choice_card() {
    let root = tempfile::tempdir().expect("temporary profile");
    let session = SessionId::new("e6-session");
    let run = RunId::new("e6-run");
    let effect = EffectId::new("e6-effect");
    {
        let store = Store::open(root.path()).expect("seed store");
        let mut dispatched = envelope(
            &session,
            "e6-dispatched",
            store.worker_generation(),
            EventPayload::Effect(EffectPhase::Dispatched {
                effect: effect.clone(),
            }),
        );
        dispatched.run_id = Some(run.clone());
        store.append(&mut [dispatched]).expect("seed crash window");
    }
    let store = SqliteStoreHandle::open(root.path()).await.expect("reopen");
    let first = reconcile_dispatched_effects(&store, &DeviceId::new("e6-recovery"))
        .await
        .expect("recover");
    let second = reconcile_dispatched_effects(&store, &DeviceId::new("e6-recovery"))
        .await
        .expect("idempotent rescan");
    assert_eq!(first.reconciled_effects, vec![effect.clone()]);
    assert_eq!(first.opened_recovery_menus, vec![effect.clone()]);
    assert!(second.reconciled_effects.is_empty());
    assert!(second.opened_recovery_menus.is_empty());
    let events = store.read(&session, 0, 100).await.expect("journal");
    let payloads = events
        .iter()
        .map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone().into()).expect("typed")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect: found,
                    outcome: EffectOutcome::Unknown,
                    ..
                }) if found == &effect
            ))
            .count(),
        1
    );
    let menu = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::MenuOpened(menu)
                if matches!(&menu.kind, MenuKind::Recovery { effect: found, .. } if found == &effect) =>
            {
                Some(menu)
            }
            _ => None,
        })
        .expect("recovery menu");
    assert_eq!(
        menu.options
            .iter()
            .map(|option| option.key.as_str())
            .collect::<Vec<_>>(),
        ["probe", "mark_done", "retry", "abandon"]
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::RunState(RunState::EffectOutcomeUnknown)
            ))
            .count(),
        1
    );
    store.close().await.expect("close");
}
