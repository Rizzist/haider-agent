//! Route witness for receipts whose original child response had no route fields.
use super::{
    Connection, OptionalExtension, StoreResult, Value, corrupt, decode_envelope_column,
    map_sqlite_error,
};
use haider_protocol::{
    EventPayload,
    ids::SessionId,
    item::{ItemEvent, TurnItem},
    session::{ModelSelected, RunOutputPairPinnedV1, SessionMetadataV1, SessionProviderRebound},
    state::RunState,
};
use std::collections::BTreeMap;

type Route = [Value; 3];
use crate::RunOutputRouteHistory;

struct RunRoute {
    provider: String,
    route: Option<RunOutputRouteHistory>,
    pinned: bool,
    complete_pin: bool,
    started: bool,
    ended: bool,
    configured_provider: Option<String>,
}

impl RunRoute {
    fn new(provider: &str) -> Self {
        Self {
            provider: provider.into(),
            route: None,
            pinned: false,
            complete_pin: false,
            started: false,
            ended: false,
            configured_provider: None,
        }
    }
}

fn route(account: Option<String>, base_url: Option<String>, rebind_id: Option<String>) -> Route {
    [account, base_url, rebind_id].map(|value| value.map_or(Value::Null, Value::String))
}

fn boundary_route(
    selected_provider: &str,
    session_route: &RunOutputRouteHistory,
    run_provider: &str,
) -> RunOutputRouteHistory {
    if selected_provider == run_provider {
        session_route.clone()
    } else {
        RunOutputRouteHistory::missing()
    }
}

fn expected(
    provider: &str,
    session: &RunOutputRouteHistory,
    running: Option<&RunRoute>,
    child_provider: &str,
) -> StoreResult<Route> {
    if let Some(running) = running {
        if child_provider != running.provider {
            return Ok([Value::Null, Value::Null, Value::Null]);
        }
        let running = running.route.as_ref().ok_or_else(|| {
            haider_protocol::session::run_route_history_unavailable(
                "inherited run has no boundary witness",
            )
        })?;
        let [account, endpoint, rebind] = running.selectors()?;
        return Ok(route(account, endpoint, rebind));
    }
    if child_provider == provider {
        let [account, endpoint, rebind] = session.selectors()?;
        Ok(route(account, endpoint, rebind))
    } else {
        Ok([Value::Null, Value::Null, Value::Null])
    }
}

pub(super) struct Witness<'a> {
    pub parent: &'a str,
    pub child: &'a SessionId,
    pub child_provider: &'a str,
    pub initial_seq: i64,
    pub cutoff: i64,
}

