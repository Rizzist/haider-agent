//! Public agent and workflow automation. All state comes from native RPCs;
//! journal cursors, child identities and reports are never reconstructed IDs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use haider_client::{ClientConfig, EnsureOptions, ProfileEnv, RpcClient, resolve_profile};
use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, ChildReport, ReportVerification};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::headless::{AgentSpawnSpecV1, HeadlessRunSpecV1};
use haider_protocol::ids::{AgentId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::session::SessionInteractionModeV1;
use haider_protocol::state::RunState;
use haider_rpc::{AttachMode, CommandId, RequestBody, ResponseBody, SeqRange};
use serde_json::{Value, json};
use tokio::time::Instant;

// Observation policy, not a child lifetime: a caller can repeat wait after
// this expires. The startup and each RPC retain the client's named budgets.
const DEFAULT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);
const OBSERVATION_POLL: Duration = Duration::from_millis(50);
const PAGE_SIZE: u64 = 256;

struct Failure {
    code: String,
    message: String,
    retryable: bool,
    exit: u8,
    result: Value,
}

impl Failure {
    fn new(code: &str, message: impl ToString, exit: u8) -> Self {
        Self {
            code: code.into(),
            message: message.to_string(),
            retryable: false,
            exit,
            result: Value::Null,
        }
    }
    fn protocol(message: impl ToString) -> Self {
        Self::new("protocol_mismatch", message, 76)
    }
    fn unavailable(message: impl ToString) -> Self {
        let mut error = Self::new("unavailable", message, 69);
        error.retryable = true;
        error
    }
    fn timeout(result: Value) -> Self {
        let mut error = Self::new(
            "timeout",
            "observation deadline elapsed; accepted work continues",
            124,
        );
        error.retryable = true;
        error.result = result;
        error
    }
}

struct Options {
    family: String,
    verb: String,
    positional: Vec<String>,
    flags: BTreeMap<String, String>,
    json: bool,
    no_spawn: bool,
    timeout: Duration,
}

impl Options {
    fn parse(family: &str, args: &[String]) -> Result<Self, Failure> {
        let usage = |message: String| Failure::new("invalid_argument", message, 2);
        let verb = args
            .first()
            .ok_or_else(|| usage(format!("expected {family} subcommand")))?;
        let valid = match family {
            "agent" => ["spawn", "list", "message", "cancel", "wait"].contains(&verb.as_str()),
            "workflow" => ["run", "status", "list"].contains(&verb.as_str()),
            _ => false,
        };
        if !valid {
            return Err(usage(format!("unknown {family} command `{verb}`")));
        }
        let spawning = verb == "spawn" || verb == "run";
        let mut options = Self {
            family: family.into(),
            verb: verb.clone(),
            positional: Vec::new(),
            flags: BTreeMap::new(),
            json: false,
            no_spawn: false,
            timeout: DEFAULT_OBSERVATION_TIMEOUT,
        };
        let mut args = args[1..].iter();
        while let Some(argument) = args.next() {
            let key = match argument.as_str() {
                "-p" => "--prompt",
                other => other,
            };
            match key {
                "--" => {
                    options.positional.extend(args.cloned());
                    break;
                }
                "--json" if !options.json => options.json = true,
                "--no-spawn" if !options.no_spawn => options.no_spawn = true,
                "--timeout" | "--command-id" | "--task" | "--prompt" | "--provider" | "--model"
                | "--agent-type" | "--cwd" | "--workflow" | "--trigger" => {
                    let allowed = key == "--timeout"
                        || (key == "--command-id"
                            && (spawning || verb == "message" || verb == "cancel"))
                        || (spawning && key != "--workflow")
                        || (family == "agent" && verb == "spawn" && key == "--workflow");
                    if !allowed {
                        return Err(usage(format!("{key} is not valid for {family} {verb}")));
                    }
                    if options.flags.contains_key(key) {
                        return Err(usage(format!("duplicate {key}")));
                    }
                    let value = args
                        .next()
                        .filter(|value| !value.trim().is_empty() && !value.starts_with("--"))
                        .ok_or_else(|| usage(format!("{key} requires a value")))?;
                    options.flags.insert(key.into(), value.clone());
                }
                unknown if unknown.starts_with('-') => {
                    return Err(usage(format!("unknown or duplicate flag `{unknown}`")));
                }
                value => options.positional.push(value.into()),
            }
        }
        if let Some(timeout) = options.flags.get("--timeout") {
            options.timeout = super::run::parse_timeout(timeout).map_err(usage)?;
        }
        let count = options.positional.len();
        let expected = match (family, verb.as_str()) {
            ("agent", "spawn") => usize::from(!options.flags.contains_key("--prompt")),
            ("workflow", "run") => 1 + usize::from(!options.flags.contains_key("--prompt")),
            ("agent", "message") => 3,
            ("agent", "cancel" | "wait") => 2,
            ("workflow", "list") => 0,
            _ => 1,
        };
        if count != expected
            || options
                .positional
                .iter()
                .any(|value| value.trim().is_empty())
        {
            return Err(usage(format!(
                "{family} {verb} expects {expected} positional argument(s); see haider {family} --help"
            )));
        }
        if (verb == "run" || options.flags.contains_key("--workflow"))
            && !options.flags.contains_key("--trigger")
        {
            return Err(usage(
                "workflow execution requires an explicit --trigger".into(),
            ));
        }
        if options.flags.contains_key("--trigger")
            && verb == "spawn"
            && !options.flags.contains_key("--workflow")
        {
            return Err(usage("--trigger requires --workflow".into()));
        }
        Ok(options)
    }

