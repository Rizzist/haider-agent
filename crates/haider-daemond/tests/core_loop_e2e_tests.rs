//! v0.0.967 core-loop gate: production daemon + real IPC, with only the
//! provider boundary scripted. Tools, delegation, store, projection, and RPC
//! framing are the production implementations.

#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_accounts::{MemoryVault, Vault as _};
use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ProviderFactoryConfig, ResolvedTurnProvider,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{CredentialAlias, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, TurnItem};
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason};
use haider_protocol::session::{
    SessionInteractionModeV1, SessionMetadataV1, SessionPermissionOverridesV1,
};
use haider_protocol::session_fork::{ForkContextEpoch, SessionForkPromptSelector, SessionForked};
use haider_protocol::state::RunState;
use haider_provider::{
    AnthropicProvider, FakeProvider, FakeStep, PreparedTurn, Provider, ProviderError,
    ProviderStream, ToolDefinition, TurnRequest,
};
use haider_rpc::{
    AttachMode, CancelStatus, ClientKind, CommandId, FleetAgentStateWire, RequestBody, RequestId,
    ResponseBody, SeqRange, SessionFleetSnapshot, SshAuthInputWire, SshProfileInputWire,
    SshScopeWire, WireFrame,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use support::{UdsClient, ready_with_dependencies, test_root};

#[derive(Clone)]
struct RoutingFactory {
    providers: Arc<BTreeMap<String, Arc<dyn Provider>>>,
    fail_on_resolve: Arc<BTreeSet<String>>,
}

struct CacheAwareFixtureProvider {
    renderer: AnthropicProvider,
    scripted: Arc<FakeProvider>,
}

#[async_trait]
impl Provider for CacheAwareFixtureProvider {
    fn prepare_turn(&self, request: &TurnRequest) -> Option<PreparedTurn> {
        self.renderer.prepare_turn(request)
    }

    fn prepare_turn_with_tools(
        &self,
        request: &TurnRequest,
        tools: &[ToolDefinition],
    ) -> Option<PreparedTurn> {
        self.renderer.prepare_turn_with_tools(request, tools)
    }

    async fn capabilities(&self) -> CapabilityDoc {
        self.scripted.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.scripted.stream_turn(request).await
    }
}

#[async_trait]
impl ProviderFactory for RoutingFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        if self.fail_on_resolve.contains(&metadata.provider) {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                format!("fixture provider `{}` failed to launch", metadata.provider),
                false,
            ));
        }
        let provider = self.providers.get(&metadata.provider).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("fixture provider `{}` is not routed", metadata.provider),
                false,
            )
        })?;
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(provider),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: Some(format!("{}-fixture-account", metadata.provider)),
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

fn dependencies(
    providers: impl IntoIterator<Item = (String, Arc<dyn Provider>)>,
    fail_on_resolve: impl IntoIterator<Item = String>,
) -> DaemonDependencies {
    let providers = BTreeMap::from_iter(providers);
    let creatable = providers
        .keys()
        .cloned()
        .chain(fail_on_resolve)
        .collect::<BTreeSet<_>>();
    let fail_on_resolve = creatable
        .iter()
        .filter(|provider| !providers.contains_key(*provider))
        .cloned()
        .collect();
    DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(RoutingFactory {
                providers: Arc::new(providers),
                fail_on_resolve: Arc::new(fail_on_resolve),
            }),
            providers: creatable,
        },
        ..DaemonDependencies::default()
    }
}

fn fake_dependencies(script: Vec<FakeStep>) -> (DaemonDependencies, Arc<FakeProvider>) {
    let fake = Arc::new(FakeProvider::new(script));
    let provider: Arc<dyn Provider> = fake.clone();
    (dependencies([("fake".into(), provider)], []), fake)
}

fn cache_aware_dependencies(script: Vec<FakeStep>) -> (DaemonDependencies, Arc<FakeProvider>) {
    let vault = MemoryVault::default();
    let alias = CredentialAlias::new("core-loop-cache-secret");
    vault
        .put(&alias, b"fixture-secret-never-sent")
        .expect("stage fixture credential");
    let renderer = AnthropicProvider::new_custom_no_auth(
        vault.resolve(&alias).expect("resolve fixture credential"),
        "cache-model",
        "http://127.0.0.1:18181/v1",
    )
    .expect("construct real Anthropic renderer")
    .with_prompt_caching_verified(true);
    let fake = Arc::new(FakeProvider::new(script));
    let provider: Arc<dyn Provider> = Arc::new(CacheAwareFixtureProvider {
        renderer,
        scripted: fake.clone(),
    });
    (dependencies([("cache-fixture".into(), provider)], []), fake)
}

fn tool_round(
    call_id: &str,
    name: &str,
    args: serde_json::Value,
    continuation: &str,
) -> Vec<FakeStep> {
    vec![
        FakeStep::EmitToolCall {
            call_id: call_id.into(),
            name: name.into(),
            args,
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: call_id.into(),
        },
        FakeStep::EmitText {
            text: continuation.into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]
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

async fn next_response(client: &mut UdsClient) -> WireFrame {
    loop {
        let frame = client.next().await;
        if matches!(frame, WireFrame::Response { .. }) {
            return frame;
        }
    }
}

async fn create_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
    provider: &str,
    model: &str,
    permission_overrides: Option<SessionPermissionOverridesV1>,
    ssh_scope: Option<SshScopeWire>,
) -> (SessionId, u64) {
    send_request(
        client,
        config,
        "create",
        RequestBody::SessionCreateWithPermissionOverrides {
            command_id: CommandId::new("create-command"),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: provider.into(),
            model: model.into(),
            max_tokens: 4096,
            permission_overrides,
            cache_policy: None,
            interaction_mode: SessionInteractionModeV1::Interactive,
            ssh_scope,
        },
    )
    .await;
    let (session_id, generation) = match client.next_reply().await {
        WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    worker_generation,
                    ..
                },
            ..
        } => (session_id, worker_generation),
        other => panic!("expected session.create response, got {other:?}"),
    };
    send_request(
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
    let mut response = false;
    let mut caught_up = false;
    while !(response && caught_up) {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::SessionAttach { .. },
                ..
            } => response = true,
            WireFrame::AttachCaughtUp { .. } => caught_up = true,
            _ => {}
        }
    }
    (session_id, generation)
}

