#![allow(clippy::expect_used)]

use super::*;
use crate::{CommittedRange, MemoryStore, ReducerPage, ReducerPageCursor, StoreHandle};
use async_trait::async_trait;
use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{EventEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{DeviceId, NodeId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::state::RunState;
use haider_protocol::verify::VerifyVerdict;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

struct NoArtifacts;

#[async_trait]
impl ArtifactReader for NoArtifacts {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!("unexpected artifact read for {artifact:?}"),
            false,
        ))
    }
}

#[derive(Default)]
struct RevisionStore {
    envelopes: StdMutex<Vec<RawEnvelope>>,
    full_reads: AtomicUsize,
    suffix_reads: AtomicUsize,
}

impl RevisionStore {
    fn reset_read_counts(&self) {
        self.full_reads.store(0, AtomicOrdering::Relaxed);
        self.suffix_reads.store(0, AtomicOrdering::Relaxed);
    }

    fn full_read_count(&self) -> usize {
        self.full_reads.load(AtomicOrdering::Relaxed)
    }

    fn suffix_read_count(&self) -> usize {
        self.suffix_reads.load(AtomicOrdering::Relaxed)
    }

    fn replace_head(&self, mut replacement: RawEnvelope) {
        let mut envelopes = self.envelopes.lock().expect("revision journal");
        let head = envelopes.last_mut().expect("revision head");
        replacement.seq = head.seq;
        replacement.committed_at_ms = head.committed_at_ms;
        *head = replacement;
    }

    fn truncate(&self, through_seq: u64) {
        self.envelopes
            .lock()
            .expect("revision journal")
            .retain(|envelope| envelope.seq <= through_seq);
    }
}

