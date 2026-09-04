#![allow(clippy::expect_used)]

use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::usage::{UsageHistoryMeterSampleV1, UsageHistoryRoleV1};
use haider_store::{
    EventStore, Store, UsageLedgerCounters, UsageLedgerLane, UsageLedgerSlot, UsageLedgerWriter,
    UsageSlotAddress, read_usage_day, read_usage_range, reduce_journal_usage,
};
use std::fs;

fn root_lane(model: &str) -> UsageLedgerLane {
    UsageLedgerLane {
        account: Some("account-main".into()),
        provider: Some("anthropic-oauth".into()),
        model: Some(model.into()),
        api_family: Some("anthropic_messages".into()),
        effort: Some("high".into()),
        speed: None,
        role: UsageHistoryRoleV1::Root,
    }
}

fn slot(
    lanes: impl IntoIterator<Item = (UsageLedgerLane, UsageLedgerCounters)>,
) -> UsageLedgerSlot {
    UsageLedgerSlot {
        rows: lanes.into_iter().collect(),
        subagents_spawned: 0,
    }
}

#[test]
fn missing_grid_cells_and_sampled_zero_stay_distinct() {
    let root = tempfile::tempdir().expect("temp profile");
    let writer =
        UsageLedgerWriter::new(root.path(), "dev-0123456789abcdef0123456789abcdef", "test");
    let address = UsageSlotAddress {
        date: "2026-08-24".into(),
        slot: 1,
    };
    writer
        .append_slot(
            &address,
            &slot([(root_lane("claude-test"), UsageLedgerCounters::default())]),
            false,
        )
        .expect("append sampled zero");

    let day = read_usage_day(root.path(), "2026-08-24")
        .expect("read day")
        .expect("day exists");
    assert_eq!(day.slots.len(), 96);
    assert!(day.slots[0].is_none(), "slot zero was not sampled");
    let sampled = day.slots[1].as_ref().expect("slot one was sampled");
    assert_eq!(sampled.rows[0].requests, 0, "sampled zero stays zero");
    let range = read_usage_range(root.path(), "2026-08-24", 2).expect("read range");
    assert!(range[0].total.is_none(), "missing day stays absent");
    let total = range[1].total.as_ref().expect("sampled-zero day total");
    assert_eq!(total.sampled_slots, 1);
    assert_eq!(total.requests, 0);

    // MUTATION CHECK: zero-filling absent slots makes `slots[0].is_none()`
    // fail; zero-filling range days also makes `range[0].total.is_none()` fail.
}

#[test]
fn dictionary_can_append_mid_file_and_roles_get_distinct_keys() {
    let root = tempfile::tempdir().expect("temp profile");
    let device = "dev-11111111111111111111111111111111";
    let writer = UsageLedgerWriter::new(root.path(), device, "1.0.0");
    writer
        .append_slot(
            &UsageSlotAddress {
                date: "2026-08-24".into(),
                slot: 2,
            },
            &slot([(
                root_lane("claude-test"),
                UsageLedgerCounters {
                    requests: 1,
                    ..UsageLedgerCounters::default()
                },
            )]),
            false,
        )
        .expect("first slot");
    let mut subagent = root_lane("claude-test");
    subagent.role = UsageHistoryRoleV1::Subagent;
    writer
        .append_slot(
            &UsageSlotAddress {
                date: "2026-08-24".into(),
                slot: 3,
            },
            &slot([(
                subagent,
                UsageLedgerCounters {
                    requests: 1,
                    ..UsageLedgerCounters::default()
                },
            )]),
            false,
        )
        .expect("second slot");

    let text = fs::read_to_string(root.path().join("usage/2026-08-24.jsonl")).expect("read JSONL");
    let record_types = text
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("JSON line")["t"]
                .as_str()
                .expect("type")
                .to_owned()
        })
        .collect::<Vec<_>>();
    let first_slot = record_types
        .iter()
        .position(|kind| kind == "s")
        .expect("slot");
    assert!(
        record_types[first_slot + 1..]
            .iter()
            .any(|kind| kind == "k"),
        "the second lane descriptor must append after the first slot"
    );

    let day = read_usage_day(root.path(), "2026-08-24")
        .expect("read day")
        .expect("day exists");
    let root_id = day.slots[2].as_ref().expect("root slot").rows[0].key_id;
    let subagent_id = day.slots[3].as_ref().expect("subagent slot").rows[0].key_id;
    assert_ne!(root_id, subagent_id, "roles must not share a dictionary id");
}

