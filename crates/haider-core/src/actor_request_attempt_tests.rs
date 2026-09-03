#![allow(clippy::expect_used)]

use super::*;
use crate::{CommittedRange, MemoryStore, ProviderViewAppendOutcome};
use haider_accounts::{MemoryVault, Vault};
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::provider::{CapabilityDoc, UsageSource};
use haider_provider::{FakeProvider, FakeStep, OpenAiProvider, ProviderStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct AppendRecordingStore {
    inner: MemoryStore,
    append_calls: AtomicUsize,
    batch_sizes: Mutex<Vec<usize>>,
    batches: Mutex<Vec<Vec<EventPayload>>>,
    reject_request_boundary: bool,
    reject_done_boundary: bool,
}

#[test]
fn terminal_fence_reports_an_unresolved_promotion_without_waiting() {
    let mailbox = Arc::new(PromotedSteerMailbox::default());
    mailbox.begin_turn();
    let (commands, _receiver) = mpsc::channel(1);
    let reservation = mailbox
        .reserve("later steer".into(), commands)
        .expect("reservation opens");

    assert!(mailbox.try_finish_boundary().is_none());
    drop(reservation);
    assert!(
        mailbox
            .try_finish_boundary()
            .is_some_and(|promoted| promoted.is_empty())
    );
}

impl AppendRecordingStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            append_calls: AtomicUsize::new(0),
            batch_sizes: Mutex::new(Vec::new()),
            batches: Mutex::new(Vec::new()),
            reject_request_boundary: false,
            reject_done_boundary: false,
        }
    }

    fn rejecting_request_boundary() -> Self {
        Self {
            reject_request_boundary: true,
            ..Self::new()
        }
    }

    fn rejecting_done_boundary() -> Self {
        Self {
            reject_done_boundary: true,
            ..Self::new()
        }
    }

    fn batches(&self) -> Vec<Vec<EventPayload>> {
        self.batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl StoreHandle for AppendRecordingStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        self.append_calls.fetch_add(1, Ordering::Relaxed);
        self.batch_sizes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(envelopes.len());
        let batch = envelopes
            .iter()
            .map(|envelope| {
                serde_json::from_value(envelope.payload.clone()).expect("recorded payload is typed")
            })
            .collect::<Vec<_>>();
        let reject_request = self.reject_request_boundary
            && batch.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { kind, .. },
                        ..
                    }) if kind == PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND
                )
            });
        let reject_done = self.reject_done_boundary
            && batch
                .iter()
                .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)));
        self.batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(batch);
        if reject_request || reject_done {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "reject configured append boundary",
                false,
            ));
        }
        self.inner.append(envelopes).await
    }

    async fn persist_provider_view_and_append_owned(
        &self,
        request: ProviderViewAppendRequest,
    ) -> Result<ProviderViewAppendOutcome, HaiderError> {
        let ProviderViewAppendRequest {
            session_id,
            ledger,
            blobs,
            attempt_ordinal: _,
            envelopes,
        } = request;
        let ledger = self
            .persist_provider_view(&session_id, ledger, blobs)
            .await?;
        let envelopes = self.append_owned(envelopes).await?;
        Ok(ProviderViewAppendOutcome { ledger, envelopes })
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.latest_seq(session_id).await
    }

    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        self.inner.branch_lineage(session_id, branch_id).await
    }
}

struct ExactViewFakeProvider {
    renderer: OpenAiProvider,
    stream: FakeProvider,
    opened: AtomicBool,
}