pub(super) fn at_child_creation(
    connection: &Connection,
    mut metadata: SessionMetadataV1,
    witness: Witness<'_>,
) -> StoreResult<Option<Route>> {
    let Witness {
        parent,
        child,
        child_provider,
        initial_seq,
        cutoff,
    } = witness;
    let mut runs = BTreeMap::<String, RunRoute>::new();
    let mut session_route = RunOutputRouteHistory::from_creation(&metadata);
    let mut statement = connection.prepare(
        "SELECT envelope_json FROM events WHERE session_id = ?1 AND rowid < ?2 AND seq > ?3 ORDER BY seq",
    ).map_err(map_sqlite_error)?;
    let mut events = statement
        .query(rusqlite::params![parent, cutoff, initial_seq])
        .map_err(map_sqlite_error)?;
    while let Some(row) = events.next().map_err(map_sqlite_error)? {
        let event = decode_envelope_column(connection, row, 0)
            .map_err(|error| corrupt(format!("invalid inheritance event: {error}")))?;
        if let Some(rebound) = SessionProviderRebound::from_payload_value(&event.payload) {
            session_route.rebind_legacy(metadata.provider != rebound.provider, &rebound);
            if let Some(snapshot) = event.payload.get("route_snapshot") {
                session_route.record(serde_json::from_value(snapshot.clone()).map_err(
                    |error| corrupt(format!("invalid inherited route snapshot: {error}")),
                )?);
            }
            for running in runs
                .values_mut()
                .filter(|run| (run.pinned || run.started) && !run.ended)
            {
                running.provider.clone_from(&rebound.provider);
                running.route = Some(session_route.clone());
            }
            rebound.apply_to_metadata(&mut metadata);
        } else if let Some(selected) = ModelSelected::from_payload_value(&event.payload) {
            if selected.route_only {
                session_route.resolve_legacy(&selected);
            } else {
                let coordinate_only = selected.selection_epoch.is_none()
                    && event.payload.as_object().is_some_and(|fields| {
                        fields.len() == 3
                            && ["type", "provider", "model"]
                                .iter()
                                .all(|key| fields.contains_key(*key))
                    });
                if coordinate_only {
                    // These exact older child digests used the pre-epoch
                    // coordinate rule. Epoch/observation knowledge stays
                    // missing; this is never a full recovery witness.
                    session_route
                        .select_pre_epoch_coordinates(metadata.provider != selected.provider);
                } else {
                    session_route.select_legacy(metadata.provider != selected.provider, &selected);
                }
                metadata.provider = selected.provider;
                metadata.model = selected.model;
            }
            if let Some(snapshot) = event.payload.get("route_snapshot") {
                session_route.record(serde_json::from_value(snapshot.clone()).map_err(
                    |error| corrupt(format!("invalid inherited route snapshot: {error}")),
                )?);
            }
        }
        let Some(run_id) = event.run_id.as_ref() else {
            continue;
        };
        let running = runs
            .entry(run_id.as_str().to_owned())
            .or_insert_with(|| RunRoute::new(&metadata.provider));
        if let Some(haider_protocol::headless::HeadlessRunEventPayload::HeadlessRunConfigured(
            spec,
        )) =
            haider_protocol::headless::HeadlessRunEventPayload::from_payload_value(&event.payload)
        {
            running.configured_provider = Some(spec.provider.clone());
            if !running.pinned {
                running.provider = spec.provider;
                running.route = Some(boundary_route(
                    &metadata.provider,
                    &session_route,
                    &running.provider,
                ));
                running.pinned = true;
            }
        }
        match event.payload.decode_event() {
            Ok(EventPayload::RunState(state)) => {
                if state.is_terminal() {
                    running.ended = true;
                }
                if !matches!(state, RunState::Queued) && !running.started {
                    if !running.pinned {
                        running.provider = running
                            .configured_provider
                            .clone()
                            .unwrap_or_else(|| metadata.provider.clone());
                    }
                    if !running.pinned {
                        running.route = Some(boundary_route(
                            &metadata.provider,
                            &session_route,
                            &running.provider,
                        ));
                    }
                    running.started = true;
                }
            }
            Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) => {
                if kind == RunOutputPairPinnedV1::KIND && !running.complete_pin && !running.ended {
                    let pin: RunOutputPairPinnedV1 =
                        serde_json::from_value(data).map_err(|error| {
                            corrupt(format!("invalid inheritance run pin: {error}"))
                        })?;
                    // A legacy recovery checkpoint cannot move an existing
                    // admission/start boundary past ordinary selections.
                    if pin.route_snapshot.is_none() && (running.pinned || running.started) {
                        continue;
                    }
                    running.complete_pin = pin.route_snapshot.is_some();
                    running.provider = pin.provider;
                    let pin = pin
                        .route_snapshot
                        .map(RunOutputRouteHistory::known)
                        .or_else(|| pin.provider_route.map(RunOutputRouteHistory::legacy_pin));
                    let history = match pin {
                        Some(pin) if pin.is_complete() => pin,
                        pin => {
                            let mut history = boundary_route(
                                &metadata.provider,
                                &session_route,
                                &running.provider,
                            );
                            if let Some(pin) = pin {
                                history.overlay(pin);
                            }
                            history
                        }
                    };
                    running.route = Some(history);
                    running.pinned = true;
                } else if kind == "run_worker_pickup_v1" && !running.started {
                    if !running.pinned {
                        running.provider = running
                            .configured_provider
                            .clone()
                            .unwrap_or_else(|| metadata.provider.clone());
                    }
                    if !running.pinned {
                        running.route = Some(boundary_route(
                            &metadata.provider,
                            &session_route,
                            &running.provider,
                        ));
                    }
                    running.started = true;
                } else if kind == "provider_pair_switch_v1"
                    && (running.pinned || running.started)
                    && !running.ended
                    && let Some(provider) = data.get("to_provider").and_then(Value::as_str)
                {
                    if running.provider != provider
                        && let Some(history) = running.route.as_mut()
                    {
                        history.clear_selectors();
                    }
                    running.provider = provider.into();
                }
            }
            _ => {}
        }
    }
    // An established link identifies the original run even if it completed
    // while its create command was being retried. Before establishment, only
    // a unique running parent can prove the implicit historical binding.
    let linked: Option<String> = connection.query_row(
        "SELECT parent_run_id FROM delegations WHERE child_session_id = ?1 AND parent_session_id = ?2",
        rusqlite::params![child.as_str(), parent], |row| row.get(0),
    ).optional().map_err(map_sqlite_error)?;
    let running = if let Some(linked) = linked {
        let Some(running) = runs.get(&linked) else {
            return Ok(None);
        };
        // An accepted, unstarted parent has no run-scoped route yet. Its
        // original child admission used the historical session selection.
        Some(running).filter(|run| run.pinned || run.started)
    } else {
        let mut active = runs.values().filter(|run| run.started && !run.ended);
        let running = active.next();
        if active.next().is_some() {
            return Ok(None);
        }
        running
    };
    Ok(Some(expected(
        &metadata.provider,
        &session_route,
        running,
        child_provider,
    )?))
}
