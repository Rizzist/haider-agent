//! Black-box user-message hook parity across production submission surfaces.
#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
#[cfg(windows)]
use base64::Engine as _;
#[cfg(windows)]
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_client::{
    EnsureOptions, HeadlessEvent, HeadlessOutcome, HeadlessRunRequest, ResolvedProfile,
    required_headless_features, run_headless,
};
use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ProviderFactoryConfig, ResolvedTurnProvider,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{RunId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use haider_rpc::{
    AttachMode, ClientKind, CommandId, RequestBody, RequestId, ResponseBody, SeqRange, WireFrame,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::{UdsClient, ready_with_dependencies, test_root};
use tokio::sync::mpsc;

#[cfg(unix)]
fn capture_hook_command(capture: &Path) -> String {
    format!("(cat; printf '\\n') >> '{}'", capture.display())
}

#[cfg(windows)]
fn capture_hook_command(capture: &Path) -> String {
    let capture = capture.display().to_string().replace('\'', "''");
    let script = format!(
        "$i=[Console]::OpenStandardInput();$f=[IO.File]::Open('{capture}',[IO.FileMode]::Append,[IO.FileAccess]::Write,[IO.FileShare]::ReadWrite);$i.CopyTo($f);$f.WriteByte(10);$f.Dispose()"
    );
    let encoded = BASE64.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let executable = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(std::path::PathBuf::from)
        .map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .filter(|path| path.is_file())
        .unwrap_or_else(|| {
            std::path::PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        });
    format!(
        "\"{}\" -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}",
        executable.display()
    )
}

#[cfg(unix)]
const CAPTURE_HOOK_TIMEOUT_MS: u64 = 1_000;

#[cfg(windows)]
const CAPTURE_HOOK_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone)]
struct FakeFactory {
    fake: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderFactory for FakeFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

fn write_hook_configuration(store: &Path, workspace: &Path, capture: &Path) {
    fs::write(
        store.join("hooks.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "haider.hooks.v1",
            "policy": "trust_workspace",
            "hooks": {},
        }))
        .expect("profile hook config"),
    )
    .expect("write profile hook config");
    let command = capture_hook_command(capture);
    fs::write(
        workspace.join("hooks.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "haider.hooks.v1",
            "hooks": {
                "capture_user_message": {
                    "matcher": {"event": "user_message"},
                    "kind": "exec",
                    "command": command,
                    "timeout_ms": CAPTURE_HOOK_TIMEOUT_MS,
                }
            },
        }))
        .expect("workspace hook config"),
    )
    .expect("write workspace hook config");
}

