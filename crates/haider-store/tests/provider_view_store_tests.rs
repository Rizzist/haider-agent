#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::cache::{
    CacheRequestAttemptV1, ProviderViewAttemptV1, ProviderViewBlobV1, ProviderViewBoundaryV1,
    ProviderViewLedgerV1,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EventId, ItemId, SessionId};
use haider_protocol::item::ItemEvent;
use haider_protocol::provider::{
    CacheBreakpointHashesV1, CacheControlObservationV1, CachePrefixMatchV1,
    CacheRequestDiagnosticV1,
};
use haider_store::{EventStore, Store};
use rusqlite::{Connection, params};

fn create_session(store: &Store, session_id: &SessionId) {
    let connection = Connection::open(store.database_path()).expect("open test database");
    connection
        .execute(
            "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
            params![session_id.as_str(), 1_i64, "{}"],
        )
        .expect("insert test session");
}

fn provider_view(
    seed: &str,
    payload_len: usize,
) -> (ProviderViewLedgerV1, Vec<ProviderViewBlobV1>) {
    let system = ProviderViewBlobV1::new(format!("system-{seed}").into_bytes());
    let tools = ProviderViewBlobV1::new(format!("tools-{seed}").into_bytes());
    let history = ProviderViewBlobV1::new(vec![seed.as_bytes()[0]; payload_len]);
    let ledger = ProviderViewLedgerV1 {
        provider: "openai".into(),
        model: "gpt-test".into(),
        max_tokens: 4_096,
        dialect: "responses".into(),
        serialization_version: "haider.provider-view.json.v2".into(),
        header_epoch: "header".into(),
        cache_epoch: "cache".into(),
        compaction_epoch: "root".into(),
        reasoning_retention: "append_only_provider_opaque_v1:test".into(),
        account_scope: Some("account".into()),
        stable_history_end: 1,
        current_user_start: 1,
        latest_compaction_summary_end: None,
        trim_sentinel: "root".into(),
        boundaries: vec![ProviderViewBoundaryV1 {
            section: "history".into(),
            message_end: Some(1),
        }],
        system_block: system.block.clone(),
        tool_schema_block: tools.block.clone(),
        history_blocks: vec![history.block.clone()],
        storage: None,
    };
    (ledger, vec![system, tools, history])
}

fn provider_attempt_envelopes(
    session_id: &SessionId,
    ledger: &ProviderViewLedgerV1,
    ordinal: u64,
) -> Vec<RawEnvelope> {
    let item_id = ItemId::new("provider-view-attempt-item");
    let item = ProviderViewAttemptV1 {
        ordinal,
        view: ledger.clone(),
    }
    .extension_item()
    .expect("provider-view attempt item");
    [
        ItemEvent::Started {
            item_id: item_id.clone(),
            item: item.clone(),
        },
        ItemEvent::Completed { item_id, item },
    ]
    .into_iter()
    .enumerate()
    .map(|(index, item)| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("provider-view-attempt-{index}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("provider-view-attempt-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::Item(item))
            .expect("item payload")
            .into(),
    })
    .collect()
}

fn cache_attempt_envelopes(session_id: &SessionId) -> Vec<RawEnvelope> {
    let item_id = ItemId::new("cache-request-attempt-item");
    let item = CacheRequestAttemptV1 {
        ordinal: 1,
        correlation: None,
        diagnostic: CacheRequestDiagnosticV1 {
            history_message_count: 1,
            stable_prefix_tokens: 8,
            breakpoint_hashes: CacheBreakpointHashesV1::default(),
            cache_domain_hash: Some("domain".into()),
            cache_domain_changed: None,
            previous_breakpoint: None,
            prefix_match: CachePrefixMatchV1::Unavailable,
            control: CacheControlObservationV1::NotRequired,
            cacheable_minimum_tokens: None,
            reuse_gap_ms: None,
            rewarm: None,
            classification: None,
        },
    }
    .extension_item()
    .expect("cache request attempt item");
    [
        ItemEvent::Started {
            item_id: item_id.clone(),
            item: item.clone(),
        },
        ItemEvent::Completed { item_id, item },
    ]
    .into_iter()
    .enumerate()
    .map(|(index, item)| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("cache-request-attempt-{index}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("provider-view-attempt-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::Item(item))
            .expect("item payload")
            .into(),
    })
    .collect()
}

