#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::envelope::write_envelope_messagepack;
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_protocol::state::RunState;
use rusqlite::{Connection, params};
use std::sync::atomic::AtomicU64;

#[derive(Default)]
struct MutableProjectionStore {
    journal: StdMutex<Vec<RawEnvelope>>,
    mutation_generation: AtomicU64,
    full_read_starts: StdMutex<Vec<u64>>,
    reducer_starts: StdMutex<Vec<u64>>,
}

impl MutableProjectionStore {
    fn truncate_session(&self, session_id: &SessionId, through_seq: u64) {
        self.journal
            .lock()
            .expect("projection journal lock")
            .retain(|envelope| &envelope.session_id != session_id || envelope.seq <= through_seq);
        self.mutation_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn replace_head_event_id(&self, session_id: &SessionId, event_id: &str) {
        let mut journal = self.journal.lock().expect("projection journal lock");
        let head = journal
            .iter_mut()
            .filter(|envelope| &envelope.session_id == session_id)
            .max_by_key(|envelope| envelope.seq)
            .expect("session head");
        head.event_id = EventId::new(event_id);
        self.mutation_generation.fetch_add(1, Ordering::Relaxed);
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
        self.mutation_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn replace_at(
        &self,
        session_id: &SessionId,
        seq: u64,
        mut replacement: RawEnvelope,
        preserve_event_id: bool,
    ) {
        let mut journal = self.journal.lock().expect("projection journal lock");
        let retained = journal
            .iter_mut()
            .find(|envelope| &envelope.session_id == session_id && envelope.seq == seq)
            .expect("retained projection row");
        replacement.seq = retained.seq;
        replacement.committed_at_ms = retained.committed_at_ms;
        if preserve_event_id {
            replacement.event_id = retained.event_id.clone();
        }
        *retained = replacement;
        self.mutation_generation.fetch_add(1, Ordering::Relaxed);
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
        tokio::task::yield_now().await;
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
            observed_mutation_generation: Some(self.mutation_generation.load(Ordering::Relaxed)),
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

fn encode_envelope(envelope: &RawEnvelope) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_envelope_messagepack(&mut bytes, envelope).expect("encode projection envelope");
    bytes
}

fn cached_revision(
    cache: &WarmJournalProjectionCache,
    session_id: &SessionId,
) -> WarmJournalRevision {
    cache
        .entries
        .lock()
        .expect("projection cache lock")
        .projections
        .get(session_id)
        .and_then(|cached| cached.revision.clone())
        .expect("cached projection revision")
}

async fn seed_sqlite_projection(
    suffix: &str,
) -> (
    tempfile::TempDir,
    SessionId,
    RunId,
    WarmJournalProjectionCache,
    Vec<RawEnvelope>,
) {
    let root = tempfile::tempdir().expect("warm projection authority root");
    let session_id = SessionId::new(format!("warm-authority-{suffix}"));
    let run_id = RunId::new(format!("warm-authority-run-{suffix}"));
    let store = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("open warm projection authority store");
    let mut envelopes = vec![
        projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            &format!("{suffix}-queued"),
            serde_json::to_value(EventPayload::RunState(RunState::Queued)).expect("queued payload"),
        ),
        projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            &format!("{suffix}-configured"),
            HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                .to_payload_value()
                .expect("headless payload"),
        ),
        projection_envelope(
            &session_id,
            None,
            None,
            &format!("{suffix}-head"),
            serde_json::json!({"type":"branch_switched","branch_id":"stable"}),
        ),
    ];
    for envelope in &mut envelopes {
        envelope.worker_generation = store.worker_generation();
    }
    StoreHandle::append(&store, &mut envelopes)
        .await
        .expect("append warm projection authority fixture");
    let cache = WarmJournalProjectionCache::default();
    let seeded = cached_headless_run_context(&store, &session_id, &cache, &run_id)
        .await
        .expect("seed warm projection authority cache")
        .expect("seeded headless projection");
    assert_eq!(seeded.spec.max_output_tokens, 64);
    assert_eq!(cached_revision(&cache, &session_id).mutation_generation, 0);
    store
        .close()
        .await
        .expect("close seeded warm projection authority store");
    (root, session_id, run_id, cache, envelopes)
}

async fn assert_sqlite_cached_matches_fresh(
    root: &tempfile::TempDir,
    session_id: &SessionId,
    run_id: &RunId,
    cache: &WarmJournalProjectionCache,
) -> Option<DurableHeadlessRunContext> {
    let reopened = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen mutated warm projection store");
    let cached = cached_headless_run_context(&reopened, session_id, cache, run_id)
        .await
        .expect("revalidate seeded warm projection");
    let fresh_cache = WarmJournalProjectionCache::default();
    let fresh = cached_headless_run_context(&reopened, session_id, &fresh_cache, run_id)
        .await
        .expect("build fresh warm projection");
    assert_headless_eq(cached.clone(), fresh);
    let cached_authority = cached_revision(cache, session_id);
    let fresh_authority = cached_revision(&fresh_cache, session_id);
    assert_eq!(cached_authority, fresh_authority);
    assert!(
        cached_authority.mutation_generation > 0,
        "the seeded cache must adopt the advanced mutation generation"
    );
    reopened
        .close()
        .await
        .expect("close revalidated warm projection store");
    cached
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

async fn replay_derived_revision(
    store: &dyn StoreHandle,
    session_id: &SessionId,
) -> Result<Option<WarmJournalRevision>, HaiderError> {
    let mut cursor = 0;
    let mut replayed_head = None;
    loop {
        let page = store.read(session_id, cursor, 256).await?;
        if page.is_empty() {
            let authority = store
                .read_reducer_page_with_boundary(
                    session_id,
                    ReducerPageCursor {
                        after_seq: cursor,
                        fence_seq: (cursor != 0).then_some(cursor),
                    },
                    1,
                    1,
                    WARM_JOURNAL_PROJECTION_PAYLOAD_KINDS,
                )
                .await?;
            let observed = WarmJournalRevision::observed(&authority);
            if observed
                .as_ref()
                .map(|revision| (revision.head_seq, revision.head_event_id.clone()))
                != replayed_head
            {
                return Err(warm_projection_corrupt(
                    "replay-derived head disagrees with reducer authority",
                ));
            }
            return Ok(observed);
        }
        let Some(last) = page.last() else {
            return Err(warm_projection_corrupt(
                "nonempty revision oracle page had no last envelope",
            ));
        };
        if last.seq <= cursor {
            return Err(warm_projection_corrupt(
                "revision oracle did not advance its sequence cursor",
            ));
        }
        cursor = last.seq;
        replayed_head = Some((last.seq, last.event_id.clone()));
    }
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
    let seeded_revision = cache
        .entries
        .lock()
        .expect("projection cache lock")
        .projections
        .get(session_id)
        .and_then(|cached| cached.revision.clone());
    let replayed_revision = replay_derived_revision(store, session_id)
        .await
        .expect("replay-derived revision oracle");
    assert_eq!(seeded_revision, replayed_revision);
    store.take_full_read_starts();
    store.take_reducer_starts();
    (cache_reads, reducer_reads)
}

#[tokio::test]
async fn concurrent_warm_readers_share_one_reducer_seed() {
    let store = MutableProjectionStore::default();
    let cache = WarmJournalProjectionCache::default();
    let session_id = SessionId::new("warm-projection-concurrent");
    let run_id = RunId::new("warm-projection-concurrent-run");
    let (expected_economy, saving) =
        ContextEconomy::default().record(ContextCompactionTier::StructuralTrim24, 1_000, 700);
    append(
        &store,
        vec![
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "concurrent-queued",
                serde_json::to_value(EventPayload::RunState(RunState::Queued))
                    .expect("queued payload"),
            ),
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "concurrent-configured",
                HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                    .to_payload_value()
                    .expect("headless payload"),
            ),
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "concurrent-saving",
                savings_payload(&saving, "concurrent-saving"),
            ),
        ],
    )
    .await;

    let (headless, economy) = tokio::join!(
        cached_headless_run_context(&store, &session_id, &cache, &run_id),
        cached_latest_context_economy(&store, &session_id, &cache),
    );
    assert!(headless.expect("concurrent headless projection").is_some());
    assert_eq!(
        economy.expect("concurrent economy projection"),
        Some(expected_economy)
    );
    assert!(store.take_full_read_starts().is_empty());
    assert_eq!(store.take_reducer_starts(), vec![0, 3, 3]);
    let seeded_revision = cache
        .entries
        .lock()
        .expect("projection cache lock")
        .projections
        .get(&session_id)
        .and_then(|cached| cached.revision.clone());
    assert_eq!(
        seeded_revision,
        replay_derived_revision(&store, &session_id)
            .await
            .expect("replay-derived revision oracle")
    );
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

    let (cold_reads, cold_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(
        cold_reads.is_empty(),
        "the exact reducer boundary must seed a cold projection"
    );
    assert_eq!(cold_reducers, vec![0, 3]);
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
    let (committed_suffix_reads, committed_suffix_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(committed_suffix_reads.is_empty());
    assert_eq!(committed_suffix_reducers, vec![5]);
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
    let (missed_irrelevant_reads, missed_irrelevant_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(missed_irrelevant_reads.is_empty());
    assert_eq!(missed_irrelevant_reducers, vec![5]);

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
    let (missed_fact_reads, missed_fact_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(missed_fact_reads.is_empty());
    assert_eq!(missed_fact_reducers, vec![6, 7]);

    // A later publication after a missed commit must preserve the last
    // reducer-verified prefix. The next exact boundary catches up both facts
    // from that prefix instead of rebuilding the projection from zero.
    append(
        &store,
        vec![projection_envelope(
            &session_id,
            Some(&run_id),
            Some(&branch_id),
            "missed-before-publication",
            serde_json::json!({"type":"context_edited","revision":2}),
        )],
    )
    .await;
    let (_, saving_three) = economy_two.record(ContextCompactionTier::StructuralTrim12, 500, 450);
    let observed_after_gap = append(
        &store,
        vec![projection_envelope(
            &session_id,
            Some(&run_id),
            Some(&branch_id),
            "observed-after-gap",
            savings_payload(&saving_three, "observed-after-gap"),
        )],
    )
    .await;
    cache.observe_committed(&observed_after_gap);
    let (gap_reads, gap_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(gap_reads.is_empty());
    assert_eq!(gap_reducers, vec![7, 9]);

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
    let (child_reads, child_reducers) =
        assert_projection_parity(&store, &cache, &child_session, &child_run).await;
    assert!(child_reads.is_empty());
    assert_eq!(child_reducers, vec![0, 2]);

    // A truncation and a same-sequence/different-event replacement both
    // invalidate the seeded revision and force a reducer rebuild from zero.
    store.truncate_session(&session_id, 5);
    let (rewind_reads, rewind_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(rewind_reads.is_empty());
    assert!(rewind_reducers.contains(&0));
    assert_eq!(
        cached_latest_context_economy(&store, &session_id, &cache)
            .await
            .expect("economy after rewind"),
        Some(economy_two)
    );
    store.take_full_read_starts();
    store.take_reducer_starts();
    store.replace_head_event_id(&session_id, "warm-projection-replaced-head");
    let (replacement_reads, replacement_reducers) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(replacement_reads.is_empty());
    assert!(replacement_reducers.contains(&0));

    // A fresh daemon cache (crash/reopen equivalent) never trusts the prior
    // in-memory prefix and reconstructs from the exact reducer boundary.
    let restarted_cache = WarmJournalProjectionCache::default();
    let (restart_reads, restart_reducers) =
        assert_projection_parity(&store, &restarted_cache, &session_id, &run_id).await;
    assert!(restart_reads.is_empty());
    assert_eq!(restart_reducers, vec![0, 5]);
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
            full_reads.is_empty(),
            "an advanced warm projection must rebuild through the reducer"
        );
        assert_eq!(
            reducer_reads.first(),
            Some(&2),
            "the cached boundary must be checked before the authoritative replay"
        );
        assert!(
            reducer_reads.contains(&0),
            "a rejected cached boundary must restart a reducer seed from zero"
        );
    }
}

async fn assert_warm_prefix_mutation_replays(replaced_seq: u64, preserve_event_id: bool) {
    let store = MutableProjectionStore::default();
    let cache = WarmJournalProjectionCache::default();
    let session_id = SessionId::new(format!(
        "warm-prefix-mutation-{replaced_seq}-{preserve_event_id}"
    ));
    let run_id = RunId::new(format!(
        "warm-prefix-mutation-run-{replaced_seq}-{preserve_event_id}"
    ));
    append(
        &store,
        vec![
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "prefix-queued",
                serde_json::to_value(EventPayload::RunState(RunState::Queued))
                    .expect("queued payload"),
            ),
            projection_envelope(
                &session_id,
                Some(&run_id),
                None,
                "prefix-configured",
                HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                    .to_payload_value()
                    .expect("headless payload"),
            ),
            projection_envelope(
                &session_id,
                None,
                None,
                "prefix-stable-head",
                serde_json::json!({"type":"branch_switched","branch_id":"stable"}),
            ),
        ],
    )
    .await;
    assert_projection_parity(&store, &cache, &session_id, &run_id).await;

    let replacement = if replaced_seq == 1 {
        let mut replacement = projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            "prefix-replacement-state",
            serde_json::to_value(EventPayload::RunState(RunState::Queued))
                .expect("queued replacement payload"),
        );
        replacement.branch_id = Some(BranchId::new("replacement-branch"));
        replacement
    } else {
        let mut spec = test_headless_spec();
        spec.max_output_tokens = 128;
        projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            "prefix-replacement-config",
            HeadlessRunEventPayload::HeadlessRunConfigured(spec)
                .to_payload_value()
                .expect("replacement headless payload"),
        )
    };
    store.replace_at(&session_id, replaced_seq, replacement, preserve_event_id);
    append(
        &store,
        vec![projection_envelope(
            &session_id,
            None,
            None,
            "prefix-appended",
            serde_json::json!({"type":"branch_switched","branch_id":"appended"}),
        )],
    )
    .await;
    store.take_full_read_starts();
    store.take_reducer_starts();

    let (full_reads, reducer_reads) =
        assert_projection_parity(&store, &cache, &session_id, &run_id).await;
    assert!(
        full_reads.is_empty(),
        "retained-prefix mutation at seq {replaced_seq} must use the reducer"
    );
    assert!(
        reducer_reads.contains(&0),
        "retained-prefix mutation at seq {replaced_seq} must reseed from zero"
    );
}

