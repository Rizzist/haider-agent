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

/// S1 (973 output cap): an unknown custom model's 8,192 maximum is only the
/// unverified fallback guess. The derived budget follows it, but an explicit
/// `max_tokens` above it (here 30,000) is admitted as the user's own budget
/// and the next turn requests exactly that value; only a value above the
/// adapter maximum is refused. The provider-stated one-shot retry covers a
/// provider whose real maximum is lower.
/// MUTATION CHECK: bound explicit requests by `max_output_tokens` instead of
/// `explicit_max_output_tokens`. Expected runtime failure: the 30,000 select
/// is refused with `model_output_limit` (max 8,192).
#[tokio::test]
async fn explicit_budget_above_unverified_fallback_is_admitted() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1;
    let root = test_root("s1-fallback-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "s1-fallback",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake = Arc::new(FakeProvider::new(text_turn("fake answer")));
    let custom = Arc::new(FakeProvider::new(
        [text_turn("derived answer"), text_turn("explicit answer")].concat(),
    ));
    let task = ready_with_dependencies(
        &config,
        routed_dependencies(&[("fake", fake.clone()), ("custom973", custom.clone())]),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "s1-fallback-client",
        "s1-fallback-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) =
        create_with_budget_and_attach(&mut client, &config, &workspace, "fake", "fake-v1", 0).await;

    let response = select_budget(
        &mut client,
        &config,
        "s1-select-custom",
        select_body(
            "s1-select-custom",
            &session_id,
            generation,
            "custom-unknown",
            Some("custom973"),
        ),
    )
    .await;
    assert_eq!(committed_budget(&response).max_tokens, 8_192, "derived");
    run_turn(&mut client, &config, &session_id, generation, "s1-derived").await;
    assert_eq!(custom.requests()[0].max_tokens, 8_192);

    let mut explicit = select_body(
        "s1-explicit",
        &session_id,
        generation,
        "custom-unknown",
        None,
    );
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut explicit {
        *max_tokens = Some(30_000);
    }
    let response = select_budget(&mut client, &config, "s1-explicit", explicit).await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 30_000);
    assert_eq!(
        budget.source,
        SessionOutputBudgetSourceV1::UserSet { requested: 30_000 }
    );
    assert_eq!(budget.clamped, None);
    run_turn(
        &mut client,
        &config,
        &session_id,
        generation,
        "s1-explicit-turn",
    )
    .await;
    assert_eq!(custom.requests()[1].max_tokens, 30_000);

    let mut over = select_body("s1-over", &session_id, generation, "custom-unknown", None);
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut over {
        *max_tokens = Some(haider_provider::MAX_OUTPUT_LIMIT + 1);
    }
    let response = select_budget(&mut client, &config, "s1-over", over).await;
    let ResponseBody::Error { data, .. } = response else {
        panic!("expected typed refusal, got {response:?}");
    };
    assert!(matches!(
        data,
        Some(ErrorData::ModelOutputLimit {
            max_output_tokens: haider_provider::MAX_OUTPUT_LIMIT,
            ..
        })
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// 973-output-cap-fixes B1: Kimi Code (`kimi-oauth/kimi-for-coding`) and
/// Haider Code (`haider-code/glm-5.3`, seeded as a fetched catalog row with
/// no output maximum) publish a context window but no output maximum, so
/// their 30,000 derived default is not an explicit ceiling. Over the real
/// socket: session.create with 60,000,
/// a session-config update to 60,000, and a model switch that inherits a
/// user-set 60,000 are all admitted unclamped, and each next turn requests
/// exactly 60,000. The context bound still refuses above half the window.
/// MUTATION CHECK: restore `output_limit_sourced: context_window.is_some()`
/// on those rows. Expected runtime failure: session.create is refused with
/// `model_output_limit` (max 30,000) and the switch clamps to 30,000.
#[tokio::test]
async fn unpublished_output_maximum_admits_explicit_budgets_on_create_config_and_switch() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1;
    let root = test_root("b1-unpublished-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "b1-unpublished",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    // Seed the persisted public catalog before daemon startup, as the
    // account RPC tests do. The real inventory authority must select GLM.
    let glm = haider_provider::DiscoveredModel {
        slug: "glm-5.3".into(),
        display_name: "GLM 5.3".into(),
        context_window: Some(200_000),
        max_output_tokens: None,
        description: None,
        default_effort: None,
        supported_efforts: Vec::new(),
        visible: true,
        priority: None,
        use_responses_lite: None,
        extensions: None,
    };
    let store = haider_store::Store::open(&config.store_dir).expect("open catalog store");
    let models_json = serde_json::to_string(&vec![glm]).expect("serialize catalog row");
    let fetched_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64;
    store
        .put_provider_models("haider-code", &models_json, None, fetched_at_ms)
        .expect("seed public Haider Code inventory");
    // Store::open holds the profile lock; release it before daemon startup.
    drop(store);
    let kimi = Arc::new(FakeProvider::new(
        [
            text_turn("kimi one"),
            text_turn("kimi two"),
            text_turn("kimi three"),
            text_turn("kimi four"),
        ]
        .concat(),
    ));
    let haider_code = Arc::new(FakeProvider::new(
        [
            text_turn("glm one"),
            text_turn("glm two"),
            text_turn("glm three"),
            text_turn("glm four"),
        ]
        .concat(),
    ));
    let task = ready_with_dependencies(
        &config,
        routed_dependencies(&[
            ("kimi-oauth", kimi.clone()),
            ("haider-code", haider_code.clone()),
        ]),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "b1-unpublished-client",
        "b1-unpublished-instance",
        ClientKind::Cli,
    )
    .await;

    // Session A: create on Kimi Code with an explicit 60,000.
    let (session_a, generation_a) = create_with_budget_and_attach(
        &mut client,
        &config,
        &workspace,
        "kimi-oauth",
        "kimi-for-coding",
        60_000,
    )
    .await;
    run_turn(&mut client, &config, &session_a, generation_a, "b1-a-kimi").await;
    assert_eq!(
        kimi.requests()[0].max_tokens,
        60_000,
        "create admits 60,000"
    );
    // Switch to Haider Code: the user-set 60,000 is inherited unclamped.
    let response = select_budget(
        &mut client,
        &config,
        "b1-a-switch",
        select_body(
            "b1-a-switch",
            &session_a,
            generation_a,
            "glm-5.3",
            Some("haider-code"),
        ),
    )
    .await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 60_000);
    assert_eq!(
        budget.source,
        SessionOutputBudgetSourceV1::UserSet { requested: 60_000 }
    );
    assert_eq!(budget.clamped, None, "no clamp to the 30,000 default");
    run_turn(&mut client, &config, &session_a, generation_a, "b1-a-glm").await;
    assert_eq!(haider_code.requests()[0].max_tokens, 60_000);

    // Session B: derived default on Haider Code, then a config update.
    let (session_b, generation_b) = create_with_budget_and_attach(
        &mut client,
        &config,
        &workspace,
        "haider-code",
        "glm-5.3",
        0,
    )
    .await;
    run_turn(
        &mut client,
        &config,
        &session_b,
        generation_b,
        "b1-b-derived",
    )
    .await;
    assert_eq!(
        haider_code.requests()[1].max_tokens,
        30_000,
        "the conservative derived default is kept"
    );
    let mut update = select_body("b1-b-config", &session_b, generation_b, "glm-5.3", None);
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut update {
        *max_tokens = Some(60_000);
    }
    let response = select_budget(&mut client, &config, "b1-b-config", update).await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 60_000, "config update admits 60,000");
    assert_eq!(budget.clamped, None);
    run_turn(
        &mut client,
        &config,
        &session_b,
        generation_b,
        "b1-b-explicit",
    )
    .await;
    assert_eq!(haider_code.requests()[2].max_tokens, 60_000);
    // The known window still bounds explicit budgets (half of 200,000).
    let mut over = select_body("b1-b-over", &session_b, generation_b, "glm-5.3", None);
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut over {
        *max_tokens = Some(100_001);
    }
    let response = select_budget(&mut client, &config, "b1-b-over", over).await;
    let ResponseBody::Error { data, .. } = response else {
        panic!("expected typed refusal, got {response:?}");
    };
    assert!(matches!(
        data,
        Some(ErrorData::ModelOutputLimit {
            requested: 100_001,
            max_output_tokens: 100_000,
            ..
        })
    ));
    // Switch to Kimi Code: the user-set 60,000 is inherited unclamped.
    let response = select_budget(
        &mut client,
        &config,
        "b1-b-switch",
        select_body(
            "b1-b-switch",
            &session_b,
            generation_b,
            "kimi-for-coding",
            Some("kimi-oauth"),
        ),
    )
    .await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 60_000);
    assert_eq!(budget.clamped, None);
    run_turn(&mut client, &config, &session_b, generation_b, "b1-b-kimi").await;
    assert_eq!(kimi.requests()[1].max_tokens, 60_000);

    // Cover the two other provider/operation pairs explicitly: GLM create
    // with 60,000, and Kimi config update from its 30,000 derived default.
    let (session_c, generation_c) = create_with_budget_and_attach(
        &mut client,
        &config,
        &workspace,
        "haider-code",
        "glm-5.3",
        60_000,
    )
    .await;
    run_turn(&mut client, &config, &session_c, generation_c, "b1-c-glm").await;
    assert_eq!(haider_code.requests()[3].max_tokens, 60_000);

    let (session_d, generation_d) = create_with_budget_and_attach(
        &mut client,
        &config,
        &workspace,
        "kimi-oauth",
        "kimi-for-coding",
        0,
    )
    .await;
    run_turn(
        &mut client,
        &config,
        &session_d,
        generation_d,
        "b1-d-derived",
    )
    .await;
    assert_eq!(kimi.requests()[2].max_tokens, 30_000);
    let mut update = select_body(
        "b1-d-config",
        &session_d,
        generation_d,
        "kimi-for-coding",
        None,
    );
    if let RequestBody::SessionSelectModel { max_tokens, .. } = &mut update {
        *max_tokens = Some(60_000);
    }
    let response = select_budget(&mut client, &config, "b1-d-config", update).await;
    let budget = committed_budget(&response);
    assert_eq!(budget.max_tokens, 60_000);
    assert_eq!(budget.clamped, None);
    run_turn(
        &mut client,
        &config,
        &session_d,
        generation_d,
        "b1-d-explicit",
    )
    .await;
    assert_eq!(kimi.requests()[3].max_tokens, 60_000);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// B2: equal projected values do not erase the catalog declaration. Both
/// rows resolve to 8,192, but only the row that declares it rejects 20,000.
#[tokio::test]
async fn declared_8192_refuses_20000_while_equal_fallback_reaches_provider() {
    let root = test_root("b2-equal-8192-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "b2-equal-8192",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let row = |slug: &str, declared: Option<u64>| haider_provider::DiscoveredModel {
        slug: slug.into(),
        display_name: slug.into(),
        context_window: Some(1_048_576),
        max_output_tokens: declared,
        description: None,
        default_effort: None,
        supported_efforts: Vec::new(),
        visible: true,
        priority: None,
        use_responses_lite: None,
        extensions: None,
    };
    let models = [
        row("probe-declared-8192", Some(8_192)),
        row("probe-fallback-8192", None),
    ];
    let models_json = serde_json::to_string(&models).expect("serialize catalog rows");
    let fetched_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64;
    haider_store::Store::open(&config.store_dir)
        .expect("open catalog store")
        .put_provider_models("haider-code", &models_json, None, fetched_at_ms)
        .expect("seed public Haider Code inventory");
    let fake = Arc::new(FakeProvider::new(text_turn("fallback accepted")));
    let task = ready_with_dependencies(
        &config,
        routed_dependencies(&[("haider-code", fake.clone())]),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "b2-equal-client",
        "b2-equal-instance",
        ClientKind::Cli,
    )
    .await;
    send(
        &mut client,
        &config,
        "b2-declared-create",
        RequestBody::SessionCreate {
            command_id: CommandId::new("b2-declared-command"),
            cwd: workspace.display().to_string(),
            provider: "haider-code".into(),
            model: "probe-declared-8192".into(),
            max_tokens: 20_000,
        },
    )
    .await;
    let response = next_response(&mut client).await;
    assert!(
        matches!(
            &response,
            ResponseBody::Error {
                data: Some(ErrorData::ModelOutputLimit {
                    requested: 20_000,
                    max_output_tokens: 8_192,
                    ..
                }),
                ..
            }
        ),
        "declared catalog maximum must give a typed refusal: {response:?}"
    );
    assert!(
        fake.requests().is_empty(),
        "refusal precedes provider dispatch"
    );

    let (session_id, generation) = create_with_budget_and_attach(
        &mut client,
        &config,
        &workspace,
        "haider-code",
        "probe-fallback-8192",
        20_000,
    )
    .await;
    run_turn(
        &mut client,
        &config,
        &session_id,
        generation,
        "b2-fallback-turn",
    )
    .await;
    assert_eq!(fake.requests()[0].max_tokens, 20_000);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
