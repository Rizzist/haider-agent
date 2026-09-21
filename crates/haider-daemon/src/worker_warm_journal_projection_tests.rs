#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_protocol::state::RunState;

#[derive(Default)]
struct MutableProjectionStore {
    journal: StdMutex<Vec<RawEnvelope>>,
    full_read_starts: StdMutex<Vec<u64>>,
    reducer_starts: StdMutex<Vec<u64>>,
}

impl MutableProjectionStore {
    fn truncate_session(&self, session_id: &SessionId, through_seq: u64) {
        self.journal
            .lock()
            .expect("projection journal lock")
            .retain(|envelope| &envelope.session_id != session_id || envelope.seq <= through_seq);
    }

    fn replace_head_event_id(&self, session_id: &SessionId, event_id: &str) {
        let mut journal = self.journal.lock().expect("projection journal lock");
        let head = journal
            .iter_mut()
            .filter(|envelope| &envelope.session_id == session_id)
            .max_by_key(|envelope| envelope.seq)
            .expect("session head");
        head.event_id = EventId::new(event_id);
    }

    fn replace_head(&self, session_id: &SessionId, mut replacement: RawEnvelope) {
        let mut journal = self.journal.lock().expect("projection journal lock");
        let head = journal
            .iter_mut()
            .filter(|envelope| &envelope.session_id == session_id)
            .max_by_key(|envelope| envelope.seq)
            .expect("session head");
        replacement.seq = head.seq;
        replacement.committed_at_ms = head.committed_at_ms;
        *head = replacement;
    }

    fn take_full_read_starts(&self) -> Vec<u64> {
        std::mem::take(
            &mut *self
                .full_read_starts
                .lock()
                .expect("projection read counter"),
        )
    }

    fn take_reducer_starts(&self) -> Vec<u64> {
        std::mem::take(
            &mut *self
                .reducer_starts
                .lock()
                .expect("projection reducer counter"),
        )
    }
}

