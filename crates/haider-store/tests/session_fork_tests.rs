#![allow(clippy::expect_used)]

use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::TreeNode;
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EventId, ItemId, LeaseId, NodeId, RunId, SessionId,
};
use haider_protocol::session_fork::{
    ForkContextEpoch, SessionForkMode, SessionForked, SessionMetaforkProposal,
    SessionMetaforkRemoval, SessionMetaforkReviewManifest,
};
use haider_protocol::state::RunState;
use haider_store::{
    BranchCreateCommand, DelegationRecord, DelegationState, EventStore, SessionCreateCommand,
    SessionForkCommand, SessionForkOutcome, SessionMetaforkCommit, Store, TurnAcceptCommand,
    TurnAcceptOutcome,
};
use rusqlite::types::ValueRef;
use std::collections::HashSet;

fn create_session(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("create-digest-{session_id}"),
            request_json: format!(r#"{{"session_id":"{session_id}"}}"#),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "session-fork-test-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("session-fork-test-device"),
        })
        .expect("create source session");
}

fn source_turn(store: &Store, session_id: &SessionId, text: &str) -> (RunId, NodeId, u64, u64) {
    source_turn_for_agent(store, session_id, None, text)
}

fn source_turn_for_agent(
    store: &Store,
    session_id: &SessionId,
    agent_id: Option<AgentId>,
    text: &str,
) -> (RunId, NodeId, u64, u64) {
    let run_id = RunId::new(format!("run-{session_id}"));
    let request_json = serde_json::json!({
        "session_id": session_id,
        "worker_generation": store.worker_generation(),
        "text": text,
    })
    .to_string();
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(&TurnAcceptCommand {
            command_id: format!("turn-{session_id}"),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            branch_id: None,
            text: text.into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new(format!("queued-{session_id}")),
            user_event_id: EventId::new(format!("user-{session_id}")),
            active_event_id: EventId::new(format!("active-{session_id}")),
            device_id: DeviceId::new("session-fork-test-device"),
        })
        .expect("accept source turn")
    else {
        panic!("fresh source turn commits");
    };
    let user_seq = envelopes
        .iter()
        .find(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::UserMessage { .. })
            )
        })
        .expect("user message")
        .seq;
    let (node_id, node_seq) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(TreeNode { node, .. }) =
                serde_json::from_value(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node, envelope.seq))
        })
        .expect("user node");
    let mut done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("done-{session_id}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id,
        device_id: DeviceId::new("session-fork-test-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("payload"),
    }];
    store
        .append_worker(&mut done)
        .expect("terminalize source turn");
    (run_id, node_id, node_seq, user_seq)
}

fn source_bytes(store: &Store, session_id: &SessionId) -> Vec<Vec<u8>> {
    store
        .journal_replay(session_id)
        .expect("source replay")
        .iter()
        .map(|envelope| rmp_serde::to_vec_named(envelope).expect("encode source envelope"))
        .collect()
}

