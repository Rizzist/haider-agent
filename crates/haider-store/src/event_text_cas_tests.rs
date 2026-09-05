#![allow(clippy::expect_used)]

use super::*;
use crate::event_store::{EventStore, Store};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, SessionId};

fn envelope(payload: Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("text-cas-event"),
        seq: 0,
        session_id: SessionId::new("text-cas-session"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("text-cas-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    }
}

fn raw_record(store: &Store) -> Vec<u8> {
    let connection = store.connection().expect("connection");
    connection
        .query_row("SELECT envelope_json FROM events LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("stored record")
}

fn record(bytes: &[u8]) -> StoredEnvelope {
    rmp_serde::from_slice(
        &bytes.strip_prefix(RECORD_PREFIX).expect("private prefix")[LENGTH_HEADER_BYTES..],
    )
    .expect("private record")
}

#[test]
fn text_cas_threshold_is_inclusive_and_small_rows_remain_legacy_compatible() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let connection = store.connection().expect("connection");
    let below = envelope(
        serde_json::json!({"type":"future_event", "text":"x".repeat(TEXT_CAS_THRESHOLD - 1)}),
    );
    assert!(
        encode(&connection, &below)
            .expect("encode below threshold")
            .is_none()
    );
    let inline = rmp_serde::to_vec_named(&below).expect("legacy MessagePack");
    assert_eq!(
        decode(&connection, &inline).expect("read legacy MessagePack"),
        below
    );
    let at =
        envelope(serde_json::json!({"type":"future_event", "text":"x".repeat(TEXT_CAS_THRESHOLD)}));
    let encoded = encode(&connection, &at)
        .expect("encode threshold")
        .expect("externalized");
    assert!(encoded.len() < 2_048);
    assert_eq!(record(&encoded).strings.len(), 1);
    assert_eq!(
        decode(&connection, &encoded).expect("hydrate threshold"),
        at
    );
    let memory = Connection::open_in_memory().expect("memory database");
    assert!(encode(&memory, &at).expect("in-memory fallback").is_none());
}

#[test]
fn text_cas_unknown_nested_payload_is_transparent_across_reopen_and_all_replay_paths() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let text = "雪\\\"\n".repeat(TEXT_CAS_THRESHOLD / 4);
    let mut events = [envelope(serde_json::json!({
        "type":"future_event", "nested":[{"a/b~c":text}],
        "digest":"a user-owned field", "strings":[], "reply":null
    }))];
    store.append(&mut events).expect("append");
    let bytes = raw_record(&store);
    assert!(bytes.len() < 2_048);
    let stored = record(&bytes);
    assert_eq!(stored.strings[0].path, "/nested/0/a~1b~0c");
    let digest = stored.strings[0].object.digest.as_str();
    let expected_json = serde_json::to_vec(&events[0]).expect("client schema");
    assert!(!String::from_utf8_lossy(&expected_json).contains(digest));
    assert_eq!(
        store.read(&events[0].session_id, 0, 10).expect("read"),
        events
    );
    assert_eq!(
        store
            .read_page(&events[0].session_id, 0, 10, 1)
            .expect("bounded read"),
        events
    );
    assert_eq!(
        store
            .read_reducer_page(&events[0].session_id, 0, 10, 1, &["future_event"])
            .expect("reducer read"),
        events
    );
    assert_eq!(
        store.pending_hook_dispatches(10).expect("outbox read"),
        events
    );
    drop(store);
    let reopened = Store::open(root.path()).expect("reopen");
    let replayed = reopened
        .read(&events[0].session_id, 0, 10)
        .expect("replay after restart");
    assert_eq!(
        serde_json::to_vec(&replayed[0]).expect("replay schema"),
        expected_json
    );
}

#[test]
fn text_cas_segmented_reply_and_repeated_json_text_share_one_digest() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let text = "a".repeat(TEXT_CAS_THRESHOLD);
    let mut reply = envelope(serde_json::json!({
        "type":"item", "event":"delta", "item_id":"reply", "delta":{"delta":"text", "text":""}
    }));
    let mut arena = haider_protocol::reply::ReplyArenaWriter::new();
    let first = arena.append(text[..text.len() / 2].to_owned());
    let second = arena.append(text[text.len() / 2..].to_owned());
    let _ = (first, second);
    let reply_text = arena.seal();
    assert!(reply.payload.replace_reply_text(reply_text));
    let generic = envelope(serde_json::json!({"type":"future_event", "first":text, "second":text}));
    let connection = store.connection().expect("connection");
    let encoded_reply = encode(&connection, &reply)
        .expect("encode reply")
        .expect("indirect reply");
    let encoded_generic = encode(&connection, &generic)
        .expect("encode strings")
        .expect("indirect strings");
    assert!(encoded_reply.len() < 2_048);
    let reply_record = record(&encoded_reply);
    let generic_record = record(&encoded_generic);
    let digest = &reply_record.reply.expect("reply descriptor").digest;
    assert_eq!(generic_record.strings.len(), 2);
    assert!(
        generic_record
            .strings
            .iter()
            .all(|field| &field.object.digest == digest)
    );
    assert_eq!(
        decode(&connection, &encoded_reply).expect("hydrate reply"),
        reply
    );
    assert_eq!(
        decode(&connection, &encoded_generic).expect("hydrate strings"),
        generic
    );
}