impl ExactViewFakeProvider {
    fn new() -> Self {
        let vault = MemoryVault::new();
        let alias = CredentialAlias::new("provider-view-request-test");
        vault
            .put(&alias, b"provider-view-request-test-secret")
            .expect("store renderer credential");
        Self {
            renderer: OpenAiProvider::new(
                vault.resolve(&alias).expect("resolve renderer credential"),
                "gpt-5.6",
            )
            .expect("construct exact-view renderer"),
            stream: FakeProvider::new(vec![FakeStep::Finish {
                reason: FinishReason::EndTurn,
            }]),
            opened: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Provider for ExactViewFakeProvider {
    fn prepare_turn(&self, request: &TurnRequest) -> Option<haider_provider::PreparedTurn> {
        self.renderer.prepare_turn(request)
    }

    fn prepare_turn_with_tools(
        &self,
        request: &TurnRequest,
        tools: &[ToolDefinition],
    ) -> Option<haider_provider::PreparedTurn> {
        self.renderer.prepare_turn_with_tools(request, tools)
    }

    async fn capabilities(&self) -> CapabilityDoc {
        self.stream.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.opened.store(true, Ordering::Relaxed);
        self.stream.stream_turn(request).await
    }

    async fn stream_prepared_turn_ref(
        &self,
        request: &TurnRequest,
        _prepared: Option<haider_provider::PreparedTurn>,
    ) -> Result<ProviderStream, ProviderError> {
        self.opened.store(true, Ordering::Relaxed);
        self.stream.stream_turn(request.clone()).await
    }
}

fn assert_request_boundary_golden(payloads: &[EventPayload]) {
    assert!(matches!(
        payloads,
        [
            EventPayload::Item(ItemEvent::Started {
                item_id: provider_started_id,
                item: TurnItem::Extension {
                    kind: provider_started_kind,
                    ..
                },
            }),
            EventPayload::Item(ItemEvent::Completed {
                item_id: provider_completed_id,
                item: TurnItem::Extension {
                    kind: provider_completed_kind,
                    ..
                },
            }),
            EventPayload::RunState(RunState::Thinking),
            EventPayload::Item(ItemEvent::Started {
                item_id: cache_started_id,
                item: TurnItem::Extension {
                    kind: cache_started_kind,
                    ..
                },
            }),
            EventPayload::Item(ItemEvent::Completed {
                item_id: cache_completed_id,
                item: TurnItem::Extension {
                    kind: cache_completed_kind,
                    ..
                },
            }),
        ] if provider_started_id == provider_completed_id
            && provider_started_kind == PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND
            && provider_completed_kind == PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND
            && cache_started_id == cache_completed_id
            && cache_started_kind == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND
            && cache_completed_kind == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND
    ));
}

#[test]
fn provider_timeout_retry_requires_a_full_post_backoff_budget() {
    let provider_budget_ms = 5_000;
    let timeout = ProviderError::new(ProviderErrorKind::Transport, "provider timeout")
        .with_presentation(ErrorPresentation::new(
            "provider-timeout",
            "Provider request timed out",
            "The provider did not open in time.",
            ErrorScope::Turn,
            [ErrorAction::Retry],
        ))
        .with_timeout_budget(provider_budget_ms, provider_budget_ms)
        .with_retry_after_ms(Some(250));
    let run_id = RunId::new("provider-timeout-retry-budget");

    let mut roomy = timeout.clone();
    assert!(provider_error_allows_retry(
        &mut roomy,
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(7)),
        &run_id,
        1,
    ));
    assert!(roomy.retryable);

    let mut short = timeout;
    assert!(!provider_error_allows_retry(
        &mut short,
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(5)),
        &run_id,
        1,
    ));
    assert!(!short.retryable);
    assert_eq!(short.presentation.allowed_actions, vec![ErrorAction::None]);

    let mut timeout = ProviderError::new(ProviderErrorKind::Transport, "provider timeout")
        .with_presentation(ErrorPresentation::new(
            "provider-timeout",
            "Provider request timed out",
            "The provider did not open in time.",
            ErrorScope::Turn,
            [ErrorAction::Retry],
        ))
        // The restored default can park for 60 seconds. With no outer
        // deadline, only the typed reason prevents another identical wait.
        .with_timeout_budget(60_000, 60_000)
        .with_timeout_reason(ProviderTimeoutReason::ResponseOpen);

    assert!(!provider_error_allows_retry(
        &mut timeout,
        None,
        &RunId::new("response-open-timeout"),
        1,
    ));
    assert!(timeout.retryable, "the caller may explicitly retry");
    assert_eq!(
        timeout.presentation.allowed_actions,
        vec![ErrorAction::Retry]
    );
}

#[test]
fn automatic_rate_limit_retry_preserves_the_terminal_deadline_margin() {
    let rate_limit = ProviderError::new(ProviderErrorKind::RateLimited, "rate limited")
        .with_retry_after_ms(Some(2_000));
    let run_id = RunId::new("rate-limit-retry-budget");

    let delay = std::time::Duration::from_secs(2);
    let admission_margin =
        PROVIDER_DEADLINE_SAFETY_MARGIN.saturating_add(PROVIDER_DEADLINE_SAFETY_MARGIN);
    assert!(!provider_retry_fits_deadline(
        delay + admission_margin - std::time::Duration::from_millis(1),
        delay,
    ));
    assert!(
        !provider_retry_fits_deadline(delay + admission_margin, delay),
        "equality is a deterministic refusal"
    );
    assert!(provider_retry_fits_deadline(
        delay + admission_margin + std::time::Duration::from_millis(1),
        delay,
    ));

    let mut roomy = rate_limit.clone();
    assert!(provider_error_allows_retry(
        &mut roomy,
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(5)),
        &run_id,
        1,
    ));