#[test]
fn provider_view_index_and_full_attempt_batch_commit_or_rollback_together() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-fused-transaction");
    create_session(&store, &session_id);

    let (ledger, blobs) = provider_view("fused", 64);
    let mut incomplete = provider_attempt_envelopes(&session_id, &ledger, 1);
    incomplete.pop();
    incomplete.extend(cache_attempt_envelopes(&session_id));
    store
        .persist_provider_view_and_append_owned(&session_id, ledger, blobs, 1, &mut incomplete)
        .expect_err("an incomplete attempt pair rolls the transaction back");

    let connection = Connection::open(store.database_path()).expect("open test database");
    let counts = connection
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM provider_view_requests),
                 (SELECT COUNT(*) FROM events)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .expect("count rolled-back rows");
    assert_eq!(counts, (0, 0));

    let (ledger, blobs) = provider_view("fused-success", 64);
    let mut envelopes = provider_attempt_envelopes(&session_id, &ledger, 1);
    envelopes.extend(cache_attempt_envelopes(&session_id));
    let stored = store
        .persist_provider_view_and_append_owned(&session_id, ledger, blobs, 1, &mut envelopes)
        .expect("fused provider-view attempt");
    assert!(stored.storage.is_some());
    assert_eq!(
        envelopes
            .iter()
            .map(|envelope| envelope.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    store
        .verify_provider_view(&stored)
        .expect("indexed CAS view");
    assert_eq!(
        store.read(&session_id, 0, 10).expect("attempt journal"),
        envelopes
    );
}

/// MEMORY ITEM 962: request/session growth may add only disk rows and small
/// hashes. Even actively reading every block cannot grow the resident cache
/// beyond its byte cap, and an oversize block is never admitted.
#[test]
fn provider_view_resident_state_stays_byte_capped_as_requests_grow() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    for session_index in 0..8 {
        let session_id = SessionId::new(format!("provider-view-{session_index}"));
        create_session(&store, &session_id);
        for request_index in 0..16 {
            let seed = format!("{session_index:x}{request_index:x}");
            let (ledger, blobs) = provider_view(&seed, 8 * 1024);
            let stored = store
                .persist_provider_view(&session_id, ledger, blobs)
                .expect("persist provider view");
            for block in std::iter::once(&stored.system_block)
                .chain(std::iter::once(&stored.tool_schema_block))
                .chain(stored.history_blocks.iter())
            {
                let bytes = store
                    .read_provider_view_block(&stored, block)
                    .expect("lazy block read");
                assert_eq!(bytes.len() as u64, block.byte_len);
                drop(bytes);
            }
            assert!(
                store
                    .provider_view_resident_bytes()
                    .expect("resident bytes")
                    <= 64 * 1024,
                "hot-block LRU exceeded its byte cap"
            );
        }
    }

    let session_id = SessionId::new("provider-view-oversize");
    create_session(&store, &session_id);
    let (ledger, blobs) = provider_view("z", 128 * 1024);
    let stored = store
        .persist_provider_view(&session_id, ledger, blobs)
        .expect("persist oversize block");
    let journal_ledger = serde_json::to_vec(&stored).expect("serialize hashes-only ledger");
    assert!(journal_ledger.len() < 2 * 1024);
    assert!(
        !journal_ledger
            .windows(32)
            .any(|window| window == [b'z'; 32])
    );
    let bytes = store
        .read_provider_view_block(&stored, &stored.history_blocks[0])
        .expect("read oversize block");
    drop(bytes);
    assert!(
        store
            .provider_view_resident_bytes()
            .expect("resident bytes")
            <= 64 * 1024
    );
}

