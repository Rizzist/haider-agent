//! Device-local append-only usage history.
//!
//! Each UTC day is one JSONL stream beneath `usage/`. A profile lock gives
//! the stream exactly one writer; dictionary records may appear between slot
//! records, and readers fold the file rather than trusting positional blocks.

use crate::{StoreResult, store_error};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::ErrorCode;
use haider_protocol::provider::{NormalizedUsage, UsageRequestKind, UsageScope};
use haider_protocol::session_fork::SessionForked;
use haider_protocol::usage::{
    USAGE_HISTORY_MAX_RANGE_DAYS, USAGE_HISTORY_SLOTS_PER_DAY, UsageHistoryDailyTotalV1,
    UsageHistoryDayV1, UsageHistoryKeyV1, UsageHistoryMeterSampleV1, UsageHistoryRangeDayV1,
    UsageHistoryRoleV1, UsageHistoryRowV1, UsageHistorySlotV1, UsageHistoryVersionChangeV1,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const LEDGER_SCHEMA_VERSION: u32 = 1;
const MILLIS_PER_DAY: u64 = 86_400_000;
const MILLIS_PER_SLOT: u64 = 15 * 60 * 1_000;

/// Optional dimensions for one compact dictionary lane.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsageLedgerLane {
    pub account: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_family: Option<String>,
    pub effort: Option<String>,
    pub speed: Option<String>,
    pub role: UsageHistoryRoleV1,
}

/// Additive counters carried by one lane in one slot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageLedgerCounters {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

impl UsageLedgerCounters {
    fn add(&mut self, other: &Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.errors = self.errors.saturating_add(other.errors);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
}

/// One closed UTC quarter-hour ready to append.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageLedgerSlot {
    pub rows: BTreeMap<UsageLedgerLane, UsageLedgerCounters>,
    pub subagents_spawned: u64,
}

/// UTC day and quarter-hour address for a journal fact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsageSlotAddress {
    pub date: String,
    pub slot: u8,
}

/// Single-writer appender for one profile's usage streams.
#[derive(Debug)]
pub struct UsageLedgerWriter {
    root: PathBuf,
    device_id: String,
    daemon_version: String,
}

impl UsageLedgerWriter {
    pub fn new(
        profile_root: impl AsRef<Path>,
        device_id: impl Into<String>,
        daemon_version: impl Into<String>,
    ) -> Self {
        Self {
            root: profile_root.as_ref().join("usage"),
            device_id: device_id.into(),
            daemon_version: daemon_version.into(),
        }
    }

    /// Appends one slot exactly once. Dictionary records for newly seen lanes
    /// are emitted immediately before the slot, so dictionaries can grow in
    /// the middle of a day file without rewriting its prefix.
    pub fn append_slot(
        &self,
        address: &UsageSlotAddress,
        slot: &UsageLedgerSlot,
        backfilled: bool,
    ) -> StoreResult<bool> {
        validate_ledger_device_id(&self.device_id)?;
        validate_date(&address.date)?;
        if usize::from(address.slot) >= USAGE_HISTORY_SLOTS_PER_DAY {
            return Err(invalid("usage-history slot must be in 0..96"));
        }
        fs::create_dir_all(&self.root).map_err(io_error("create usage-history directory"))?;
        let path = self.day_path(&address.date);
        self.ensure_header(&path, &address.date, backfilled)?;
        let state = read_day_state(&path, &self.device_id)?;
        if state.sampled_slots.contains(&address.slot) {
            return Ok(false);
        }

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(io_error("open usage-history day for append"))?;
        if !backfilled && state.last_daemon_version.as_deref() != Some(&self.daemon_version) {
            append_json_line(
                &mut file,
                &json!({
                    "t": "v",
                    "daemon_version": self.daemon_version,
                    "at_ms": slot_start_ms(&address.date, address.slot)?,
                }),
            )?;
        }

        let mut known = state.lanes;
        let mut next_id = state.next_key_id;
        let mut encoded_rows = Vec::with_capacity(slot.rows.len());
        for (lane, counters) in &slot.rows {
            let key_id = if let Some(key_id) = known.get(lane) {
                *key_id
            } else {
                let key_id = next_id;
                next_id = next_id
                    .checked_add(1)
                    .ok_or_else(|| corrupt("usage-history dictionary id space is exhausted"))?;
                append_json_line(&mut file, &key_record(key_id, lane))?;
                known.insert(lane.clone(), key_id);
                key_id
            };
            encoded_rows.push(json!([
                key_id,
                role_name(lane.role),
                counters.requests,
                counters.errors,
                counters.input_tokens,
                counters.output_tokens,
                counters.cache_read_tokens,
                counters.cache_write_tokens,
                counters.reasoning_tokens,
            ]));
        }
        append_json_line(
            &mut file,
            &json!({
                "t": "s",
                "slot": address.slot,
                "rows": encoded_rows,
                "sp": slot.subagents_spawned,
            }),
        )?;
        file.sync_data()
            .map_err(io_error("sync usage-history slot"))?;
        Ok(true)
    }