    fn flag(&self, name: &str) -> Option<String> {
        self.flags.get(name).cloned()
    }
    fn command_id(&self) -> String {
        self.flag("--command-id").unwrap_or_else(|| {
            format!(
                "agentcli-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |time| time.as_nanos())
            )
        })
    }
}

struct Connection {
    client: RpcClient,
    drain: tokio::task::JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.client.close();
        self.drain.abort();
    }
}

impl Connection {
    async fn open(options: &Options) -> Result<Self, Failure> {
        let profile = resolve_profile(&ProfileEnv::capture()).map_err(Failure::unavailable)?;
        let config = ClientConfig::default(); // LongLived services Ping/Pong during every observation wait (#95).
        let client = if options.no_spawn {
            haider_client::connect(&profile.endpoint_path, config)
                .await
                .map_err(Failure::unavailable)?
                .client
        } else {
            haider_client::ensure_daemon(
                &profile,
                EnsureOptions {
                    client: config,
                    required_features: BTreeSet::new(),
                    ..EnsureOptions::default()
                },
            )
            .await
            .map_err(Failure::unavailable)?
            .client
        };
        if client.welcome().profile_id != profile.profile_id {
            let _ = client.close();
            return Err(Failure::protocol("daemon profile identity mismatch"));
        }
        if client.welcome().lifecycle_phase != haider_rpc::LifecyclePhase::Ready {
            let _ = client.close();
            return Err(Failure::protocol("daemon is not ready"));
        }
        let mut events = client
            .take_events()
            .ok_or_else(|| Failure::protocol("event receiver unavailable"))?;
        // Attach grants control only. Consume its replay continuously, keeping
        // memory bounded; public results use independently paged session.read.
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        Ok(Self { client, drain })
    }

    fn require(&self, feature: &'static str) -> Result<(), Failure> {
        if self.client.welcome().features.contains(feature) {
            Ok(())
        } else {
            Err(Failure::protocol(format!(
                "daemon does not advertise {feature}"
            )))
        }
    }