async fn send_request(
    client: &mut UdsClient,
    config: &DaemonConfig,
    request_id: &str,
    body: RequestBody,
) {
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

async fn direct_rpc_submit(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
) -> (SessionId, RunId) {
    send_request(
        client,
        config,
        "rpc-create",
        RequestBody::SessionCreate {
            command_id: CommandId::new("rpc-create-command"),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4_096,
        },
    )
    .await;
    let (session_id, generation) = loop {
        if let WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    worker_generation,
                    ..
                },
            ..
        } = client.next().await
        {
            break (session_id, worker_generation);
        }
    };
    send_request(
        client,
        config,
        "rpc-attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    let mut attached = false;
    let mut caught_up = false;
    while !(attached && caught_up) {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::SessionAttach { .. },
                ..
            } => attached = true,
            WireFrame::AttachCaughtUp { .. } => caught_up = true,
            _ => {}
        }
    }
    send_request(
        client,
        config,
        "rpc-submit",
        RequestBody::TurnSubmit {
            command_id: CommandId::new("rpc-submit-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: "surface-neutral text".into(),
            attachments: vec![],
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    let run_id = loop {
        if let WireFrame::Response {
            body: ResponseBody::TurnSubmit { run_id, .. },
            ..
        } = client.next().await
        {
            break run_id;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        for attempt in 0_u32.. {
            let terminal = read_session(
                client,
                config,
                &session_id,
                &format!("read-rpc-terminal-{attempt}"),
            )
            .await
            .into_iter()
            .any(|envelope| {
                envelope.run_id.as_ref() == Some(&run_id)
                    && serde_json::from_value::<EventPayload>(envelope.payload).is_ok_and(
                        |payload| {
                            matches!(
                                payload,
                                EventPayload::RunState(
                                    RunState::Done | RunState::Errored | RunState::Cancelled
                                )
                            )
                        },
                    )
            });
            if terminal {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("direct RPC terminal");
    (session_id, run_id)
}

async fn read_session(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionRead {
            session_id: session_id.clone(),
            range: SeqRange {
                start_seq: 1,
                end_seq: 1_024,
            },
        },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionRead { result },
            ..
        } = client.next().await
        {
            return result.envelopes;
        }
    }
}

async fn hook_fired_count(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    request_id: &str,
) -> usize {
    read_session(client, config, session_id, request_id)
        .await
        .iter()
        .filter(|envelope| {
            envelope
                .payload
                .get("type")
                .and_then(serde_json::Value::as_str)
                == Some("hook_fired")
        })
        .count()
}

async fn wait_for_hook_fired(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    prefix: &str,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        for attempt in 0_u32.. {
            if hook_fired_count(client, config, session_id, &format!("{prefix}-{attempt}")).await
                == 1
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("one durable HookFired fact");
}

/// MUTATION CHECK: branch on client kind/surface, dispatch before the common
/// acceptance fact, or include envelope-only ids/timestamps in hook JSON.
/// Expected RUNTIME failure: one surface has no/double HookFired fact or the
/// normalized captured JSON values differ.
#[tokio::test]
async fn user_message_hook_fires_for_headless_and_rpc_submissions_identically() {
    let root = test_root("h5-user-hook-");
    let store = root.path().join("store");
    let runtime = root.path().join("runtime");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&store).expect("store dir");
    fs::create_dir_all(&runtime).expect("runtime dir");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let store = fs::canonicalize(store).expect("canonical store");
    let runtime = fs::canonicalize(runtime).expect("canonical runtime");
    let workspace = fs::canonicalize(workspace).expect("canonical workspace");
    let capture = root.path().join("user-message-events.jsonl");
    write_hook_configuration(&store, &workspace, &capture);

    let config = DaemonConfig::new("h5-user-hook-profile", &store, &runtime);
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "RPC answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "headless answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(FakeFactory { fake }),
            providers: BTreeSet::from(["fake".to_owned()]),
        },
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies).await;

    let mut rpc = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "h5-rpc-client",
        "h5-rpc-instance",
        ClientKind::Cli,
    )
    .await;
    let (rpc_session, rpc_run) = direct_rpc_submit(&mut rpc, &config, &workspace).await;

    let profile = ResolvedProfile {
        profile_id: config.profile_id.clone(),
        store_dir: store.clone(),
        runtime_dir: runtime.clone(),
        endpoint_path: config.endpoint_path(),
        default_provider: "fake".into(),
        default_model: "fake-model".into(),
        default_max_tokens: 4_096,
    };
    let ensure = EnsureOptions {
        required_features: required_headless_features(SessionPermissionOverridesV1::default()),
        startup_deadline: Duration::from_secs(5),
        daemon_binary: None,
        client: haider_client::ClientConfig::default(),
        daemon_lifetime: haider_client::DaemonLifetime::Persistent,
    };
    let (output, mut events) = mpsc::channel::<HeadlessEvent>(64);
    let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
    let result = run_headless(
        &profile,
        ensure,
        HeadlessRunRequest {
            cwd: workspace.to_string_lossy().into_owned(),
            prompt: "surface-neutral text".into(),
            attachments: vec![],
            durable_attachments: vec![],
            provider: Some("fake".into()),
            model: Some("fake-model".into()),
            max_tokens: 4_096,
            budget: haider_rpc::haider_protocol::headless::RunBudgetV1::default(),
            seed: None,
            replay_of: None,
            journal_pin: false,
            detached: false,
            permission_overrides: SessionPermissionOverridesV1::default(),
            trust_hooks: false,
            timeout: Some(Duration::from_secs(5)),
            terminal_grace: Duration::from_secs(2),
        },
        output,
    )
    .await
    .expect("headless run");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    drain.await.expect("headless event drain");

    wait_for_hook_fired(&mut rpc, &config, &rpc_session, "read-rpc-hook").await;
    wait_for_hook_fired(&mut rpc, &config, &result.session_id, "read-headless-hook").await;
    let contents = fs::read_to_string(&capture).expect("captured hook JSONL");
    let mut values = contents
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("hook JSON line"))
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 2);
    let rpc_index = values
        .iter()
        .position(|value| value["session"] == rpc_session.as_str())
        .expect("RPC hook event");
    let headless_index = values
        .iter()
        .position(|value| value["session"] == result.session_id.as_str())
        .expect("headless hook event");
    assert_ne!(rpc_index, headless_index);
    assert_eq!(values[rpc_index]["run"], rpc_run.as_str());
    assert_eq!(values[headless_index]["run"], result.run_id.as_str());
    for value in &mut values {
        value["session"] = serde_json::Value::String("<session>".into());
        value["run"] = serde_json::Value::String("<run>".into());
    }
    assert_eq!(values[rpc_index], values[headless_index]);
    assert_eq!(values[rpc_index]["branch"], serde_json::Value::Null);
    assert_eq!(values[rpc_index]["mode"], "queue");
    assert_eq!(values[rpc_index]["attachments"]["count"], 0);
    assert!(values[rpc_index].get("surface").is_none());

    drop(rpc);
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