#[async_trait]
impl StoreHandle for RevisionStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let mut stored = self.envelopes.lock().expect("revision journal");
        let first_seq = u64::try_from(stored.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        for envelope in envelopes.iter_mut() {
            envelope.seq = u64::try_from(stored.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            envelope.committed_at_ms = envelope.seq;
            stored.push(envelope.clone());
        }
        Ok(CommittedRange {
            first_seq: if envelopes.is_empty() { 0 } else { first_seq },
            last_seq: envelopes.last().map_or(0, |envelope| envelope.seq),
        })
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.full_reads.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(self
            .envelopes
            .lock()
            .expect("revision journal")
            .iter()
            .filter(|envelope| envelope.session_id == *session_id && envelope.seq > since_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn read_reducer_page_with_boundary(
        &self,
        session_id: &SessionId,
        cursor: ReducerPageCursor,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<ReducerPage, HaiderError> {
        self.suffix_reads.fetch_add(1, AtomicOrdering::Relaxed);
        let stored = self.envelopes.lock().expect("revision journal");
        let observed_head = stored
            .iter()
            .rev()
            .find(|envelope| envelope.session_id == *session_id)
            .map(|envelope| (envelope.seq, envelope.event_id.clone()));
        let observed_fence = cursor.fence_seq.and_then(|fence_seq| {
            stored
                .iter()
                .find(|envelope| envelope.session_id == *session_id && envelope.seq == fence_seq)
                .map(|envelope| (envelope.seq, envelope.event_id.clone()))
        });
        let mut spent = 0_usize;
        let mut selected = Vec::new();
        for envelope in stored.iter().filter(|envelope| {
            envelope.session_id == *session_id
                && envelope.seq > cursor.after_seq
                && payload_kinds.contains(&crate::envelope_payload_kind(envelope))
        }) {
            let weight = envelope_weight_bytes(envelope);
            if !selected.is_empty() && spent.saturating_add(weight) > byte_budget {
                break;
            }
            spent = spent.saturating_add(weight);
            selected.push(envelope.clone());
            if selected.len() >= limit || spent >= byte_budget {
                break;
            }
        }
        Ok(ReducerPage {
            envelopes: selected,
            observed_head,
            observed_fence,
        })
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        Ok(self
            .envelopes
            .lock()
            .expect("revision journal")
            .iter()
            .rev()
            .find(|envelope| envelope.session_id == *session_id)
            .map_or(0, |envelope| envelope.seq))
    }

    async fn branch_lineage(
        &self,
        _session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        if branch_id.is_some() {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "revision fixture has no named branches",
                false,
            ));
        }
        Ok(Vec::new())
    }
}

fn pressure_envelope(session_id: &SessionId, ordinal: u64) -> RawEnvelope {
    let padding = (0..128)
        .map(|index| serde_json::json!({ "field": index }))
        .collect::<Vec<_>>();
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("prompt-pressure-{ordinal}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("prompt-pressure-source")),
        agent_id: None,
        device_id: DeviceId::new("prompt-pressure-device"),
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
        payload: serde_json::json!({
            "type": "prompt_cache_pressure_probe",
            "padding": padding,
        })
        .into(),
    }
}

fn visible_user(session_id: &SessionId, run_id: &RunId, event_id: &str, text: &str) -> RawEnvelope {
    let mut envelope = pressure_envelope(session_id, 0);
    envelope.event_id = EventId::new(event_id);
    envelope.run_id = Some(run_id.clone());
    envelope.render.prompt = PromptRender::Verbatim;
    envelope.payload = serde_json::to_value(EventPayload::UserMessage {
        text: text.into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    })
    .expect("user payload")
    .into();
    envelope
}

#[tokio::test]
async fn terminal_projection_skips_irrelevant_rows_and_decodes_only_the_suffix() {
    let store = RevisionStore::default();
    let session = SessionId::new("prompt-terminal-suffix");
    let run = RunId::new("prompt-terminal-suffix-run");
    let mut initial = [visible_user(&session, &run, "terminal-user-1", "first")];
    store.append(&mut initial).await.expect("append first user");

    let cache = PromptHistoryCache::default();
    let first = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("compile initial projection");
    cache.compact_session_history(&session).await;
    store.reset_read_counts();

    let mut irrelevant = [pressure_envelope(&session, 10)];
    store
        .append(&mut irrelevant)
        .await
        .expect("append irrelevant durable row");
    let unchanged = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("advance across irrelevant row");
    assert_eq!(unchanged, first);
    assert_eq!(store.full_read_count(), 0);
    assert!(store.suffix_read_count() > 0);

    store.reset_read_counts();
    let mut suffix = [visible_user(&session, &run, "terminal-user-2", "second")];
    store
        .append(&mut suffix)
        .await
        .expect("append visible suffix");
    let extended = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("extend terminal projection");
    assert_eq!(store.full_read_count(), 0, "suffix path must stay bounded");
    assert_eq!(
        extended.messages,
        vec![Message::user_text("first"), Message::user_text("second")]
    );
}

#[tokio::test]
async fn same_sequence_replacement_and_truncation_rebuild_from_journal_authority() {
    let store = RevisionStore::default();
    let session = SessionId::new("prompt-terminal-revision");
    let run = RunId::new("prompt-terminal-revision-run");
    let mut initial = [visible_user(&session, &run, "revision-user-1", "first")];
    store
        .append(&mut initial)
        .await
        .expect("append initial head");
    let cache = PromptHistoryCache::default();
    cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("prime revision cache");
    cache.compact_session_history(&session).await;

    store.reset_read_counts();
    store.replace_head(visible_user(
        &session,
        &run,
        "revision-user-replaced",
        "replacement",
    ));
    let replaced = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("replacement rebuild");
    assert_eq!(replaced.messages, vec![Message::user_text("replacement")]);
    assert!(
        store.full_read_count() > 0,
        "same-sequence event-id replacement must discard the terminal projection"
    );

    let mut appended = [visible_user(
        &session,
        &run,
        "revision-user-appended",
        "later",
    )];
    store
        .append(&mut appended)
        .await
        .expect("append later user");
    cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("cache later head");
    cache.compact_session_history(&session).await;
    store.reset_read_counts();
    store.truncate(1);
    let truncated = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("truncation rebuild");
    assert_eq!(truncated.messages, vec![Message::user_text("replacement")]);
    assert!(
        store.full_read_count() > 0,
        "journal rewind must discard the terminal projection"
    );
}

#[tokio::test]
async fn advanced_boundary_revalidates_the_cached_terminal_event() {
    for appended_count in 1..=3_u64 {
        let store = RevisionStore::default();
        let session = SessionId::new(format!(
            "prompt-terminal-advanced-revision-{appended_count}"
        ));
        let run = RunId::new(format!(
            "prompt-terminal-advanced-revision-run-{appended_count}"
        ));
        let mut initial = [visible_user(
            &session,
            &run,
            "advanced-revision-original",
            "original terminal prompt",
        )];
        store
            .append(&mut initial)
            .await
            .expect("append original terminal prompt");

        let cache = PromptHistoryCache::default();
        cache
            .compile_provider_projection_with_artifacts(
                &store,
                &NoArtifacts,
                &session,
                None,
                None,
                &run,
            )
            .await
            .expect("prime terminal projection");
        cache.compact_session_history(&session).await;

        store.replace_head(visible_user(
            &session,
            &run,
            "advanced-revision-replacement",
            "replacement terminal prompt",
        ));
        let mut appended = (1..=appended_count)
            .map(|ordinal| {
                visible_user(
                    &session,
                    &run,
                    &format!("advanced-revision-appended-{ordinal}"),
                    &format!("immediate next turn {ordinal}"),
                )
            })
            .collect::<Vec<_>>();
        store
            .append(&mut appended)
            .await
            .expect("append rows after replaced terminal prompt");
        store.reset_read_counts();

        let projected = cache
            .compile_provider_projection_with_artifacts(
                &store,
                &NoArtifacts,
                &session,
                None,
                None,
                &run,
            )
            .await
            .expect("advance terminal projection");
        let rebuild_reads = store.full_read_count();
        let oracle = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &run,
        )
        .await
        .expect("compile fresh journal oracle");

        assert_eq!(
            projected, oracle,
            "cached projection diverged with the replaced boundary {appended_count} rows behind the head"
        );
        assert!(
            rebuild_reads > 0,
            "an advanced boundary must rebuild after its cached event id changes"
        );
    }
}

#[tokio::test]
async fn compacted_retraction_and_context_corruption_match_the_full_oracle() {
    use haider_protocol::retraction::PromptRetractedV1;

    let store = RevisionStore::default();
    let session = SessionId::new("prompt-terminal-invalidation");
    let first_run = RunId::new("prompt-terminal-invalidation-first");
    let next_run = RunId::new("prompt-terminal-invalidation-next");
    let mut initial = [visible_user(
        &session,
        &first_run,
        "invalidation-first-user",
        "draft",
    )];
    store.append(&mut initial).await.expect("append draft");
    let cache = PromptHistoryCache::default();
    cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &first_run,
        )
        .await
        .expect("prime invalidation cache");
    cache.compact_session_history(&session).await;
    store.reset_read_counts();

    let mut retraction = pressure_envelope(&session, 20);
    retraction.event_id = EventId::new("invalidation-retraction");
    retraction.run_id = Some(first_run.clone());
    retraction.payload = PromptRetractedV1 {
        prompt_seq: initial[0].seq,
        prompt_node_id: NodeId::new("invalidation-draft-node"),
        text: "draft".into(),
        attachments: Vec::new(),
    }
    .to_payload_value()
    .expect("retraction payload")
    .into();
    let mut terminal = pressure_envelope(&session, 21);
    terminal.event_id = EventId::new("invalidation-terminal");
    terminal.run_id = Some(first_run);
    terminal.payload = serde_json::to_value(EventPayload::RunState(RunState::Cancelled))
        .expect("terminal payload")
        .into();
    let replacement = visible_user(
        &session,
        &next_run,
        "invalidation-replacement-user",
        "replacement",
    );
    store
        .append(&mut [retraction, terminal, replacement])
        .await
        .expect("append retraction replacement");
    let projected = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &next_run,
        )
        .await
        .expect("retraction rebuild");
    assert_eq!(projected.messages, vec![Message::user_text("replacement")]);
    assert!(store.full_read_count() > 0);

    cache.compact_session_history(&session).await;
    store.reset_read_counts();
    let mut malformed = pressure_envelope(&session, 22);
    malformed.event_id = EventId::new("invalidation-malformed-context");
    malformed.run_id = Some(next_run.clone());
    malformed.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("invalidation-malformed-context-item"),
        item: TurnItem::Extension {
            kind: CONTEXT_SAVINGS_EXTENSION_KIND.into(),
            data: serde_json::json!({"operation_count": "not-a-number"}),
        },
    }))
    .expect("malformed context payload")
    .into();
    store
        .append(std::slice::from_mut(&mut malformed))
        .await
        .expect("append malformed context event");
    let cached_error = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &next_run,
        )
        .await
        .expect_err("cached malformed context must fail closed");
    let fresh_error = PromptHistoryCompiler::compile_provider_projection_with_artifacts(
        &store,
        &NoArtifacts,
        &session,
        None,
        None,
        &next_run,
    )
    .await
    .expect_err("fresh malformed context must fail closed");
    assert_eq!(cached_error.code, fresh_error.code);
    assert_eq!(cached_error.message, fresh_error.message);
    assert!(store.full_read_count() > 0);
}