    /// Creates only the first-line day header. Backfill uses this for a
    /// currently open slot so the eventual close remains appendable while
    /// the day still records that older journal facts seeded it.
    pub fn ensure_day(&self, date: &str, backfilled: bool) -> StoreResult<()> {
        validate_ledger_device_id(&self.device_id)?;
        validate_date(date)?;
        fs::create_dir_all(&self.root).map_err(io_error("create usage-history directory"))?;
        let path = self.day_path(date);
        self.ensure_header(&path, date, backfilled)?;
        let _ = read_day_state(&path, &self.device_id)?;
        Ok(())
    }

    /// Appends one opportunistic provider reading. The supplied integer basis
    /// points ride the file verbatim; this seam performs no denominator math.
    pub fn append_meter_sample(&self, sample: &UsageHistoryMeterSampleV1) -> StoreResult<()> {
        validate_ledger_device_id(&self.device_id)?;
        if sample.basis_points > 10_000 {
            return Err(invalid("meter basis points must be in 0..=10000"));
        }
        let address = slot_address(sample.sampled_at_ms);
        fs::create_dir_all(&self.root).map_err(io_error("create usage-history directory"))?;
        let path = self.day_path(&address.date);
        self.ensure_header(&path, &address.date, false)?;
        let state = read_day_state(&path, &self.device_id)?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(io_error("open usage-history day for meter append"))?;
        if state.last_daemon_version.as_deref() != Some(&self.daemon_version) {
            append_json_line(
                &mut file,
                &json!({
                    "t": "v",
                    "daemon_version": self.daemon_version,
                    "at_ms": sample.sampled_at_ms,
                }),
            )?;
        }
        let mut value = json!({
            "t": "m",
            "account": sample.account,
            "window": sample.window,
            "bp": sample.basis_points,
            "sampled_at_ms": sample.sampled_at_ms,
        });
        let object = value
            .as_object_mut()
            .ok_or_else(|| corrupt("meter record did not encode as an object"))?;
        insert_optional(object, "resets_at_ms", sample.resets_at_ms);
        insert_optional_clone(object, "plan", sample.plan.as_ref());
        insert_optional(object, "stale", sample.stale);
        append_json_line(&mut file, &value)?;
        file.sync_data()
            .map_err(io_error("sync usage-history meter sample"))
    }

    fn day_path(&self, date: &str) -> PathBuf {
        self.root.join(format!("{date}.jsonl"))
    }

    fn ensure_header(&self, path: &Path, date: &str, backfilled: bool) -> StoreResult<()> {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                let mut header = json!({
                    "t": "h",
                    "v": LEDGER_SCHEMA_VERSION,
                    "date": date,
                    "device_id": self.device_id,
                    "daemon_version": self.daemon_version,
                });
                if backfilled {
                    header
                        .as_object_mut()
                        .ok_or_else(|| corrupt("header did not encode as an object"))?
                        .insert("backfilled".into(), Value::Bool(true));
                }
                append_json_line(&mut file, &header)?;
                file.sync_all()
                    .map_err(io_error("sync usage-history day header"))?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(io_error("create usage-history day")(error)),
        }
    }
}

