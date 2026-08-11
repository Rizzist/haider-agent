//! G3 wire laws for `session.select_effort` / `session.select_fast` over a
//! real UnixStream — the exact `session.select_model` law set, cloned:
//! receipt replay, generation fence, and validation through the ONE
//! authority (LE1/LE2/LE4).

#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
use haider_daemon::ProviderFactoryConfig;
use haider_daemon::{DaemonConfig, DaemonDependencies, ProviderFactory, ResolvedTurnProvider};
use haider_protocol::error::HaiderError;
use haider_protocol::provider::FinishReason;
use haider_protocol::session::{EffortSelected, FastModeSelected, SessionMetadataV1};
use haider_provider::{FakeProvider, FakeStep};
use haider_rpc::{
    AttachMode, ClientKind, CommandId, ERROR_CODE_EFFORT_UNSUPPORTED, ERROR_CODE_FAST_UNSUPPORTED,
    ERROR_CODE_STALE_GENERATION, ErrorData, RequestBody, RequestId, ResponseBody, WireFrame,
};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::sync::Arc;
use support::{UdsClient, ready_with_dependencies, test_root};

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
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
        })
    }
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
    send(
        client,
        config,
        "create",
        RequestBody::SessionCreate {
            command_id: CommandId::new(format!("create-command-{provider}")),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: provider.into(),
            model: model.into(),
            max_tokens: 4096,
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

fn effort_body(
    command_id: &str,
    session_id: &haider_protocol::ids::SessionId,
    generation: u64,
    effort: Option<&str>,
) -> RequestBody {
    RequestBody::SessionSelectEffort {
        command_id: CommandId::new(command_id),
        session_id: session_id.clone(),
        worker_generation: generation,
        effort: effort.map(str::to_owned),
        confirm_new_epoch: false,
    }
}

fn fast_body(
    command_id: &str,
    session_id: &haider_protocol::ids::SessionId,
    generation: u64,
    enabled: bool,
) -> RequestBody {
    RequestBody::SessionSelectFast {
        command_id: CommandId::new(command_id),
        session_id: session_id.clone(),
        worker_generation: generation,
        enabled,
        confirm_new_epoch: false,
    }
}

fn idle_fake() -> Arc<FakeProvider> {
    Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "unused".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]))
}

