//! Upgrade equivalence for the exact delegated-create schema before explicit
//! route inheritance joined its digest. New-schema receipts stay byte strict.
use super::{CreatedSession, StoreResult, corrupt, decode_envelope_column, map_sqlite_error};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

mod historical_route;

const LEGACY_KEYS: [&str; 10] = [
    "cache_policy",
    "cwd",
    "delegation_agent",
    "effort",
    "fast",
    "interaction_mode",
    "max_tokens",
    "model",
    "permission_overrides",
    "provider",
];
const ROUTE_KEYS: [&str; 3] = [
    "inherited_account_alias",
    "inherited_provider_base_url",
    "inherited_provider_rebind_id",
];

pub(super) fn legacy_delegated_create_matches(
    connection: &Connection,
    stored: &str,
    incoming: &str,
    response: Option<&str>,
) -> StoreResult<bool> {
    let (Ok(Value::Object(mut old)), Ok(Value::Object(mut new))) =
        (serde_json::from_str(stored), serde_json::from_str(incoming))
    else {
        return Ok(false);
    };
    if !old.get("delegation_agent").is_some_and(Value::is_string) {
        return Ok(false);
    }
    // A current receipt already binds both budget intent and route witness;
    // only the pre-classification/pre-route schemas permit upgrade equivalence.
    if old.contains_key("inheritance_parent_session_id") && old.contains_key("max_tokens_source") {
        return Ok(false);
    }
    let old_source = old.remove("max_tokens_source");
    let new_source = new.remove("max_tokens_source");
    if old_source.is_some() || new_source.is_some() {
        let Some(response) = response else {
            return Ok(false);
        };
        let created: CreatedSession = serde_json::from_str(response)
            .map_err(|error| corrupt(format!("invalid legacy create receipt: {error}")))?;
        let admitted = haider_protocol::output_budget::SessionOutputBudgetSourceV1::classify(
            created.metadata.max_tokens_source,
            created.metadata.max_tokens,
        );
        for source in [old_source, new_source].into_iter().flatten() {
            let Ok(source) = serde_json::from_value::<
                Option<haider_protocol::output_budget::SessionOutputBudgetSourceV1>,
            >(source) else {
                return Ok(false);
            };
            if haider_protocol::output_budget::SessionOutputBudgetSourceV1::classify(
                source,
                created.metadata.max_tokens,
            ) != admitted
            {
                return Ok(false);
            }
        }
    }
    // Source-only upgrade: every route/witness and caller key remains exact.
    if old == new {
        let known_keys = LEGACY_KEYS.iter().all(|key| old.contains_key(*key));
        let recorded_route = ROUTE_KEYS.iter().all(|key| old.contains_key(*key));
        let known_shape = old.len() == LEGACY_KEYS.len()
            || (recorded_route && old.len() == LEGACY_KEYS.len() + ROUTE_KEYS.len())
            || (recorded_route
                && old.len() == LEGACY_KEYS.len() + ROUTE_KEYS.len() + 1
                && old
                    .get("inheritance_parent_session_id")
                    .is_some_and(Value::is_string));
        return Ok(known_keys && known_shape);
    }
    let parent = new.remove("inheritance_parent_session_id");
    // 6223198e already recorded all route choices, but had no history witness.
    // Adding that witness changes no choice; all its recorded values stay strict.
    if old.len() == LEGACY_KEYS.len() + ROUTE_KEYS.len()
        && LEGACY_KEYS
            .iter()
            .chain(ROUTE_KEYS.iter())
            .all(|key| old.contains_key(*key))
        && parent.as_ref().is_some_and(Value::is_string)
    {
        return Ok(old == new);
    }
    if old.len() != LEGACY_KEYS.len()
        || !LEGACY_KEYS.iter().all(|key| old.contains_key(*key))
        || !ROUTE_KEYS.iter().all(|key| new.contains_key(*key))
    {
        return Ok(false);
    }
    let route = ROUTE_KEYS.map(|key| new.remove(key).unwrap_or(Value::Null));
    if old != new {
        return Ok(false);
    }
    let Some(response) = response else {
        return Ok(false);
    };
    let created: CreatedSession = serde_json::from_str(response)
        .map_err(|error| corrupt(format!("invalid legacy create receipt: {error}")))?;
    let Some(parent) = parent else {
        // The old public Store seam can prove only the choices frozen in
        // its original child response. Nulls cannot discard a recorded pin
        // or endpoint, and later child metadata cannot change this proof.
        let recorded = [
            created.metadata.account_alias,
            created.metadata.provider_base_url,
            created.metadata.provider_rebind_id,
        ]
        .map(|value| value.map_or(Value::Null, Value::String));
        return Ok(route == recorded);
    };
    let Some(parent) = parent.as_str() else {
        return Ok(false);
    };
    let Some(child_provider) = old.get("provider").and_then(Value::as_str) else {
        return Ok(false);
    };
    let expected = frozen_or_historical_route(connection, &created, parent, child_provider)?;
    Ok(expected.is_some_and(|expected| route == expected))
}