async fn submit_turn(
    client: &mut UdsClient,
    config: &DaemonConfig,
    command_id: &str,
    session_id: SessionId,
    generation: u64,
    text: &str,
) -> RunId {
    send_request(
        client,
        config,
        command_id,
        RequestBody::TurnSubmit {
            command_id: CommandId::new(command_id),
            session_id,
            worker_generation: generation,
            text: text.into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    loop {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::TurnSubmit { run_id, .. },
                ..
            } => return run_id,
            WireFrame::Response {
                body: ResponseBody::Error { code, message, .. },
                ..
            } => panic!("turn.submit `{command_id}` failed ({code}): {message}"),
            WireFrame::ProtocolError(error) => {
                panic!("turn.submit `{command_id}` failed: {error}")
            }
            _ => {}
        }
    }
}

async fn events_until_terminal(client: &mut UdsClient, run_id: &RunId) -> Vec<EventPayload> {
    tokio::time::timeout(support::DEADLINE, async {
        let mut events = Vec::new();
        loop {
            if let WireFrame::Event { envelope, .. } = client.next().await {
                if envelope.run_id.as_ref() != Some(run_id) {
                    continue;
                }
                let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                    continue;
                };
                let terminal = matches!(
                    payload,
                    EventPayload::RunState(
                        RunState::Done | RunState::Errored | RunState::Cancelled
                    )
                );
                events.push(payload);
                if terminal {
                    return events;
                }
            }
        }
    })
    .await
    .expect("run reaches a terminal event")
}

async fn cancel_and_collect_terminal(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    generation: u64,
    run_id: RunId,
    label: &str,
) -> Vec<EventPayload> {
    send_request(
        client,
        config,
        label,
        RequestBody::TurnCancel {
            command_id: CommandId::new(format!("{label}-command")),
            session_id,
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    tokio::time::timeout(support::DEADLINE, async {
        let mut response_seen = false;
        let mut events = Vec::new();
        loop {
            match client.next().await {
                WireFrame::Response {
                    body: ResponseBody::TurnCancel { .. },
                    ..
                } => response_seen = true,
                WireFrame::Event { envelope, .. } if envelope.run_id.as_ref() == Some(&run_id) => {
                    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload)
                    else {
                        continue;
                    };
                    let terminal = matches!(payload, EventPayload::RunState(RunState::Cancelled));
                    events.push(payload);
                    if terminal && response_seen {
                        return events;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .expect("cancellation response and terminal event")
}

async fn read_session(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionRead {
            session_id,
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

async fn attach_existing(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionAttach {
            session_id,
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    let mut response = false;
    let mut caught_up = false;
    let mut replay = Vec::new();
    while !(response && caught_up) {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::SessionAttach { .. },
                ..
            } => response = true,
            WireFrame::AttachCaughtUp { .. } => caught_up = true,
            WireFrame::Event { envelope, .. } => replay.push(envelope),
            _ => {}
        }
    }
    replay
}

async fn fleet(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> SessionFleetSnapshot {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionFleet { session_id },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionFleet { snapshot },
            ..
        } = client.next().await
        {
            return snapshot;
        }
    }
}

async fn tools_inventory(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> haider_protocol::tool::ToolInventorySnapshot {
    send_request(
        client,
        config,
        request_id,
        RequestBody::ToolsInventory { session_id },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::ToolsInventory { inventory, .. },
            ..
        } = client.next().await
        {
            return inventory;
        }
    }
}

async fn submit_turn_allow_always(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    generation: u64,
    text: &str,
) -> (RunId, Vec<EventPayload>) {
    let run_id = submit_turn(
        client,
        config,
        "allow-always-turn",
        session_id.clone(),
        generation,
        text,
    )
    .await;
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            let frame = client.next().await;
            let WireFrame::Event { envelope, .. } = frame else {
                continue;
            };
            if envelope.run_id.as_ref() != Some(&run_id) {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            if let EventPayload::MenuOpened(menu) = &payload
                && let Some((index, option)) =
                    menu.options.iter().enumerate().find(|(_, option)| {
                        option.decision == Some(haider_protocol::menu::DecisionKind::AllowAlways)
                    })
            {
                let option_key = option.key.clone();
                client
                    .send(
                        &WireFrame::MenuAnswer {
                            request_id: Some(RequestId::new("allow-always-answer")),
                            command_id: CommandId::new("allow-always-answer-command"),
                            session_id: session_id.clone(),
                            menu_id: menu.id.clone(),
                            request_seq: envelope.seq,
                            worker_generation: envelope.worker_generation,
                            option_key,
                            option_index: u32::try_from(index).expect("menu option index"),
                            input: None,
                        },
                        config.frame_limit,
                    )
                    .await;
            }
            let terminal = matches!(payload, EventPayload::RunState(RunState::Done));
            events.push(payload);
            if terminal {
                return events;
            }
        }
    })
    .await
    .expect("allow-always turn reaches Done");
    (run_id, events)
}

async fn assert_isolated_test_passes(test_name: &str, marker: &str) {
    let executable = std::env::current_exe().expect("current integration-test executable");
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(marker, "1")
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(8), command.output())
        .await
        .unwrap_or_else(|_| panic!("isolated regression `{test_name}` hung past 8 seconds"))
        .expect("launch isolated regression process");
    assert!(
        output.status.success(),
        "isolated regression `{test_name}` failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn continuation_seen(events: &[EventPayload], marker: &str) -> bool {
    events.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) if text.contains(marker)
        )
    })
}

fn tool_preview<'a>(events: &'a [EventPayload], call_id: &str) -> &'a str {
    events
        .iter()
        .find_map(|payload| match payload {
            EventPayload::ToolResult {
                call_id: seen,
                result,
            } if seen == call_id => Some(result.preview.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing tool result for {call_id}"))
}

fn stdout_bytes(events: &[EventPayload]) -> Vec<u8> {
    events
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Delta {
                delta:
                    ItemDelta::CommandOutput {
                        stream: OutputStream::Stdout,
                        chunk_b64,
                    },
                ..
            }) => Some(BASE64.decode(chunk_b64).expect("command output base64")),
            _ => None,
        })
        .flatten()
        .collect()
}

