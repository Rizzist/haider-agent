//! F1 wire laws for `session.select_model` over a real UnixStream: sessions
//! are provider-agnostic, so selecting a model row — same provider or a
//! different one — is one receipted command whose committed pair the next
//! logical turn resolves through.

#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
use haider_daemon::ProviderFactoryConfig;
use haider_daemon::{DaemonConfig, DaemonDependencies, ProviderFactory, ResolvedTurnProvider};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::error::HaiderError;
use haider_protocol::provider::FinishReason;
use haider_protocol::session::{ModelSelected, SessionMetadataV1};
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use haider_rpc::{
    AttachMode, ClientKind, CommandId, ERROR_CODE_MODEL_UNKNOWN, ERROR_CODE_PROVIDER_UNAVAILABLE,
    ErrorData, RequestBody, RequestId, ResponseBody, WireFrame,
};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::sync::Arc;
use support::{UdsClient, ready_with_dependencies, test_root};

/// Routes each turn by `metadata.provider` — two recording fakes make the
/// landing provider a wire-observable fact.
#[derive(Clone)]
struct RoutingFactory {
    providers: HashMap<String, Arc<FakeProvider>>,
}

#[async_trait]
impl ProviderFactory for RoutingFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        let fake = self
            .providers
            .get(&metadata.provider)
            .unwrap_or_else(|| panic!("no injected fake for provider {}", metadata.provider));
        Ok(ResolvedTurnProvider {
            provider: fake.clone(),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            account_incarnation: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

fn text_turn(text: &str) -> Vec<FakeStep> {
    vec![
        FakeStep::EmitText { text: text.into() },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]
}

fn routed_dependencies(fakes: &[(&str, Arc<FakeProvider>)]) -> DaemonDependencies {
    DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(RoutingFactory {
                providers: fakes
                    .iter()
                    .map(|(name, fake)| ((*name).to_owned(), fake.clone()))
                    .collect(),
            }),
            providers: fakes
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect::<BTreeSet<_>>(),
        },
        ..DaemonDependencies::default()
    }
}

async fn send(client: &mut UdsClient, config: &DaemonConfig, request_id: &str, body: RequestBody) {
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new(request_id),
                body,
            },
            config.frame_limit,
        )
        .await;
}

async fn next_response(client: &mut UdsClient) -> ResponseBody {
    loop {
        if let WireFrame::Response { body, .. } = client.next().await {
            return body;
        }
    }
}

async fn create_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &std::path::Path,
    provider: &str,
    model: &str,
) -> (haider_protocol::ids::SessionId, u64) {
    create_with_budget_and_attach(client, config, workspace, provider, model, 4096).await
}

async fn create_with_budget_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &std::path::Path,
    provider: &str,
    model: &str,
    max_tokens: u64,
) -> (haider_protocol::ids::SessionId, u64) {
    send(
        client,
        config,
        "create",
        RequestBody::SessionCreate {
            command_id: CommandId::new(format!("create-command-{provider}-{max_tokens}")),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: provider.into(),
            model: model.into(),
            max_tokens,
        },
    )
    .await;
    let ResponseBody::SessionCreate {
        session_id,
        worker_generation,
        ..
    } = next_response(client).await
    else {
        panic!("expected session.create response");
    };
    send(
        client,
        config,
        "attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    loop {
        if matches!(client.next().await, WireFrame::AttachCaughtUp { .. }) {
            break;
        }
    }
    (session_id, worker_generation)
}