/// Every physical request ordinal survives the journal reducer as one request
/// in its attributed lane, and the range reader exposes provider/model rows
/// ordered by descending token total.
///
/// MUTATION CHECK: remove `request_ordinal` from `ChunkKey`; the two updates
/// replace one another and the exact requests/input pins fail.
#[test]
fn physical_requests_land_in_attributed_model_rows() {
    let usage_event = |id: &str, ordinal: u64, input: u64, output: u64| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(id),
        seq: ordinal,
        session_id: SessionId::new("usage-request-rows"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("journal-process-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1_777_075_200_000,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({
            "type": "usage",
            "input": input,
            "output": output,
            "source": "provider_reported",
            "account": "account-main",
            "scope": {
                "provider": "openai-oauth",
                "model": "gpt-attributed",
                "auth_scope": "oauth_subscription",
                "cache_epoch": "epoch-rows"
            },
            "request": {
                "ordinal": ordinal,
                "input": input,
                "output": output,
                "cached": input / 2,
                "source": "provider_reported",
                "account": "account-main"
            }
        })
        .into(),
    };
    let reduced = reduce_journal_usage(&[
        usage_event("usage-request-one", 1, 12, 3),
        usage_event("usage-request-two", 2, 20, 5),
    ]);
    let (address, reduced_slot) = reduced.iter().next().expect("one UTC slot");
    let counters = reduced_slot.rows.values().next().expect("attributed lane");
    assert_eq!(counters.requests, 2, "one row contribution per request");
    assert_eq!(counters.input_tokens, 32);
    assert_eq!(counters.output_tokens, 8);
    assert_eq!(counters.cache_read_tokens, 16);

    let root = tempfile::tempdir().expect("temp profile");
    let writer =
        UsageLedgerWriter::new(root.path(), "dev-abababababababababababababababab", "1.0.0");
    let mut persisted_slot = reduced_slot.clone();
    persisted_slot.rows.insert(
        root_lane("claude-small"),
        UsageLedgerCounters {
            requests: 1,
            input_tokens: 1,
            ..UsageLedgerCounters::default()
        },
    );
    writer
        .append_slot(address, &persisted_slot, false)
        .expect("append attributed requests");
    let day = read_usage_day(root.path(), &address.date)
        .expect("read attributed day")
        .expect("attributed day exists");
    let gpt_key = day
        .keys
        .iter()
        .find(|key| {
            key.model.as_deref() == Some("gpt-attributed")
                && key.provider.as_deref() == Some("openai-oauth")
        })
        .expect("provider/model dictionary row");
    let day_requests = day
        .slots
        .iter()
        .flatten()
        .flat_map(|slot| &slot.rows)
        .filter(|row| row.key_id == gpt_key.id)
        .map(|row| row.requests)
        .sum::<u64>();
    assert_eq!(day_requests, 2, "history_day preserves request rows");
    let range = read_usage_range(root.path(), &address.date, 1).expect("read attributed range");
    assert_eq!(range[0].models.len(), 2);
    assert_eq!(range[0].models[0].model, "gpt-attributed");
    assert_eq!(range[0].models[0].provider, "openai-oauth");
    assert_eq!(range[0].models[0].requests, 2);
    assert_eq!(range[0].models[0].input_tokens, 32);
    assert_eq!(range[0].models[1].model, "claude-small");
}

#[test]
fn meter_basis_points_are_written_verbatim() {
    let root = tempfile::tempdir().expect("temp profile");
    let writer =
        UsageLedgerWriter::new(root.path(), "dev-22222222222222222222222222222222", "1.0.0");
    writer
        .append_meter_sample(&UsageHistoryMeterSampleV1 {
            account: "account-main".into(),
            window: "five_hour".into(),
            basis_points: 6_789,
            resets_at_ms: Some(1_900_000_000_000),
            grace_until_ms: Some(1_900_000_060_000),
            sampled_at_ms: 1_777_075_200_000,
            plan: Some("max".into()),
            credits: Some(17),
            hold: Some(-3),
            stale: Some(false),
        })
        .expect("append meter");
    let day = read_usage_day(root.path(), "2026-04-25")
        .expect("read meter day")
        .expect("meter day exists");
    assert_eq!(day.meter_samples[0].basis_points, 6_789);
    assert_eq!(day.meter_samples[0].credits, Some(17));
    assert_eq!(day.meter_samples[0].hold, Some(-3));

    // MUTATION CHECK: normalizing or percent-rounding the supplied integer
    // makes this exact equality fail.
}