#[tokio::test]
async fn warm_projection_revalidates_every_retained_position_below_head() {
    assert_warm_prefix_mutation_replays(1, false).await;
    assert_warm_prefix_mutation_replays(2, false).await;
}

#[tokio::test]
async fn warm_projection_revalidates_same_id_payload_mutation() {
    assert_warm_prefix_mutation_replays(2, true).await;
}

#[tokio::test]
async fn seeded_warm_projection_obeys_every_mutation_authority_fence() {
    // `payload_kind` participates in reducer membership. A seeded entry must
    // reject its old generation and converge with a fresh reducer seed.
    let (root, session_id, run_id, cache, _) = seed_sqlite_projection("payload-kind").await;
    let connection = Connection::open(root.path().join("store.sqlite"))
        .expect("open payload-kind mutation connection");
    connection
        .execute(
            "UPDATE events SET payload_kind = 'excluded_fixture'
              WHERE session_id = ?1 AND seq = 2",
            [session_id.as_str()],
        )
        .expect("change warm projection membership");
    drop(connection);
    assert!(
        assert_sqlite_cached_matches_fresh(&root, &session_id, &run_id, &cache)
            .await
            .is_none(),
        "the excluded configuration must disappear from both reducer seeds"
    );

    // An authoritative payload rewrite below the head keeps the same event
    // identity but changes the projected value.
    let (root, session_id, run_id, cache, envelopes) =
        seed_sqlite_projection("below-head-update").await;
    let mut replacement = envelopes[1].clone();
    let mut spec = test_headless_spec();
    spec.max_output_tokens = 128;
    replacement.payload = HeadlessRunEventPayload::HeadlessRunConfigured(spec)
        .to_payload_value()
        .expect("updated headless payload")
        .into();
    let connection = Connection::open(root.path().join("store.sqlite"))
        .expect("open below-head mutation connection");
    connection
        .execute(
            "UPDATE events SET envelope_json = ?3
              WHERE session_id = ?1 AND seq = ?2",
            params![session_id.as_str(), 2_i64, encode_envelope(&replacement)],
        )
        .expect("rewrite warm projection row below head");
    drop(connection);
    let updated = assert_sqlite_cached_matches_fresh(&root, &session_id, &run_id, &cache)
        .await
        .expect("updated seeded projection");
    assert_eq!(updated.spec.max_output_tokens, 128);

    // SQLite's implicit DELETE for a same-key REPLACE is not recursive. The
    // high-water/key-shadow insert guard must still advance authority.
    let (root, session_id, run_id, cache, envelopes) =
        seed_sqlite_projection("same-key-replace").await;
    let mut replacement = envelopes[1].clone();
    let mut spec = test_headless_spec();
    spec.max_output_tokens = 256;
    replacement.payload = HeadlessRunEventPayload::HeadlessRunConfigured(spec)
        .to_payload_value()
        .expect("replacement headless payload")
        .into();
    let connection = Connection::open(root.path().join("store.sqlite"))
        .expect("open same-key replace connection");
    connection
        .execute(
            "INSERT OR REPLACE INTO events(
                 session_id, seq, envelope_json, event_id, committed_at_ms, payload_kind
             )
             SELECT session_id, seq, ?3, event_id, committed_at_ms, payload_kind
               FROM events WHERE session_id = ?1 AND seq = ?2",
            params![session_id.as_str(), 2_i64, encode_envelope(&replacement)],
        )
        .expect("replace same warm projection key");
    drop(connection);
    let replaced = assert_sqlite_cached_matches_fresh(&root, &session_id, &run_id, &cache)
        .await
        .expect("same-key replacement projection");
    assert_eq!(replaced.spec.max_output_tokens, 256);

    // A unique-event-id REPLACE can implicitly delete a row from a different
    // session. The shadow guard must invalidate the seeded source session.
    let (root, session_id, run_id, cache, envelopes) =
        seed_sqlite_projection("event-id-replace").await;
    let other_session = SessionId::new("warm-authority-event-id-replace-other");
    let store = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store for conflict destination");
    let mut other = [projection_envelope(
        &other_session,
        None,
        None,
        "event-id-replace-other-head",
        serde_json::json!({"type":"branch_switched","branch_id":"other"}),
    )];
    other[0].worker_generation = store.worker_generation();
    StoreHandle::append(&store, &mut other)
        .await
        .expect("append conflict destination fixture");
    store
        .close()
        .await
        .expect("close conflict destination store");
    let mut moved = envelopes[1].clone();
    moved.session_id = other_session.clone();
    moved.seq = 2;
    let connection = Connection::open(root.path().join("store.sqlite"))
        .expect("open event-id replace connection");
    connection
        .pragma_update(None, "foreign_keys", false)
        .expect("disable foreign keys for raw cross-session replacement");
    let payload_kind: String = connection
        .query_row(
            "SELECT payload_kind FROM events WHERE session_id = ?1 AND seq = 2",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .expect("read conflict payload kind");
    connection
        .execute(
            "INSERT OR REPLACE INTO events(
                 session_id, seq, envelope_json, event_id, committed_at_ms, payload_kind
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                other_session.as_str(),
                2_i64,
                encode_envelope(&moved),
                moved.event_id.as_str(),
                i64::try_from(moved.committed_at_ms).expect("test timestamp fits SQLite"),
                payload_kind,
            ],
        )
        .expect("replace unique event identity across sessions");
    drop(connection);
    assert!(
        assert_sqlite_cached_matches_fresh(&root, &session_id, &run_id, &cache)
            .await
            .is_none(),
        "the source projection must lose the implicitly deleted configuration"
    );

    // Losing an events trigger creates an unguarded interval. Store open must
    // reinstall the authority class and advance the generation before a
    // previously seeded cache entry can be trusted again.
    let (root, session_id, run_id, cache, _) = seed_sqlite_projection("trigger-repair").await;
    let connection =
        Connection::open(root.path().join("store.sqlite")).expect("open trigger repair connection");
    connection
        .execute_batch("DROP TRIGGER events_authority_updated;")
        .expect("drop warm projection authority trigger");
    drop(connection);
    let repaired = assert_sqlite_cached_matches_fresh(&root, &session_id, &run_id, &cache)
        .await
        .expect("trigger-repaired projection");
    assert_eq!(repaired.spec.max_output_tokens, 64);
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