/// Expiry removes the request cursor and unreferenced bytes, while a shared
/// content-addressed block remains readable for a non-expired request.
#[test]
fn expired_provider_view_sweep_preserves_live_shared_blocks() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-expiry");
    create_session(&store, &session_id);

    let (expired_ledger, expired_blobs) = provider_view("s", 4 * 1024);
    let expired = store
        .persist_provider_view_until(&session_id, expired_ledger, expired_blobs, 10)
        .expect("persist expired view");
    let (live_ledger, live_blobs) = provider_view("s", 4 * 1024);
    let live = store
        .persist_provider_view_until(&session_id, live_ledger, live_blobs, 20)
        .expect("persist live view");

    assert_eq!(
        store
            .sweep_expired_provider_views(10)
            .expect("sweep expired views"),
        1
    );
    assert!(store.verify_provider_view(&expired).is_err());
    store
        .verify_provider_view(&live)
        .expect("live view verifies");
    assert_eq!(
        store
            .read_provider_view_block(&live, &live.history_blocks[0])
            .expect("shared live block"),
        vec![b's'; 4 * 1024]
    );

    assert_eq!(
        store
            .sweep_expired_provider_views(20)
            .expect("sweep final view"),
        1
    );
    assert!(store.verify_provider_view(&live).is_err());
    assert_eq!(
        store
            .provider_view_resident_bytes()
            .expect("resident bytes"),
        0
    );

    let (next_ledger, next_blobs) = provider_view("n", 256);
    let next = store
        .persist_provider_view_until(&session_id, next_ledger, next_blobs, 30)
        .expect("persist after sweep");
    assert!(
        next.storage
            .as_ref()
            .expect("next storage cursor")
            .request_ordinal
            > live
                .storage
                .as_ref()
                .expect("live storage cursor")
                .request_ordinal
    );
}

/// Cleanup works in multiple fixed-size SQL batches without admitting the
/// expired bytes into the resident hot-block cache.
#[test]
fn expired_provider_view_sweep_handles_more_than_one_batch() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-batched-expiry");
    create_session(&store, &session_id);

    for index in 0..257 {
        let (ledger, blobs) = provider_view(&format!("expired-{index}"), 64);
        store
            .persist_provider_view_until(&session_id, ledger, blobs, 10)
            .expect("persist expired provider view");
    }
    assert_eq!(
        store
            .sweep_expired_provider_views(10)
            .expect("sweep all expiry batches"),
        257
    );
    assert_eq!(
        store
            .provider_view_resident_bytes()
            .expect("resident bytes"),
        0
    );

    let connection = Connection::open(store.database_path()).expect("open test database");
    let request_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_view_requests", [], |row| {
            row.get(0)
        })
        .expect("count provider-view requests");
    let gc_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_view_gc", [], |row| {
            row.get(0)
        })
        .expect("count provider-view GC queue");
    assert_eq!(request_count, 0);
    assert_eq!(gc_count, 0);
}

/// Request indexes intentionally outlive a deleted source session for fork
/// replay, so recreating an opaque ID must resume above retained ordinals.
#[test]
fn provider_view_request_ordinal_survives_session_id_reuse() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-reused-session");
    create_session(&store, &session_id);

    let (first_ledger, first_blobs) = provider_view("first", 64);
    let first = store
        .persist_provider_view_until(&session_id, first_ledger, first_blobs, u64::MAX / 2)
        .expect("persist first provider view");
    store
        .delete_session(&session_id)
        .expect("delete source session");
    let connection = Connection::open(store.database_path()).expect("open test database");
    let cursor_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_view_session_cursors WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .expect("count deleted session cursors");
    assert_eq!(cursor_count, 0);
    drop(connection);
    create_session(&store, &session_id);

    let (second_ledger, second_blobs) = provider_view("second", 64);
    let second = store
        .persist_provider_view_until(&session_id, second_ledger, second_blobs, u64::MAX / 2)
        .expect("persist provider view after ID reuse");
    assert!(
        second
            .storage
            .as_ref()
            .expect("second storage cursor")
            .request_ordinal
            > first
                .storage
                .as_ref()
                .expect("first storage cursor")
                .request_ordinal
    );
    store
        .verify_provider_view(&first)
        .expect("retained source provider view");
    store
        .verify_provider_view(&second)
        .expect("reused-session provider view");
}