    let mut short = rate_limit;
    assert!(!provider_error_allows_retry(
        &mut short,
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(4)),
        &run_id,
        1,
    ));
    assert!(
        !short.retryable,
        "the accepted run exhausted its retry budget"
    );
    assert_eq!(short.presentation.allowed_actions, vec![ErrorAction::None]);
}

#[test]
fn deadline_terminal_class_is_derived_from_provider_retry_state() {
    let retrying = RunState::Retrying {
        attempt: 2,
        max: 10,
        delay_ms: 3_000,
        reason: WaitReason::RateLimit,
    };
    let retry_states = [
        retrying,
        RunState::Waiting {
            reason: WaitReason::RateLimit,
        },
        RunState::Waiting {
            reason: WaitReason::ProviderBackoff,
        },
    ];
    for state in &retry_states {
        let mut error = deadline_exhausted_error(
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(3),
        );
        assert!(classify_provider_deadline_terminal(
            &mut error,
            Some(state),
            false,
        ));
        let terminal = provider_error_to_haider(error);
        assert_eq!(terminal.code, ErrorCode::ProviderError);
        assert!(!terminal.retryable);
        assert_eq!(
            terminal
                .presentation
                .expect("state-classified terminal has presentation")
                .allowed_actions,
            vec![ErrorAction::None]
        );
    }

    let mut admission =
        deadline_exhausted_error(std::time::Duration::ZERO, std::time::Duration::ZERO);
    assert!(classify_provider_deadline_terminal(
        &mut admission,
        Some(&RunState::Thinking),
        true,
    ));
    assert_eq!(
        provider_error_to_haider(admission).code,
        ErrorCode::ProviderError,
        "an expiry while the next retry is being admitted is the same bounded failure"
    );

    for state in [
        RunState::Thinking,
        RunState::Streaming,
        RunState::Waiting {
            reason: WaitReason::NetworkUnavailable,
        },
    ] {
        let mut in_flight = deadline_exhausted_error(
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(3),
        );
        assert!(!classify_provider_deadline_terminal(
            &mut in_flight,
            Some(&state),
            false,
        ));
        assert_eq!(
            provider_error_to_haider(in_flight).code,
            ErrorCode::ProviderTimeout,
            "non-retry state retains the provider timeout terminal"
        );
    }
}

fn completed_extension_item<'a>(payloads: &'a [EventPayload], expected_kind: &str) -> &'a TurnItem {
    payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Completed {
                item: item @ TurnItem::Extension { kind, .. },
                ..
            }) if kind == expected_kind => Some(item),
            _ => None,
        })
        .expect("completed extension is present in request boundary")
}

fn reported_usage() -> Usage {
    Usage {
        input: 11,
        output: 7,
        reasoning: 2,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    }
}

