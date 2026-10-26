//! Reconstruct run boundaries from committed historical witnesses, never today's metadata.
use super::*;
use haider_protocol::session::RunOutputRouteSnapshotV1;
use std::collections::BTreeMap;

use haider_protocol::session::run_route_history_unavailable as unavailable;

pub(super) fn at(
    connection: &Connection,
    session_id: &SessionId,
    through_seq: u64,
) -> StoreResult<Option<SessionOutputPairSnapshot>> {
    let mut receipts = connection.prepare(
        "SELECT method, response_json, accepted_seq FROM command_receipts
         WHERE session_id = ?1 AND state = 'committed' AND accepted_seq <= ?2
         AND method IN ('session.create', 'session.fork', 'session.metafork', 'session.select_model', 'session.provider.rebind')
         ORDER BY accepted_seq",
    ).map_err(map_sqlite_error)?;
    let mut rows = receipts
        .query(params![
            session_id.as_str(),
            to_sqlite_integer(through_seq)?
        ])
        .map_err(map_sqlite_error)?;
    let mut metadata = None;
    let mut selections = BTreeMap::new();
    let mut rebinds = BTreeMap::new();
    let mut initial_seq = 0;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let method: String = row.get(0).map_err(map_sqlite_error)?;
        let response: String = row.get(1).map_err(map_sqlite_error)?;
        let seq = u64::try_from(row.get::<_, i64>(2).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("negative historical receipt sequence"))?;
        if method == "session.select_model" {
            let selected: SelectedModel = serde_json::from_str(&response)
                .map_err(|error| corrupt(format!("invalid selection receipt: {error}")))?;
            selections.insert(seq, selected);
        } else if method == "session.provider.rebind" {
            let rebound: ReboundSessionProvider = serde_json::from_str(&response)
                .map_err(|error| corrupt(format!("invalid rebind receipt: {error}")))?;
            rebinds.insert(seq, rebound);
        } else if metadata.is_none() {
            // All three creation responses contain the same typed metadata;
            // fork-specific fields are deliberately irrelevant to this projection.
            let response: serde_json::Value = serde_json::from_str(&response)
                .map_err(|error| corrupt(format!("invalid creation receipt: {error}")))?;
            let initial = serde_json::from_value::<SessionMetadataV1>(response["metadata"].clone())
                .map_err(|error| corrupt(format!("invalid creation metadata: {error}")))?;
            // The frozen child metadata is the run's actual configuration.
            // Canonical retry digests can describe an inherited intent that an
            // older writer never stored; they cannot turn None into a pin.
            metadata = Some(initial);
            initial_seq = seq;
        }
    }
    let Some(mut metadata) = metadata else {
        return Ok(None);
    };
    let mut route = crate::RunOutputRouteHistory::from_creation(&metadata);
    let mut events = connection.prepare(
        "SELECT envelope_json FROM events WHERE session_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
    ).map_err(map_sqlite_error)?;
    let mut rows = events
        .query(params![
            session_id.as_str(),
            to_sqlite_integer(initial_seq)?,
            to_sqlite_integer(through_seq)?
        ])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let event = decode_envelope_column(connection, row, 0)
            .map_err(|error| corrupt(format!("invalid route history event: {error}")))?;
        // A run-only automatic preparation preserves the next-turn choice.
        if event
            .payload
            .get("automatic_only")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let rebound = SessionProviderRebound::from_payload_value(&event.payload);
        let selected = ModelSelected::from_payload_value(&event.payload);
        let route_fact = rebound.is_some() || selected.is_some();
        if let Some(rebound) = rebound {
            route.rebind_legacy(metadata.provider != rebound.provider, &rebound);
            rebound.apply_to_metadata(&mut metadata);
            if rebound.output_budget.is_none()
                && let Some(receipt) = rebinds
                    .get(&event.seq)
                    .filter(|receipt| receipt.max_tokens != 0)
            {
                metadata.max_tokens = receipt.max_tokens;
                metadata.max_tokens_source = Some(receipt.max_tokens_source);
            }
        }
        if let Some(selected) = selected {
            if selected.route_only {
                route.resolve_legacy(&selected);
            } else {
                route.select_legacy(metadata.provider != selected.provider, &selected);
                metadata.provider = selected.provider;
                metadata.model = selected.model;
                if let Some(receipt) = selections.get(&event.seq) {
                    if let Some(budget) = receipt.output_budget {
                        metadata.max_tokens = budget.max_tokens;
                        metadata.max_tokens_source = Some(budget.source);
                    }
                    if let Some(snapshot) = &receipt.route_snapshot {
                        route.record(snapshot.clone());
                    }
                } else {
                    return Err(unavailable("selection receipt is missing"));
                }
            }
        }
        if route_fact && let Some(snapshot) = event.payload.get("route_snapshot") {
            route.record(
                serde_json::from_value::<RunOutputRouteSnapshotV1>(snapshot.clone())
                    .map_err(|error| corrupt(format!("invalid historical route: {error}")))?,
            );
        }
    }
    Ok(Some(SessionOutputPairSnapshot {
        route,
        provider: metadata.provider,
        model: metadata.model,
        max_tokens: metadata.max_tokens,
        source: metadata.max_tokens_source,
    }))
}

pub(super) fn automatic_run_switches(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<std::collections::BTreeMap<u64, RunId>> {
    let mut statement = connection
        .prepare(
            "SELECT accepted_seq, request_json FROM command_receipts
             WHERE session_id = ?1 AND method = 'session.select_model'
             AND state = 'committed' AND accepted_seq IS NOT NULL ORDER BY accepted_seq",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut selections = std::collections::BTreeMap::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let request: String = row.get(1).map_err(map_sqlite_error)?;
        if let Ok(request) = serde_json::from_str::<serde_json::Value>(&request)
            && request
                .get("automatic")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            && let Some(run) = request.get("run_id").and_then(serde_json::Value::as_str)
        {
            let seq = u64::try_from(row.get::<_, i64>(0).map_err(map_sqlite_error)?)
                .map_err(|_| corrupt("negative automatic selection sequence"))?;
            selections.insert(seq, RunId::new(run));
        }
    }
    Ok(selections)
}

/// Envelopes use the journal's binary/CAS codec as well as legacy JSON.
/// The transaction owning this read serializes the target install with rebinds.
pub(super) fn latest_rebind_seq(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<u64> {
    let mut statement = connection
        .prepare("SELECT envelope_json FROM events WHERE session_id = ?1 ORDER BY seq DESC")
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let event = decode_envelope_column(connection, row, 0)
            .map_err(|error| corrupt(format!("invalid rebind generation witness: {error}")))?;
        if event
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            == Some("session_provider_rebound")
        {
            if SessionProviderRebound::from_payload_value(&event.payload).is_none() {
                return Err(corrupt("invalid rebind generation witness"));
            }
            return Ok(event.seq);
        }
    }
    Ok(0)
}