/// A growing view replaces its volatile last block and adds one stable block
/// per request, matching the real provider renderer. SQLite's
/// checkpoint frame counter is a portable proxy for pages written since the
/// preceding turn; the late turn must remain bounded by that delta.
#[test]
fn growing_provider_view_writes_delta_frames_and_replays_after_restart() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-growing-delta");
    create_session(&store, &session_id);
    let observer = Connection::open(store.database_path()).expect("observer");
    let mut early_frames = Vec::new();
    let mut late_frames = Vec::new();
    let mut last_envelopes = Vec::new();

    for turn in 1..=64_usize {
        let system = ProviderViewBlobV1::new(b"fixed-system".to_vec());
        let tools = ProviderViewBlobV1::new(b"fixed-tools".to_vec());
        let history = (1..=turn)
            .map(|index| {
                let text = if index == turn {
                    format!("volatile-{index:04}")
                } else {
                    format!("stable-{index:04}")
                };
                ProviderViewBlobV1::new(text.into_bytes())
            })
            .collect::<Vec<_>>();
        let mut ledger = provider_view("fixed", 16).0;
        ledger.system_block = system.block.clone();
        ledger.tool_schema_block = tools.block.clone();
        ledger.history_blocks = history.iter().map(|blob| blob.block.clone()).collect();
        ledger.stable_history_end = turn as u64;
        ledger.current_user_start = turn as u64;
        let mut blobs = vec![system, tools];
        blobs.extend(history);
        let mut envelopes = provider_attempt_envelopes(&session_id, &ledger, turn as u64);
        for (index, envelope) in envelopes.iter_mut().enumerate() {
            envelope.event_id = EventId::new(format!("growing-{turn}-{index}"));
        }
        let stored = store
            .persist_provider_view_and_append_owned(
                &session_id,
                ledger,
                blobs,
                turn as u64,
                &mut envelopes,
            )
            .expect("persist growing view and journal markers");
        let ordinal = stored.storage.as_ref().expect("storage").request_ordinal;
        let direct_count: i64 = observer
            .query_row(
                "SELECT COUNT(*) FROM provider_view_blocks
                 WHERE session_id = ?1 AND request_ordinal = ?2",
                params![
                    session_id.as_str(),
                    i64::try_from(ordinal).expect("ordinal fits i64")
                ],
                |row| row.get(0),
            )
            .expect("direct block count");
        assert_eq!(direct_count, 2, "history references are shared by segment");
        let segment_count: i64 = observer
            .query_row(
                "SELECT COUNT(*) FROM provider_view_history_blocks",
                [],
                |row| row.get(0),
            )
            .expect("segment block count");
        let expected_rows = if turn == 1 { 1 } else { turn * 3 - 3 };
        assert_eq!(
            segment_count, expected_rows as i64,
            "only confirmed stable blocks and the short volatile tail are indexed"
        );
        let (busy, frames, checkpointed): (i64, i64, i64) = observer
            .query_row("PRAGMA wal_checkpoint(RESTART)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("checkpoint frame counter");
        assert_eq!(busy, 0);
        assert_eq!(frames, checkpointed);
        if (5..=12).contains(&turn) {
            early_frames.push(frames);
        }
        if turn >= 57 {
            late_frames.push(frames);
        }
        if turn == 64 {
            let inline_bytes: i64 = observer
                .query_row(
                    "SELECT MAX(length(envelope_json)) FROM events
                     WHERE session_id = ?1 AND seq > ?2",
                    params![
                        session_id.as_str(),
                        i64::try_from(envelopes[0].seq - 1).expect("sequence fits i64")
                    ],
                    |row| row.get(0),
                )
                .expect("compact journal bytes");
            assert!(
                inline_bytes < 4_096,
                "attempt envelope stores a history cursor"
            );
            last_envelopes = envelopes;
        }
    }
    assert!(
        *late_frames.iter().max().expect("late frames")
            <= *early_frames.iter().max().expect("early frames") + 32,
        "WAL frames per turn grew with retained history: early={early_frames:?} late={late_frames:?}"
    );
    drop(observer);
    drop(store);
    let reopened = Store::open(root.path()).expect("restart store");
    let start = last_envelopes[0].seq - 1;
    assert_eq!(
        reopened
            .read(&session_id, start, 2)
            .expect("replay compact attempt"),
        last_envelopes,
        "restart reconstructs the exact public journal events"
    );
    assert_eq!(
        reopened
            .sweep_expired_provider_views(4_000_000_000_000)
            .expect("expire provider-view CAS indexes"),
        64
    );
    assert_eq!(
        reopened
            .read(&session_id, start, 2)
            .expect("replay after expiry"),
        last_envelopes,
        "the authoritative journal remains readable after CAS expiry"
    );
}