fn key_record(id: u32, lane: &UsageLedgerLane) -> Value {
    let mut value = json!({"t": "k", "id": id});
    if let Some(object) = value.as_object_mut() {
        insert_optional_clone(object, "account", lane.account.as_ref());
        insert_optional_clone(object, "provider", lane.provider.as_ref());
        insert_optional_clone(object, "model", lane.model.as_ref());
        insert_optional_clone(object, "api_family", lane.api_family.as_ref());
        insert_optional_clone(object, "effort", lane.effort.as_ref());
        insert_optional_clone(object, "speed", lane.speed.as_ref());
    }
    value
}

fn insert_optional<T: serde::Serialize>(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
    {
        object.insert(key.to_owned(), value);
    }
}

fn insert_optional_clone<T: serde::Serialize>(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<&T>,
) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
    {
        object.insert(key.to_owned(), value);
    }
}

fn append_json_line(file: &mut File, value: &Value) -> StoreResult<()> {
    serde_json::to_writer(&mut *file, value).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!("cannot encode usage-history record: {error}"),
            false,
        )
    })?;
    file.write_all(b"\n")
        .map_err(io_error("append usage-history record"))
}

#[derive(Default)]
struct DayState {
    lanes: HashMap<UsageLedgerLane, u32>,
    sampled_slots: BTreeSet<u8>,
    next_key_id: u32,
    last_daemon_version: Option<String>,
}

fn read_day_state(path: &Path, expected_device_id: &str) -> StoreResult<DayState> {
    let day = read_day_file(path)?;
    if day.device_id != expected_device_id {
        return Err(corrupt(format!(
            "usage-history day {} belongs to device {}, expected {}",
            path.display(),
            day.device_id,
            expected_device_id
        )));
    }
    let mut roles = HashMap::<u32, UsageHistoryRoleV1>::new();
    let mut sampled_slots = BTreeSet::new();
    for (index, slot) in day.slots.iter().enumerate() {
        if let Some(slot) = slot {
            sampled_slots.insert(u8::try_from(index).unwrap_or(u8::MAX));
            for row in &slot.rows {
                roles.entry(row.key_id).or_insert(row.role);
            }
        }
    }
    let mut lanes = HashMap::new();
    let mut next_key_id = 1;
    for key in &day.keys {
        next_key_id = next_key_id.max(key.id.saturating_add(1));
        if let Some(role) = roles.get(&key.id) {
            lanes.insert(
                UsageLedgerLane {
                    account: key.account.clone(),
                    provider: key.provider.clone(),
                    model: key.model.clone(),
                    api_family: key.api_family.clone(),
                    effort: key.effort.clone(),
                    speed: key.speed.clone(),
                    role: *role,
                },
                key.id,
            );
        }
    }
    let last_daemon_version = day
        .version_changes
        .last()
        .map(|change| change.daemon_version.clone());
    let last_daemon_version = match last_daemon_version {
        Some(version) => Some(version),
        None => read_header_version(path)?,
    };
    Ok(DayState {
        lanes,
        sampled_slots,
        next_key_id,
        last_daemon_version,
    })
}