#[tokio::test]
async fn seeded_and_cached_projection_refuse_the_same_corrupt_matching_envelope() {
    let root = tempfile::tempdir().expect("warm projection corrupt store root");
    let session_id = SessionId::new("warm-projection-store-corrupt");
    let run_id = RunId::new("warm-projection-store-corrupt-run");
    let store = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("open warm projection corrupt store");
    let mut prefix = vec![
        projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            "store-corrupt-queued",
            serde_json::to_value(EventPayload::RunState(RunState::Queued)).expect("queued payload"),
        ),
        projection_envelope(
            &session_id,
            Some(&run_id),
            None,
            "store-corrupt-configured",
            HeadlessRunEventPayload::HeadlessRunConfigured(test_headless_spec())
                .to_payload_value()
                .expect("headless payload"),
        ),
    ];
    for envelope in &mut prefix {
        envelope.worker_generation = store.worker_generation();
    }
    StoreHandle::append(&store, &mut prefix)
        .await
        .expect("append warm projection corrupt prefix");

    let cached = WarmJournalProjectionCache::default();
    cached_headless_run_context(&store, &session_id, &cached, &run_id)
        .await
        .expect("seed cached projection");

    let (_, savings) =
        ContextEconomy::default().record(ContextCompactionTier::StructuralTrim24, 1_000, 700);
    let mut corrupt = [projection_envelope(
        &session_id,
        Some(&run_id),
        None,
        "store-corrupt-matching",
        savings_payload(&savings, "store-corrupt-matching"),
    )];
    corrupt[0].worker_generation = store.worker_generation();
    StoreHandle::append(&store, &mut corrupt)
        .await
        .expect("append warm projection corruption target");
    store.close().await.expect("close before corruption");

    let raw = rusqlite::Connection::open(root.path().join("store.sqlite"))
        .expect("open raw warm projection journal");
    raw.execute(
        "UPDATE events SET envelope_json = X'C1' WHERE event_id = ?1",
        [corrupt[0].event_id.as_str()],
    )
    .expect("corrupt matching warm projection envelope");
    drop(raw);

    let reopened = haider_core::SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen corrupt warm projection store");
    let cached_error =
        match cached_turn_start_projection(&reopened, &session_id, &cached, &run_id).await {
            Ok(_) => panic!("cached suffix must refuse corrupt matching envelope"),
            Err(error) => error,
        };
    let seeded = WarmJournalProjectionCache::default();
    let seeded_error =
        match cached_turn_start_projection(&reopened, &session_id, &seeded, &run_id).await {
            Ok(_) => panic!("seeded projection must refuse corrupt matching envelope"),
            Err(error) => error,
        };
    assert_eq!(cached_error, seeded_error);
    reopened
        .close()
        .await
        .expect("close corrupt warm projection store");
}
