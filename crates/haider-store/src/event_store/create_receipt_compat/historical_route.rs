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

struct RunRoute {
    provider: String,
    route: Option<Route>,
    pinned: bool,
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
            started: false,
            ended: false,
            configured_provider: None,
        }
    }
}

fn route(account: Option<String>, base_url: Option<String>, rebind_id: Option<String>) -> Route {
    [account, base_url, rebind_id].map(|value| value.map_or(Value::Null, Value::String))
}

fn expected(
    metadata: SessionMetadataV1,
    running: Option<&RunRoute>,
    child_provider: &str,
) -> Route {
    if let Some(running) = running {
        if child_provider != running.provider {
            return [Value::Null, Value::Null, Value::Null];
        }
        if let Some(route) = &running.route {
            return route.clone();
        }
        // A legacy run without an explicit binding has the same clear rule as
        // restore_route: a different current provider cannot supply its route.
        if metadata.provider != running.provider {
            return [Value::Null, Value::Null, Value::Null];
        }
    }
    if child_provider == metadata.provider {
        route(
            metadata.account_alias,
            metadata.provider_base_url,
            metadata.provider_rebind_id,
        )
    } else {
        [Value::Null, Value::Null, Value::Null]
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
            for running in runs
                .values_mut()
                .filter(|run| (run.pinned || run.started) && !run.ended)
            {
                running.provider.clone_from(&rebound.provider);
                running.route = Some(route(
                    rebound.account.clone(),
                    rebound.base_url.clone(),
                    Some(rebound.rebind_id.clone()),
                ));
            }
            rebound.apply_to_metadata(&mut metadata);
        } else if let Some(selected) = ModelSelected::from_payload_value(&event.payload) {
            if selected.cleared_account_pin.is_some() {
                metadata.account_alias = None;
            }
            if metadata.provider != selected.provider {
                if metadata.provider_rebind_id.is_some() {
                    metadata.account_alias = None;
                }
                metadata.provider_base_url = None;
                metadata.provider_rebind_id = None;
            }
            metadata.provider = selected.provider;
            metadata.model = selected.model;
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
            running.configured_provider = Some(spec.provider);
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
                    running.started = true;
                }
            }
            Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) => {
                if kind == RunOutputPairPinnedV1::KIND && !running.pinned && !running.ended {
                    let pin: RunOutputPairPinnedV1 =
                        serde_json::from_value(data).map_err(|error| {
                            corrupt(format!("invalid inheritance run pin: {error}"))
                        })?;
                    running.provider = pin.provider;
                    running.route = pin
                        .provider_route
                        .map(|pin| route(pin.account, pin.base_url, Some(pin.rebind_id)));
                    running.pinned = true;
                } else if kind == "run_worker_pickup_v1" && !running.started {
                    if !running.pinned {
                        running.provider = running
                            .configured_provider
                            .clone()
                            .unwrap_or_else(|| metadata.provider.clone());
                    }
                    running.started = true;
                } else if kind == "provider_pair_switch_v1"
                    && (running.pinned || running.started)
                    && !running.ended
                    && let Some(provider) = data.get("to_provider").and_then(Value::as_str)
                {
                    if running.provider != provider {
                        running.route = None;
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
    Ok(Some(expected(metadata, running, child_provider)))
}