#[test]
fn changed_history_prefix_starts_a_new_immutable_segment() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-prefix-change");
    create_session(&store, &session_id);
    let mut previous = Vec::new();
    for history_text in [
        &["first", "second", "third"][..],
        &["first", "replacement", "third"],
        &["first", "replacement"],
        &["first", "replacement", "new"],
    ] {
        let system = ProviderViewBlobV1::new(b"system".to_vec());
        let tools = ProviderViewBlobV1::new(b"tools".to_vec());
        let history = history_text
            .iter()
            .map(|text| ProviderViewBlobV1::new(text.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let mut ledger = provider_view("fixed", 16).0;
        ledger.system_block = system.block.clone();
        ledger.tool_schema_block = tools.block.clone();
        ledger.history_blocks = history.iter().map(|blob| blob.block.clone()).collect();
        let mut blobs = vec![system, tools];
        blobs.extend(history);
        previous.push(
            store
                .persist_provider_view(&session_id, ledger, blobs)
                .expect("persist changed view"),
        );
    }
    let connection = Connection::open(store.database_path()).expect("observer");
    let segments: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_view_history_segments",
            [],
            |row| row.get(0),
        )
        .expect("segment count");
    assert_eq!(
        segments, 5,
        "shortening preserves an immutable parent prefix"
    );
    for ledger in &previous {
        store
            .verify_provider_view(ledger)
            .expect("old and new exact views remain valid");
    }
}

#[test]
fn pre_segment_request_rows_remain_verifiable_after_upgrade() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-legacy-index");
    create_session(&store, &session_id);
    let (ledger, blobs) = provider_view("legacy", 64);
    let stored = store
        .persist_provider_view(&session_id, ledger, blobs)
        .expect("persist fixture view");
    let storage = stored.storage.as_ref().expect("storage");
    let connection = Connection::open(store.database_path()).expect("observer");
    connection
        .execute(
            "INSERT INTO provider_view_blocks(
                provider, model, cache_epoch, session_id, request_ordinal,
                section, block_ordinal, content_hash, byte_len, expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'history', 0, ?6, ?7, ?8)",
            params![
                &stored.provider,
                &stored.model,
                &stored.cache_epoch,
                session_id.as_str(),
                i64::try_from(storage.request_ordinal).expect("ordinal fits i64"),
                &stored.history_blocks[0].content_hash,
                i64::try_from(stored.history_blocks[0].byte_len).expect("length fits i64"),
                i64::try_from(storage.expires_at_ms).expect("expiry fits i64"),
            ],
        )
        .expect("restore v24 history index row");
    connection
        .execute(
            "DELETE FROM provider_view_request_history
             WHERE session_id = ?1 AND request_ordinal = ?2",
            params![
                session_id.as_str(),
                i64::try_from(storage.request_ordinal).expect("ordinal fits i64")
            ],
        )
        .expect("remove v32 request cursor to model an upgraded v24 row");
    store
        .verify_provider_view(&stored)
        .expect("legacy index verification");
    assert_eq!(
        store
            .read_provider_view_block(&stored, &stored.history_blocks[0])
            .expect("legacy indexed block"),
        vec![b'l'; 64]
    );
}

