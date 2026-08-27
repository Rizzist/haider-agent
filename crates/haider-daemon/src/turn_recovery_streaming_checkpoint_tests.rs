#![allow(clippy::expect_used)]

use super::*;

#[derive(Default)]
struct RecordingVisitor {
    active_session: Option<SessionId>,
    counts: HashMap<SessionId, usize>,
    pages: usize,
}

#[async_trait::async_trait]
impl StartupJournalVisitor for RecordingVisitor {
    async fn start_session(&mut self, session_id: &SessionId) -> Result<u64, HaiderError> {
        assert!(self.active_session.replace(session_id.clone()).is_none());
        Ok(0)
    }

    async fn visit_page(
        &mut self,
        session_id: &SessionId,
        page: &[RawEnvelope],
    ) -> Result<(), HaiderError> {
        assert_eq!(self.active_session.as_ref(), Some(session_id));
        *self.counts.entry(session_id.clone()).or_default() += page.len();
        self.pages += 1;
        Ok(())
    }

    async fn finish_session(
        &mut self,
        _store: &SqliteStoreHandle,
        session_id: &SessionId,
    ) -> Result<(), HaiderError> {
        assert_eq!(self.active_session.take().as_ref(), Some(session_id));
        Ok(())
    }
}

fn fact(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    event_id: &str,
    payload: EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("turn-checkpoint-test"),
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
        payload: serde_json::to_value(payload).expect("encode recovery fact"),
    }
}

#[tokio::test]
async fn turn_recovery_checkpoint_resumes_at_its_journal_high_water() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("turn-checkpoint-resume");
    let run_id = RunId::new("turn-checkpoint-run");
    let mut prefix = [
        fact(
            &store,
            &session_id,
            &run_id,
            "turn-checkpoint-user",
            EventPayload::UserMessage {
                text: "hello".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Steer,
            },
        ),
        fact(
            &store,
            &session_id,
            &run_id,
            "turn-checkpoint-thinking",
            EventPayload::RunState(RunState::Thinking),
        ),
    ];
    StoreHandle::append(&store, &mut prefix)
        .await
        .expect("append prefix");
    let mut reductions = HashMap::new();
    for envelope in &prefix {
        reduce(&mut reductions, envelope);
    }
    let boundary = prefix.last().expect("prefix boundary");
    put_recovery_checkpoint(
        &store,
        &session_id,
        boundary.seq,
        boundary.event_id.clone(),
        &reductions,
    )
    .await
    .expect("persist checkpoint");
    let mut suffix = [fact(
        &store,
        &session_id,
        &run_id,
        "turn-checkpoint-streaming",
        EventPayload::RunState(RunState::Streaming),
    )];
    StoreHandle::append(&store, &mut suffix)
        .await
        .expect("append suffix");

    let (resumed, cursor) = load_recovery_checkpoint(&store, &session_id)
        .await
        .expect("load checkpoint");
    assert_eq!(cursor, 2, "only the suffix remains to fold");
    let resumed_bytes = rmp_serde::to_vec_named(&DurableTurnRecoveryCheckpointRef {
        shape_version: CHECKPOINT_SHAPE_VERSION,
        reducer_version: CHECKPOINT_REDUCER_VERSION,
        through_seq: cursor,
        reductions: &resumed,
    })
    .expect("encode resumed state");
    let checkpoint = store
        .projection_checkpoint(
            &session_id,
            CHECKPOINT_PROJECTION.to_owned(),
            CHECKPOINT_TIMELINE.to_owned(),
        )
        .await
        .expect("read checkpoint")
        .expect("checkpoint exists");
    assert_eq!(resumed_bytes, checkpoint.payload);

    let recovered =
        recover_interrupted_turns(&store, &DeviceId::new("turn-checkpoint-resume-device"))
            .await
            .expect("fold checkpoint suffix");
    assert!(recovered.is_empty());
    let (resumed, cursor) = load_recovery_checkpoint(&store, &session_id)
        .await
        .expect("load advanced checkpoint");
    assert_eq!(cursor, 3);
    let reduction = resumed.get(&run_id).expect("resumed run reduction");
    assert_eq!(reduction.user_seq, Some(1));
    assert_eq!(
        reduction.state.as_ref().map(|(state, _)| state),
        Some(&RunState::Streaming)
    );
}