#[test]
fn restart_reuses_header_and_dictionary_ids() {
    let root = tempfile::tempdir().expect("temp profile");
    let device = "dev-33333333333333333333333333333333";
    let first = UsageLedgerWriter::new(root.path(), device, "1.0.0");
    first
        .append_slot(
            &UsageSlotAddress {
                date: "2026-08-24".into(),
                slot: 4,
            },
            &slot([(root_lane("claude-test"), UsageLedgerCounters::default())]),
            false,
        )
        .expect("first process append");
    drop(first);

    let reopened = UsageLedgerWriter::new(root.path(), device, "1.0.0");
    reopened
        .append_slot(
            &UsageSlotAddress {
                date: "2026-08-24".into(),
                slot: 5,
            },
            &slot([(root_lane("claude-test"), UsageLedgerCounters::default())]),
            false,
        )
        .expect("reopened append");

    let text = fs::read_to_string(root.path().join("usage/2026-08-24.jsonl")).expect("read JSONL");
    assert_eq!(
        text.lines()
            .filter(|line| line.contains(r#""t":"h""#))
            .count(),
        1
    );
    let day = read_usage_day(root.path(), "2026-08-24")
        .expect("read day")
        .expect("day exists");
    assert_eq!(day.keys.len(), 1, "the reopened writer must reuse the key");
    assert_eq!(
        day.slots[4].as_ref().expect("slot four").rows[0].key_id,
        day.slots[5].as_ref().expect("slot five").rows[0].key_id
    );
}

#[test]
fn profile_installation_id_survives_reopen_and_backfill() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("open store");
    let before = store.profile_installation_id().expect("installation id");
    assert_eq!(before.len(), 36);
    assert!(before.starts_with("dev-"));
    assert!(
        before[4..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    store.initialize_usage_history().expect("empty backfill");
    let after_backfill = store.profile_installation_id().expect("id after backfill");
    assert_eq!(
        before, after_backfill,
        "backfill must not regenerate identity"
    );
    drop(store);

    let reopened = Store::open(root.path()).expect("reopen store");
    assert_eq!(
        before,
        reopened.profile_installation_id().expect("reopened id")
    );

    // MUTATION CHECK: regenerate-on-open makes the final equality fail; the
    // first Store is dropped before the restart-shaped second read.
}

#[test]
fn separate_profiles_get_separate_installation_ids() {
    let first_root = tempfile::tempdir().expect("first profile");
    let second_root = tempfile::tempdir().expect("second profile");
    let first = Store::open(first_root.path())
        .expect("open first profile")
        .profile_installation_id()
        .expect("first installation id");
    let second = Store::open(second_root.path())
        .expect("open second profile")
        .profile_installation_id()
        .expect("second installation id");
    assert_ne!(first, second, "profile scope deliberately defines devices");
}

#[test]
fn profile_reads_reject_a_foreign_device_day() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let own_device = store.profile_installation_id().expect("own device");
    let foreign_device = if own_device == "dev-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" {
        "dev-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    } else {
        "dev-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    };
    UsageLedgerWriter::new(root.path(), foreign_device, "1.0.0")
        .append_slot(
            &UsageSlotAddress {
                date: "2026-08-24".into(),
                slot: 1,
            },
            &slot([(root_lane("claude-test"), UsageLedgerCounters::default())]),
            false,
        )
        .expect("write foreign day fixture");

    let day_error = store
        .usage_history_day("2026-08-24")
        .expect_err("foreign day must fail");
    assert_eq!(day_error.code, haider_store::ErrorCode::StoreCorrupt);
    let range_error = store
        .usage_history_range("2026-08-24", 1)
        .expect_err("foreign range cell must fail");
    assert_eq!(range_error.code, haider_store::ErrorCode::StoreCorrupt);

    // MUTATION CHECK: dropping either device comparison turns the associated
    // read into a successful merge of two distinct profile streams.
}

#[test]
fn journal_backfill_marks_its_day_header() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("open store");
    let session_id = SessionId::new("usage-backfill-session");
    let mut envelopes = [
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("usage-backfill-event"),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("journal-process-device"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::json!({
                "type": "usage",
                "input": 12,
                "output": 3,
                "reasoning": 1,
                "cached": 4,
                "source": "provider_reported",
                "account": "account-main",
                "scope": {
                    "provider": "anthropic-oauth",
                    "model": "claude-test",
                    "auth_scope": "oauth_subscription",
                    "cache_epoch": "epoch-1"
                }
            })
            .into(),
        },
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("usage-backfill-failure"),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("journal-process-device"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::json!({
                "type": "run_failed",
                "code": "provider_error",
                "message": "provider request failed",
                "retryable": true
            })
            .into(),
        },
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("usage-live-dimensions"),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("journal-process-device"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::json!({
                "type": "usage",
                "input": 20,
                "output": 5,
                "reasoning": 2,
                "cached": 0,
                "source": "provider_reported",
                "account": "live-account",
                "scope": {
                    "provider": "openai",
                    "model": "gpt-live",
                    "auth_scope": "oauth_subscription",
                    "api_family": "openai_responses",
                    "effort": "xhigh",
                    "speed": "standard",
                    "cache_epoch": "epoch-live"
                }
            })
            .into(),
        },
    ];
    store.append(&mut envelopes).expect("append usage journal");
    let reduced = reduce_journal_usage(
        &store
            .journal_replay(&session_id)
            .expect("read usage journal"),
    );
    let (lane, counters) = reduced
        .values()
        .next()
        .expect("reduced usage slot")
        .rows
        .iter()
        .find(|(_, counters)| counters.input_tokens == 12)
        .expect("reduced usage lane");
    assert_eq!(counters.input_tokens, 12);
    assert_eq!(counters.errors, 0);
    assert!(lane.api_family.is_none());
    assert!(lane.effort.is_none());
    assert!(lane.speed.is_none());
    let (failure_lane, failure) = reduced
        .values()
        .next()
        .expect("reduced usage slot")
        .rows
        .iter()
        .find(|(_, counters)| counters.errors == 1)
        .expect("role-only failure lane");
    assert!(failure_lane.account.is_none());
    assert!(failure_lane.provider.is_none());
    assert!(failure_lane.model.is_none());
    assert_eq!(failure.input_tokens, 0);
    let (live_lane, live) = reduced
        .values()
        .next()
        .expect("reduced usage slot")
        .rows
        .iter()
        .find(|(_, counters)| counters.input_tokens == 20)
        .expect("live dimensional lane");
    assert_eq!(live_lane.api_family.as_deref(), Some("openai_responses"));
    assert_eq!(live_lane.effort.as_deref(), Some("xhigh"));
    assert_eq!(live_lane.speed.as_deref(), Some("standard"));
    assert_eq!(live.output_tokens, 5);
    store.initialize_usage_history().expect("backfill");

    let usage_dir = root.path().join("usage");
    let day_path = fs::read_dir(&usage_dir)
        .expect("usage dir")
        .next()
        .expect("one day")
        .expect("day entry")
        .path();
    let date = day_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("date filename");
    let day = read_usage_day(root.path(), date)
        .expect("read backfilled day")
        .expect("backfilled day exists");
    assert!(day.backfilled, "backfill-produced headers must be marked");
    assert_eq!(
        day.slots.iter().flatten().count(),
        0,
        "the current UTC quarter must remain open after backfill"
    );

    let before = fs::read_to_string(&day_path).expect("backfill JSONL before retry");
    store
        .initialize_usage_history()
        .expect("already-complete backfill retry");
    let after = fs::read_to_string(&day_path).expect("backfill JSONL after retry");
    assert_eq!(
        before, after,
        "completed backfill must not append duplicates"
    );

    // MUTATION CHECK: inventing historical dimensions or dropping exact live
    // ones fails the paired lane assertions; attributing generic RunFailed to
    // the latest request fails the role-only lane assertions; finalizing the
    // open quarter fails the slot pin; rerunning backfill breaks byte equality.
}