#[test]
fn prompt_suffix_kind_matrix_covers_every_compiler_input_family() {
    assert_eq!(
        PROMPT_PROJECTION_PAYLOAD_KINDS,
        [
            "branch_created",
            "item",
            "item_tool_call",
            "menu_answered",
            "menu_opened",
            "node_committed",
            "peer.message",
            "process_signal_recorded",
            "prompt_retracted",
            "run_state",
            "task_completed",
            "task_started",
            "tool_result",
            "user_message",
        ]
    );
}

/// MUTATION CHECK: restore serialized-journal accounting or evict the compiled
/// terminal prefix with the decoded bodies. The retained estimate crosses the
/// cap while the serialized body does not; the large value trees must leave
/// without forcing the next hit to decode them again.
#[tokio::test]
async fn retained_value_trees_compact_to_the_terminal_projection() {
    const ENVELOPE_COUNT: u64 = 225;

    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-retained-heap-cap");
    let current_run = RunId::new("prompt-retained-heap-current");
    let mut envelopes = (0..ENVELOPE_COUNT)
        .map(|ordinal| pressure_envelope(&session_id, ordinal))
        .collect::<Vec<_>>();
    let mut visible = pressure_envelope(&session_id, ENVELOPE_COUNT);
    visible.run_id = Some(current_run.clone());
    visible.render.prompt = PromptRender::Verbatim;
    *visible.payload = serde_json::json!({
        "type": "user_message",
        "text": "projection rebuilt after retained-body eviction",
        "attachments": []
    });
    envelopes.push(visible);
    store
        .append(&mut envelopes)
        .await
        .expect("pressure journal appends");

    let serialized_bytes = serde_json::to_vec(&envelopes)
        .expect("pressure journal serializes")
        .len();
    let retained_bytes = envelopes
        .iter()
        .map(envelope_weight_bytes)
        .fold(0_usize, usize::saturating_add);
    assert!(serialized_bytes < PROMPT_CACHE_RETAINED_BYTES_LIMIT);
    assert!(retained_bytes > PROMPT_CACHE_RETAINED_BYTES_LIMIT);
    drop(envelopes);

    let expected = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
        .await
        .expect("fresh compile succeeds");
    assert!(!expected.is_empty(), "known visible fact must project");
    let cache = PromptHistoryCache::default();
    let first = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("cached compile succeeds");
    assert_eq!(first.messages, expected);
    {
        let sessions = cache.sessions.lock().await;
        let cached = sessions
            .get(&session_id)
            .expect("terminal projection retained");
        assert!(!cached.bodies_evicted);
        assert!(cached.journal_prefix_compacted);
        assert_eq!(cached.envelopes.capacity(), 0);
        assert_eq!(cached.append_prefixes.len(), 1);
        assert_eq!(cached.projections.len(), 1);
    }

    let recompiled = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("compacted hit reuses the terminal projection");
    assert_eq!(recompiled.messages, expected);
}