#[async_trait]
impl StoreHandle for MutableProjectionStore {
    async fn append(
        &self,
        envelopes: &mut [RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        let mut journal = self.journal.lock().expect("projection journal lock");
        let mut next_by_session = HashMap::<SessionId, u64>::new();
        for envelope in &*journal {
            next_by_session
                .entry(envelope.session_id.clone())
                .and_modify(|head| *head = (*head).max(envelope.seq))
                .or_insert(envelope.seq);
        }
        for envelope in &mut *envelopes {
            let next = next_by_session
                .entry(envelope.session_id.clone())
                .or_default();
            *next = next.saturating_add(1);
            envelope.seq = *next;
            if envelope.committed_at_ms == 0 {
                envelope.committed_at_ms = envelope.seq.saturating_mul(10);
            }
            journal.push(envelope.clone());
        }
        Ok(haider_core::CommittedRange {
            first_seq: envelopes.first().map_or(0, |envelope| envelope.seq),
            last_seq: envelopes.last().map_or(0, |envelope| envelope.seq),
        })
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.full_read_starts
            .lock()
            .expect("projection read counter")
            .push(since_seq);
        Ok(self
            .journal
            .lock()
            .expect("projection journal lock")
            .iter()
            .filter(|envelope| &envelope.session_id == session_id && envelope.seq > since_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn read_reducer_page_with_boundary(
        &self,
        session_id: &SessionId,
        cursor: haider_core::ReducerPageCursor,
        limit: usize,
        _byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<haider_core::ReducerPage, HaiderError> {
        self.reducer_starts
            .lock()
            .expect("projection reducer counter")
            .push(cursor.after_seq);
        let journal = self.journal.lock().expect("projection journal lock");
        let observed_head = journal
            .iter()
            .filter(|envelope| &envelope.session_id == session_id)
            .max_by_key(|envelope| envelope.seq)
            .map(|envelope| (envelope.seq, envelope.event_id.clone()));
        let observed_fence = cursor.fence_seq.and_then(|fence_seq| {
            journal
                .iter()
                .find(|envelope| &envelope.session_id == session_id && envelope.seq == fence_seq)
                .map(|envelope| (envelope.seq, envelope.event_id.clone()))
        });
        let envelopes = journal
            .iter()
            .filter(|envelope| {
                &envelope.session_id == session_id
                    && envelope.seq > cursor.after_seq
                    && envelope
                        .payload
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| payload_kinds.contains(&kind))
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(haider_core::ReducerPage {
            envelopes,
            observed_head,
            observed_fence,
        })
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        Ok(self
            .journal
            .lock()
            .expect("projection journal lock")
            .iter()
            .filter(|envelope| &envelope.session_id == session_id)
            .map(|envelope| envelope.seq)
            .max()
            .unwrap_or(0))
    }

    async fn branch_lineage(
        &self,
        _session_id: &SessionId,
        _branch_id: Option<&BranchId>,
    ) -> Result<Vec<haider_protocol::branch::BranchDescriptor>, HaiderError> {
        Ok(Vec::new())
    }
}

fn projection_envelope(
    session_id: &SessionId,
    run_id: Option<&RunId>,
    branch_id: Option<&BranchId>,
    suffix: &str,
    payload: serde_json::Value,
) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("warm-projection-{suffix}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: branch_id.cloned(),
        run_id: run_id.cloned(),
        agent_id: None,
        device_id: DeviceId::new("warm-projection-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    }
}

fn test_headless_spec() -> HeadlessRunSpecV1 {
    HeadlessRunSpecV1 {
        agent_spawn: None,
        continuation_of: None,
        cwd: "warm-projection-workspace".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_output_tokens: 64,
        effort: None,
        fast: false,
        seed: Some(7),
        permission_overrides: SessionPermissionOverridesV1::default(),
        trust_hooks: false,
        budget: RunBudgetV1 {
            max_time_ms: Some(60_000),
            ..RunBudgetV1::default()
        },
        request_deadline_unix_ms: None,
        replay_of: None,
    }
}

fn savings_payload(event: &ContextSavingsEvent, item: &str) -> serde_json::Value {
    serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(item),
        item: event.extension_item().expect("context savings item"),
    }))
    .expect("context savings payload")
}

async fn append(store: &dyn StoreHandle, mut envelopes: Vec<RawEnvelope>) -> Vec<RawEnvelope> {
    store
        .append(&mut envelopes)
        .await
        .expect("append projection fixture");
    envelopes
}

async fn legacy_headless_run_context(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<Option<DurableHeadlessRunContext>, HaiderError> {
    let mut cursor = 0;
    let mut accepted_at_ms = None;
    let mut branch_id = None;
    let mut spec = None;
    let mut exhausted = None;
    let mut deadline_exceeded = None;
    loop {
        let page = store.read(session_id, cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        for envelope in page {
            cursor = envelope.seq;
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            if accepted_at_ms.is_none() {
                accepted_at_ms = Some(envelope.committed_at_ms);
                branch_id = envelope.branch_id.clone();
            }
            match HeadlessRunEventPayload::from_payload_value(&envelope.payload) {
                Some(HeadlessRunEventPayload::HeadlessRunConfigured(configured)) => {
                    spec = Some(configured);
                }
                Some(HeadlessRunEventPayload::RunBudgetExhausted(fact)) => {
                    exhausted = Some(fact);
                }
                Some(HeadlessRunEventPayload::RunDeadlineExceeded(fact)) => {
                    deadline_exceeded = Some(fact);
                }
                None => {}
            }
        }
    }
    Ok(spec.map(|spec| DurableHeadlessRunContext {
        spec,
        accepted_at_ms: accepted_at_ms.unwrap_or(0),
        branch_id,
        exhausted,
        deadline_exceeded,
    }))
}

fn assert_headless_eq(
    cached: Option<DurableHeadlessRunContext>,
    legacy: Option<DurableHeadlessRunContext>,
) {
    match (cached, legacy) {
        (None, None) => {}
        (Some(cached), Some(legacy)) => {
            assert_eq!(cached.spec, legacy.spec);
            assert_eq!(cached.accepted_at_ms, legacy.accepted_at_ms);
            assert_eq!(cached.branch_id, legacy.branch_id);
            assert_eq!(cached.exhausted, legacy.exhausted);
            assert_eq!(cached.deadline_exceeded, legacy.deadline_exceeded);
        }
        _ => panic!("cached and legacy headless projections disagree on presence"),
    }
}

async fn assert_projection_parity(
    store: &MutableProjectionStore,
    cache: &WarmJournalProjectionCache,
    session_id: &SessionId,
    run_id: &RunId,
) -> (Vec<u64>, Vec<u64>) {
    let (cached_headless, cached_economy) =
        cached_turn_start_projection(store, session_id, cache, run_id)
            .await
            .expect("cached turn-start projection");
    let cache_reads = store.take_full_read_starts();
    let reducer_reads = store.take_reducer_starts();
    let legacy_headless = legacy_headless_run_context(store, session_id, run_id)
        .await
        .expect("legacy headless projection");
    let legacy_economy = PromptHistoryCompiler::latest_context_economy(store, session_id)
        .await
        .expect("legacy economy projection");
    assert_headless_eq(cached_headless, legacy_headless);
    assert_eq!(cached_economy, legacy_economy);
    store.take_full_read_starts();
    (cache_reads, reducer_reads)
}

#[tokio::test]
async fn warm_projection_matches_oracles_across_every_session_lifecycle_edge() {
    let store = MutableProjectionStore::default();
    let cache = WarmJournalProjectionCache::default();
    let session_id = SessionId::new("warm-projection-main");
    let run_id = RunId::new("warm-projection-run");
    let branch_id = BranchId::new("warm-projection-branch");
    let (economy_one, saving_one) =
        ContextEconomy::default().record(ContextCompactionTier::StructuralTrim24, 1_000, 700);
    append(
        &store,
        vec![
            projection_envelope(
                &session_id,
                Some(&run_id),
                Some(&branch_id),
                "queued",
                serde_json::to_value(EventPayload::RunState(RunState::Queued))
                    .expect("queued payload"),
            ),
            projection_envelope(
                &session_id,
                Some(&run_id),
                Some(&branch_id),
                "configured",
                HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                    .to_payload_value()
                    .expect("headless payload"),
            ),
            projection_envelope(
                &session_id,
                Some(&run_id),
                Some(&branch_id),
                "saving-one",
                savings_payload(&saving_one, "saving-one"),
            ),
        ],
    )
    .await;

    let (cold_reads, _) = assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert_eq!(cold_reads.first(), Some(&0));
    let (warm_reads, warm_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(
        warm_reads.is_empty(),
        "warm hit must not decode the journal"
    );
    assert!(warm_reducers.iter().all(|cursor| *cursor == 3));

    // Branch switching changes the durable head but not these session-global
    // facts. The verified boundary advances without a full replay.
    let branch_commit = append(
        &store,
        vec![projection_envelope(
            &session_id,
            None,
            Some(&BranchId::new("warm-projection-other-branch")),
            "branch-switch",
            serde_json::json!({"type":"branch_switched","branch_id":"other"}),
        )],
    )
    .await;
    cache.observe_committed(&branch_commit);
    let (branch_reads, branch_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(branch_reads.is_empty());
    assert!(branch_reducers.iter().all(|cursor| *cursor == 4));

    // Compaction/context editing contributes a new savings coordinate and an
    // irrelevant boundary event respectively; both stay oracle-identical.
    let (economy_two, saving_two) =
        economy_one.record(ContextCompactionTier::StructuralTrim12, 700, 500);
    let savings_commit = append(
        &store,
        vec![projection_envelope(
            &session_id,
            Some(&run_id),
            Some(&branch_id),
            "saving-two",
            savings_payload(&saving_two, "saving-two"),
        )],
    )
    .await;
    cache.observe_committed(&savings_commit);
    let (_, _) = assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    append(
        &store,
        vec![projection_envelope(
            &session_id,
            Some(&run_id),
            Some(&branch_id),
            "context-edit",
            serde_json::json!({"type":"context_edited","revision":1}),
        )],
    )
    .await;
    let (_, _) = assert_projection_parity(&store, &cache, &session_id, &run_id).await;

    // Checkpoint resume adds a later headless deadline fact.
    append(
        &store,
        vec![projection_envelope(
            &session_id,
            Some(&run_id),
            Some(&branch_id),
            "checkpoint-resume",
            HeadlessRunEventPayload::RunDeadlineExceeded(RunDeadlineExceededV1 {
                deadline_unix_ms: 42_000,
            })
            .to_payload_value()
            .expect("deadline payload"),
        )],
    )
    .await;
    let (_, _) = assert_projection_parity(&store, &cache, &session_id, &run_id).await;

    // A fork is a separate durable session and therefore a separate cache
    // coordinate; it must never inherit the parent's in-memory projection.
    let child_session = SessionId::new("warm-projection-child");
    let child_run = RunId::new("warm-projection-child-run");
    append(
        &store,
        vec![
            projection_envelope(
                &child_session,
                Some(&child_run),
                None,
                "child-queued",
                serde_json::to_value(EventPayload::RunState(RunState::Queued))
                    .expect("child queued payload"),
            ),
            projection_envelope(
                &child_session,
                Some(&child_run),
                None,
                "child-configured",
                HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                    .to_payload_value()
                    .expect("child headless payload"),
            ),
        ],
    )
    .await;
    let (child_reads, _) =
        assert_projection_parity(&store, &cache, &child_session, &child_run).await;
    assert_eq!(child_reads.first(), Some(&0));

    // A truncation and a same-sequence/different-event replacement both
    // invalidate the revision and force a full authoritative rebuild.
    store.truncate_session(&session_id, 5);
    let (rewind_reads, _) = assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(rewind_reads.contains(&0));
    assert_eq!(
        cached_latest_context_economy(&store, &session_id, &cache)
            .await
            .expect("economy after rewind"),
        Some(economy_two)
    );
    store.take_full_read_starts();
    store.take_reducer_starts();
    store.replace_head_event_id(&session_id, "warm-projection-replaced-head");
    let (replacement_reads, _) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(replacement_reads.contains(&0));

    // A fresh daemon cache (crash/reopen equivalent) never trusts the prior
    // in-memory prefix and reconstructs from durable authority.
    let restarted_cache = WarmJournalProjectionCache::default();
    let (restart_reads, _) =
        assert_projection_parity(&store, &restarted_cache, &session_id, &run_id).await;
    assert_eq!(restart_reads.first(), Some(&0));
}

#[tokio::test]
async fn warm_projection_revalidates_an_advanced_cached_boundary() {
    for appended_count in 1..=3_u64 {
        let store = MutableProjectionStore::default();
        let cache = WarmJournalProjectionCache::default();
        let session_id = SessionId::new(format!(
            "warm-projection-advanced-revision-{appended_count}"
        ));
        let run_id = RunId::new(format!(
            "warm-projection-advanced-revision-run-{appended_count}"
        ));
        append(
            &store,
            vec![
                projection_envelope(
                    &session_id,
                    Some(&run_id),
                    None,
                    "advanced-queued",
                    serde_json::to_value(EventPayload::RunState(RunState::Queued))
                        .expect("queued payload"),
                ),
                projection_envelope(
                    &session_id,
                    Some(&run_id),
                    None,
                    "advanced-configured",
                    HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                        .to_payload_value()
                        .expect("headless payload"),
                ),
            ],
        )
        .await;
        assert!(
            cached_headless_run_context(&store, &session_id, &cache, &run_id)
                .await
                .expect("prime warm projection")
                .is_some()
        );

        store.replace_head(
            &session_id,
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "advanced-replacement",
                serde_json::to_value(EventPayload::RunState(RunState::Done))
                    .expect("terminal replacement payload"),
            ),
        );
        append(
            &store,
            (1..=appended_count)
                .map(|ordinal| {
                    projection_envelope(
                        &session_id,
                        Some(&run_id),
                        None,
                        &format!("advanced-irrelevant-{ordinal}"),
                        serde_json::json!({
                            "type": "branch_switched",
                            "branch_id": format!("advanced-{ordinal}"),
                        }),
                    )
                })
                .collect(),
        )
        .await;
        store.take_full_read_starts();
        store.take_reducer_starts();

        let (full_reads, reducer_reads) =
            assert_projection_parity(&store, &cache, &session_id, &run_id).await;
        assert!(
            full_reads.contains(&0),
            "an advanced warm projection must rebuild when its cached boundary event changes"
        );
        assert_eq!(
            reducer_reads.first(),
            Some(&2),
            "the cached boundary must be checked before the authoritative replay"
        );
    }
}

#[tokio::test]
async fn malformed_context_record_refuses_identically_without_poisoning_headless_lookup() {
    let store = MutableProjectionStore::default();
    let cache = WarmJournalProjectionCache::default();
    let session_id = SessionId::new("warm-projection-corrupt");
    let run_id = RunId::new("warm-projection-corrupt-run");
    append(
        &store,
        vec![
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "corrupt-queued",
                serde_json::to_value(EventPayload::RunState(RunState::Queued))
                    .expect("queued payload"),
            ),
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "corrupt-configured",
                HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                    .to_payload_value()
                    .expect("headless payload"),
            ),
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "corrupt-saving",
                serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("corrupt-saving"),
                    item: TurnItem::Extension {
                        kind: haider_protocol::context::CONTEXT_SAVINGS_EXTENSION_KIND.into(),
                        data: serde_json::json!({"session_operation_count":"broken"}),
                    },
                }))
                .expect("malformed savings carrier"),
            ),
        ],
    )
    .await;

    let cached_headless = cached_headless_run_context(&store, &session_id, &cache, &run_id)
        .await
        .expect("headless projection ignores unrelated semantic corruption");
    let legacy_headless = legacy_headless_run_context(&store, &session_id, &run_id)
        .await
        .expect("legacy headless lookup");
    assert_headless_eq(cached_headless, legacy_headless);

    let cached_error = cached_latest_context_economy(&store, &session_id, &cache)
        .await
        .expect_err("cached projection must reject malformed savings");
    let legacy_error = PromptHistoryCompiler::latest_context_economy(&store, &session_id)
        .await
        .expect_err("legacy projection must reject malformed savings");
    assert_eq!(cached_error, legacy_error);
}

#[tokio::test]
async fn sqlite_reopen_rebuilds_the_projection_from_the_durable_journal() {
    let root = tempfile::tempdir().expect("warm projection store root");
    let session_id = SessionId::new("warm-projection-reopen");
    let run_id = RunId::new("warm-projection-reopen-run");
    let store = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("open projection store");
    let mut envelopes = vec![
        projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            "reopen-queued",
            serde_json::to_value(EventPayload::RunState(RunState::Queued)).expect("queued payload"),
        ),
        projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            "reopen-configured",
            HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                .to_payload_value()
                .expect("headless payload"),
        ),
    ];
    for envelope in &mut envelopes {
        envelope.worker_generation = store.worker_generation();
    }
    StoreHandle::append(&store, &mut envelopes)
        .await
        .expect("append reopen fixture");
    let first_cache = WarmJournalProjectionCache::default();
    let first = cached_headless_run_context(&store, &session_id, &first_cache, &run_id)
        .await
        .expect("first projection");
    drop(first_cache);
    store.close().await.expect("close projection store");

    let reopened = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen projection store");
    let reopened_cache = WarmJournalProjectionCache::default();
    let replayed = cached_headless_run_context(&reopened, &session_id, &reopened_cache, &run_id)
        .await
        .expect("replayed projection");
    assert_headless_eq(replayed, first);
    reopened.close().await.expect("close reopened store");
}