#[test]
fn text_cas_corruption_missing_object_and_wrong_length_fail_closed() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let mut events = [envelope(
        serde_json::json!({"type":"future_event", "text":"x".repeat(TEXT_CAS_THRESHOLD)}),
    )];
    store.append(&mut events).expect("append");
    let original = raw_record(&store);
    let stored = record(&original);
    let object = &stored.strings[0].object;
    let object_path = store.cas().path_for(&object.digest).expect("CAS path");
    let mut bad_length = record(&original);
    bad_length.strings[0].object.byte_len += 1;
    let mut bad_record = original[..RECORD_PREFIX.len() + LENGTH_HEADER_BYTES].to_vec();
    rmp_serde::encode::write_named(&mut bad_record, &bad_length).expect("mutated record");
    let connection = store.connection().expect("connection");
    assert!(
        decode(&connection, &bad_record)
            .expect_err("length mismatch")
            .contains("length differs")
    );
    drop(connection);
    std::fs::write(&object_path, "y".repeat(TEXT_CAS_THRESHOLD)).expect("corrupt object");
    assert_eq!(
        store
            .read(&events[0].session_id, 0, 10)
            .expect_err("digest mismatch")
            .code,
        ErrorCode::StoreCorrupt
    );
    std::fs::remove_file(&object_path).expect("remove object");
    assert_eq!(
        store
            .read(&events[0].session_id, 0, 10)
            .expect_err("missing object")
            .code,
        ErrorCode::StoreCorrupt
    );
}

#[test]
fn text_cas_sql_abort_never_publishes_partial_envelopes() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let mut events = [envelope(
        serde_json::json!({"type":"future_event", "text":"x".repeat(TEXT_CAS_THRESHOLD)}),
    )];
    store.append(&mut events).expect("first append");
    let mut duplicate = events[0].clone();
    duplicate.payload =
        serde_json::json!({"type":"future_event", "text":"y".repeat(TEXT_CAS_THRESHOLD)}).into();
    store
        .append(&mut [duplicate])
        .expect_err("duplicate event aborts SQL");
    assert_eq!(store.latest_seq(&events[0].session_id).expect("head"), 1);
    assert_eq!(
        store
            .read(&events[0].session_id, 0, 10)
            .expect("unmodified journal"),
        events
    );
}

#[test]
fn text_cas_hook_metadata_budget_counts_hydrated_bytes_without_reading_objects() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let mut events = [
        envelope(serde_json::json!({"type":"future_event", "text":"x".repeat(TEXT_CAS_THRESHOLD)})),
        envelope(serde_json::json!({"type":"future_event", "text":"y".repeat(TEXT_CAS_THRESHOLD)})),
    ];
    events[1].event_id = EventId::new("text-cas-second");
    store.append(&mut events).expect("append pair");
    let stored = record(&raw_record(&store));
    let path = store
        .cas()
        .path_for(&stored.strings[0].object.digest)
        .expect("CAS path");
    std::fs::remove_file(path).expect("remove CAS text to prove metadata does not hydrate");
    let metadata = store
        .pending_hook_dispatch_metadata_bounded(
            &events[0].session_id,
            0,
            10,
            TEXT_CAS_THRESHOLD + 2_048,
        )
        .expect("metadata page");
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].seq, 1);
    let next = store
        .pending_hook_dispatch_metadata_bounded(&events[0].session_id, 1, 10, 1)
        .expect("oversized first row progresses");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].seq, 2);
}

#[test]
fn text_cas_preserves_provider_resume_opaque_text_and_mixed_reply_fields() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open store");
    let connection = store.connection().expect("connection");
    for reply_len in [7, TEXT_CAS_THRESHOLD] {
        let event = envelope(serde_json::json!({
            "type":"item", "event":"completed", "item_id":"opaque-reply",
            "item":{"item":"extension", "kind":"provider_opaque", "data":{
                "provider":"anthropic", "data":{
                    "type":"thinking", "thinking":"a".repeat(reply_len),
                    "signature":"b".repeat(TEXT_CAS_THRESHOLD)
                }
            }}
        }));
        assert!(event.payload.reply_text().is_some());
        let encoded = encode(&connection, &event)
            .expect("encode mixed opaque reply")
            .expect("indirected");
        let replayed = decode(&connection, &encoded).expect("hydrate mixed opaque reply");
        assert_eq!(
            serde_json::to_vec(&replayed).expect("replayed JSON"),
            serde_json::to_vec(&event).expect("original JSON")
        );
        assert!(replayed.payload.provider_opaque_data().is_some());
    }
}