/// Recover retry coordinates only from an exact pre-witness admission shape.
/// Recorded digest choices outrank a reply that predates route fields. A
/// pre-route digest uses the same frozen reply/history proof as comparison.
/// This does not change the original reply or the existing child's metadata.
pub(super) fn legacy_admission_route(
    connection: &Connection,
    stored: &str,
    created: &CreatedSession,
    parent: &str,
) -> StoreResult<Option<[Option<String>; 3]>> {
    let Ok(Value::Object(mut old)) = serde_json::from_str(stored) else {
        return Ok(None);
    };
    old.remove("max_tokens_source");
    if !old.get("delegation_agent").is_some_and(Value::is_string)
        || !LEGACY_KEYS.iter().all(|key| old.contains_key(*key))
    {
        return Ok(None);
    }
    let recorded_route = ROUTE_KEYS.iter().all(|key| old.contains_key(*key));
    let route = if recorded_route && old.len() == LEGACY_KEYS.len() + ROUTE_KEYS.len() {
        Some(ROUTE_KEYS.map(|key| old[key].clone()))
    } else if old.len() == LEGACY_KEYS.len() {
        let Some(provider) = old.get("provider").and_then(Value::as_str) else {
            return Ok(None);
        };
        frozen_or_historical_route(connection, created, parent, provider)?
    } else {
        None
    };
    route
        .map(|route| {
            let [account, endpoint, rebind] = route;
            let decode = |value| {
                serde_json::from_value::<Option<String>>(value)
                    .map_err(|error| corrupt(format!("invalid child admission route: {error}")))
            };
            Ok([decode(account)?, decode(endpoint)?, decode(rebind)?])
        })
        .transpose()
}

fn frozen_or_historical_route(
    connection: &Connection,
    created: &CreatedSession,
    parent: &str,
    child_provider: &str,
) -> StoreResult<Option<[Value; 3]>> {
    // Creation receipts freeze the initial metadata; today's parent metadata
    // would misattribute an account changed after the original child create.
    let original: Option<String> = connection.query_row(
        "SELECT response_json FROM command_receipts WHERE method IN ('session.create', 'session.fork', 'session.metafork')
         AND state = 'committed' AND session_id = ?1 ORDER BY updated_at_ms LIMIT 1",
        [parent], |row| row.get(0),
    ).optional().map_err(map_sqlite_error)?.flatten();
    let Some(original) = original else {
        return Ok(None);
    };
    let original: CreatedSession = serde_json::from_str(&original)
        .map_err(|error| corrupt(format!("invalid parent create receipt: {error}")))?;
    if original.session_id.as_str() != parent {
        return Ok(None);
    }
    let initial_seq = super::to_sqlite_integer(original.created_seq)?;
    let metadata = original.metadata;
    // rowid supplies the machine-wide commit order even when two transactions
    // have the same millisecond timestamp. The child's Created fact is the
    // exact cutoff, including the crash-before-establishment case.
    let cutoff: Option<i64> = connection
        .query_row(
            "SELECT rowid FROM events WHERE session_id = ?1 AND seq = 1",
            [created.session_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    let Some(cutoff) = cutoff else {
        return Ok(None);
    };
    let parent_created: Option<i64> = connection
        .query_row(
            "SELECT rowid FROM events WHERE session_id = ?1 AND seq = ?2",
            rusqlite::params![parent, initial_seq],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    if parent_created.is_none_or(|created| created >= cutoff) {
        return Ok(None);
    }
    // A running parent can already have a different next-turn selection when
    // its child is created. Prefer the route frozen by that child's original
    // admission; only old responses without a route need parent history.
    let recorded = [
        created.metadata.account_alias.clone(),
        created.metadata.provider_base_url.clone(),
        created.metadata.provider_rebind_id.clone(),
    ]
    .map(|value| value.map_or(Value::Null, Value::String));
    if recorded.iter().any(|value| !value.is_null()) {
        return Ok(Some(recorded));
    }
    historical_route::at_child_creation(
        connection,
        metadata,
        historical_route::Witness {
            parent,
            child: &created.session_id,
            child_provider,
            initial_seq,
            cutoff,
        },
    )
}
