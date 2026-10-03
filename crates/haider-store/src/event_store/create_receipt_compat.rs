//! Upgrade equivalence for the exact delegated-create schema before explicit
//! route inheritance joined its digest. New-schema receipts stay byte strict.
use super::{CreatedSession, StoreResult, corrupt, decode_envelope_column, map_sqlite_error};
use haider_protocol::session::{ModelSelected, SessionProviderRebound};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

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
    let (Ok(Value::Object(old)), Ok(Value::Object(mut new))) =
        (serde_json::from_str(stored), serde_json::from_str(incoming))
    else {
        return Ok(false);
    };
    if !old.get("delegation_agent").is_some_and(Value::is_string) {
        return Ok(false);
    }
    if let Some(source) = new.remove("max_tokens_source") {
        let Some(response) = response else {
            return Ok(false);
        };
        let created: CreatedSession = serde_json::from_str(response)
            .map_err(|error| corrupt(format!("invalid legacy create receipt: {error}")))?;
        let Ok(source) = serde_json::from_value::<
            Option<haider_protocol::output_budget::SessionOutputBudgetSourceV1>,
        >(source) else {
            return Ok(false);
        };
        if haider_protocol::output_budget::SessionOutputBudgetSourceV1::classify(
            source,
            created.metadata.max_tokens,
        ) != haider_protocol::output_budget::SessionOutputBudgetSourceV1::classify(
            created.metadata.max_tokens_source,
            created.metadata.max_tokens,
        ) {
            return Ok(false);
        }
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
    // Creation receipts freeze the initial metadata; today's parent metadata
    // would misattribute an account changed after the original child create.
    let original: Option<String> = connection.query_row(
        "SELECT response_json FROM command_receipts WHERE method IN ('session.create', 'session.fork', 'session.metafork')
         AND state = 'committed' AND session_id = ?1 ORDER BY updated_at_ms LIMIT 1",
        [parent], |row| row.get(0),
    ).optional().map_err(map_sqlite_error)?.flatten();
    let Some(original) = original else {
        return Ok(false);
    };
    let original: CreatedSession = serde_json::from_str(&original)
        .map_err(|error| corrupt(format!("invalid parent create receipt: {error}")))?;
    if original.session_id.as_str() != parent {
        return Ok(false);
    }
    let initial_seq = super::to_sqlite_integer(original.created_seq)?;
    let mut metadata = original.metadata;
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
        return Ok(false);
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
        return Ok(false);
    }
    let mut statement = connection.prepare(
        "SELECT envelope_json FROM events WHERE session_id = ?1 AND rowid < ?2 AND seq > ?3
         AND (payload_kind IN ('model_selected', 'session_provider_rebound') OR payload_kind IS NULL) ORDER BY seq",
    ).map_err(map_sqlite_error)?;
    let mut events = statement
        .query(rusqlite::params![parent, cutoff, initial_seq])
        .map_err(map_sqlite_error)?;
    while let Some(row) = events.next().map_err(map_sqlite_error)? {
        let event = decode_envelope_column(connection, row, 0)
            .map_err(|error| corrupt(format!("invalid inheritance event: {error}")))?;
        if let Some(rebound) = SessionProviderRebound::from_payload_value(&event.payload) {
            rebound.apply_to_metadata(&mut metadata);
        } else if let Some(selected) = ModelSelected::from_payload_value(&event.payload) {
            if metadata.provider != selected.provider {
                // The ten-key receipt was written before route inheritance.
                // All events before its child's Created cutoff therefore use
                // the old daemon rule (v0.0.972 / 8ffc5866): creation pins
                // survive a provider pick; rebind pins do not. Events record
                // a payload schema version, not the writing daemon version.
                // Newer receipt schemas never enter this reconstruction.
                if metadata.provider_rebind_id.is_some() {
                    metadata.account_alias = None;
                }
                metadata.provider_base_url = None;
                metadata.provider_rebind_id = None;
            }
            metadata.provider = selected.provider;
            metadata.model = selected.model;
        }
    }
    let expected =
        if old.get("provider").and_then(Value::as_str) == Some(metadata.provider.as_str()) {
            [
                metadata.account_alias,
                metadata.provider_base_url,
                metadata.provider_rebind_id,
            ]
            .map(|value| value.map_or(Value::Null, Value::String))
        } else {
            [Value::Null, Value::Null, Value::Null]
        };
    Ok(route == expected)
}