/// LAW (LE1, wire half): the effort selection commits with a receipt (the
/// same-command retry replays the exact coordinates), the `effort_selected`
/// fact is PUBLISHED to the live attachment, a revert (`effort: null`)
/// commits `None`, and a stale worker generation is refused with the stable
/// code, mutating nothing.
///
/// LAW (LE2, anthropic-static half): an out-of-ladder effort on an
/// anthropic pair refuses with `effort_unsupported` naming the exact static
/// ladder.
#[tokio::test]
async fn select_effort_is_receipted_validated_and_replays() {
    let root = test_root("g3-effort-wire-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "g3-effort-wire",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task =
        ready_with_dependencies(&config, routed_dependencies(&[("anthropic", idle_fake())])).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "g3-effort-wire-client",
        "g3-effort-wire-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "anthropic",
        "claude-opus-5",
    )
    .await;

    send(
        &mut client,
        &config,
        "select-effort",
        effort_body("effort-command", &session_id, generation, Some("xhigh")),
    )
    .await;
    // The committed response and the PUBLISHED fact arrive on independent
    // lanes, in either order — collect both.
    let mut first = None;
    let mut fact = None;
    while first.is_none() || fact.is_none() {
        match client.next().await {
            WireFrame::Response { body, .. } => first = Some(body),
            WireFrame::Event { envelope, .. } => {
                if let Some(selected) = EffortSelected::from_payload_value(&envelope.payload) {
                    fact = Some(selected);
                }
            }
            _ => {}
        }
    }
    let first = first.expect("select response");
    assert_eq!(
        fact.expect("published effort_selected fact")
            .effort
            .as_deref(),
        Some("xhigh")
    );
    let ResponseBody::SessionSelectEffort {
        session_id: responded_session,
        effort,
        selected_seq,
        worker_generation,
    } = first.clone()
    else {
        panic!("expected session.select_effort response, got {first:?}");
    };
    assert_eq!(responded_session, session_id);
    assert_eq!(effort.as_deref(), Some("xhigh"));
    assert_eq!(worker_generation, generation);
    assert!(selected_seq > 0);

    // R2: the same command replays the exact committed coordinates.
    send(
        &mut client,
        &config,
        "select-effort-retry",
        effort_body("effort-command", &session_id, generation, Some("xhigh")),
    )
    .await;
    assert_eq!(next_response(&mut client).await, first);

    // LE2: out-of-ladder refuses with the typed data naming the ladder.
    send(
        &mut client,
        &config,
        "select-effort-bad",
        effort_body("effort-bad-command", &session_id, generation, Some("ultra")),
    )
    .await;
    let ResponseBody::Error {
        code,
        retryable,
        data,
        ..
    } = next_response(&mut client).await
    else {
        panic!("expected typed effort refusal");
    };
    assert_eq!(code, ERROR_CODE_EFFORT_UNSUPPORTED);
    assert!(!retryable);
    assert_eq!(
        data,
        Some(ErrorData::EffortUnsupported {
            provider: "anthropic".into(),
            model: "claude-opus-5".into(),
            effort: "ultra".into(),
            supported: ["low", "medium", "high", "xhigh", "max"]
                .map(str::to_owned)
                .to_vec(),
        })
    );

    // A revert commits None.
    send(
        &mut client,
        &config,
        "select-effort-revert",
        effort_body("effort-revert-command", &session_id, generation, None),
    )
    .await;
    let ResponseBody::SessionSelectEffort { effort, .. } = next_response(&mut client).await else {
        panic!("expected revert response");
    };
    assert_eq!(effort, None);

    // A stale worker generation is refused with the stable code.
    send(
        &mut client,
        &config,
        "select-effort-stale",
        effort_body(
            "effort-stale-command",
            &session_id,
            generation + 1,
            Some("low"),
        ),
    )
    .await;
    let ResponseBody::Error { code, .. } = next_response(&mut client).await else {
        panic!("expected stale-generation refusal");
    };
    assert_eq!(code, ERROR_CODE_STALE_GENERATION);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// LAW (LE4, refusal half) + LAW (LE2, empty-ladder half): enabling fast on
/// a gate pair commits and publishes its fact; enabling it on an anthropic
/// pair OUTSIDE the static gate refuses `fast_unsupported`; DISABLING is
/// always accepted; and a pair with no declared effort ladder refuses every
/// effort with the EMPTY supported list.
#[tokio::test]
async fn select_fast_gates_statically_and_empty_ladders_refuse() {
    let root = test_root("g3-fast-wire-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "g3-fast-wire",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(
        &config,
        routed_dependencies(&[("anthropic", idle_fake()), ("fake", idle_fake())]),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "g3-fast-wire-client",
        "g3-fast-wire-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "anthropic",
        "claude-opus-5",
    )
    .await;

    // Enabling on a gate pair commits and publishes its fact.
    send(
        &mut client,
        &config,
        "select-fast",
        fast_body("fast-command", &session_id, generation, true),
    )
    .await;
    let mut first = None;
    let mut fact = None;
    while first.is_none() || fact.is_none() {
        match client.next().await {
            WireFrame::Response { body, .. } => first = Some(body),
            WireFrame::Event { envelope, .. } => {
                if let Some(selected) = FastModeSelected::from_payload_value(&envelope.payload) {
                    fact = Some(selected);
                }
            }
            _ => {}
        }
    }
    assert!(fact.expect("published fast_mode_selected fact").enabled);
    let ResponseBody::SessionSelectFast { enabled, .. } = first.expect("fast response") else {
        panic!("expected session.select_fast response");
    };
    assert!(enabled);

    // Switch the pair OUT of the gate (anthropic has no discovered
    // inventory here, so the selection is accepted honestly)…
    send(
        &mut client,
        &config,
        "select-model",
        RequestBody::SessionSelectModel {
            command_id: CommandId::new("model-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            model: "claude-sonnet-5".into(),
            provider: None,
            confirm_new_epoch: false,
        },
    )
    .await;
    let ResponseBody::SessionSelectModel { model, .. } = next_response(&mut client).await else {
        panic!("expected model response");
    };
    assert_eq!(model, "claude-sonnet-5");

    // …then enabling refuses typed, while DISABLING still commits.
    send(
        &mut client,
        &config,
        "select-fast-bad",
        fast_body("fast-bad-command", &session_id, generation, true),
    )
    .await;
    let ResponseBody::Error { code, data, .. } = next_response(&mut client).await else {
        panic!("expected typed fast refusal");
    };
    assert_eq!(code, ERROR_CODE_FAST_UNSUPPORTED);
    assert_eq!(
        data,
        Some(ErrorData::FastUnsupported {
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
        })
    );
    send(
        &mut client,
        &config,
        "select-fast-off",
        fast_body("fast-off-command", &session_id, generation, false),
    )
    .await;
    let ResponseBody::SessionSelectFast { enabled, .. } = next_response(&mut client).await else {
        panic!("expected fast-off response");
    };
    assert!(!enabled);

    // A pair with no declared ladder refuses every effort (empty supported)
    // and fast entirely.
    let (fake_session, fake_generation) =
        create_and_attach(&mut client, &config, &workspace, "fake", "fake-v1").await;
    send(
        &mut client,
        &config,
        "select-effort-fake",
        effort_body(
            "effort-fake-command",
            &fake_session,
            fake_generation,
            Some("high"),
        ),
    )
    .await;
    let ResponseBody::Error { code, data, .. } = next_response(&mut client).await else {
        panic!("expected empty-ladder refusal");
    };
    assert_eq!(code, ERROR_CODE_EFFORT_UNSUPPORTED);
    assert_eq!(
        data,
        Some(ErrorData::EffortUnsupported {
            provider: "fake".into(),
            model: "fake-v1".into(),
            effort: "high".into(),
            supported: Vec::new(),
        })
    );
    send(
        &mut client,
        &config,
        "select-fast-fake",
        fast_body("fast-fake-command", &fake_session, fake_generation, true),
    )
    .await;
    let ResponseBody::Error { code, .. } = next_response(&mut client).await else {
        panic!("expected fake fast refusal");
    };
    assert_eq!(code, ERROR_CODE_FAST_UNSUPPORTED);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