/// Raw SQLite storage snapshot for every source-owned durable byte surface.
/// This deliberately does not decode/re-encode envelopes or metadata.
fn source_storage_bytes(root: &std::path::Path, session_id: &SessionId) -> Vec<Vec<u8>> {
    let connection = rusqlite::Connection::open(root.join("store.sqlite")).expect("snapshot db");
    let mut snapshot = Vec::new();
    let mut table_query = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("table inventory");
    let tables = table_query
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table rows")
        .map(|row| row.expect("table name"))
        .collect::<Vec<_>>();
    drop(table_query);
    for table in tables {
        let quoted_table = format!("\"{}\"", table.replace('"', "\"\""));
        let mut column_query = connection
            .prepare(&format!("PRAGMA table_info({quoted_table})"))
            .expect("column inventory");
        let columns = column_query
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column rows")
            .map(|row| row.expect("column name"))
            .collect::<Vec<_>>();
        drop(column_query);
        let session_columns = columns
            .iter()
            .filter(|column| {
                column.as_str() == "session_id"
                    || column.ends_with("_session_id")
                    || (table == "sessions" && column.as_str() == "id")
            })
            .collect::<Vec<_>>();
        if session_columns.is_empty() {
            continue;
        }
        let quoted_columns = columns
            .iter()
            .map(|column| format!("\"{}\"", column.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        let predicates = session_columns
            .iter()
            .enumerate()
            .map(|(index, column)| format!("\"{}\" = ?{}", column.replace('"', "\"\""), index + 1))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT {} FROM {quoted_table} WHERE {predicates} ORDER BY {}",
            quoted_columns.join(", "),
            quoted_columns.join(", ")
        );
        let mut statement = connection.prepare(&sql).expect("source table query");
        let mut rows = statement
            .query(rusqlite::params_from_iter(
                session_columns.iter().map(|_| session_id.as_str()),
            ))
            .expect("source table rows");
        while let Some(row) = rows.next().expect("source table row") {
            let mut encoded = Vec::new();
            encoded.extend_from_slice(table.as_bytes());
            encoded.push(0xff);
            for index in 0..columns.len() {
                let value = row.get_ref(index).expect("source cell");
                match value {
                    ValueRef::Null => encoded.push(0),
                    ValueRef::Integer(value) => {
                        encoded.push(1);
                        encoded.extend_from_slice(&value.to_be_bytes());
                    }
                    ValueRef::Real(value) => {
                        encoded.push(2);
                        encoded.extend_from_slice(&value.to_bits().to_be_bytes());
                    }
                    ValueRef::Text(value) => {
                        encoded.push(3);
                        encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
                        encoded.extend_from_slice(value);
                    }
                    ValueRef::Blob(value) => {
                        encoded.push(4);
                        encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
                        encoded.extend_from_slice(value);
                    }
                }
            }
            snapshot.push(encoded);
        }
    }
    snapshot
}

fn fork_command(
    store: &Store,
    command_id: &str,
    source: &SessionId,
    child: &str,
    fork_node_id: NodeId,
    fork_seq: u64,
    metafork: Option<SessionMetaforkCommit>,
) -> SessionForkCommand {
    let method = if metafork.is_some() {
        "session.metafork"
    } else {
        "session.fork"
    };
    let request_json = serde_json::json!({
        "method": method,
        "source": source,
        "fork_node_id": fork_node_id,
        "fork_seq": fork_seq,
        "metafork": metafork,
    })
    .to_string();
    SessionForkCommand {
        command_id: command_id.into(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        source_session_id: source.clone(),
        session_id: SessionId::new(child),
        worker_generation: store.worker_generation(),
        source_branch_id: None,
        fork_node_id,
        fork_seq,
        name: Some(format!("child {child}")),
        metafork,
        audit_event_id: EventId::new(format!("audit-{child}")),
        device_id: DeviceId::new("session-fork-test-device"),
    }
}

fn accept_metafork_review(command: &mut SessionForkCommand) -> String {
    let metafork = command.metafork.as_ref().expect("metafork command");
    let digest = SessionMetaforkReviewManifest {
        command_id: command.command_id.clone(),
        source_session_id: command.source_session_id.clone(),
        worker_generation: command.worker_generation,
        source_branch_id: command.source_branch_id.clone(),
        fork_node_id: command.fork_node_id.clone(),
        fork_seq: command.fork_seq,
        name: command.name.clone(),
        description: metafork.description.clone(),
        model_proposal: metafork.model_proposal.clone(),
    }
    .digest()
    .expect("review digest");
    command
        .metafork
        .as_mut()
        .expect("metafork command")
        .accepted_proposal_digest = digest.clone();
    command.request_json = serde_json::json!({
        "method": "session.metafork",
        "source": &command.source_session_id,
        "fork_node_id": &command.fork_node_id,
        "fork_seq": command.fork_seq,
        "metafork": &command.metafork,
    })
    .to_string();
    command.request_digest = blake3::hash(command.request_json.as_bytes())
        .to_hex()
        .to_string();
    digest
}

/// MUTATION CHECK: change the clone loop's child `session_id` predicate to the
/// source id. Expected RUNTIME failure: this byte snapshot changes, proving a
/// fork corrupted the only authoritative parent transcript.
///
/// MUTATION CHECK: move receipt lookup after child insertion. Expected RUNTIME
/// failure: replaying the command creates a second session or grows the first
/// child journal instead of returning identical coordinates.
///
/// MUTATION CHECK: retain source event/causation IDs in the copied envelopes.
/// Expected RUNTIME failure: source and child ID sets overlap or a copied
/// causal link still points into the parent journal.
#[test]
fn session_fork_keeps_parent_byte_identical_and_replays_idempotently() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("fork-parent");
    create_session(&store, &source);
    let (_, node, seq, user_seq) = source_turn(&store, &source, "keep this history");
    let source_events = store
        .journal_replay(&source)
        .expect("source causal fixture");
    let queued = source_events
        .iter()
        .find(|envelope| {
            envelope.seq <= seq
                && matches!(
                    serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                    Ok(EventPayload::RunState(RunState::Queued))
                )
        })
        .expect("queued causal target");
    let user = source_events
        .iter()
        .find(|envelope| envelope.seq == user_seq)
        .expect("user causal target");
    let mut linked_node = source_events
        .iter()
        .find(|envelope| envelope.seq == seq)
        .expect("linked node")
        .clone();
    linked_node.causation_id = Some(user.event_id.clone());
    linked_node.correlation_id = Some(queued.event_id.clone());
    let fixture_connection =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("causal fixture db");
    fixture_connection
        .execute(
            "UPDATE events SET envelope_json = ?3 WHERE session_id = ?1 AND seq = ?2",
            rusqlite::params![
                source.as_str(),
                i64::try_from(seq).expect("node seq fits sqlite"),
                rmp_serde::to_vec_named(&linked_node).expect("encode linked source node"),
            ],
        )
        .expect("seed source causal links");
    drop(fixture_connection);
    let before = source_bytes(&store, &source);
    let storage_before = source_storage_bytes(root.path(), &source);
    let command = fork_command(
        &store,
        "fork-command",
        &source,
        "fork-child",
        node,
        seq,
        None,
    );
    let SessionForkOutcome::Committed { created, envelopes } =
        store.fork_session(&command).expect("fork session")
    else {
        panic!("fresh fork commits");
    };
    assert!(
        !serde_json::to_value(&created)
            .expect("created fork serializes")
            .as_object()
            .expect("created fork is an object")
            .contains_key("source_branch_id"),
        "main-line durable fork receipt must omit its absent source branch"
    );
    assert_eq!(source_bytes(&store, &source), before);
    assert_eq!(source_storage_bytes(root.path(), &source), storage_before);
    let source_event_ids = store
        .journal_replay(&source)
        .expect("source ids")
        .into_iter()
        .map(|envelope| envelope.event_id)
        .collect::<HashSet<_>>();
    let child_event_ids = envelopes
        .iter()
        .map(|envelope| envelope.event_id.clone())
        .collect::<HashSet<_>>();
    assert!(source_event_ids.is_disjoint(&child_event_ids));
    for envelope in &envelopes {
        if let Some(causation) = &envelope.causation_id {
            assert!(child_event_ids.contains(causation));
            assert!(!source_event_ids.contains(causation));
        }
        if let Some(correlation) = &envelope.correlation_id {
            assert!(child_event_ids.contains(correlation));
            assert!(!source_event_ids.contains(correlation));
        }
    }
    let child_linked_node = envelopes
        .iter()
        .find(|envelope| envelope.seq == seq)
        .expect("copied linked node");
    let child_user = envelopes
        .iter()
        .find(|envelope| envelope.seq == user_seq)
        .expect("copied user target");
    let child_queued = envelopes
        .iter()
        .find(|envelope| envelope.seq == queued.seq)
        .expect("copied queued target");
    assert_eq!(
        child_linked_node.causation_id.as_ref(),
        Some(&child_user.event_id)
    );
    assert_eq!(
        child_linked_node.correlation_id.as_ref(),
        Some(&child_queued.event_id)
    );
    let child_head = store
        .journal_replay(&created.session_id)
        .expect("child replay")
        .len();
    let session_count = store.session_ids().expect("sessions").len();

    let mut replay = command.clone();
    replay.session_id = SessionId::new("must-not-be-created");
    replay.audit_event_id = EventId::new("must-not-be-written");
    let SessionForkOutcome::IdempotentReplay { created: replayed } =
        store.fork_session(&replay).expect("fork receipt replay")
    else {
        panic!("replay is response-only");
    };
    assert_eq!(replayed, created);
    assert_eq!(store.session_ids().expect("sessions").len(), session_count);
    assert_eq!(
        store
            .journal_replay(&created.session_id)
            .expect("child replay")
            .len(),
        child_head
    );
    assert_eq!(source_bytes(&store, &source), before);
    assert_eq!(source_storage_bytes(root.path(), &source), storage_before);

    // Deleting the child removes its journal, never the append-only command
    // receipt proving that this fork already executed.
    store
        .delete_session(&created.session_id)
        .expect("forked child deletes");
    let mut replay_after_delete = command.clone();
    replay_after_delete.session_id = SessionId::new("must-not-be-created-after-delete");
    replay_after_delete.audit_event_id = EventId::new("must-not-be-written-after-delete");
    let SessionForkOutcome::IdempotentReplay {
        created: replayed_after_delete,
    } = store
        .fork_session(&replay_after_delete)
        .expect("fork receipt survives child deletion")
    else {
        panic!("replay after child deletion committed a second fork");
    };
    assert_eq!(replayed_after_delete, created);
    assert!(
        !store
            .session_ids()
            .expect("sessions after deleted-child replay")
            .contains(&replay_after_delete.session_id),
        "replay must not mint a replacement child"
    );
    drop(store);
    let reopened = Store::open(root.path()).expect("store reopens after child deletion");
    assert_eq!(
        reopened
            .session_fork_receipt(
                &command.command_id,
                &command.request_digest,
                &command.request_json,
            )
            .expect("fork receipt lookup after restart"),
        Some(created),
        "the original response remains recoverable after restart"
    );
}

/// MUTATION CHECK: persist `PromptRender::Omit` back into the matching source
/// event row instead of limiting it to the child clone. Expected RUNTIME
/// failure: the decoded and raw parent byte snapshots change after metafork,
/// exposing an unrecoverable transcript mutation.
///
/// MUTATION CHECK: remove `description` or `omissions` from `SessionForked`.
/// Expected RUNTIME failure: the child can no longer explain what was omitted
/// or which human instruction directed it.
///
/// MUTATION CHECK: delete selected rows instead of changing child prompt
/// rendering. Expected RUNTIME failure: `chocolate` disappears from the raw
/// child journal rather than remaining auditable with `prompt = omit`.
#[test]
fn metafork_omits_only_child_prompt_and_records_exact_removal() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("metafork-parent");
    create_session(&store, &source);
    let (_, node, seq, chocolate_seq) = source_turn(
        &store,
        &source,
        "chocolate belongs only in the auditable child journal",
    );
    let before = source_bytes(&store, &source);
    let storage_before = source_storage_bytes(root.path(), &source);
    let source_chocolate = store
        .journal_replay(&source)
        .expect("source chocolate event")
        .into_iter()
        .find(|envelope| envelope.seq == chocolate_seq)
        .expect("source chocolate coordinate");
    let proposal = SessionMetaforkProposal {
        removals: vec![SessionMetaforkRemoval {
            from_seq: chocolate_seq,
            through_seq: chocolate_seq,
            reason: "remove the chocolate discussion".into(),
            preview: Some("chocolate belongs only…".into()),
            reviewed_events: Vec::new(),
        }],
    };
    let mut command = fork_command(
        &store,
        "metafork-command",
        &source,
        "metafork-child",
        node,
        seq,
        Some(SessionMetaforkCommit {
            description: "remove parts about chocolate".into(),
            model_proposal: proposal,
            accepted_proposal_digest: String::new(),
        }),
    );
    let proposal_digest = accept_metafork_review(&mut command);
    let SessionForkOutcome::Committed { created, envelopes } =
        store.fork_session(&command).expect("metafork session")
    else {
        panic!("fresh metafork commits");
    };
    assert_eq!(source_bytes(&store, &source), before);
    assert_eq!(source_storage_bytes(root.path(), &source), storage_before);
    assert_eq!(created.mode, SessionForkMode::Metafork);
    assert_eq!(created.omission_count, 1);

    let omitted = envelopes
        .iter()
        .find(|envelope| envelope.payload.to_string().contains("chocolate belongs"))
        .expect("omitted content remains in child journal");
    assert_eq!(omitted.render.prompt, PromptRender::Omit);
    let record = envelopes
        .last()
        .and_then(|envelope| SessionForked::from_payload_value(&envelope.payload))
        .expect("fork audit fact");
    assert_eq!(
        record.description.as_deref(),
        Some("remove parts about chocolate")
    );
    assert_eq!(
        record.accepted_proposal_digest.as_deref(),
        Some(proposal_digest.as_str())
    );
    assert_eq!(record.omissions.len(), 1);
    assert_eq!(record.omissions[0].source_seq, chocolate_seq);
    assert_eq!(record.omissions[0].child_seq, omitted.seq);
    assert_eq!(
        record.omissions[0].source_event_id,
        source_chocolate.event_id
    );
    assert_eq!(record.omissions[0].child_event_id, omitted.event_id);
    assert_eq!(record.omissions[0].payload_kind, "user_message");
    assert_eq!(
        record.omissions[0].reason,
        "remove the chocolate discussion"
    );
    assert_eq!(record.context_epoch, ForkContextEpoch::Fresh);
}