fn read_header_version(path: &Path) -> StoreResult<Option<String>> {
    let file = File::open(path).map_err(io_error("open usage-history header"))?;
    let line = BufReader::new(file)
        .lines()
        .next()
        .transpose()
        .map_err(io_error("read usage-history header"))?;
    let Some(line) = line else { return Ok(None) };
    let value: Value = serde_json::from_str(&line)
        .map_err(|_| corrupt("usage-history header is not valid JSON"))?;
    Ok(value
        .get("daemon_version")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// Reads and folds one UTC day. A missing file is successful absence.
pub fn read_usage_day(profile_root: &Path, date: &str) -> StoreResult<Option<UsageHistoryDayV1>> {
    validate_date(date)?;
    let path = profile_root.join("usage").join(format!("{date}.jsonl"));
    if !path.exists() {
        return Ok(None);
    }
    let day = read_day_file(&path)?;
    if day.date != date {
        return Err(corrupt(
            "usage-history header date does not match its filename",
        ));
    }
    Ok(Some(day))
}

/// Reads exactly `days` dated heatmap cells ending at `through_date`.
/// Missing files remain `total=None`; no zero-filled day is fabricated.
pub fn read_usage_range(
    profile_root: &Path,
    through_date: &str,
    days: u16,
) -> StoreResult<Vec<UsageHistoryRangeDayV1>> {
    read_usage_range_inner(profile_root, through_date, days, None)
}

pub(crate) fn read_usage_range_for_device(
    profile_root: &Path,
    through_date: &str,
    days: u16,
    expected_device_id: &str,
) -> StoreResult<Vec<UsageHistoryRangeDayV1>> {
    read_usage_range_inner(profile_root, through_date, days, Some(expected_device_id))
}

fn read_usage_range_inner(
    profile_root: &Path,
    through_date: &str,
    days: u16,
    expected_device_id: Option<&str>,
) -> StoreResult<Vec<UsageHistoryRangeDayV1>> {
    if days == 0 || days > USAGE_HISTORY_MAX_RANGE_DAYS {
        return Err(invalid(format!(
            "usage-history range days must be in 1..={USAGE_HISTORY_MAX_RANGE_DAYS}"
        )));
    }
    let through = days_from_date(through_date)?;
    let first = through
        .checked_sub(i64::from(days) - 1)
        .ok_or_else(|| invalid("usage-history range is before the supported calendar"))?;
    let mut range = Vec::with_capacity(usize::from(days));
    for offset in 0..days {
        let date = date_from_days(first + i64::from(offset));
        let day = read_usage_day(profile_root, &date)?;
        if let (Some(expected), Some(day)) = (expected_device_id, day.as_ref())
            && day.device_id != expected
        {
            return Err(corrupt(format!(
                "usage-history day {date} belongs to device {}, expected {expected}",
                day.device_id
            )));
        }
        let total = day.as_ref().and_then(fold_daily_total);
        range.push(UsageHistoryRangeDayV1 { date, total });
    }
    Ok(range)
}

fn fold_daily_total(day: &UsageHistoryDayV1) -> Option<UsageHistoryDailyTotalV1> {
    let mut total = UsageHistoryDailyTotalV1::default();
    for slot in day.slots.iter().flatten() {
        total.sampled_slots = total.sampled_slots.saturating_add(1);
        total.subagents_spawned = total
            .subagents_spawned
            .saturating_add(slot.subagents_spawned);
        for row in &slot.rows {
            total.requests = total.requests.saturating_add(row.requests);
            total.errors = total.errors.saturating_add(row.errors);
            total.input_tokens = total.input_tokens.saturating_add(row.input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(row.output_tokens);
            total.cache_read_tokens = total
                .cache_read_tokens
                .saturating_add(row.cache_read_tokens);
            total.cache_write_tokens = total
                .cache_write_tokens
                .saturating_add(row.cache_write_tokens);
            total.reasoning_tokens = total.reasoning_tokens.saturating_add(row.reasoning_tokens);
        }
    }
    (total.sampled_slots > 0).then_some(total)
}

fn read_day_file(path: &Path) -> StoreResult<UsageHistoryDayV1> {
    let file = File::open(path).map_err(io_error("open usage-history day"))?;
    let mut date = None;
    let mut device_id = None;
    let mut backfilled = false;
    let mut keys = BTreeMap::<u32, UsageHistoryKeyV1>::new();
    let mut slots = vec![None; USAGE_HISTORY_SLOTS_PER_DAY];
    let mut meter_samples = Vec::new();
    let mut version_changes = Vec::new();
    let mut saw_header = false;

    for (line_number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(io_error("read usage-history day"))?;
        let value: Value = serde_json::from_str(&line).map_err(|_| {
            corrupt(format!(
                "usage-history line {} in {} is not valid JSON",
                line_number + 1,
                path.display()
            ))
        })?;
        let kind = value.get("t").and_then(Value::as_str).ok_or_else(|| {
            corrupt(format!(
                "usage-history line {} has no record type",
                line_number + 1
            ))
        })?;
        if !saw_header && kind != "h" {
            return Err(corrupt("usage-history first line is not a day header"));
        }
        match kind {
            "h" => {
                if saw_header || line_number != 0 {
                    return Err(corrupt("usage-history contains a duplicate day header"));
                }
                let version = required_u64(&value, "v")?;
                if version != u64::from(LEDGER_SCHEMA_VERSION) {
                    return Err(corrupt(format!(
                        "unsupported usage-history schema version {version}"
                    )));
                }
                date = Some(required_string(&value, "date")?);
                device_id = Some(required_string(&value, "device_id")?);
                backfilled = value
                    .get("backfilled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                saw_header = true;
            }
            "k" => {
                let id = u32::try_from(required_u64(&value, "id")?)
                    .map_err(|_| corrupt("usage-history key id exceeds u32"))?;
                if id == 0 || keys.contains_key(&id) {
                    return Err(corrupt("usage-history dictionary id is zero or duplicated"));
                }
                keys.insert(
                    id,
                    UsageHistoryKeyV1 {
                        id,
                        account: optional_string(&value, "account")?,
                        provider: optional_string(&value, "provider")?,
                        model: optional_string(&value, "model")?,
                        api_family: optional_string(&value, "api_family")?,
                        effort: optional_string(&value, "effort")?,
                        speed: optional_string(&value, "speed")?,
                    },
                );
            }
            "s" => {
                let slot_index = usize::try_from(required_u64(&value, "slot")?)
                    .map_err(|_| corrupt("usage-history slot index exceeds usize"))?;
                if slot_index >= USAGE_HISTORY_SLOTS_PER_DAY || slots[slot_index].is_some() {
                    return Err(corrupt("usage-history slot index is invalid or duplicated"));
                }
                let raw_rows = value
                    .get("rows")
                    .and_then(Value::as_array)
                    .ok_or_else(|| corrupt("usage-history slot rows are not an array"))?;
                let mut rows = Vec::with_capacity(raw_rows.len());
                for raw in raw_rows {
                    rows.push(decode_row(raw, &keys)?);
                }
                slots[slot_index] = Some(UsageHistorySlotV1 {
                    rows,
                    subagents_spawned: value.get("sp").and_then(Value::as_u64).unwrap_or(0),
                });
            }
            "m" => {
                let basis_points = u32::try_from(required_u64(&value, "bp")?)
                    .map_err(|_| corrupt("usage-history meter basis points exceed u32"))?;
                if basis_points > 10_000 {
                    return Err(corrupt("usage-history meter basis points exceed 10000"));
                }
                meter_samples.push(UsageHistoryMeterSampleV1 {
                    account: required_string(&value, "account")?,
                    window: required_string(&value, "window")?,
                    basis_points,
                    resets_at_ms: optional_u64(&value, "resets_at_ms")?,
                    sampled_at_ms: required_u64(&value, "sampled_at_ms")?,
                    plan: optional_string(&value, "plan")?,
                    stale: optional_bool(&value, "stale")?,
                });
            }
            "v" => version_changes.push(UsageHistoryVersionChangeV1 {
                daemon_version: required_string(&value, "daemon_version")?,
                changed_at_ms: required_u64(&value, "at_ms")?,
            }),
            _ => {}
        }
    }
    if !saw_header {
        return Err(corrupt("usage-history day is empty"));
    }
    let date = date.ok_or_else(|| corrupt("usage-history header has no date"))?;
    validate_date(&date).map_err(|_| corrupt("usage-history header date is invalid"))?;
    let device_id = device_id.ok_or_else(|| corrupt("usage-history header has no device id"))?;
    validate_ledger_device_id(&device_id)
        .map_err(|_| corrupt("usage-history header device id is invalid"))?;
    let mut roles = HashMap::<u32, UsageHistoryRoleV1>::new();
    for slot in slots.iter().flatten() {
        for row in &slot.rows {
            if let Some(previous) = roles.insert(row.key_id, row.role)
                && previous != row.role
            {
                return Err(corrupt(
                    "usage-history dictionary id is reused across lane roles",
                ));
            }
        }
    }
    Ok(UsageHistoryDayV1 {
        date,
        device_id,
        backfilled,
        keys: keys.into_values().collect(),
        slots,
        meter_samples,
        version_changes,
    })
}

fn decode_row(
    value: &Value,
    keys: &BTreeMap<u32, UsageHistoryKeyV1>,
) -> StoreResult<UsageHistoryRowV1> {
    let values = value
        .as_array()
        .ok_or_else(|| corrupt("usage-history row is not an array"))?;
    if !(4..=9).contains(&values.len()) {
        return Err(corrupt("usage-history row must contain 4 through 9 fields"));
    }
    let key_id = values[0]
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| corrupt("usage-history row has an invalid key id"))?;
    if !keys.contains_key(&key_id) {
        return Err(corrupt("usage-history row references an unknown key id"));
    }
    let role = match values[1].as_str() {
        Some("root") => UsageHistoryRoleV1::Root,
        Some("subagent") => UsageHistoryRoleV1::Subagent,
        Some(_) => UsageHistoryRoleV1::Unknown,
        None => return Err(corrupt("usage-history row role is not a string")),
    };
    let number = |index: usize| -> StoreResult<u64> {
        values.get(index).map_or(Ok(0), |value| {
            value
                .as_u64()
                .ok_or_else(|| corrupt("usage-history row counter is not an integer"))
        })
    };
    Ok(UsageHistoryRowV1 {
        key_id,
        role,
        requests: number(2)?,
        errors: number(3)?,
        input_tokens: number(4)?,
        output_tokens: number(5)?,
        cache_read_tokens: number(6)?,
        cache_write_tokens: number(7)?,
        reasoning_tokens: number(8)?,
    })
}

/// Reduces journal usage snapshots into slot rollups. Modern response-local
/// records replace by request ordinal; legacy cumulative records replace by
/// their `(session, run, agent, provider, model, cache epoch, request kind)`
/// lane so intermediate snapshots are never summed twice.
pub fn reduce_journal_usage(
    envelopes: &[RawEnvelope],
) -> BTreeMap<UsageSlotAddress, UsageLedgerSlot> {
    let fork_boundaries = envelopes
        .iter()
        .filter_map(|envelope| {
            SessionForked::from_payload_value(&envelope.payload)
                .map(|_| (envelope.session_id.clone(), envelope.seq))
        })
        .collect::<HashMap<_, _>>();
    let mut chunks = BTreeMap::<ChunkKey, ObservedChunk>::new();
    let mut spawns = BTreeMap::<UsageSlotAddress, u64>::new();
    let mut errors = BTreeMap::<(UsageSlotAddress, UsageLedgerLane), u64>::new();
    for envelope in envelopes {
        if fork_boundaries
            .get(&envelope.session_id)
            .is_some_and(|audit_seq| envelope.seq < *audit_seq)
        {
            // Forks physically copy the source prefix. The audit immediately
            // after that prefix is the boundary between inherited history and
            // work truly performed by the child; counting the prefix again
            // would duplicate the source device's usage.
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        match payload {
            EventPayload::Usage(usage) => {
                let request_ordinal = usage.request.as_ref().map(|request| request.ordinal);
                if let Some(request) = usage.request {
                    let scope = usage.scope.as_ref();
                    let account = request.account.or(usage.account);
                    let counters = usage_counters(
                        request.input,
                        request.output,
                        request.reasoning.unwrap_or(0),
                        request.cached.unwrap_or(0),
                        request.normalized.as_ref(),
                    );
                    insert_chunk(
                        &mut chunks,
                        envelope,
                        scope,
                        account.map(|alias| alias.as_str().to_owned()),
                        request_ordinal,
                        counters,
                    );
                } else if !usage.accounts.is_empty() {
                    for subtotal in usage.accounts {
                        let counters = usage_counters(
                            subtotal.input,
                            subtotal.output,
                            subtotal.reasoning,
                            subtotal.cached,
                            subtotal.normalized.as_ref(),
                        );
                        insert_chunk(
                            &mut chunks,
                            envelope,
                            subtotal.scope.as_ref(),
                            Some(subtotal.account.as_str().to_owned()),
                            request_ordinal,
                            counters,
                        );
                    }
                } else if let Some(account) = usage.account {
                    let counters = usage_counters(
                        usage.input,
                        usage.output,
                        usage.reasoning,
                        usage.cached,
                        usage.normalized.as_ref(),
                    );
                    insert_chunk(
                        &mut chunks,
                        envelope,
                        usage.scope.as_ref(),
                        Some(account.as_str().to_owned()),
                        request_ordinal,
                        counters,
                    );
                }
            }
            EventPayload::AgentSpawned(_) => {
                let address = slot_address(envelope.committed_at_ms);
                let count = spawns.entry(address).or_default();
                *count = count.saturating_add(1);
            }
            EventPayload::RunFailed { .. } => {
                // RunFailed has no request/account/provider/model coordinates
                // and also represents recovery and tool failures. Assigning it
                // to the most recent request would fabricate dimensions after
                // rotation, so preserve only the exact role coordinate.
                let lane = UsageLedgerLane {
                    role: if envelope.agent_id.is_some() {
                        UsageHistoryRoleV1::Subagent
                    } else {
                        UsageHistoryRoleV1::Root
                    },
                    ..UsageLedgerLane::default()
                };
                let count = errors
                    .entry((slot_address(envelope.committed_at_ms), lane))
                    .or_default();
                *count = count.saturating_add(1);
            }
            _ => {}
        }
    }
    let mut slots = BTreeMap::<UsageSlotAddress, UsageLedgerSlot>::new();
    for chunk in chunks.into_values() {
        slots
            .entry(chunk.address)
            .or_default()
            .rows
            .entry(chunk.lane)
            .or_default()
            .add(&chunk.counters);
    }
    for (address, spawned) in spawns {
        slots.entry(address).or_default().subagents_spawned = spawned;
    }
    for ((address, lane), count) in errors {
        let counters = slots
            .entry(address)
            .or_default()
            .rows
            .entry(lane)
            .or_default();
        counters.errors = counters.errors.saturating_add(count);
    }
    slots
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ChunkKey {
    session: String,
    run: String,
    agent: String,
    provider: String,
    model: String,
    cache_epoch: String,
    request_kind: UsageRequestKind,
    request_ordinal: Option<u64>,
    account: String,
}

struct ObservedChunk {
    address: UsageSlotAddress,
    lane: UsageLedgerLane,
    counters: UsageLedgerCounters,
}

fn insert_chunk(
    chunks: &mut BTreeMap<ChunkKey, ObservedChunk>,
    envelope: &RawEnvelope,
    scope: Option<&UsageScope>,
    account: Option<String>,
    request_ordinal: Option<u64>,
    counters: UsageLedgerCounters,
) {
    let provider = scope.map_or_else(String::new, |scope| scope.provider.clone());
    let model = scope.map_or_else(String::new, |scope| scope.model.clone());
    let agent = scope
        .and_then(|scope| scope.agent.as_ref())
        .or(envelope.agent_id.as_ref())
        .map_or_else(String::new, |agent| agent.as_str().to_owned());
    let key = ChunkKey {
        session: envelope.session_id.as_str().to_owned(),
        run: scope
            .and_then(|scope| scope.run.as_ref())
            .or(envelope.run_id.as_ref())
            .map_or_else(String::new, |run| run.as_str().to_owned()),
        agent: agent.clone(),
        provider: provider.clone(),
        model: model.clone(),
        cache_epoch: scope.map_or_else(String::new, |scope| scope.cache_epoch.clone()),
        request_kind: scope.map_or(UsageRequestKind::MainTurn, |scope| scope.request_kind),
        request_ordinal,
        account: account.clone().unwrap_or_default(),
    };
    let lane = UsageLedgerLane {
        account,
        provider: (!provider.is_empty()).then_some(provider),
        model: (!model.is_empty()).then_some(model),
        // New usage facts carry adapter-owned dimensions; historical facts
        // deserialize these fields as absent, which backfill preserves.
        api_family: scope.and_then(|scope| scope.api_family.clone()),
        effort: scope.and_then(|scope| scope.effort.clone()),
        speed: scope.and_then(|scope| scope.speed.clone()),
        role: if agent.is_empty() {
            UsageHistoryRoleV1::Root
        } else {
            UsageHistoryRoleV1::Subagent
        },
    };
    chunks.insert(
        key,
        ObservedChunk {
            address: slot_address(envelope.committed_at_ms),
            lane,
            counters,
        },
    );
}

fn usage_counters(
    input: u64,
    output: u64,
    reasoning: u64,
    cached: u64,
    normalized: Option<&NormalizedUsage>,
) -> UsageLedgerCounters {
    UsageLedgerCounters {
        requests: 1,
        errors: 0,
        input_tokens: normalized.map_or(input, |usage| usage.logical_input),
        output_tokens: normalized.map_or(output, |usage| usage.billed_output),
        cache_read_tokens: normalized.map_or(cached, |usage| usage.cache_read_input),
        cache_write_tokens: normalized.map_or(0, |usage| usage.cache_write_input),
        reasoning_tokens: reasoning,
    }
}

fn role_name(role: UsageHistoryRoleV1) -> &'static str {
    match role {
        UsageHistoryRoleV1::Root => "root",
        UsageHistoryRoleV1::Subagent => "subagent",
        _ => "unknown",
    }
}

pub fn slot_address(timestamp_ms: u64) -> UsageSlotAddress {
    let day = timestamp_ms / MILLIS_PER_DAY;
    let within_day = timestamp_ms % MILLIS_PER_DAY;
    UsageSlotAddress {
        date: date_from_days(i64::try_from(day).unwrap_or(i64::MAX)),
        slot: u8::try_from(within_day / MILLIS_PER_SLOT).unwrap_or(95),
    }
}

pub(crate) fn slot_start_ms(date: &str, slot: u8) -> StoreResult<u64> {
    let days = days_from_date(date)?;
    let days = u64::try_from(days)
        .map_err(|_| invalid("usage-history dates before 1970 are not supported"))?;
    Ok(days
        .saturating_mul(MILLIS_PER_DAY)
        .saturating_add(u64::from(slot).saturating_mul(MILLIS_PER_SLOT)))
}

fn validate_date(date: &str) -> StoreResult<()> {
    let _ = days_from_date(date)?;
    Ok(())
}

fn validate_ledger_device_id(device_id: &str) -> StoreResult<()> {
    let suffix = device_id
        .strip_prefix("dev-")
        .ok_or_else(|| invalid("usage-history device id must start with dev-"))?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "usage-history device id must contain 32 lowercase hex digits",
        ));
    }
    Ok(())
}

fn days_from_date(date: &str) -> StoreResult<i64> {
    if !date.is_ascii() || date.len() != 10 || &date[4..5] != "-" || &date[7..8] != "-" {
        return Err(invalid("usage-history date must be YYYY-MM-DD"));
    }
    let year = date[0..4]
        .parse::<i64>()
        .map_err(|_| invalid("usage-history date has an invalid year"))?;
    let month = date[5..7]
        .parse::<u32>()
        .map_err(|_| invalid("usage-history date has an invalid month"))?;
    let day = date[8..10]
        .parse::<u32>()
        .map_err(|_| invalid("usage-history date has an invalid day"))?;
    if year < 1970 || !(1..=12).contains(&month) {
        return Err(invalid(
            "usage-history date is outside the supported calendar",
        ));
    }
    let month_days = [
        31,
        28 + u32::from(is_leap(year)),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > month_days[usize::try_from(month - 1).unwrap_or(0)] {
        return Err(invalid("usage-history date has an invalid day"));
    }
    Ok(days_from_civil(year, month, day))
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn date_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn required_string(value: &Value, key: &str) -> StoreResult<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt(format!("usage-history field `{key}` is not a string")))
}

fn optional_string(value: &Value, key: &str) -> StoreResult<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(corrupt(format!(
            "usage-history field `{key}` is not a string"
        ))),
    }
}

fn required_u64(value: &Value, key: &str) -> StoreResult<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| corrupt(format!("usage-history field `{key}` is not an integer")))
}

fn optional_u64(value: &Value, key: &str) -> StoreResult<Option<u64>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| corrupt(format!("usage-history field `{key}` is not an integer"))),
    }
}

fn optional_bool(value: &Value, key: &str) -> StoreResult<Option<bool>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| corrupt(format!("usage-history field `{key}` is not a boolean"))),
    }
}

fn invalid(message: impl Into<String>) -> haider_protocol::error::HaiderError {
    store_error(ErrorCode::InvalidArgument, message, false)
}

fn corrupt(message: impl Into<String>) -> haider_protocol::error::HaiderError {
    store_error(ErrorCode::StoreCorrupt, message, false)
}

fn io_error(
    operation: &'static str,
) -> impl FnOnce(std::io::Error) -> haider_protocol::error::HaiderError {
    move |error| {
        store_error(
            ErrorCode::Internal,
            format!("cannot {operation}: {error}"),
            error.kind() == std::io::ErrorKind::Interrupted,
        )
    }
}