fn init_git_workspace(workspace: &Path) {
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .status()
        .expect("initialize git fixture");
    assert!(status.success(), "git fixture initializes");
}

#[cfg(unix)]
fn background_child_command() -> String {
    "(sleep 0.05; printf child) & printf leader; wait".into()
}

#[cfg(windows)]
fn background_child_command() -> String {
    concat!(
        "$job=Start-Job { Start-Sleep -Milliseconds 50; [Console]::Out.Write('child') };",
        "[Console]::Out.Write('leader');Wait-Job $job|Out-Null;Receive-Job $job"
    )
    .into()
}

#[cfg(unix)]
fn outliving_pipe_child_command() -> String {
    "sh ./outliving-leader.sh".into()
}

#[cfg(windows)]
fn outliving_pipe_child_command() -> String {
    "cmd.exe /D /S /C outliving-leader.cmd".into()
}

#[cfg(unix)]
fn install_outliving_pipe_fixture(workspace: &Path) {
    fs::write(
        workspace.join("outliving-leader.sh"),
        concat!(
            "#!/bin/sh\n",
            "(sleep 0.2; printf ran > outliving-child-ran.log; printf child) &\n",
            "printf leader\n",
            "exit 0\n"
        ),
    )
    .expect("write outliving-child leader script");
}

#[cfg(windows)]
fn install_outliving_pipe_fixture(workspace: &Path) {
    fs::write(
        workspace.join("outliving-leader.cmd"),
        concat!(
            "@echo off\r\n",
            "start \"\" /b cmd.exe /D /S /C \"ping -n 2 127.0.0.1 >nul & ",
            "echo ran>outliving-child-ran.log & ^<nul set /p =child\"\r\n",
            "<nul set /p =leader\r\n",
            "exit /b 0\r\n"
        ),
    )
    .expect("write outliving-child leader script");
}

#[cfg(unix)]
fn simple_output_command() -> String {
    "printf nonrepo-real-output".into()
}

#[cfg(windows)]
fn simple_output_command() -> String {
    "[Console]::Out.Write('nonrepo-real-output')".into()
}

#[cfg(unix)]
fn no_output_command() -> String {
    ":".into()
}

#[cfg(windows)]
fn no_output_command() -> String {
    "$null=1".into()
}

#[cfg(unix)]
fn large_output_command(bytes: usize) -> String {
    format!("/usr/bin/head -c {bytes} /dev/zero")
}

#[cfg(unix)]
fn shell_round_trip_command() -> String {
    "printf shell-real-output; printf shell-side-effect > shell.txt".into()
}

#[cfg(windows)]
fn shell_round_trip_command() -> String {
    concat!(
        "[Console]::Out.Write('shell-real-output');",
        "[IO.File]::WriteAllText('shell.txt','shell-side-effect',[Text.Encoding]::ASCII)"
    )
    .into()
}

#[cfg(unix)]
fn cancellable_process_command() -> String {
    concat!(
        "(printf started > descendant-started.log; sleep 0.5; ",
        "printf survived > descendant-survived.log) & ",
        "while :; do printf x >> heartbeat.log; sleep 0.01; done"
    )
    .into()
}

#[cfg(windows)]
fn cancellable_process_command() -> String {
    concat!(
        "$workspace=(Get-Location).Path;[Environment]::CurrentDirectory=$workspace;",
        "$start=[Diagnostics.ProcessStartInfo]::new();",
        "$start.FileName=(Join-Path ([Environment]::SystemDirectory) 'cmd.exe');",
        "$start.Arguments='/D /S /C \"echo started>descendant-started.log & ",
        "ping -n 3 127.0.0.1 >nul & echo survived>descendant-survived.log\"';",
        "$start.WorkingDirectory=$workspace;$start.UseShellExecute=$false;",
        "$child=[Diagnostics.Process]::Start($start);",
        "if($null -eq $child){throw 'descendant did not start'};$child.Dispose();",
        "$heartbeat=Join-Path $workspace 'heartbeat.log';",
        "while($true){[IO.File]::AppendAllText($heartbeat,'x',[Text.Encoding]::ASCII);",
        "Start-Sleep -Milliseconds 10}"
    )
    .into()
}

#[cfg(windows)]
fn large_output_command(bytes: usize) -> String {
    format!(
        "$b=New-Object byte[] {bytes};$s=[Console]::OpenStandardOutput();$s.Write($b,0,$b.Length)"
    )
}