/// MUTATION CHECK: remove the accepted-proposal digest comparison. Expected
/// RUNTIME failure: an unreviewed metafork creates a child and claims a receipt
/// even though the operator never accepted this exact removal manifest.
#[test]
fn unaccepted_metafork_commits_nothing() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("unaccepted-parent");
    create_session(&store, &source);
    let (_, node, seq, user_seq) = source_turn(&store, &source, "do not remove yet");
    let before = source_bytes(&store, &source);
    let storage_before = source_storage_bytes(root.path(), &source);
    let sessions_before = store.session_ids().expect("sessions");
    let proposal = SessionMetaforkProposal {
        removals: vec![SessionMetaforkRemoval {
            from_seq: user_seq,
            through_seq: user_seq,
            reason: "model proposal awaiting review".into(),
            preview: Some("do not remove yet".into()),
            reviewed_events: Vec::new(),
        }],
    };
    let command = fork_command(
        &store,
        "unaccepted-command",
        &source,
        "unaccepted-child",
        node,
        seq,
        Some(SessionMetaforkCommit {
            description: "remove the proposed text".into(),
            model_proposal: proposal,
            accepted_proposal_digest: "not-the-reviewed-digest".into(),
        }),
    );
    assert!(store.fork_session(&command).is_err());
    assert_eq!(store.session_ids().expect("sessions"), sessions_before);
    assert_eq!(source_bytes(&store, &source), before);
    assert_eq!(source_storage_bytes(root.path(), &source), storage_before);
    assert!(
        store
            .session_metafork_receipt(
                &command.command_id,
                &command.request_digest,
                &command.request_json,
            )
            .expect("receipt lookup")
            .is_none()
    );
}