    async fn request(
        &self,
        request: RequestBody,
        deadline: Instant,
    ) -> Result<ResponseBody, Failure> {
        let response = tokio::time::timeout_at(deadline, self.client.request(request))
            .await
            .map_err(|_| Failure::timeout(Value::Null))?
            .map_err(|error| match error {
                haider_client::ClientError::MissingFeature(_) => Failure::protocol(error),
                _ => Failure::unavailable(error),
            })?;
        match response {
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => {
                let mut failure = Failure::new(&code, message, 70);
                failure.retryable = retryable;
                Err(failure)
            }
            response => Ok(response),
        }
    }

    async fn control(&self, session_id: &SessionId, deadline: Instant) -> Result<u64, Failure> {
        match self
            .request(
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: AttachMode::Control,
                    sealed_replay: true,
                },
                deadline,
            )
            .await?
        {
            ResponseBody::SessionAttach { attach_state, .. } => Ok(attach_state.worker_generation),
            _ => Err(Failure::protocol("session.attach response mismatch")),
        }
    }

    async fn read(
        &self,
        session_id: &SessionId,
        cursor: u64,
        deadline: Instant,
    ) -> Result<haider_rpc::SessionReadResult, Failure> {
        match self
            .request(
                RequestBody::SessionRead {
                    session_id: session_id.clone(),
                    range: SeqRange {
                        start_seq: cursor.saturating_add(1),
                        end_seq: cursor.saturating_add(PAGE_SIZE),
                    },
                },
                deadline,
            )
            .await?
        {
            ResponseBody::SessionRead { result } if result.session_id == *session_id => Ok(result),
            _ => Err(Failure::protocol("session.read response mismatch")),
        }
    }
}

pub(crate) async fn command(family: &str, args: &[String]) -> ExitCode {
    let flag_args = &args[..args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len())];
    if flag_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        let help = if family == "agent" {
            "haider agent spawn <prompt> [--task NAME] [--provider NAME] [--model NAME] [--agent-type ID] [--cwd PATH]\nhaider agent list <parent-session-id>\nhaider agent message <parent-session-id> <agent-id> <message>\nhaider agent cancel <parent-session-id> <agent-id>\nhaider agent wait <parent-session-id> <agent-id> [--timeout DURATION]\nAll verbs: [--json] [--no-spawn] [--timeout DURATION]. Mutations: [--command-id ID]. Spawn also accepts --prompt/-p and --workflow ID --trigger REASON."
        } else {
            "haider workflow run <workflow-id> <prompt> --trigger REASON [--provider NAME] [--model NAME] [--cwd PATH]\nhaider workflow status <child-session-id>\nhaider workflow list\nAll verbs: [--json] [--no-spawn] [--timeout DURATION]. Run also accepts --prompt/-p and --command-id ID."
        };
        return write_text(help).map_or(ExitCode::from(74), |()| ExitCode::SUCCESS);
    }
    let json_output = flag_args.iter().any(|arg| arg == "--json");
    let verb = args.first().map_or("unknown", String::as_str);
    let schema = format!("haider.{family}.{verb}.v1");
    let result = match Options::parse(family, args) {
        Ok(options) => execute(&options).await,
        Err(error) => Err(error),
    };
    let (document, exit) = match result {
        Ok((result, exit)) => (
            json!({"schema":schema,"ok":exit == 0,"result":result,"error":null}),
            exit,
        ),
        Err(error) => (
            json!({"schema":schema,"ok":false,"result":error.result,"error":{
                "code":error.code,"message":error.message,"retryable":error.retryable,
            }}),
            error.exit,
        ),
    };
    let written = if json_output {
        write_json(&document)
    } else {
        let payload = if document["error"].is_null() {
            &document["result"]
        } else {
            &document["error"]
        };
        write_text(&serde_json::to_string_pretty(payload).unwrap_or_default())
    };
    if written.is_err() {
        ExitCode::from(74)
    } else {
        ExitCode::from(exit)
    }
}