/// A1: one provider script forces every real tool result back through a
/// second provider request. Removing the dispatcher, output stream, receipt
/// bounds, background-child output, or terminal projection leaves a missing
/// marker/output assertion.
#[tokio::test]
async fn tool_calls_execute_and_continue_over_real_rpc() {
    let root = test_root("core-loop-tools-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join(".gitignore"), "target/\n").expect("gitignore");
    fs::create_dir(workspace.join("target")).expect("ignored build directory");
    fs::File::create(workspace.join("target/ignored-build.bin"))
        .expect("ignored build file")
        .set_len(1024 * 1024 * 1024 * 1024)
        .expect("large sparse ignored build file");
    init_git_workspace(&workspace);

    let calls = [
        (
            "exec-desc",
            "process_exec",
            serde_json::json!({"command": background_child_command()}),
            "continued-exec-desc",
        ),
        (
            "exec-none",
            "process_exec",
            serde_json::json!({"command": no_output_command()}),
            "continued-exec-none",
        ),
        (
            "exec-large",
            "process_exec",
            serde_json::json!({"command": large_output_command(512 * 1024)}),
            "continued-exec-large",
        ),
        (
            "exec-cap",
            "process_exec",
            serde_json::json!({"command": large_output_command(2 * 1024 * 1024)}),
            "continued-exec-cap",
        ),
        (
            "write",
            "fs_write",
            serde_json::json!({"path": "note.txt", "content": "alpha needle\n"}),
            "continued-write",
        ),
        (
            "read",
            "fs_read",
            serde_json::json!({"path": "note.txt"}),
            "continued-read",
        ),
        (
            "edit",
            "fs_edit",
            serde_json::json!({"path": "note.txt", "edits": [{"old": "alpha", "new": "beta"}]}),
            "continued-edit",
        ),
        (
            "search",
            "fs_search",
            serde_json::json!({"path": ".", "pattern": "beta needle", "mode": "literal"}),
            "continued-search",
        ),
    ];
    let script = calls
        .iter()
        .flat_map(|(call_id, name, args, marker)| tool_round(call_id, name, args.clone(), marker))
        .collect();
    let (dependencies, fake) = fake_dependencies(script);
    let config = DaemonConfig::new(
        "core-loop-tools",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "tool-client",
        ClientKind::Headless,
    )
    .await;
    let overrides = SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: true,
        allow_mobile: false,
        auto_allow: false,
    };
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(overrides),
        None,
    )
    .await;

    let mut observed = BTreeMap::new();
    for (index, (call_id, _, _, marker)) in calls.iter().enumerate() {
        let run = submit_turn(
            &mut client,
            &config,
            &format!("tool-turn-{index}"),
            session.clone(),
            generation,
            marker,
        )
        .await;
        let events = events_until_terminal(&mut client, &run).await;
        assert!(
            continuation_seen(&events, marker),
            "turn did not continue after {call_id}"
        );
        observed.insert(
            *call_id,
            (
                tool_preview(&events, call_id).to_owned(),
                stdout_bytes(&events),
            ),
        );
    }

    assert_eq!(observed["exec-desc"].1, b"leaderchild");
    assert!(observed["exec-none"].1.is_empty());
    assert_eq!(observed["exec-large"].1.len(), 512 * 1024);
    let cap: serde_json::Value =
        serde_json::from_str(&observed["exec-cap"].0).expect("capped process result JSON");
    assert_eq!(cap["status"], "failed");
    assert_eq!(cap["limits"]["max_output_bytes"], 1024 * 1024);
    assert!(observed["read"].0.contains("alpha needle"));
    assert!(observed["search"].0.contains("beta needle"));
    assert_eq!(
        fs::read_to_string(workspace.join("note.txt")).expect("edited file"),
        "beta needle\n"
    );
    assert_eq!(fake.requests().len(), calls.len() * 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Historical regression name retained for the release gate. Foreground
/// `process_exec` owns its process group, so leader exit must sweep descendants
/// rather than let `server &` escape the tool's resource and sandbox boundary.
/// Long-lived commands use the explicit background-process path instead.
#[tokio::test]
async fn process_exec_drains_output_from_child_that_outlives_leader() {
    let root = test_root("core-loop-outliving-pipe-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    init_git_workspace(&workspace);
    install_outliving_pipe_fixture(&workspace);
    let (dependencies, fake) = fake_dependencies(tool_round(
        "outliving-pipe",
        "process_exec",
        serde_json::json!({"command": outliving_pipe_child_command()}),
        "continued-outliving-pipe",
    ));
    let config = DaemonConfig::new(
        "core-loop-outliving-pipe",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "outliving-pipe-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(SessionPermissionOverridesV1 {
            allow_exec: true,
            ..SessionPermissionOverridesV1::default()
        }),
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "outliving-pipe-turn",
        session,
        generation,
        "drain output after the leader exits",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(
        !workspace.join("outliving-child-ran.log").exists(),
        "foreground process_exec leaked a descendant after its leader exited"
    );
    assert_eq!(stdout_bytes(&events), b"leader");
    assert!(continuation_seen(&events, "continued-outliving-pipe"));
    assert_eq!(fake.requests().len(), 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// The 966 T2 shape: a non-repository root must reach spawn and return real
/// output; `coverage=unknown` is receipt truth, never a command failure.
#[tokio::test]
async fn process_exec_runs_in_a_non_repository_workspace() {
    let root = test_root("core-loop-nonrepo-");
    let workspace = root.path().join("plain-workspace");
    fs::create_dir(&workspace).expect("non-repository workspace");
    let (dependencies, fake) = fake_dependencies(tool_round(
        "nonrepo-exec",
        "process_exec",
        serde_json::json!({"command": simple_output_command()}),
        "continued-nonrepo",
    ));
    let config = DaemonConfig::new(
        "core-loop-nonrepo",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "nonrepo-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(SessionPermissionOverridesV1 {
            allow_exec: true,
            ..SessionPermissionOverridesV1::default()
        }),
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "nonrepo-turn",
        session,
        generation,
        "run outside git",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    assert_eq!(stdout_bytes(&events), b"nonrepo-real-output");
    assert!(continuation_seen(&events, "continued-nonrepo"));
    assert_eq!(fake.requests().len(), 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A1: the user `!` path is a real `shell.exec` RPC, not a provider tool
/// simulation. Its subprocess output and side effect become durable, then the
/// next provider turn observes the raw command record.
#[tokio::test]
async fn direct_shell_rpc_executes_and_is_visible_to_the_next_turn() {
    let root = test_root("core-loop-shell-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "provider-observed-shell-record".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let config = DaemonConfig::new(
        "core-loop-shell",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "shell-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
    )
    .await;
    let command = shell_round_trip_command();
    send_request(
        &mut client,
        &config,
        "shell-exec",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-command"),
            session_id: session.clone(),
            worker_generation: generation,
            command: command.clone(),
            cwd: None,
        },
    )
    .await;
    let shell_run = match next_response(&mut client).await {
        WireFrame::Response {
            body: ResponseBody::ShellExec {
                run_id: Some(run), ..
            },
            ..
        } => run,
        other => panic!("expected shell.exec receipt, got {other:?}"),
    };
    let shell_events = events_until_terminal(&mut client, &shell_run).await;
    assert_eq!(stdout_bytes(&shell_events), b"shell-real-output");
    assert_eq!(
        fs::read_to_string(workspace.join("shell.txt")).expect("shell side effect"),
        "shell-side-effect"
    );

    let next_run = submit_turn(
        &mut client,
        &config,
        "after-shell-turn",
        session,
        generation,
        "explain the shell result",
    )
    .await;
    let events = events_until_terminal(&mut client, &next_run).await;
    assert!(continuation_seen(&events, "provider-observed-shell-record"));
    assert_eq!(fake.requests().len(), 1);
    let provider_saw_raw_command = fake.requests()[0].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(block, Block::Text { text } if text.contains("[user-initiated shell command]") && text.contains(&command))
        })
    });
    assert!(
        provider_saw_raw_command,
        "next turn did not see shell record"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A1: cancellation crosses RPC into the worker and process supervisor. Both
/// the leader heartbeat and its independently running descendant must stop;
/// merely returning a timeout/error while leaving either process alive fails.
#[tokio::test]
async fn cancelling_process_exec_kills_the_real_process_group() {
    let root = test_root("core-loop-process-cancel-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (dependencies, _) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "cancel-process".into(),
            name: "process_exec".into(),
            args: serde_json::json!({"command": cancellable_process_command()}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
    ]);
    let config = DaemonConfig::new(
        "core-loop-process-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "process-cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(SessionPermissionOverridesV1 {
            allow_exec: true,
            ..SessionPermissionOverridesV1::default()
        }),
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "cancel-process-turn",
        session.clone(),
        generation,
        "start cancellable process tree",
    )
    .await;
    let heartbeat = workspace.join("heartbeat.log");
    let descendant_started = workspace.join("descendant-started.log");
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            if fs::metadata(&heartbeat).is_ok_and(|metadata| metadata.len() >= 2)
                && descendant_started.exists()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("leader and descendant actually start before cancellation");

    let cancel_events = cancel_and_collect_terminal(
        &mut client,
        &config,
        session,
        generation,
        run,
        "cancel-process",
    )
    .await;
    assert!(cancel_events.contains(&EventPayload::RunState(RunState::Cancelled)));
    let stopped_size = fs::metadata(&heartbeat).expect("heartbeat exists").len();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(
        fs::metadata(&heartbeat)
            .expect("heartbeat remains inspectable")
            .len(),
        stopped_size,
        "cancelled leader kept running"
    );
    assert!(
        !workspace.join("descendant-survived.log").exists(),
        "outliving descendant escaped the process group"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2: provider A delegates to provider B through the public tool schema.
/// Both providers run, B's report returns through the tool result to A, and
/// the fleet projection carries the durable task/model/provider identity.
#[tokio::test]
async fn cross_provider_subagent_returns_to_parent_and_fleet_is_truthful() {
    let root = test_root("core-loop-cross-agent-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cross-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "cross-provider-check",
                "prompt": "return the child report",
                "model": "child-model",
                "provider": "child-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "cross-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-observed-child-completion".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "real-child-report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let dependencies = dependencies(
        [
            ("parent-provider".into(), parent_provider),
            ("child-provider".into(), child_provider),
        ],
        [],
    );
    let config = DaemonConfig::new(
        "core-loop-cross-agent",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "cross-agent-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "cross-agent-turn",
        session.clone(),
        generation,
        "delegate",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    assert!(continuation_seen(
        &events,
        "parent-observed-child-completion"
    ));
    assert_eq!(parent.requests().len(), 2);
    assert_eq!(child.requests().len(), 1);
    assert_eq!(child.requests()[0].model, "child-model");

    let snapshot = fleet(&mut client, &config, session, "cross-agent-fleet").await;
    assert_eq!(snapshot.roots.len(), 1);
    let node = &snapshot.roots[0];
    assert_eq!(node.task, "cross-provider-check");
    assert_eq!(node.model.as_deref(), Some("child-model"));
    assert_eq!(node.provider.as_deref(), Some("child-provider"));
    assert!(
        node.callsign
            .as_deref()
            .is_some_and(|name| !name.is_empty())
    );
    assert_eq!(node.state, FleetAgentStateWire::Done);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2: a child whose provider cannot resolve must become a typed failed
/// descendant and still give the parent a tool result it can continue from.
#[tokio::test]
async fn subagent_launch_failure_returns_to_parent_instead_of_hanging() {
    let root = test_root("core-loop-agent-launch-failure-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "failed-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "must-fail-to-launch",
                "prompt": "this provider fails during worker resolution",
                "model": "missing-model",
                "provider": "missing-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "failed-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-observed-launch-failure".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let dependencies = dependencies(
        [("parent-provider".into(), parent_provider)],
        ["missing-provider".into()],
    );
    let config = DaemonConfig::new(
        "core-loop-agent-launch-failure",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-launch-failure-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "failed-agent-turn",
        session.clone(),
        generation,
        "delegate to a provider that fails",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    assert!(continuation_seen(&events, "parent-observed-launch-failure"));
    let snapshot = fleet(&mut client, &config, session, "failed-agent-fleet").await;
    assert_eq!(snapshot.roots.len(), 1);
    assert_eq!(snapshot.roots[0].state, FleetAgentStateWire::Failed);
    assert_eq!(snapshot.roots[0].model.as_deref(), Some("missing-model"));
    assert_eq!(
        snapshot.roots[0].provider.as_deref(),
        Some("missing-provider")
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2: an operator cancellation of a live child crosses the same RPC surface
/// used by clients. The child becomes durably Cancelled, its parent receives
/// the collapsed result, and no provider task is left holding the parent.
///
/// MUTATION CHECK: route through child `turn.cancel`, skip the durable cancel
/// transition, or fail to collect the cancelled report. Expected runtime
/// failure: the public response, terminal fleet state, or parent continuation
/// assertion below times out or changes shape.
#[tokio::test]
async fn manually_cancelled_running_child_releases_parent() {
    let root = test_root("core-loop-agent-cancel-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cancel-child-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "cancel-me",
                "prompt": "remain active until an operator cancels",
                "model": "child-model",
                "provider": "child-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "cancel-child-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-observed-child-cancellation".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![FakeStep::Hang]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let config = DaemonConfig::new(
        "core-loop-agent-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(
        &config,
        dependencies(
            [
                ("parent-provider".into(), parent_provider),
                ("child-provider".into(), child_provider),
            ],
            [],
        ),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let parent_run = submit_turn(
        &mut client,
        &config,
        "cancel-child-parent-turn",
        session.clone(),
        generation,
        "spawn a cancellable child",
    )
    .await;
    let child_agent = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = fleet(
                &mut client,
                &config,
                session.clone(),
                "cancel-child-fleet-wait",
            )
            .await;
            let Some(node) = snapshot
                .roots
                .first()
                .filter(|node| node.state == FleetAgentStateWire::Live)
            else {
                tokio::task::yield_now().await;
                continue;
            };
            break node.agent_id.clone();
        }
    })
    .await
    .expect("running child has a durable agent coordinate");

    send_request(
        &mut client,
        &config,
        "cancel-running-child",
        RequestBody::AgentCancel {
            command_id: CommandId::new("cancel-running-child-command"),
            session_id: session.clone(),
            worker_generation: generation,
            agent: child_agent.clone(),
        },
    )
    .await;
    let cancel_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        next_response(&mut client),
    )
    .await
    .expect("child-cancel response arrives");
    assert!(
        matches!(
            cancel_response,
            WireFrame::Response {
                body: ResponseBody::AgentCancel {
                    status: CancelStatus::Accepted,
                    ..
                },
                ..
            }
        ),
        "expected accepted agent.cancel response, got {cancel_response:?}"
    );
    let parent_events = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let mut attempt = 0_u32;
        loop {
            let label = format!("cancelled-parent-read-{attempt}");
            attempt = attempt.saturating_add(1);
            let journal = read_session(&mut client, &config, session.clone(), &label).await;
            let events = journal
                .into_iter()
                .filter(|envelope| envelope.run_id.as_ref() == Some(&parent_run))
                .filter_map(|envelope| serde_json::from_value(envelope.payload).ok())
                .collect::<Vec<EventPayload>>();
            let terminal = events.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::RunState(
                        RunState::Done | RunState::Errored | RunState::Cancelled
                    )
                )
            });
            if terminal && continuation_seen(&events, "parent-observed-child-cancellation") {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled child releases parent through durable RPC projection");
    assert!(continuation_seen(
        &parent_events,
        "parent-observed-child-cancellation"
    ));
    let snapshot = fleet(&mut client, &config, session, "cancelled-child-fleet").await;
    assert_eq!(snapshot.roots.len(), 1);
    assert_eq!(snapshot.roots[0].state, FleetAgentStateWire::Cancelled);

    send_request(
        &mut client,
        &config,
        "cancel-finished-child",
        RequestBody::AgentCancel {
            command_id: CommandId::new("cancel-finished-child-command"),
            session_id: snapshot.session_id,
            worker_generation: generation,
            agent: child_agent,
        },
    )
    .await;
    let finished_response = next_response(&mut client).await;
    assert!(matches!(
        finished_response,
        WireFrame::Response {
            body: ResponseBody::AgentCancel {
                status: CancelStatus::AlreadyTerminal,
                terminal_seq: Some(_),
                ..
            },
            ..
        }
    ));
    assert_eq!(parent.requests().len(), 2);
    assert_eq!(child.requests().len(), 1);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A3: a prompt-oriented fork crosses the public RPC boundary after two real
/// provider turns. The source journal remains byte-for-byte untouched, the
/// selected prompt returns as an unsent draft, the built-in Anthropic renderer
/// carries the inherited cache cohort into the child request, and neither an
/// explicit SSH deny-all scope nor a remembered AllowAlways grant widens.
#[tokio::test]
async fn fork_from_prompt_preserves_source_cache_and_privilege_boundaries() {
    let root = test_root("core-loop-prompt-fork-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let script = vec![
        FakeStep::EmitToolCall {
            call_id: "fork-source-write".into(),
            name: "fs_write".into(),
            args: serde_json::json!({"path":"fork-source.txt","content":"source write\n"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "fork-source-write".into(),
        },
        FakeStep::EmitText {
            text: "first source answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "second source answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "fork child used inherited prefix".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ];
    let (dependencies, fake) = cache_aware_dependencies(script);
    let config = DaemonConfig::new(
        "core-loop-prompt-fork",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "prompt-fork-client",
        ClientKind::Headless,
    )
    .await;

    send_request(
        &mut client,
        &config,
        "ssh-add",
        RequestBody::SshAdd {
            profile: SshProfileInputWire {
                name: "fork-denied-host".into(),
                description: Some("scope inheritance fixture".into()),
                host: "127.0.0.1".into(),
                port: 22,
                user: "fixture".into(),
                auth: SshAuthInputWire::Agent,
                default_cwd: None,
            },
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut client).await,
        WireFrame::Response {
            body: ResponseBody::SshAdd { .. },
            ..
        }
    ));

    let (source, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "cache-fixture",
        "cache-model",
        None,
        Some(SshScopeWire::None),
    )
    .await;
    let (_, first_events) = submit_turn_allow_always(
        &mut client,
        &config,
        source.clone(),
        generation,
        "write once and remember that permission",
    )
    .await;
    assert!(continuation_seen(&first_events, "first source answer"));
    assert_eq!(
        fs::read_to_string(workspace.join("fork-source.txt")).expect("real source write"),
        "source write\n"
    );
    let source_inventory = tools_inventory(
        &mut client,
        &config,
        source.clone(),
        "source-inventory-before-fork",
    )
    .await;
    assert_eq!(
        source_inventory.remembered_grants.len(),
        1,
        "fixture must establish a real AllowAlways grant before forking"
    );

    let second_run = submit_turn(
        &mut client,
        &config,
        "fork-source-second-turn",
        source.clone(),
        generation,
        "editable second prompt",
    )
    .await;
    let second_events = events_until_terminal(&mut client, &second_run).await;
    assert!(continuation_seen(&second_events, "second source answer"));
    let source_before =
        read_session(&mut client, &config, source.clone(), "source-before-fork").await;
    let prompt_seq = source_before
        .iter()
        .find_map(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::UserMessage { text, .. }) if text == "editable second prompt"
            )
            .then_some(envelope.seq)
        })
        .expect("selected source prompt has a durable sequence");
    assert!(source_before.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&second_run)
            && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Done))
    }));

    send_request(
        &mut client,
        &config,
        "prompt-fork",
        RequestBody::SessionFork {
            command_id: CommandId::new("prompt-fork-command"),
            session_id: source.clone(),
            worker_generation: generation,
            source_branch_id: None,
            fork_node_id: None,
            fork_seq: None,
            prompt: Some(SessionForkPromptSelector { seq: prompt_seq }),
            name: Some("Core-loop prompt fork".into()),
        },
    )
    .await;
    let (child, child_generation, forked_from, draft) = match next_response(&mut client).await {
        WireFrame::Response {
            body:
                ResponseBody::SessionFork {
                    session_id,
                    worker_generation,
                    forked_from,
                    draft,
                    ..
                },
            ..
        } => (session_id, worker_generation, forked_from, draft),
        other => panic!("expected prompt fork response, got {other:?}"),
    };
    assert_eq!(
        forked_from.as_ref().map(|value| value.seq),
        Some(prompt_seq)
    );
    let draft = draft.expect("prompt fork returns editable draft");
    assert_eq!(draft.text, "editable second prompt");
    assert!(draft.attachments.is_empty());

    let source_after =
        read_session(&mut client, &config, source.clone(), "source-after-fork").await;
    assert_eq!(
        source_after, source_before,
        "fork mutated the original transcript or terminal"
    );

    let child_replay = attach_existing(
        &mut client,
        &config,
        child.clone(),
        "attach-prompt-fork-child",
    )
    .await;
    assert!(!child_replay.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::UserMessage { text, .. }) if text == "editable second prompt"
        )
    }));
    let fork_audit = child_replay
        .iter()
        .find_map(|envelope| SessionForked::from_payload_value(&envelope.payload))
        .expect("child carries prompt-fork audit fact");
    assert_eq!(
        fork_audit.forked_from.as_ref().map(|value| value.seq),
        Some(prompt_seq)
    );
    assert_eq!(fork_audit.context_epoch, ForkContextEpoch::Inherited);
    let inherited = fork_audit
        .inherited_cache_segment
        .expect("byte-identical copied prefix inherits cache segment");

    send_request(
        &mut client,
        &config,
        "child-ssh-list",
        RequestBody::SshList {
            session_id: Some(child.clone()),
        },
    )
    .await;
    match next_response(&mut client).await {
        WireFrame::Response {
            body: ResponseBody::SshList { profiles },
            ..
        } => {
            assert_eq!(profiles.len(), 1);
            assert!(
                !profiles[0].in_scope,
                "fork widened explicit SSH deny-all scope"
            );
        }
        other => panic!("expected child ssh.list, got {other:?}"),
    }
    let child_inventory = tools_inventory(
        &mut client,
        &config,
        child.clone(),
        "child-inventory-after-fork",
    )
    .await;
    assert!(
        child_inventory.remembered_grants.is_empty(),
        "AllowAlways grant crossed the fork audit boundary"
    );

    let child_run = submit_turn(
        &mut client,
        &config,
        "fork-child-turn",
        child.clone(),
        child_generation,
        &draft.text,
    )
    .await;
    let child_events = events_until_terminal(&mut client, &child_run).await;
    assert!(continuation_seen(
        &child_events,
        "fork child used inherited prefix"
    ));
    let requests = fake.requests();
    assert_eq!(requests.len(), 4, "two source rounds plus child request");
    let child_cache = requests
        .last()
        .and_then(|request| request.cache_metadata.as_ref())
        .expect("child provider request carries cache metadata");
    assert_eq!(child_cache.session_scope, child.as_str());
    assert_eq!(
        child_cache.cache_cohort.as_deref(),
        Some(inherited.cache_route.as_str()),
        "child request missed the inherited prompt-cache route"
    );
    assert_ne!(
        child_cache.cache_cohort.as_deref(),
        Some(child_cache.session_scope.as_str()),
        "cache hit was replaced by a fresh child cohort"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2 regression shape reported by the owner. The second child run is queued
/// while the first is live. When run one reaches terminal, run two starts, so
/// the child session emits no aggregate Idle settlement for run one. The
/// parent must consume run one's durable terminal report after the bounded
/// best-effort tail and continue.
#[tokio::test]
async fn terminal_child_run_without_session_idle_still_releases_parent() {
    const ISOLATION_MARKER: &str = "HAIDER_CORE_LOOP_NO_IDLE_CHILD";
    if std::env::var_os(ISOLATION_MARKER).is_none() {
        assert_isolated_test_passes(
            "terminal_child_run_without_session_idle_still_releases_parent",
            ISOLATION_MARKER,
        )
        .await;
        return;
    }

    let root = test_root("core-loop-agent-no-idle-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "no-idle-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "terminal-without-idle",
                "prompt": "finish the first child run",
                "model": "child-model",
                "provider": "child-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "no-idle-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-progressed-without-child-idle".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![
        FakeStep::Delay { ms: 800 },
        FakeStep::EmitText {
            text: "first child run terminal report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Hang,
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let config = DaemonConfig::new(
        "core-loop-agent-no-idle",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(
        &config,
        dependencies(
            [
                ("parent-provider".into(), parent_provider),
                ("child-provider".into(), child_provider),
            ],
            [],
        ),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-no-idle-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let parent_run = submit_turn(
        &mut client,
        &config,
        "no-idle-parent-turn",
        session.clone(),
        generation,
        "delegate then continue",
    )
    .await;
    let child_session = tokio::time::timeout(support::DEADLINE, async {
        let mut attempt = 0_u32;
        loop {
            let label = format!("no-idle-fleet-wait-{attempt}");
            attempt = attempt.saturating_add(1);
            let snapshot = fleet(&mut client, &config, session.clone(), &label).await;
            if let Some(node) = snapshot.roots.first() {
                break node.session_id.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("spawned child appears in real fleet projection");
    let mut child_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-no-idle-child-client",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut child_client,
        &config,
        child_session.clone(),
        "attach-no-idle-child",
    )
    .await;
    let queued_run = submit_turn(
        &mut child_client,
        &config,
        "queued-child-turn",
        child_session.clone(),
        generation,
        "keep the child session active after run one",
    )
    .await;
    let queued_response = read_session(
        &mut child_client,
        &config,
        child_session.clone(),
        "queued-child-read",
    )
    .await;
    assert!(queued_response.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&queued_run)
            && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Queued)))
    }));
    let (parent_events, child_journal) =
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            let mut attempt = 0_u32;
            loop {
                let parent_label = format!("no-idle-parent-read-{attempt}");
                let journal =
                    read_session(&mut client, &config, session.clone(), &parent_label).await;
                let events = journal
                    .into_iter()
                    .filter(|envelope| envelope.run_id.as_ref() == Some(&parent_run))
                    .filter_map(|envelope| serde_json::from_value(envelope.payload).ok())
                    .collect::<Vec<EventPayload>>();
                let terminal = events
                    .iter()
                    .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)));
                if terminal && continuation_seen(&events, "parent-progressed-without-child-idle") {
                    let child_label = format!("no-idle-child-read-{attempt}");
                    let child_journal = read_session(
                        &mut child_client,
                        &config,
                        child_session.clone(),
                        &child_label,
                    )
                    .await;
                    break (events, child_journal);
                }
                attempt = attempt.saturating_add(1);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable child terminal releases its parent without aggregate Idle");

    assert!(continuation_seen(
        &parent_events,
        "parent-progressed-without-child-idle"
    ));
    assert!(
        parent_events
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
    );
    let child_terminal_seq = child_journal
        .iter()
        .find_map(|envelope| {
            if envelope
                .run_id
                .as_ref()
                .is_none_or(|run_id| run_id == &queued_run)
            {
                return None;
            }
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
                .then_some(envelope.seq)
        })
        .expect("first child run reaches durable Done");
    assert!(
        child_journal.iter().any(|envelope| {
            envelope.run_id.as_ref() == Some(&queued_run)
                && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::RunState(
                                RunState::Thinking | RunState::Streaming | RunState::RunningTool
                            )
                        )
                    },
                )
        }),
        "queued child run did not keep the session active"
    );
    assert!(
        !child_journal.iter().any(|envelope| {
            envelope.seq > child_terminal_seq
                && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::SessionState(
                                haider_protocol::state::SessionState::Idle { .. }
                            )
                        )
                    },
                )
        }),
        "child emitted Idle after the first run terminal, invalidating the regression shape"
    );
    task.shutdown_handle().request("test complete");
    tokio::time::timeout(std::time::Duration::from_secs(2), task.join())
        .await
        .expect("daemon shutdown stays bounded")
        .expect("daemon joins");
}