/// MUTATION CHECK: removing the quiescent-session eviction leaves decoded
/// envelope trees and compiled provider messages resident after idle.
#[tokio::test]
async fn explicit_idle_eviction_drops_bodies_but_keeps_replay_correct() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-idle-release");
    let current_run = RunId::new("prompt-idle-release-run");
    let mut visible = pressure_envelope(&session_id, 1);
    visible.run_id = Some(current_run.clone());
    visible.render.prompt = PromptRender::Verbatim;
    *visible.payload = serde_json::json!({
        "type": "user_message",
        "text": "rebuild me after idle",
        "attachments": []
    });
    store
        .append(std::slice::from_mut(&mut visible))
        .await
        .expect("append visible prompt fact");

    let cache = PromptHistoryCache::default();
    let first = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("initial cached projection");
    let released = cache.evict_session_bodies(&session_id).await;
    assert!(released > 0, "the idle release must own measurable bodies");
    {
        let sessions = cache.sessions.lock().await;
        let cached = sessions.get(&session_id).expect("cursor shell retained");
        assert!(cached.bodies_evicted);
        assert_eq!(cached.envelopes.capacity(), 0);
    }
    let rebuilt = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("idle-evicted projection rebuilds");
    assert_eq!(rebuilt, first);
}

/// MUTATION CHECK: make reply canonicalization local to one 256-envelope
/// store page. The completed item and assistant node then retain independent
/// decoded strings from the delta ranges after a restart replay.
#[tokio::test]
async fn reply_arena_remains_canonical_across_history_page_boundaries() {
    fn assert_canonical(envelopes: &[RawEnvelope], expected: &str) {
        let mut ranges = Vec::new();
        for envelope in envelopes {
            match envelope
                .payload
                .decode_event()
                .expect("decode cached event")
            {
                EventPayload::Item(ItemEvent::Delta {
                    delta: ItemDelta::Text { text },
                    ..
                })
                | EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::AgentMessage { text },
                    ..
                }) => ranges.push(text),
                EventPayload::NodeCommitted(TreeNode {
                    kind: NodeKind::AssistantCommit { text, .. },
                    ..
                }) => ranges.push(text),
                _ => {}
            }
        }
        let canonical = ranges.last().expect("assistant node range");
        assert_eq!(canonical, expected);
        assert!(
            ranges
                .iter()
                .all(|range| range.shares_arena_with(canonical)),
            "every delta, completed item, and node must share one replay arena"
        );
    }

    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-reply-arena-page-boundary");
    let item_id = ItemId::new("prompt-reply-arena-item");
    let mut envelopes = Vec::new();
    let mut next_seq = 1_u64;
    let mut push_round_tripped = |payload: EventPayload| {
        let mut envelope = pressure_envelope(&session_id, next_seq);
        *envelope.payload = serde_json::to_value(payload).expect("encode reply event");
        let encoded = serde_json::to_vec(&envelope).expect("encode stored envelope");
        let decoded: RawEnvelope =
            serde_json::from_slice(&encoded).expect("decode independent stored envelope");
        envelopes.push(decoded);
        next_seq = next_seq.saturating_add(1);
    };

    push_round_tripped(EventPayload::Item(ItemEvent::Started {
        item_id: item_id.clone(),
        item: TurnItem::AgentMessage {
            text: ReplyText::default(),
        },
    }));
    for _ in 0..HISTORY_PAGE {
        push_round_tripped(EventPayload::Item(ItemEvent::Delta {
            item_id: item_id.clone(),
            delta: ItemDelta::Text { text: "x".into() },
        }));
    }
    let expected = "x".repeat(HISTORY_PAGE);
    push_round_tripped(EventPayload::Item(ItemEvent::Completed {
        item_id,
        item: TurnItem::AgentMessage {
            text: expected.clone().into(),
        },
    }));
    push_round_tripped(EventPayload::NodeCommitted(TreeNode {
        node: NodeId::new("prompt-reply-arena-node"),
        parent: None,
        kind: NodeKind::AssistantCommit {
            text: expected.clone().into(),
            verdict: VerifyVerdict::NotApplicable,
        },
    }));
    store
        .append(&mut envelopes)
        .await
        .expect("append independently decoded reply events");
    drop(envelopes);

    let uncached = read_all(&store, &session_id).await.expect("paged read_all");
    assert_canonical(&uncached, &expected);
    let head = store.latest_seq(&session_id).await.expect("journal head");
    let cached = replay_cached_session(&store, &session_id, head)
        .await
        .expect("paged cached replay");
    assert_canonical(&cached.envelopes, &expected);
    assert!(cached.active_reply_arenas.is_empty());
    assert!(cached.completed_reply_arenas.is_empty());
}