/// MUTATION CHECK: count an already-`omit` source envelope as a removal, or
/// commit the child before verifying every removal changes prompt visibility.
/// Expected RUNTIME failure: a no-op removal is reported as editing history,
/// or the late validation error leaves a partial child/receipt behind.
#[test]
fn metafork_rejects_noop_omission_and_rolls_back_late_writes() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("noop-metafork-parent");
    create_session(&store, &source);
    let (_, node, seq, _) = source_turn(&store, &source, "visible history remains intact");
    let already_omitted_seq = store
        .journal_replay(&source)
        .expect("source journal")
        .into_iter()
        .find(|envelope| envelope.seq <= seq && envelope.render.prompt == PromptRender::Omit)
        .expect("copied structural omit")
        .seq;
    let source_before = source_storage_bytes(root.path(), &source);
    let sessions_before = store.session_ids().expect("sessions before");
    let mut command = fork_command(
        &store,
        "noop-metafork-command",
        &source,
        "noop-metafork-child",
        node,
        seq,
        Some(SessionMetaforkCommit {
            description: "remove only an already hidden structural event".into(),
            model_proposal: SessionMetaforkProposal {
                removals: vec![SessionMetaforkRemoval {
                    from_seq: already_omitted_seq,
                    through_seq: already_omitted_seq,
                    reason: "no visible prompt transition".into(),
                    preview: Some("already omitted".into()),
                    reviewed_events: Vec::new(),
                }],
            },
            accepted_proposal_digest: String::new(),
        }),
    );
    accept_metafork_review(&mut command);
    assert!(store.fork_session(&command).is_err());
    assert_eq!(
        store.session_ids().expect("sessions after"),
        sessions_before
    );
    assert_eq!(source_storage_bytes(root.path(), &source), source_before);
    assert!(
        store
            .session_metafork_receipt(
                &command.command_id,
                &command.request_digest,
                &command.request_json,
            )
            .expect("receipt lookup")
            .is_none()
    );
}