fn persist_history_attempt(
    store: &Store,
    session_id: &SessionId,
    history_text: &[&str],
    attempt: u64,
) -> Vec<RawEnvelope> {
    let system = ProviderViewBlobV1::new(b"compact-system".to_vec());
    let tools = ProviderViewBlobV1::new(b"compact-tools".to_vec());
    let history = history_text
        .iter()
        .map(|text| ProviderViewBlobV1::new(text.as_bytes().to_vec()))
        .collect::<Vec<_>>();
    let mut ledger = provider_view("fixed", 16).0;
    ledger.system_block = system.block.clone();
    ledger.tool_schema_block = tools.block.clone();
    ledger.history_blocks = history.iter().map(|blob| blob.block.clone()).collect();
    let mut blobs = vec![system, tools];
    blobs.extend(history);
    let mut envelopes = provider_attempt_envelopes(session_id, &ledger, attempt);
    for (index, envelope) in envelopes.iter_mut().enumerate() {
        envelope.event_id = EventId::new(format!("compact-{attempt}-{index}"));
    }
    store
        .persist_provider_view_and_append_owned(session_id, ledger, blobs, attempt, &mut envelopes)
        .expect("persist view and attempt facts");
    envelopes
}

fn stored_record(store: &Store, session_id: &SessionId, seq: u64) -> Vec<u8> {
    Connection::open(store.database_path())
        .expect("observer")
        .query_row(
            "SELECT envelope_json FROM events WHERE session_id = ?1 AND seq = ?2",
            params![
                session_id.as_str(),
                i64::try_from(seq).expect("sequence fits i64")
            ],
            |row| row.get(0),
        )
        .expect("stored journal record")
}

/// MUTATION CHECK: dropping the digest comparison in the compact-record
/// hydration must fail the tampered read below; lowering the compaction
/// minimum must fail the three-block inline assertion.
#[test]
fn compact_history_record_round_trips_and_fails_closed_on_segment_drift() {
    const INDIRECT_RECORD_MARKER: u8 = 0xc1;
    let root = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("provider-view-compact-record");
    let (short, long) = {
        let store = Store::open(root.path()).expect("store");
        create_session(&store, &session_id);
        let short = persist_history_attempt(&store, &session_id, &["one", "two", "three"], 1);
        let long =
            persist_history_attempt(&store, &session_id, &["one", "two", "three", "four"], 2);
        assert_ne!(
            stored_record(&store, &session_id, short[0].seq)[0],
            INDIRECT_RECORD_MARKER,
            "short history ledgers stay self-contained"
        );
        assert_eq!(
            stored_record(&store, &session_id, long[0].seq)[0],
            INDIRECT_RECORD_MARKER,
            "longer history ledgers are stored as a segment cursor"
        );
        (short, long)
    };
    let store = Store::open(root.path()).expect("reopen store");
    assert_eq!(
        store
            .read(&session_id, short[0].seq - 1, 4)
            .expect("replay inline and compact records"),
        [short, long.clone()].concat(),
        "hydration reproduces the exact public envelopes"
    );
    drop(store);
    Connection::open(root.path().join("store.sqlite"))
        .expect("tamper connection")
        .execute(
            "UPDATE provider_view_history_blocks SET byte_len = byte_len + 1",
            [],
        )
        .expect("tamper segment rows");
    let store = Store::open(root.path()).expect("reopen tampered store");
    assert!(
        store.read(&session_id, long[0].seq - 1, 2).is_err(),
        "a compact record whose segment rows drifted must fail closed"
    );
}