/// MUTATION CHECK: retaining the complete decoded journal at terminal idle
/// makes the post-compaction envelope count stay at the old head; dropping the
/// compiled append prefix makes the next run fall back instead of exercising
/// the bounded suffix-extension seam. Dropping the node ancestry spine makes
/// the next assistant node falsely report a missing parent.
#[tokio::test]
async fn idle_compaction_keeps_cross_run_prefix_and_replays_only_the_new_suffix() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-idle-prefix-compaction");
    let first_run = RunId::new("prompt-idle-prefix-first");
    let next_run = RunId::new("prompt-idle-prefix-next");

    let mut first_user = pressure_envelope(&session_id, 1);
    first_user.run_id = Some(first_run.clone());
    first_user.render.prompt = PromptRender::Verbatim;
    *first_user.payload = serde_json::to_value(EventPayload::UserMessage {
        text: "first cached turn".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    })
    .expect("first user payload");
    let mut first_events = vec![first_user, {
        let mut envelope = pressure_envelope(&session_id, 2);
        envelope.run_id = Some(first_run.clone());
        *envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("prompt-idle-prefix-first-user-node"),
            parent: None,
            kind: NodeKind::UserTurn {
                text: "first cached turn".into(),
                attachments: Vec::new(),
            },
        }))
        .expect("first user node payload");
        envelope
    }];
    store
        .append(&mut first_events)
        .await
        .expect("append first user and node");

    let cache = PromptHistoryCache::default();
    cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &first_run,
        )
        .await
        .expect("compile first request prefix");
    let before = cache.retention_stats().await;
    let released = cache.compact_session_history(&session_id).await;
    let compacted = cache.retention_stats().await;
    assert!(released > 0, "decoded prefix journal must be released");
    assert_eq!(compacted.envelopes, 1, "only the user-node spine remains");
    assert_eq!(compacted.append_prefixes, 1);
    assert_eq!(compacted.exact_projections, 1);
    assert!(compacted.projection_bytes > 0);
    assert!(compacted.body_bytes < before.body_bytes);

    let mut suffix = vec![
        {
            let mut envelope = pressure_envelope(&session_id, 3);
            envelope.run_id = Some(first_run.clone());
            envelope.render.prompt = PromptRender::Verbatim;
            *envelope.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("prompt-idle-prefix-answer"),
                item: TurnItem::AgentMessage {
                    text: "answer retained through suffix extension".into(),
                },
            }))
            .expect("assistant payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 4);
            envelope.run_id = Some(first_run.clone());
            *envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new("prompt-idle-prefix-first-answer-node"),
                parent: Some(NodeId::new("prompt-idle-prefix-first-user-node")),
                kind: NodeKind::AssistantCommit {
                    text: "answer retained through suffix extension".into(),
                    verdict: VerifyVerdict::NotApplicable,
                },
            }))
            .expect("assistant node payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 5);
            envelope.run_id = Some(first_run.clone());
            *envelope.payload = serde_json::to_value(EventPayload::RunState(RunState::Done))
                .expect("terminal payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 6);
            envelope.run_id = Some(next_run.clone());
            envelope.render.prompt = PromptRender::Verbatim;
            *envelope.payload = serde_json::to_value(EventPayload::UserMessage {
                text: "next cached turn".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            })
            .expect("next user payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 7);
            envelope.run_id = Some(next_run.clone());
            *envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new("prompt-idle-prefix-next-user-node"),
                parent: Some(NodeId::new("prompt-idle-prefix-first-answer-node")),
                kind: NodeKind::UserTurn {
                    text: "next cached turn".into(),
                    attachments: Vec::new(),
                },
            }))
            .expect("next user node payload");
            envelope
        },
    ];
    store
        .append(&mut suffix)
        .await
        .expect("append next-run suffix");

    let expected = PromptHistoryCompiler::compile(&store, &session_id, None, None, &next_run)
        .await
        .expect("fresh suffix oracle");
    let extended = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &next_run,
        )
        .await
        .expect("extend compacted prefix");
    assert_eq!(extended.messages, expected);
    assert!(extended.messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(block, Block::Text { text } if text == "answer retained through suffix extension")
        })
    }));
}