async fn execute(options: &Options) -> Result<(Value, u8), Failure> {
    let connection = Connection::open(options).await?;
    let deadline = Instant::now() + options.timeout;
    match (options.family.as_str(), options.verb.as_str()) {
        ("agent", "spawn") | ("workflow", "run") => spawn(&connection, options, deadline)
            .await
            .map(|value| (value, 0)),
        ("agent", "list") => {
            connection.require(haider_rpc::FEATURE_SESSION_FLEET_V1)?;
            match connection
                .request(
                    RequestBody::SessionFleet {
                        session_id: SessionId::new(&options.positional[0]),
                    },
                    deadline,
                )
                .await?
            {
                ResponseBody::SessionFleet { snapshot } => Ok((json!(snapshot), 0)),
                _ => Err(Failure::protocol("session.fleet response mismatch")),
            }
        }
        ("agent", "message" | "cancel") => {
            connection.require(if options.verb == "message" {
                haider_rpc::FEATURE_AGENT_MESSAGE_V1
            } else {
                haider_rpc::FEATURE_AGENT_CANCEL_V1
            })?;
            let session_id = SessionId::new(&options.positional[0]);
            let worker_generation = connection.control(&session_id, deadline).await?;
            let agent = AgentId::new(&options.positional[1]);
            let command_id = CommandId::new(options.command_id());
            let request = if options.verb == "message" {
                RequestBody::AgentMessage {
                    command_id,
                    session_id,
                    worker_generation,
                    agent,
                    text: options.positional[2].clone(),
                }
            } else {
                RequestBody::AgentCancel {
                    command_id,
                    session_id,
                    worker_generation,
                    agent,
                }
            };
            match connection.request(request, deadline).await? {
                ResponseBody::AgentMessage { receipt } => Ok((json!({"receipt":receipt}), 0)),
                ResponseBody::AgentCancel {
                    agent,
                    child_session_id,
                    child_run_id,
                    status,
                    terminal_seq,
                } => Ok((
                    json!({"agent":agent,"child_session_id":child_session_id,"child_run_id":child_run_id,"status":status,"terminal_seq":terminal_seq}),
                    0,
                )),
                _ => Err(Failure::protocol("agent control response mismatch")),
            }
        }
        ("agent", "wait") => wait(&connection, options, deadline).await,
        ("workflow", "list") => {
            connection.require(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1)?;
            match connection
                .request(
                    RequestBody::LoomList {
                        include_archived: false,
                    },
                    deadline,
                )
                .await?
            {
                ResponseBody::LoomList {
                    workflow_catalog, ..
                } => Ok((json!({"workflows":workflow_catalog}), 0)),
                _ => Err(Failure::protocol("loom.list response mismatch")),
            }
        }
        ("workflow", "status") => {
            connection.require(haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1)?;
            connection.require(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)?;
            let session_id = SessionId::new(&options.positional[0]);
            let graph = match connection
                .request(
                    RequestBody::GraphStatus {
                        session_id: session_id.clone(),
                    },
                    deadline,
                )
                .await?
            {
                ResponseBody::GraphStatus { status } => status,
                _ => return Err(Failure::protocol("graph.status response mismatch")),
            };
            let activation = match connection
                .request(
                    RequestBody::WorkflowGraphState {
                        session_id: session_id.clone(),
                        graph_id: None,
                    },
                    deadline,
                )
                .await?
            {
                ResponseBody::WorkflowGraphState { state } => state,
                _ => return Err(Failure::protocol("workflow.graph.state response mismatch")),
            };
            Ok((
                json!({"session_id":session_id,"graph":graph,"activation":activation}),
                0,
            ))
        }
        _ => Err(Failure::protocol("unhandled command")),
    }
}