/// MUTATION CHECK: preserve copied source `branch_id`s instead of flattening
/// the selected lineage. Expected RUNTIME failure: the branch discussion is
/// absent from the child's ordinary main history even though it was selected.
#[test]
fn fork_from_named_branch_materializes_only_that_lineage_as_child_main() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("named-branch-parent");
    create_session(&store, &source);
    let (_, main_node, main_seq, _) = source_turn(&store, &source, "shared ancestor");
    let branch_id = BranchId::new("named-source-branch");
    let branch_request = serde_json::json!({
        "source": source,
        "fork_node_id": main_node,
        "fork_seq": main_seq,
    })
    .to_string();
    store
        .create_branch(&BranchCreateCommand {
            command_id: "create-named-source-branch".into(),
            request_digest: blake3::hash(branch_request.as_bytes()).to_hex().to_string(),
            request_json: branch_request,
            session_id: source.clone(),
            worker_generation: store.worker_generation(),
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id: main_node,
            fork_seq: main_seq,
            name: Some("named source".into()),
            event_id: EventId::new("named-source-branch-created"),
            device_id: DeviceId::new("session-fork-test-device"),
        })
        .expect("create source branch");

    let branch_run = RunId::new("named-source-branch-run");
    let turn_json = r#"{"text":"branch-only cocoa detail"}"#.to_owned();
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(&TurnAcceptCommand {
            command_id: "named-source-branch-turn".into(),
            request_digest: blake3::hash(turn_json.as_bytes()).to_hex().to_string(),
            request_json: turn_json,
            session_id: source.clone(),
            worker_generation: store.worker_generation(),
            run_id: branch_run.clone(),
            agent_id: None,
            branch_id: Some(branch_id.clone()),
            text: "branch-only cocoa detail".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("named-source-branch-queued"),
            user_event_id: EventId::new("named-source-branch-user"),
            active_event_id: EventId::new("named-source-branch-active"),
            device_id: DeviceId::new("session-fork-test-device"),
        })
        .expect("accept source branch turn")
    else {
        panic!("fresh branch turn commits");
    };
    let (branch_node, branch_seq) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, envelope.seq))
        })
        .expect("branch node");
    let mut done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("named-source-branch-done"),
        seq: 0,
        session_id: source.clone(),
        branch_id: Some(branch_id.clone()),
        run_id: Some(branch_run),
        agent_id: None,
        device_id: DeviceId::new("session-fork-test-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("payload"),
    }];
    store
        .append_worker(&mut done)
        .expect("terminal branch turn");

    let mut command = fork_command(
        &store,
        "fork-named-source-branch",
        &source,
        "named-branch-child",
        branch_node,
        branch_seq,
        None,
    );
    command.source_branch_id = Some(branch_id);
    let SessionForkOutcome::Committed { envelopes, .. } = store
        .fork_session(&command)
        .expect("fork named source branch")
    else {
        panic!("fresh named-branch fork commits");
    };
    assert!(
        envelopes
            .iter()
            .any(|envelope| envelope.payload.to_string().contains("branch-only cocoa"))
    );
    assert!(
        envelopes
            .iter()
            .all(|envelope| envelope.branch_id.is_none())
    );
    assert!(!envelopes.iter().any(|envelope| {
        envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            == Some("branch_created")
    }));
}