/// MUTATION CHECK: bypassing the resident prompt projection here restores one
/// complete journal replay per actor, even though provider prompt compilation
/// itself advances only the filtered suffix.
#[tokio::test]
async fn cached_tree_head_uses_the_revision_checked_terminal_projection() {
    let store = RevisionStore::default();
    let session = SessionId::new("prompt-cached-tree-head");
    let first_run = RunId::new("prompt-cached-tree-head-first");
    let next_run = RunId::new("prompt-cached-tree-head-next");
    let first_node_id = NodeId::new("prompt-cached-tree-head-first-node");
    let answer_node_id = NodeId::new("prompt-cached-tree-head-answer-node");
    let next_node_id = NodeId::new("prompt-cached-tree-head-next-node");

    let mut first_node = pressure_envelope(&session, 2);
    first_node.run_id = Some(first_run.clone());
    *first_node.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
        node: first_node_id.clone(),
        parent: None,
        kind: NodeKind::UserTurn {
            text: "first turn".into(),
            attachments: Vec::new(),
        },
    }))
    .expect("first node");
    store
        .append(&mut [
            visible_user(
                &session,
                &first_run,
                "prompt-cached-tree-head-user",
                "first turn",
            ),
            first_node,
        ])
        .await
        .expect("append initial tree");

    let cache = PromptHistoryCache::default();
    cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session,
            None,
            None,
            &first_run,
        )
        .await
        .expect("compile terminal projection");
    cache.compact_session_history(&session).await;

    let mut answer_node = pressure_envelope(&session, 3);
    answer_node.run_id = Some(first_run.clone());
    *answer_node.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
        node: answer_node_id.clone(),
        parent: Some(first_node_id),
        kind: NodeKind::AssistantCommit {
            text: "first answer".into(),
            verdict: VerifyVerdict::NotApplicable,
        },
    }))
    .expect("answer node");
    let mut terminal = pressure_envelope(&session, 4);
    terminal.run_id = Some(first_run);
    *terminal.payload =
        serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("terminal state");
    let mut next_node = pressure_envelope(&session, 6);
    next_node.run_id = Some(next_run.clone());
    *next_node.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
        node: next_node_id.clone(),
        parent: Some(answer_node_id),
        kind: NodeKind::UserTurn {
            text: "next turn".into(),
            attachments: Vec::new(),
        },
    }))
    .expect("next node");
    store
        .append(&mut [
            answer_node,
            terminal,
            visible_user(
                &session,
                &next_run,
                "prompt-cached-tree-head-next-user",
                "next turn",
            ),
            next_node,
        ])
        .await
        .expect("append tree suffix");
    store.reset_read_counts();

    let cached = cache
        .latest_tree_head(&store, &session, None, None)
        .await
        .expect("resolve cached tree head");
    assert_eq!(cached, Some(Some(next_node_id.clone())));
    assert_eq!(store.full_read_count(), 0, "cache path must not replay");
    assert!(
        store.suffix_read_count() > 0,
        "cache path must verify revision"
    );

    let oracle = PromptHistoryCompiler::latest_head(&store, &session, None, None)
        .await
        .expect("journal tree-head oracle");
    assert_eq!(cached.flatten(), oracle);
}