/// MUTATION CHECK: move the provider-view marker back to a standalone append
/// in the real request path. The production batch containing that marker is
/// no longer the exact five-event pre-change journal sequence.
#[tokio::test]
async fn production_request_path_batches_provider_view_with_attempt_facts() {
    let session_id = SessionId::new("provider-view-production-batch");
    let mut config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("provider-view-production-device"),
        1,
        1,
    );
    config.model = "gpt-5.6".into();
    config.system_prompt = Some("stable system".into());
    let provider = Arc::new(ExactViewFakeProvider::new());
    let store = Arc::new(AppendRecordingStore::new());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());

    let outcome = handle
        .submit_turn(SubmitTurn::new("batch the exact provider view"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert!(provider.opened.load(Ordering::Relaxed));

    let request_batches = store
        .batches()
        .into_iter()
        .filter(|batch| {
            batch.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { kind, .. },
                        ..
                    }) if kind == PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND
                )
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(request_batches.len(), 1);
    assert_request_boundary_golden(&request_batches[0]);
    let provider_attempt = ProviderViewAttemptV1::try_from_extension_item(
        completed_extension_item(&request_batches[0], PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND),
    )
    .expect("provider-view attempt data is typed")
    .expect("provider-view attempt kind is recognized");
    let cache_attempt = CacheRequestAttemptV1::from_extension_item(completed_extension_item(
        &request_batches[0],
        CACHE_REQUEST_ATTEMPT_EXTENSION_KIND,
    ))
    .expect("cache-attempt data is typed");
    assert_eq!(provider_attempt.ordinal, cache_attempt.ordinal);
    assert!(
        provider_attempt.view.storage.is_none(),
        "a journal-only store must not claim a durable provider-view cursor"
    );
}

/// MUTATION CHECK: publish either half before the combined append succeeds or
/// open the provider before that durability boundary. A rejected boundary then
/// leaks request facts into the journal or flips the provider-open latch.
#[tokio::test]
async fn rejected_request_boundary_publishes_neither_half_and_never_opens_provider() {
    let session_id = SessionId::new("provider-view-rejected-batch");
    let mut config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("provider-view-rejected-device"),
        1,
        1,
    );
    config.model = "gpt-5.6".into();
    config.system_prompt = Some("stable system".into());
    let provider = Arc::new(ExactViewFakeProvider::new());
    let store = Arc::new(AppendRecordingStore::rejecting_request_boundary());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    let mut live_events = handle.subscribe();

    let outcome = handle
        .submit_turn(SubmitTurn::new("reject the request boundary"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert!(!provider.opened.load(Ordering::Relaxed));

    let published = std::iter::from_fn(|| live_events.try_recv().ok()).collect::<Vec<_>>();
    assert!(!published.iter().any(|event| {
        let Ok(payload) = serde_json::from_value::<EventPayload>(event.payload.clone()) else {
            return false;
        };
        matches!(&payload, EventPayload::RunState(RunState::Thinking))
            || matches!(
                &payload,
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::Extension { kind, .. },
                    ..
                }) | EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { kind, .. },
                    ..
                }) if kind == PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND
                    || kind == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND
            )
    }));

    let journal = store.inner.events(&session_id).await;
    assert!(!journal.iter().any(|event| {
        matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::Item(ItemEvent::Started {
                item: TurnItem::Extension { kind, .. },
                ..
            }) | EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, .. },
                ..
            })) if kind == PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND
                || kind == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND
        )
    }));
}

/// MUTATION CHECK: broadcast or update the state watch before the store
/// accepts the combined append. The direct failed boundary then becomes
/// observable even though no committed envelope exists.
#[tokio::test]
async fn rejected_combined_append_does_not_publish_or_advance_state() {
    let session_id = SessionId::new("provider-view-rejected-publication");
    let config = HarnessConfig::for_session(
        session_id,
        DeviceId::new("provider-view-rejected-publication-device"),
        1,
        1,
    );
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(AppendRecordingStore::rejecting_request_boundary());
    let (mut actor, handle) = HarnessActor::new(config, provider, store);
    let mut events = handle.subscribe();
    let state = handle.state_receiver();
    let mut thinking_pending = true;

    actor
        .commit_request_attempt(
            &RunId::new("provider-view-rejected-publication-run"),
            1,
            None,
            RequestAttemptMarkers {
                provider_view: Some(serde_json::json!({"provider_view": "exact"})),
                cache: serde_json::json!({"cache_attempt": "diagnostic"}),
                response_epoch: 0,
            },
            &mut thinking_pending,
        )
        .await
        .expect_err("combined append is rejected");

    assert!(thinking_pending);
    assert!(events.try_recv().is_err());
    assert!(state.borrow().is_none());
}