#[tokio::test]
async fn corrupt_turn_recovery_checkpoint_falls_back_to_streaming_from_zero() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("turn-checkpoint-corrupt");
    let run_id = RunId::new("turn-checkpoint-corrupt-run");
    let mut envelope = [fact(
        &store,
        &session_id,
        &run_id,
        "turn-checkpoint-corrupt-fact",
        EventPayload::RunState(RunState::Thinking),
    )];
    StoreHandle::append(&store, &mut envelope)
        .await
        .expect("append journal fact");
    store
        .put_projection_checkpoint(SessionProjectionCheckpoint {
            session_id: session_id.clone(),
            projection: CHECKPOINT_PROJECTION.to_owned(),
            timeline_key: CHECKPOINT_TIMELINE.to_owned(),
            through_seq: envelope[0].seq,
            boundary_event_id: envelope[0].event_id.clone(),
            payload: b"not a turn reducer checkpoint".to_vec(),
        })
        .await
        .expect("install corrupt checkpoint payload");

    let (reductions, cursor) = load_recovery_checkpoint(&store, &session_id)
        .await
        .expect("corrupt checkpoint is a cache miss");
    assert_eq!(cursor, 0);
    assert!(reductions.is_empty());

    recover_interrupted_turns(&store, &DeviceId::new("turn-checkpoint-corrupt-device"))
        .await
        .expect("full streaming fallback repairs checkpoint");
    let (reductions, cursor) = load_recovery_checkpoint(&store, &session_id)
        .await
        .expect("load repaired checkpoint");
    assert_eq!(cursor, 1);
    assert_eq!(
        reductions
            .get(&run_id)
            .and_then(|reduction| reduction.state.as_ref())
            .map(|(state, _)| state),
        Some(&RunState::Thinking)
    );
}

#[tokio::test]
async fn shared_visitor_receives_each_multi_session_page_once() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut expected = HashMap::new();
    for name in ["shared-stream-alpha", "shared-stream-beta"] {
        let session_id = SessionId::new(name);
        let first_run_id = RunId::new(format!("{name}-run-0"));
        let mut envelopes = (0..=PAGE_SIZE)
            .map(|index| {
                let run_id = RunId::new(format!("{name}-run-{index}"));
                fact(
                    &store,
                    &session_id,
                    &run_id,
                    &format!("{name}-{index}"),
                    EventPayload::RunState(RunState::Done),
                )
            })
            .collect::<Vec<_>>();
        let mut post_terminal_hook = fact(
            &store,
            &session_id,
            &first_run_id,
            &format!("{name}-post-terminal-hook"),
            EventPayload::RunState(RunState::Done),
        );
        post_terminal_hook.payload = haider_protocol::hook::HookEventPayload::HookNotice(
            haider_protocol::hook::HookNotice {
                hook: Some("post-terminal".into()),
                digest: None,
                source: "startup-test".into(),
                reason: "valid asynchronous suffix".into(),
            },
        )
        .to_payload_value()
        .expect("encode hook suffix");
        envelopes.push(post_terminal_hook);
        let selected_envelopes = envelopes.len() - 1;
        StoreHandle::append(&store, &mut envelopes)
            .await
            .expect("append page-crossing recovery journal");
        expected.insert(session_id, selected_envelopes);
    }

    let mut visitor = RecordingVisitor::default();
    let recovery = recover_interrupted_turns_report_with_visitor(
        &store,
        &DeviceId::new("shared-stream-device"),
        &mut visitor,
    )
    .await
    .expect("shared startup scan");
    assert!(recovery.work.is_empty());
    assert_eq!(visitor.counts, expected);
    assert!(
        visitor.pages >= 4,
        "each session must cross a page boundary"
    );
    assert!(visitor.active_session.is_none());
}