/// MUTATION: omit the retraction fact index or preserve a pre-retraction
/// compiled prefix. The next request must never resurrect the editable draft,
/// including after the cache evicts the original journal bodies.
#[tokio::test]
async fn retracted_prompt_is_excluded_from_cold_warm_compacted_and_evicted_history() {
    use haider_protocol::retraction::PromptRetractedV1;
    for cache_mode in ["cold", "warm", "compacted", "evicted"] {
        let store = MemoryStore::new();
        let session = SessionId::new(format!("retraction-history-{cache_mode}"));
        let first_run = RunId::new("retraction-history-first");
        let next_run = RunId::new("retraction-history-next");
        let mut user = pressure_envelope(&session, 1);
        user.run_id = Some(first_run.clone());
        user.render.prompt = PromptRender::Verbatim;
        *user.payload = serde_json::to_value(EventPayload::UserMessage {
            text: "draft that must leave history".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        })
        .expect("first user");
        let mut user_node = pressure_envelope(&session, 2);
        user_node.run_id = Some(first_run.clone());
        *user_node.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("retract-user-node"),
            parent: None,
            kind: NodeKind::UserTurn {
                text: "draft that must leave history".into(),
                attachments: Vec::new(),
            },
        }))
        .expect("first node");
        let mut accepted = [user, user_node];
        store.append(&mut accepted).await.expect("accepted prompt");
        let cache = PromptHistoryCache::default();
        if cache_mode != "cold" {
            let initial = cache
                .compile_provider_projection_with_artifacts(
                    &store,
                    &NoArtifacts,
                    &session,
                    None,
                    None,
                    &first_run,
                )
                .await
                .expect("initial history");
            assert_eq!(initial.messages.len(), 1);
            if cache_mode == "compacted" {
                cache.compact_session_history(&session).await;
            }
            if cache_mode == "evicted" {
                cache.evict_session_bodies(&session).await;
            }
        }
        let mut fact = pressure_envelope(&session, 3);
        fact.run_id = Some(first_run.clone());
        *fact.payload = PromptRetractedV1 {
            prompt_seq: accepted[0].seq,
            prompt_node_id: NodeId::new("retract-user-node"),
            text: "draft that must leave history".into(),
            attachments: Vec::new(),
        }
        .to_payload_value()
        .expect("retraction fact");
        let mut terminal = pressure_envelope(&session, 4);
        terminal.run_id = Some(first_run.clone());
        *terminal.payload =
            serde_json::json!({"type":"run_state", "state":"cancelled", "reason":"retracted"});
        let mut next_user = pressure_envelope(&session, 5);
        next_user.run_id = Some(next_run.clone());
        next_user.render.prompt = PromptRender::Verbatim;
        *next_user.payload = serde_json::to_value(EventPayload::UserMessage {
            text: "edited replacement".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        })
        .expect("replacement user");
        let mut next_node = pressure_envelope(&session, 6);
        next_node.run_id = Some(next_run.clone());
        *next_node.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("replacement-user-node"),
            parent: Some(NodeId::new("retract-user-node")),
            kind: NodeKind::UserTurn {
                text: "edited replacement".into(),
                attachments: Vec::new(),
            },
        }))
        .expect("replacement node");
        store
            .append(&mut [fact, terminal, next_user, next_node])
            .await
            .expect("retract and replace");
        let projection = cache
            .compile_provider_projection_with_artifacts(
                &store,
                &NoArtifacts,
                &session,
                None,
                None,
                &next_run,
            )
            .await
            .expect("replacement history");
        assert_eq!(
            projection.messages,
            vec![Message::user_text("edited replacement")],
            "{cache_mode}"
        );
        let cold = PromptHistoryCache::default()
            .compile_provider_projection_with_artifacts(
                &store,
                &NoArtifacts,
                &session,
                None,
                None,
                &next_run,
            )
            .await
            .expect("cold parity");
        assert_eq!(projection, cold, "{cache_mode}");
    }
}