/// MUTATION CHECK: restore the provider-view marker append before the S6
/// Thinking/cache-attempt batch. The append count becomes two even though the
/// journal payload sequence remains superficially correct.
#[tokio::test]
async fn provider_view_and_request_attempt_share_one_ordered_append() {
    let session_id = SessionId::new("provider-view-request-batch");
    let config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("provider-view-request-device"),
        1,
        1,
    );
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(AppendRecordingStore::new());
    let (mut actor, _handle) = HarnessActor::new(config, provider, store.clone());
    let mut thinking_pending = true;

    actor
        .commit_request_attempt(
            &RunId::new("provider-view-request-run"),
            1,
            None,
            RequestAttemptMarkers {
                provider_view: Some(serde_json::json!({"provider_view": "exact"})),
                cache: serde_json::json!({"cache_attempt": "diagnostic"}),
                response_epoch: 0,
            },
            &mut thinking_pending,
        )
        .await
        .expect("commit request boundary");

    assert_eq!(store.append_calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        store
            .batch_sizes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_slice(),
        [5]
    );
    assert!(!thinking_pending);

    let journal = store.inner.events(&session_id).await;
    assert_eq!(
        journal.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    for index in [0, 1, 3, 4] {
        assert_eq!(journal[index].render, hidden_prompt_omit_render());
    }
    assert_eq!(journal[2].render, prompt_omit_render());
    let payloads = journal
        .iter()
        .map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone())
                .expect("typed journal payload")
        })
        .collect::<Vec<_>>();
    assert_request_boundary_golden(&payloads);
}

/// MUTATION CHECK: restore the independent usage, message-completion, and
/// Done appends. The terminal suffix no longer appears in one recorded batch.
/// Reorder publication and the live event-id sequence differs from storage.
#[tokio::test]
async fn no_boundary_post_stream_facts_share_one_ordered_append() {
    let session_id = SessionId::new("post-stream-terminal-batch");
    let config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("post-stream-terminal-device"),
        1,
        1,
    );
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "batched terminal text".into(),
        },
        FakeStep::EmitUsage {
            usage: reported_usage(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(AppendRecordingStore::new());
    let handle = HarnessActor::spawn(config, provider, store.clone());
    let mut live_events = handle.subscribe();

    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("post-stream-terminal-run"),
            messages: vec![Message::user_text("finish in one append")],
        })
        .await
        .expect("committed turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);

    let terminal_batch = store
        .batches()
        .into_iter()
        .find(|batch| {
            batch
                .iter()
                .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
        })
        .expect("terminal append recorded");
    assert!(matches!(
        terminal_batch.as_slice(),
        [
            EventPayload::Usage(_),
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { .. },
                ..
            }),
            EventPayload::NodeCommitted(_),
            EventPayload::RunState(RunState::Done),
        ]
    ));
    let completed_text = terminal_batch
        .iter()
        .find_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Completed {
                item_id: _,
                item: TurnItem::AgentMessage { text },
            }) => Some(text),
            _ => None,
        })
        .expect("completed assistant item");
    let node_text = terminal_batch
        .iter()
        .find_map(|payload| match payload {
            EventPayload::NodeCommitted(TreeNode {
                kind: NodeKind::AssistantCommit { text, .. },
                ..
            }) => Some(text),
            _ => None,
        })
        .expect("assistant history node");
    assert_eq!(node_text, completed_text);

    let journal = store.inner.events(&session_id).await;
    let terminal = journal
        .windows(4)
        .find(|window| {
            matches!(
                window
                    .iter()
                    .map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()))
                    .collect::<Result<Vec<_>, _>>()
                    .as_deref(),
                Ok([
                    EventPayload::Usage(_),
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::AgentMessage { .. },
                        ..
                    }),
                    EventPayload::NodeCommitted(_),
                    EventPayload::RunState(RunState::Done),
                ])
            )
        })
        .expect("terminal journal suffix");
    assert!(
        terminal
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1)
    );

    let terminal_ids = terminal
        .iter()
        .map(|event| event.event_id.clone())
        .collect::<Vec<_>>();
    let published_ids = std::iter::from_fn(|| live_events.try_recv().ok())
        .filter(|event| terminal_ids.contains(&event.event_id))
        .map(|event| event.event_id)
        .collect::<Vec<_>>();
    assert_eq!(published_ids, terminal_ids);
}