/// Drives one queued text turn to its durable Done event.
async fn run_turn(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &haider_protocol::ids::SessionId,
    generation: u64,
    label: &str,
) {
    send(
        client,
        config,
        label,
        RequestBody::TurnSubmit {
            command_id: CommandId::new(format!("{label}-command")),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: format!("{label} question"),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && serde_json::from_value::<EventPayload>(envelope.payload.into())
                .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
        {
            return;
        }
    }
}

fn select_body(
    command_id: &str,
    session_id: &haider_protocol::ids::SessionId,
    generation: u64,
    model: &str,
    provider: Option<&str>,
) -> RequestBody {
    RequestBody::SessionSelectModel {
        command_id: CommandId::new(command_id),
        session_id: session_id.clone(),
        worker_generation: generation,
        model: model.into(),
        provider: provider.map(str::to_owned),
        confirm_new_epoch: false,
        max_tokens: None,
    }
}

/// LAW (pair_switch_is_receipted_and_next_turn_resolves_the_new_provider,
/// wire half): the selection commits with a receipt (same-command retry
/// replays the same coordinates), the `model_selected` fact is PUBLISHED to
/// the live attachment, and the next `turn.submit` lands on the selected
/// row's provider.
#[tokio::test]
async fn select_model_is_receipted_published_and_next_turn_lands_on_the_new_pair() {
    let root = test_root("f1-wire-switch-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "f1-wire-switch",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake_a = Arc::new(FakeProvider::new(text_turn("answer from a")));
    let fake_b = Arc::new(FakeProvider::new(text_turn("answer from b")));
    let task = ready_with_dependencies(
        &config,
        routed_dependencies(&[("fake-a", fake_a.clone()), ("fake-b", fake_b.clone())]),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "f1-wire-switch-client",
        "f1-wire-switch-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) =
        create_and_attach(&mut client, &config, &workspace, "fake-a", "model-a").await;
    run_turn(&mut client, &config, &session_id, generation, "turn-one").await;
    assert_eq!(fake_a.requests().len(), 1);
    assert_eq!(fake_a.requests()[0].model, "model-a");

    send(
        &mut client,
        &config,
        "select-pair",
        select_body(
            "select-pair-command",
            &session_id,
            generation,
            "model-b",
            Some("fake-b"),
        ),
    )
    .await;
    // The committed response and the PUBLISHED fact event arrive on
    // independent lanes, in either order — collect both.
    let mut first = None;
    let mut fact = None;
    while first.is_none() || fact.is_none() {
        match client.next().await {
            WireFrame::Response { body, .. } => first = Some(body),
            WireFrame::Event { envelope, .. } => {
                if let Some(selected) = ModelSelected::from_payload_value(&envelope.payload) {
                    fact = Some(selected);
                }
            }
            _ => {}
        }
    }
    let first = first.expect("select response");
    let fact = fact.expect("published model_selected fact");
    assert_eq!(fact.provider, "fake-b");
    assert_eq!(fact.model, "model-b");
    let ResponseBody::SessionSelectModel {
        session_id: responded_session,
        provider,
        model,
        selected_seq,
        worker_generation,
        ..
    } = first.clone()
    else {
        panic!("expected session.select_model response, got {first:?}");
    };
    assert_eq!(responded_session, session_id);
    assert_eq!(provider, "fake-b");
    assert_eq!(model, "model-b");
    assert_eq!(worker_generation, generation);
    assert!(selected_seq > 0);

    // R2: the same command replays the exact committed coordinates.
    send(
        &mut client,
        &config,
        "select-pair-retry",
        select_body(
            "select-pair-command",
            &session_id,
            generation,
            "model-b",
            Some("fake-b"),
        ),
    )
    .await;
    assert_eq!(next_response(&mut client).await, first);

    run_turn(&mut client, &config, &session_id, generation, "turn-two").await;
    assert_eq!(fake_b.requests().len(), 1, "turn two lands on provider B");
    assert_eq!(fake_b.requests()[0].model, "model-b");
    assert_eq!(fake_a.requests().len(), 1, "provider A saw only turn one");

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// LAW (absent_provider_keeps_legacy_bytes_and_behavior, wire behavior half):
/// a model-only selection stays within the session's current provider — and
/// LAW (unavailable_provider_refused_typed): a row on an uncreatable
/// provider refuses with the stable typed code and data, mutating nothing.
#[tokio::test]
async fn absent_provider_selects_in_place_and_unavailable_provider_refuses_typed() {
    let root = test_root("f1-wire-legacy-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "f1-wire-legacy",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake = Arc::new(FakeProvider::new(
        [text_turn("first answer"), text_turn("second answer")].concat(),
    ));
    let task =
        ready_with_dependencies(&config, routed_dependencies(&[("fake", fake.clone())])).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "f1-wire-legacy-client",
        "f1-wire-legacy-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) =
        create_and_attach(&mut client, &config, &workspace, "fake", "fake-v1").await;

    // Model-only selection: today's behavior — the provider stays put.
    send(
        &mut client,
        &config,
        "select-legacy",
        select_body(
            "select-legacy-command",
            &session_id,
            generation,
            "fake-v2",
            None,
        ),
    )
    .await;
    let ResponseBody::SessionSelectModel {
        provider, model, ..
    } = next_response(&mut client).await
    else {
        panic!("expected session.select_model response");
    };
    assert_eq!(provider, "fake", "absent provider keeps the current one");
    assert_eq!(model, "fake-v2");

    // The next turn runs the newly selected model on the same provider.
    run_turn(&mut client, &config, &session_id, generation, "turn-one").await;
    assert_eq!(fake.requests().len(), 1);
    assert_eq!(fake.requests()[0].model, "fake-v2");

    // A row on an uncreatable provider is a typed refusal…
    send(
        &mut client,
        &config,
        "select-unavailable",
        select_body(
            "select-unavailable-command",
            &session_id,
            generation,
            "some-model",
            Some("frontier-imaginary"),
        ),
    )
    .await;
    let ResponseBody::Error {
        code,
        retryable,
        data,
        ..
    } = next_response(&mut client).await
    else {
        panic!("expected typed refusal");
    };
    assert_eq!(code, ERROR_CODE_PROVIDER_UNAVAILABLE);
    assert!(!retryable);
    assert_eq!(
        data,
        Some(ErrorData::ProviderUnavailable {
            provider: "frontier-imaginary".into()
        })
    );
    // …and refusals never mutate: the committed pair still serves turns.
    run_turn(&mut client, &config, &session_id, generation, "turn-two").await;
    assert_eq!(fake.requests().len(), 2);
    assert_eq!(fake.requests()[1].model, "fake-v2");
    let _ = ERROR_CODE_MODEL_UNKNOWN; // inventory refusals are pinned in-crate

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

async fn select_budget(
    client: &mut UdsClient,
    config: &DaemonConfig,
    label: &str,
    body: RequestBody,
) -> ResponseBody {
    send(client, config, label, body).await;
    next_response(client).await
}

fn committed_budget(
    response: &ResponseBody,
) -> haider_protocol::output_budget::SessionOutputBudgetV1 {
    let ResponseBody::SessionSelectModel {
        output_budget: Some(budget),
        ..
    } = response
    else {
        panic!("expected a committed output budget, got {response:?}");
    };
    *budget
}

/// D1 (973 output cap): a DERIVED session budget follows the selected model.
/// A default session (clients send 0) on a 30,000-budget model switches to
/// gpt-4o (16,384) and to an unknown custom model (8,192) without a refusal,
/// and each next turn requests exactly the re-derived budget. Switching back
/// re-derives the shared default again.
/// MUTATION CHECK: re-check the stored budget against the new maximum
/// instead of re-deriving. Expected runtime failure: the gpt-4o switch is
/// refused with `model_output_limit`.
#[tokio::test]
async fn derived_budget_re_derives_on_every_model_switch() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1;
    let root = test_root("d1-derived-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "d1-derived",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake = Arc::new(FakeProvider::new(text_turn("fake answer")));
    let openai = Arc::new(FakeProvider::new(text_turn("gpt-4o answer")));
    let custom = Arc::new(FakeProvider::new(text_turn("custom answer")));
    let task = ready_with_dependencies(
        &config,
        routed_dependencies(&[
            ("fake", fake.clone()),
            ("openai", openai.clone()),
            ("custom973", custom.clone()),
        ]),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "d1-derived-client",
        "d1-derived-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) =
        create_with_budget_and_attach(&mut client, &config, &workspace, "fake", "fake-v1", 0).await;
    run_turn(&mut client, &config, &session_id, generation, "turn-fake").await;
    assert_eq!(fake.requests()[0].max_tokens, 30_000, "derived default");

    let response = select_budget(
        &mut client,
        &config,
        "select-gpt-4o",
        select_body(
            "select-gpt-4o",
            &session_id,
            generation,
            "gpt-4o",
            Some("openai"),
        ),
    )
    .await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 16_384);
    assert_eq!(budget.source, SessionOutputBudgetSourceV1::Derived);
    assert_eq!(
        budget.clamped, None,
        "a derived budget never produces a notice"
    );
    run_turn(&mut client, &config, &session_id, generation, "turn-gpt-4o").await;
    assert_eq!(openai.requests()[0].max_tokens, 16_384);

    let response = select_budget(
        &mut client,
        &config,
        "select-custom",
        select_body(
            "select-custom",
            &session_id,
            generation,
            "custom-unknown",
            Some("custom973"),
        ),
    )
    .await;
    assert_eq!(committed_budget(&response).max_tokens, 8_192);
    run_turn(&mut client, &config, &session_id, generation, "turn-custom").await;
    assert_eq!(custom.requests()[0].max_tokens, 8_192);

    let response = select_budget(
        &mut client,
        &config,
        "select-back",
        select_body(
            "select-back",
            &session_id,
            generation,
            "fake-v1",
            Some("fake"),
        ),
    )
    .await;
    assert_eq!(
        committed_budget(&response).max_tokens,
        30_000,
        "switching back re-derives the shared default"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// D1 (973 output cap): a USER-SET budget is kept as the user's request and
/// clamped, with a typed notice, when the selected model's maximum is
/// smaller; switching back restores it. An explicit `max_tokens` on
/// `session.select_model` changes the budget (0 returns to derived) and an
/// explicit value above the model maximum is a typed refusal that mutates
/// nothing.
#[tokio::test]
async fn user_set_budget_clamps_with_notice_and_select_model_sets_budgets() {
    use haider_protocol::output_budget::{OutputBudgetClampV1, SessionOutputBudgetSourceV1};
    let root = test_root("d1-user-set-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "d1-user-set",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake = Arc::new(FakeProvider::new(text_turn("fake answer")));
    let openai = Arc::new(FakeProvider::new(
        [text_turn("clamped answer"), text_turn("explicit answer")].concat(),
    ));
    let task = ready_with_dependencies(
        &config,
        routed_dependencies(&[("fake", fake.clone()), ("openai", openai.clone())]),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "d1-user-set-client",
        "d1-user-set-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) =
        create_with_budget_and_attach(&mut client, &config, &workspace, "fake", "fake-v1", 30_000)
            .await;

    let response = select_budget(
        &mut client,
        &config,
        "user-gpt-4o",
        select_body(
            "user-gpt-4o",
            &session_id,
            generation,
            "gpt-4o",
            Some("openai"),
        ),
    )
    .await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 16_384);
    assert_eq!(
        budget.source,
        SessionOutputBudgetSourceV1::UserSet { requested: 30_000 }
    );
    let clamp = budget.clamped.expect("typed clamp notice");
    assert_eq!(
        clamp,
        OutputBudgetClampV1 {
            requested: 30_000,
            max_output_tokens: 16_384
        }
    );
    assert!(clamp.notice().contains("30000") && clamp.notice().contains("16384"));
    run_turn(
        &mut client,
        &config,
        &session_id,
        generation,
        "turn-clamped",
    )
    .await;
    assert_eq!(openai.requests()[0].max_tokens, 16_384);

    // An explicit request above this model's maximum is refused typed.
    let mut over = select_body("explicit-over", &session_id, generation, "gpt-4o", None);
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut over {
        *max_tokens = Some(20_000);
    }
    let response = select_budget(&mut client, &config, "explicit-over", over).await;
    let ResponseBody::Error { data, .. } = response else {
        panic!("expected typed refusal, got {response:?}");
    };
    assert!(matches!(
        data,
        Some(ErrorData::ModelOutputLimit {
            requested: 20_000,
            max_output_tokens: 16_384,
            ..
        })
    ));

    // An explicit in-range budget on the CURRENT model changes only the budget.
    let mut lower = select_body("explicit-lower", &session_id, generation, "gpt-4o", None);
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut lower {
        *max_tokens = Some(12_000);
    }
    let response = select_budget(&mut client, &config, "explicit-lower", lower).await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 12_000);
    assert_eq!(
        budget.source,
        SessionOutputBudgetSourceV1::UserSet { requested: 12_000 }
    );
    run_turn(
        &mut client,
        &config,
        &session_id,
        generation,
        "turn-explicit",
    )
    .await;
    assert_eq!(openai.requests()[1].max_tokens, 12_000);

    // `0` returns the session to the derived budget.
    let mut derive = select_body(
        "explicit-derive",
        &session_id,
        generation,
        "fake-v1",
        Some("fake"),
    );
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut derive {
        *max_tokens = Some(0);
    }
    let response = select_budget(&mut client, &config, "explicit-derive", derive).await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 30_000);
    assert_eq!(budget.source, SessionOutputBudgetSourceV1::Derived);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