/// MUTATION CHECK: hard-code `agent_id.is_none()` in session-fork coordinate
/// validation or omit the owning-lane flatten. Expected RUNTIME failure: a
/// complete delegated source session cannot fork, or its copied discussion is
/// invisible from the independent child's ordinary root lane.
#[test]
fn delegated_source_owner_lane_becomes_child_root_history() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let parent = SessionId::new("delegated-fork-parent");
    let source = SessionId::new("delegated-fork-source");
    create_session(&store, &parent);
    create_session(&store, &source);
    let agent = AgentId::new("delegated-fork-agent");
    store
        .create_delegation(&DelegationRecord {
            agent_id: agent.clone(),
            child_session_id: source.clone(),
            child_run_id: RunId::new("delegated-fork-child-run"),
            parent_session_id: parent.clone(),
            parent_run_id: RunId::new("delegated-fork-parent-run"),
            parent_branch_id: None,
            call_id: "delegated-fork-call".into(),
            tool_item_id: ItemId::new("delegated-fork-item"),
            parent_agent_id: None,
            root_session_id: parent,
            depth: 1,
            task: "fork this delegated history".into(),
            prompt: "preserve the complete child session".into(),
            manifest: AgentManifest {
                agent: agent.clone(),
                role: AgentRole::Subagent,
                task: "fork this delegated history".into(),
                callsign: None,
                model_profile: "fake-model".into(),
                grant: Grant {
                    tools: Vec::new(),
                    effect_ceiling: Vec::new(),
                },
                budget_tokens: None,
                placement: Placement::Local,
                lease: LeaseId::new("delegated-fork-lease"),
                fencing_epoch: 1,
                attempt: 0,
                parent: None,
                coordinates: None,
                cli_scope: None,
            },
            state: DelegationState::Spawned,
            report: None,
        })
        .expect("record delegated source owner");
    let (_, node, seq, _) = source_turn_for_agent(
        &store,
        &source,
        Some(agent.clone()),
        "delegated history becomes a standalone root",
    );
    let source_before = source_storage_bytes(root.path(), &source);
    let command = fork_command(
        &store,
        "fork-delegated-source",
        &source,
        "forked-delegated-child",
        node,
        seq,
        None,
    );
    let SessionForkOutcome::Committed { envelopes, .. } = store
        .fork_session(&command)
        .expect("fork delegated source session")
    else {
        panic!("delegated fork commits");
    };
    let copied = envelopes
        .iter()
        .find(|envelope| {
            envelope
                .payload
                .to_string()
                .contains("delegated history becomes a standalone root")
        })
        .expect("delegated discussion copied");
    assert!(copied.agent_id.is_none());
    assert!(!envelopes.iter().any(|envelope| {
        envelope.agent_id.as_ref() == Some(&agent)
            && envelope
                .payload
                .to_string()
                .contains("delegated history becomes a standalone root")
    }));
    assert_eq!(source_storage_bytes(root.path(), &source), source_before);
}