/// MUTATION CHECK: consume `message`, `reasoning`, or pending usage before the
/// terminal append succeeds. The injected append rejection then lets error
/// cleanup terminalize the run with a durable Started assistant item that was
/// never closed.
#[tokio::test]
async fn rejected_post_stream_append_keeps_open_item_for_error_cleanup() {
    let session_id = SessionId::new("post-stream-rejected-batch");
    let config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("post-stream-rejected-device"),
        1,
        1,
    );
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "close me after rejection".into(),
        },
        FakeStep::EmitUsage {
            usage: reported_usage(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(AppendRecordingStore::rejecting_done_boundary());
    let handle = HarnessActor::spawn(config, provider, store.clone());

    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("post-stream-rejected-run"),
            messages: vec![Message::user_text("exercise rejected terminal append")],
        })
        .await
        .expect("committed turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);

    let journal = store.inner.events(&session_id).await;
    let payloads = journal
        .iter()
        .map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone())
                .expect("typed journal payload")
        })
        .collect::<Vec<_>>();
    let started = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Started {
                item_id,
                item: TurnItem::AgentMessage { .. },
            }) => Some(item_id),
            _ => None,
        })
        .expect("assistant item started");
    assert!(payloads.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item_id,
                item: TurnItem::AgentMessage { text },
            }) if item_id == started && text == "close me after rejection"
        )
    }));
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Errored)))
    );
    assert!(
        !payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
    );
}

/// MUTATION CHECK: `take()` any pending terminal input before the append, or
/// advance tree/state despite the store error. The exact values below then
/// disappear or an uncommitted terminal becomes observable.
#[tokio::test]
async fn rejected_post_stream_append_preserves_every_pending_input() {
    let session_id = SessionId::new("post-stream-rejected-direct");
    let config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("post-stream-rejected-direct-device"),
        1,
        1,
    );
    let store = Arc::new(AppendRecordingStore::rejecting_done_boundary());
    let provider = Arc::new(FakeProvider::new(vec![]));
    let (mut actor, handle) = HarnessActor::new(config, provider, store.clone());
    let original_head = NodeId::new("post-stream-original-head");
    actor.tree_head_initialized = true;
    actor.tree_head = Some(original_head.clone());
    let mut message = Some(TextAccumulator {
        item_id: ItemId::new("post-stream-message-item"),
        text: SharedText::from("message survives"),
    });
    let mut reasoning = Some(TextAccumulator {
        item_id: ItemId::new("post-stream-reasoning-item"),
        text: SharedText::from("reasoning survives"),
    });
    let mut pending_usage = Some(PendingUsageCommit {
        footprint: None,
        usage: reported_usage(),
    });

    actor
        .commit_post_stream_facts(
            &RunId::new("post-stream-rejected-direct-run"),
            &mut message,
            &mut reasoning,
            &mut pending_usage,
        )
        .await
        .expect_err("terminal batch is rejected");

    assert_eq!(
        message
            .as_ref()
            .map(|active| (active.item_id.as_str(), active.text.as_str())),
        Some(("post-stream-message-item", "message survives"))
    );
    assert_eq!(
        reasoning
            .as_ref()
            .map(|active| (active.item_id.as_str(), active.text.as_str())),
        Some(("post-stream-reasoning-item", "reasoning survives"))
    );
    assert!(pending_usage.is_some());
    assert_eq!(actor.tree_head.as_ref(), Some(&original_head));
    assert!(handle.state_receiver().borrow().is_none());
    assert!(store.inner.events(&session_id).await.is_empty());
}

/// MUTATION CHECK: infer the boundary from the final empty tool accumulator
/// instead of recording whether dispatch occurred. Text emitted after the
/// tool then coalesces usage and Done across that external boundary.
#[tokio::test]
async fn tool_call_response_does_not_batch_usage_with_done() {
    let session_id = SessionId::new("post-stream-tool-boundary");
    let config =
        HarnessConfig::for_session(session_id, DeviceId::new("post-stream-tool-device"), 1, 1);
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "tool-boundary-call".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path": "src/lib.rs"}),
        },
        FakeStep::EmitText {
            text: "text after the tool boundary".into(),
        },
        FakeStep::EmitUsage {
            usage: reported_usage(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(AppendRecordingStore::new());
    let handle = HarnessActor::spawn(config, provider, store.clone());

    let outcome = handle
        .submit_turn(SubmitTurn::new("keep tool boundary"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert!(!store.batches().iter().any(|batch| {
        batch
            .iter()
            .any(|payload| matches!(payload, EventPayload::Usage(_)))
            && batch
                .iter()
                .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
    }));
}