async fn spawn(
    connection: &Connection,
    options: &Options,
    deadline: Instant,
) -> Result<Value, Failure> {
    connection.require(haider_rpc::FEATURE_AGENT_CLI_V1)?;
    let prompt = options
        .flag("--prompt")
        .unwrap_or_else(|| options.positional.last().cloned().unwrap_or_default());
    let workflow = if options.family == "workflow" {
        Some(options.positional[0].clone())
    } else {
        options.flag("--workflow")
    }
    .map(|selector| {
        if matches!(selector.as_str(), "plain" | "implement_verify" | "deeper")
            || selector.starts_with("workflow_ref(")
        {
            selector
        } else {
            format!("workflow_ref({selector})")
        }
    });
    let mut spec = AgentSpawnSpecV1 {
        task: options.flag("--task").unwrap_or_else(|| "CLI task".into()),
        prompt: prompt.clone(),
        provider: options.flag("--provider"),
        model: options.flag("--model"),
        agent_type: options.flag("--agent-type"),
        workflow,
        workflow_trigger: options.flag("--trigger"),
    };
    // Validate tool syntax before creating a session. The CLI independently
    // accepts --provider; session.create supplies its real default model.
    // This validation-only model is never persisted or dispatched.
    let mut syntax = spec.clone();
    if syntax.provider.is_some() && syntax.model.is_none() {
        syntax.model = Some("daemon-default".into());
    }
    haider_tools::SpawnSubagent::from_tool_args(json!(&syntax))
        .map_err(|error| Failure::new("invalid_argument", error, 2))?;
    let cwd = options
        .flag("--cwd")
        .map(std::path::PathBuf::from)
        .map_or_else(std::env::current_dir, Ok)
        .map_err(Failure::unavailable)?;
    let cwd = std::fs::canonicalize(cwd)
        .map_err(Failure::unavailable)?
        .to_string_lossy()
        .into_owned();
    let command = options.command_id();
    let (session_id, metadata) = match connection
        .request(
            RequestBody::SessionCreateWithPermissionOverrides {
                command_id: CommandId::new(format!("{command}-session")),
                cwd,
                provider: spec.provider.clone().unwrap_or_default(),
                model: spec.model.clone().unwrap_or_default(),
                max_tokens: haider_client::DEFAULT_MAX_TOKENS,
                permission_overrides: None,
                cache_policy: None,
                interaction_mode: SessionInteractionModeV1::Autonomous,
                ssh_scope: None,
                account_alias: None,
                resolve_provider: spec.provider.is_none(),
                resolve_model: spec.model.is_none(),
                effort: None,
                fast: None,
            },
            deadline,
        )
        .await?
    {
        ResponseBody::SessionCreate {
            session_id,
            metadata,
            ..
        } => (session_id, metadata),
        _ => return Err(Failure::protocol("session.create response mismatch")),
    };
    spec.provider = Some(metadata.provider.clone());
    spec.model = Some(metadata.model.clone());
    let created = json!({"session_id":session_id,"command_id":command});
    let worker_generation =
        connection
            .control(&session_id, deadline)
            .await
            .map_err(|mut error| {
                error.result = created.clone();
                error
            })?;
    let response = connection
        .request(
            RequestBody::HeadlessRunStart {
                command_id: CommandId::new(command),
                session_id: session_id.clone(),
                worker_generation,
                text: prompt,
                attachments: Vec::new(),
                trust_hooks: false,
                spec: HeadlessRunSpecV1 {
                    cwd: metadata.cwd,
                    provider: metadata.provider,
                    model: metadata.model,
                    max_output_tokens: metadata.max_tokens,
                    effort: metadata.effort,
                    fast: metadata.fast,
                    seed: None,
                    permission_overrides: metadata.permission_overrides.unwrap_or_default(),
                    trust_hooks: false,
                    budget: Default::default(),
                    request_deadline_unix_ms: None,
                    replay_of: None,
                    continuation_of: None,
                    agent_spawn: Some(spec),
                },
            },
            deadline,
        )
        .await
        .map_err(|mut error| {
            error.result = created;
            error
        })?;
    let run_id = match response {
        ResponseBody::HeadlessRunStart { run_id, .. } => run_id,
        _ => return Err(Failure::protocol("headless.run.start response mismatch")),
    };
    let accepted = json!({"session_id":session_id,"run_id":run_id});
    let mut cursor = 0;
    let mut rejection = None;
    loop {
        let page = connection
            .read(&session_id, cursor, deadline)
            .await
            .map_err(|mut error| {
                error.result = accepted.clone();
                error
            })?;
        for envelope in page.envelopes {
            advance_cursor(&mut cursor, &envelope)?;
            if envelope.run_id.as_ref() != Some(&run_id) {
                continue;
            }
            let Ok(payload) = envelope.payload.decode_event() else {
                continue;
            };
            match payload {
                EventPayload::AgentSpawned(manifest) => {
                    let child_session_id = child_session(&manifest)?;
                    let established = json!({"session_id":session_id,"run_id":run_id,
                        "agent_id":manifest.agent,"child_session_id":child_session_id});
                    // The daemon publishes child-run identity in the child
                    // journal. Never derive it by replacing an ID prefix.
                    let mut child_cursor = 0;
                    loop {
                        let page = connection
                            .read(&child_session_id, child_cursor, deadline)
                            .await
                            .map_err(|mut error| {
                                error.result = established.clone();
                                error
                            })?;
                        for child in page.envelopes {
                            advance_cursor(&mut child_cursor, &child)?;
                            if let Some(child_run_id) = child.run_id {
                                return Ok(
                                    json!({"session_id":session_id,"run_id":run_id,"agent_id":manifest.agent,
                                    "child_session_id":child_session_id,"child_run_id":child_run_id,"manifest":manifest}),
                                );
                            }
                        }
                        pause(deadline, established.clone()).await?;
                    }
                }
                EventPayload::ToolResult { result, .. } => {
                    rejection = Some(result.reason.unwrap_or(result.preview))
                }
                EventPayload::RunState(state) if state.is_terminal() => {
                    let mut failure = Failure::new(
                        "spawn_failed",
                        rejection.unwrap_or_else(|| {
                            "coordinator terminated before spawning a child".into()
                        }),
                        70,
                    );
                    failure.result = accepted;
                    return Err(failure);
                }
                _ => {}
            }
        }
        if cursor >= page.head_seq {
            pause(deadline, accepted.clone()).await?;
        }
    }
}