/// A run id is not a complete activity coordinate: each agent lane reduces
/// independently. A terminal fact in one lane must neither close nor erase a
/// different lane's nonterminal fork-boundary obligation.
#[test]
fn fork_boundary_closes_each_nonterminal_agent_scope_independently() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("fork-agent-boundary-parent");
    create_session(&store, &source);
    let shared_run = RunId::new("fork-agent-boundary-shared-run");
    let agent_a = AgentId::new("fork-agent-boundary-a");
    let agent_b = AgentId::new("fork-agent-boundary-b");
    let agent_c = AgentId::new("fork-agent-boundary-c");
    let empty_agent = AgentId::new("");
    let scoped_state = |event_id: &str, agent_id: Option<AgentId>, state: RunState| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: source.clone(),
        branch_id: None,
        run_id: Some(shared_run.clone()),
        agent_id,
        device_id: DeviceId::new("session-fork-test-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(state)).expect("run state payload"),
    };
    let mut scoped_states = [
        scoped_state("fork-root-thinking", None, RunState::Thinking),
        scoped_state(
            "fork-empty-agent-thinking",
            Some(empty_agent.clone()),
            RunState::Thinking,
        ),
        scoped_state(
            "fork-agent-a-thinking",
            Some(agent_a.clone()),
            RunState::Thinking,
        ),
        scoped_state("fork-agent-b-done", Some(agent_b.clone()), RunState::Done),
        scoped_state(
            "fork-agent-c-thinking",
            Some(agent_c.clone()),
            RunState::Thinking,
        ),
    ];
    EventStore::append(&store, &mut scoped_states).expect("append agent-scoped run states");
    let (_, fork_node, fork_seq, _) = source_turn(&store, &source, "fork after scoped work");
    let command = fork_command(
        &store,
        "fork-agent-boundary-command",
        &source,
        "fork-agent-boundary-child",
        fork_node,
        fork_seq,
        None,
    );
    let SessionForkOutcome::Committed { envelopes, .. } = store
        .fork_session(&command)
        .expect("fork with agent-scoped boundaries")
    else {
        panic!("fresh fork commits");
    };

    let cancelled_agents = envelopes
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&shared_run))
        .filter(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::RunState(RunState::Cancelled))
            )
        })
        .map(|envelope| envelope.agent_id.clone())
        .collect::<HashSet<_>>();
    assert_eq!(
        cancelled_agents,
        HashSet::from([None, Some(empty_agent), Some(agent_a), Some(agent_c)]),
        "every independently nonterminal root/agent lane needs its own closure"
    );
}