fn child_session(manifest: &AgentManifest) -> Result<SessionId, Failure> {
    manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("child_session_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(SessionId::new)
        .ok_or_else(|| Failure::protocol("spawn manifest has no child session coordinate"))
}

fn advance_cursor(cursor: &mut u64, envelope: &RawEnvelope) -> Result<(), Failure> {
    if envelope.seq != cursor.saturating_add(1) {
        return Err(Failure::protocol("session journal cursor gap"));
    }
    *cursor = envelope.seq;
    Ok(())
}

async fn pause(deadline: Instant, result: Value) -> Result<(), Failure> {
    if Instant::now() >= deadline {
        return Err(Failure::timeout(result));
    }
    tokio::time::sleep_until(deadline.min(Instant::now() + OBSERVATION_POLL)).await;
    Ok(())
}

async fn wait(
    connection: &Connection,
    options: &Options,
    deadline: Instant,
) -> Result<(Value, u8), Failure> {
    let session_id = SessionId::new(&options.positional[0]);
    let agent_id = AgentId::new(&options.positional[1]);
    let mut parent_cursor = 0;
    let mut child_cursor = 0;
    let mut child_session_id = None;
    let mut child_run_id = None;
    let mut original_run_id = None;
    let mut target_frozen = false;
    let mut child_text = String::new();
    let mut terminal: Option<(RunState, u64)> = None;
    let mut report: Option<(ChildReport, u64)> = None;
    loop {
        let page = connection
            .read(&session_id, parent_cursor, deadline)
            .await
            .map_err(|mut error| {
                error.result = json!({"session_id":session_id,"agent_id":agent_id,
                "child_session_id":child_session_id,"child_run_id":child_run_id});
                error
            })?;
        for envelope in page.envelopes {
            advance_cursor(&mut parent_cursor, &envelope)?;
            let Ok(payload) = envelope.payload.decode_event() else {
                continue;
            };
            match payload {
                EventPayload::AgentSpawned(manifest) if manifest.agent == agent_id => {
                    child_session_id = Some(child_session(&manifest)?);
                }
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ChildResult { report: found },
                    ..
                }) if found.agent == agent_id => {
                    report = Some((found, envelope.seq));
                }
                _ => {}
            }
        }
        if parent_cursor < page.head_seq {
            continue;
        }
        let Some(child_session) = child_session_id.as_ref() else {
            return Err(Failure::new(
                "not_found",
                "agent is not a direct child of this session",
                70,
            ));
        };
        let page = connection
            .read(child_session, child_cursor, deadline)
            .await
            .map_err(|mut error| {
                error.result = json!({"session_id":session_id,"agent_id":agent_id,
                "child_session_id":child_session_id,"child_run_id":child_run_id});
                error
            })?;
        for envelope in page.envelopes {
            advance_cursor(&mut child_cursor, &envelope)?;
            if original_run_id.is_none() {
                original_run_id = envelope.run_id.clone();
            }
            let Ok(payload) = envelope.payload.decode_event() else {
                continue;
            };
            // Pin the newest child turn seen during the initial replay. A
            // later message cannot move this invocation's completion target.
            if !target_frozen
                && matches!(payload, EventPayload::UserMessage { .. })
                && envelope.run_id != child_run_id
            {
                child_run_id = envelope.run_id.clone();
                terminal = None;
                child_text.clear();
            }
            if child_run_id.is_none() {
                child_run_id = envelope.run_id.clone();
            }
            if envelope.run_id != child_run_id {
                continue;
            }
            match payload {
                EventPayload::RunState(state) if state.is_terminal() => {
                    terminal = Some((state, envelope.seq))
                }
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::AgentMessage { text },
                    ..
                }) => child_text = text.to_string(),
                _ => {}
            }
        }
        if child_cursor < page.head_seq {
            continue;
        }
        target_frozen = true;
        let original = child_run_id == original_run_id;
        let selected_report = if original {
            report
                .as_ref()
                .map(|(report, seq)| (report.clone(), Some(*seq)))
        } else {
            terminal.as_ref().map(|_| {
                (
                    ChildReport {
                        agent: agent_id.clone(),
                        summary: child_text.clone(),
                        verified: ReportVerification::Unverified,
                        workspace_revision: None,
                    },
                    None,
                )
            })
        };
        if let (Some((state, terminal_seq)), Some((report, child_result_seq))) =
            (&terminal, selected_report)
        {
            let (name, exit) = match state {
                RunState::Done if report.verified != ReportVerification::Red => ("done", 0),
                RunState::Cancelled => ("cancelled", 130),
                _ => ("errored", 1),
            };
            let result = json!({"session_id":session_id,"agent_id":agent_id,"child_session_id":child_session,
                "child_run_id":child_run_id,"state":name,"terminal_seq":terminal_seq,"child_result_seq":child_result_seq,
                "report_source":if original {"child_result"} else {"child_journal"},"report":report});
            if exit != 0 {
                let mut failure = Failure::new(
                    if exit == 130 {
                        "child_cancelled"
                    } else {
                        "child_failed"
                    },
                    if report.summary.is_empty() {
                        name
                    } else {
                        &report.summary
                    },
                    exit,
                );
                failure.result = result;
                return Err(failure);
            }
            return Ok((result, exit));
        }
        pause(deadline,json!({"session_id":session_id,"agent_id":agent_id,"child_session_id":child_session_id,"child_run_id":child_run_id})).await?;
    }
}

fn write_json(document: &Value) -> io::Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, document).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn write_text(text: &str) -> io::Result<()> {
    let mut output = io::stdout().lock();
    writeln!(output, "{text}")?;
    output.flush()
}
