//! Headless durable session configuration read/write door.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::process::ExitCode;

use haider_client::{ClientError, EnsureError, EnsureOptions, ProfileEnv, resolve_profile};
use haider_protocol::agent::AgentMetricsSnapshot;
use haider_protocol::context::ContextFootprintTruth;
use haider_protocol::ids::SessionId;
use haider_rpc::{
    AttachMode, AttachmentId, Capability, CapabilitySet, ClientKind, CommandId, ModelDetailWire,
    ObserveRunStateWire, ProviderSummaryWire, RequestBody, ResponseBody, SessionObserveDigest,
};
use serde::Serialize;

use super::run::{
    EX_BLOCKED, EX_IOERR, EX_PROTOCOL, EX_PROVIDER, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE,
};

const SESSION_CONFIG_SCHEMA: &str = "haider.session_config.v1";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConfigOptions {
    pub(crate) json: bool,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) fast: Option<bool>,
    pub(crate) account: Option<String>,
    /// W-flow inline identity: `Some("none")` clears; any other id binds.
    pub(crate) agent_type: Option<String>,
    /// rev933b finding 5: a warmed session's effort/speed change invalidates
    /// the provider cache prefix, and the daemon refuses it without explicit
    /// consent. This flag IS that consent for headless callers.
    pub(crate) confirm_epoch: bool,
}

impl ConfigOptions {
    pub(crate) fn mutates(&self) -> bool {
        self.model.is_some()
            || self.effort.is_some()
            || self.fast.is_some()
            || self.account.is_some()
            || self.agent_type.is_some()
    }

    pub(crate) fn required_features(&self) -> BTreeSet<String> {
        let mut features = BTreeSet::from([
            haider_rpc::FEATURE_SESSION_CONFIG_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_OBSERVE_V1.to_owned(),
        ]);
        if self.model.is_some() {
            features.insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
        }
        if self.effort.is_some() {
            features.insert(haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1.to_owned());
        }
        if self.fast.is_some() {
            features.insert(haider_rpc::FEATURE_SESSION_FAST_SELECT_V1.to_owned());
        }
        if self.agent_type.is_some() {
            features.insert(haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned());
        }
        features
    }
}

#[derive(Serialize)]
struct SessionConfigDocument {
    schema: &'static str,
    session_id: String,
    title: String,
    run_state: &'static str,
    provider: String,
    model: String,
    effort: Option<String>,
    speed: &'static str,
    fast: bool,
    account_alias: Option<String>,
    agent_type: Option<String>,
    context_window: Option<u64>,
    workspace_cwd: String,
    max_tokens: u64,
    created_at_ms: u64,
    head_seq: u64,
    worker_generation: u64,
    turn_count: Option<u64>,
    footprint: Option<FootprintView>,
    subagent_count: usize,
    agent_metrics: Option<AgentMetricsSnapshot>,
    updated_at_ms: u64,
}

#[derive(Serialize)]
struct FootprintView {
    truth: &'static str,
    tokens: u64,
}

#[derive(Debug)]
pub(crate) enum ConfigError {
    Ensure(EnsureError),
    MissingFeatures(BTreeSet<String>),
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
    MissingMetadata,
    InvalidSelector(String),
    AccountSelectionUnsupported,
    /// rev933b finding 6: setters apply sequentially and are individually
    /// durable — a mid-sequence failure must DISCLOSE what already
    /// committed instead of reporting a clean failure.
    Partial {
        applied: Vec<&'static str>,
        error: Box<ConfigError>,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::MissingFeatures(features) => write!(
                formatter,
                "missing_feature: daemon does not advertise {}",
                features.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc {
                code,
                message,
                retryable,
            } => write!(
                formatter,
                "daemon rejected session config ({code}, retryable={retryable}): {message}"
            ),
            Self::Protocol(message) => write!(formatter, "{message}"),
            Self::MissingMetadata => write!(
                formatter,
                "session has no typed configuration metadata; it may have been created by an older daemon"
            ),
            Self::InvalidSelector(message) => write!(formatter, "invalid_argument: {message}"),
            Self::AccountSelectionUnsupported => write!(
                formatter,
                "invalid_argument: per-session account selection is not implemented; use `--model provider/model` to select the provider/model pair"
            ),
            Self::Partial { applied, error } => write!(
                formatter,
                "PARTIALLY applied — committed: {}; then failed: {error}                  (run `config --json` to read the durable state)",
                applied.join(", ")
            ),
        }
    }
}

pub(crate) async fn session_config_command(session_id: &str, rest: &[String]) -> ExitCode {
    let options = match parse_options(rest) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!(
                "usage: haider session <session-id> config [--json] [--model <model|provider/model>] [--effort <level>] [--speed <fast|normal>] [--account <alias>] [--agent-type <id|none>] [--confirm-epoch]"
            );
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("haider session config: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    if options.account.is_some() {
        return failure(&ConfigError::AccountSelectionUnsupported);
    }
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider session config: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let required_features = options.required_features();
    let mut ensure = EnsureOptions::default();
    ensure.required_features.clear();
    ensure.client = haider_client::ClientConfig {
        client_name: "haider-session-config".into(),
        client_kind: ClientKind::Headless,
        capabilities: if options.mutates() {
            CapabilitySet::from([Capability::View, Capability::Control])
        } else {
            CapabilitySet::from([Capability::View])
        },
        ..ensure.client
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(&ConfigError::Ensure(error)),
    };
    let missing = required_features
        .difference(&ensured.welcome.features)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        let _ = ensured.client.close();
        return failure(&ConfigError::MissingFeatures(missing));
    }
    let json = options.json;
    let result = execute(&ensured.client, SessionId::new(session_id), options).await;
    let _ = ensured.client.close();
    match result {
        Ok(document) if document.schema == SESSION_CONFIG_SCHEMA && json => {
            write_document(&document)
        }
        Ok(document) if document.schema == SESSION_CONFIG_SCHEMA => write_human(&document),
        Ok(_) => failure(&ConfigError::Protocol(
            "session config document schema mismatch",
        )),
        Err(error) => failure(&error),
    }
}

pub(crate) fn parse_options(rest: &[String]) -> Result<Option<ConfigOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(None);
    }
    let mut options = ConfigOptions::default();
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--json" if !options.json => options.json = true,
            "--json" => return Err("duplicate --json flag".into()),
            "--model" if options.model.is_none() => {
                index += 1;
                options.model = Some(required_value(rest, index, "--model", "a model id")?);
            }
            "--model" => return Err("duplicate --model flag".into()),
            "--effort" if options.effort.is_none() => {
                index += 1;
                options.effort = Some(required_value(rest, index, "--effort", "a level")?);
            }
            "--effort" => return Err("duplicate --effort flag".into()),
            "--speed" if options.fast.is_none() => {
                index += 1;
                let speed = required_value(rest, index, "--speed", "fast|normal")?;
                options.fast = Some(match speed.as_str() {
                    "fast" => true,
                    "normal" => false,
                    _ => return Err("--speed requires fast|normal".into()),
                });
            }
            "--speed" => return Err("duplicate --speed flag".into()),
            "--account" if options.account.is_none() => {
                index += 1;
                options.account = Some(required_value(rest, index, "--account", "an alias")?);
            }
            "--account" => return Err("duplicate --account flag".into()),
            "--agent-type" if options.agent_type.is_none() => {
                index += 1;
                options.agent_type = Some(required_value(
                    rest,
                    index,
                    "--agent-type",
                    "an id or none",
                )?);
            }
            "--agent-type" => return Err("duplicate --agent-type flag".into()),
            "--confirm-epoch" if !options.confirm_epoch => options.confirm_epoch = true,
            "--confirm-epoch" => return Err("duplicate --confirm-epoch flag".into()),
            other => return Err(format!("unknown flag `{other}`")),
        }
        index += 1;
    }
    Ok(Some(options))
}

fn required_value(
    rest: &[String],
    index: usize,
    flag: &str,
    expected: &str,
) -> Result<String, String> {
    rest.get(index)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{flag} requires {expected}"))
}

async fn execute(
    client: &haider_client::RpcClient,
    session_id: SessionId,
    options: ConfigOptions,
) -> Result<SessionConfigDocument, ConfigError> {
    // #13: one direct per-session observe replaces the session.list page
    // scan (256 summaries paged linearly to find ONE session). The digest
    // carries the same roster-truth fields from the same truth functions.
    let mut digest = session_digest(client, session_id.clone()).await?;
    let providers = provider_summaries(client).await?;
    if options.mutates() {
        let (attachment_id, mut worker_generation) =
            control_attachment(client, session_id.clone(), digest.head_seq).await?;
        let mut applied = Vec::new();
        let mutation = apply_mutations(
            client,
            &session_id,
            &providers,
            &options,
            &mut worker_generation,
            &mut applied,
        )
        .await;
        detach(client, attachment_id).await;
        mutation.map_err(|error| {
            if applied.is_empty() {
                error
            } else {
                ConfigError::Partial {
                    applied,
                    error: Box::new(error),
                }
            }
        })?;
        digest = session_digest(client, session_id).await?;
    }
    document(digest, &providers)
}

async fn apply_mutations(
    client: &haider_client::RpcClient,
    session_id: &SessionId,
    providers: &[ProviderSummaryWire],
    options: &ConfigOptions,
    worker_generation: &mut u64,
    applied: &mut Vec<&'static str>,
) -> Result<(), ConfigError> {
    if options.account.is_some() {
        return Err(ConfigError::AccountSelectionUnsupported);
    }
    if let Some(selector) = options.model.as_deref() {
        let (provider, model) = resolve_model_selector(selector, providers)?;
        let response = client
            .request(RequestBody::SessionSelectModel {
                command_id: CommandId::new(command_id("session-config-model")),
                session_id: session_id.clone(),
                worker_generation: *worker_generation,
                model,
                provider,
                confirm_new_epoch: options.confirm_epoch,
            })
            .await
            .map_err(ConfigError::Client)?;
        *worker_generation = selected_generation(
            response,
            session_id,
            SelectionKind::Model,
            "session.select_model response method mismatch",
        )?;
        applied.push("model");
    }
    if let Some(effort) = options.effort.as_ref() {
        let response = client
            .request(RequestBody::SessionSelectEffort {
                command_id: CommandId::new(command_id("session-config-effort")),
                session_id: session_id.clone(),
                worker_generation: *worker_generation,
                effort: Some(effort.clone()),
                confirm_new_epoch: options.confirm_epoch,
            })
            .await
            .map_err(ConfigError::Client)?;
        *worker_generation = selected_generation(
            response,
            session_id,
            SelectionKind::Effort,
            "session.select_effort response method mismatch",
        )?;
        applied.push("effort");
    }
    if let Some(selector) = options.agent_type.as_ref() {
        // `none` is the spoken revert — a session goes back to plain.
        let agent_type = (selector != "none").then(|| selector.clone());
        let response = client
            .request(RequestBody::SessionSelectAgentType {
                command_id: CommandId::new(command_id("session-config-agent-type")),
                session_id: session_id.clone(),
                worker_generation: *worker_generation,
                agent_type,
            })
            .await
            .map_err(ConfigError::Client)?;
        *worker_generation = selected_generation(
            response,
            session_id,
            SelectionKind::AgentType,
            "session.select_agent_type response method mismatch",
        )?;
        applied.push("agent-type");
    }
    if let Some(enabled) = options.fast {
        let response = client
            .request(RequestBody::SessionSelectFast {
                command_id: CommandId::new(command_id("session-config-speed")),
                session_id: session_id.clone(),
                worker_generation: *worker_generation,
                enabled,
                confirm_new_epoch: options.confirm_epoch,
            })
            .await
            .map_err(ConfigError::Client)?;
        *worker_generation = selected_generation(
            response,
            session_id,
            SelectionKind::Fast,
            "session.select_fast response method mismatch",
        )?;
        applied.push("speed");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SelectionKind {
    Model,
    Effort,
    Fast,
    AgentType,
}

fn selected_generation(
    response: ResponseBody,
    expected_session: &SessionId,
    kind: SelectionKind,
    mismatch: &'static str,
) -> Result<u64, ConfigError> {
    match (kind, response) {
        (
            SelectionKind::Model,
            ResponseBody::SessionSelectModel {
                session_id,
                worker_generation,
                ..
            },
        )
        | (
            SelectionKind::Effort,
            ResponseBody::SessionSelectEffort {
                session_id,
                worker_generation,
                ..
            },
        )
        | (
            SelectionKind::Fast,
            ResponseBody::SessionSelectFast {
                session_id,
                worker_generation,
                ..
            },
        )
        | (
            SelectionKind::AgentType,
            ResponseBody::SessionSelectAgentType {
                session_id,
                worker_generation,
                ..
            },
        ) if &session_id == expected_session => Ok(worker_generation),
        (
            _,
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            },
        ) => Err(ConfigError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(ConfigError::Protocol(mismatch)),
    }
}

pub(crate) fn resolve_model_selector(
    selector: &str,
    providers: &[ProviderSummaryWire],
) -> Result<(Option<String>, String), ConfigError> {
    if let Some((candidate, model)) = selector.split_once('/')
        && providers
            .iter()
            .any(|provider| provider.provider == candidate)
    {
        if model.is_empty() {
            return Err(ConfigError::InvalidSelector(
                "provider/model selector has an empty model".into(),
            ));
        }
        return Ok((Some(candidate.to_owned()), model.to_owned()));
    }
    Ok((None, selector.to_owned()))
}

async fn session_digest(
    client: &haider_client::RpcClient,
    session_id: SessionId,
) -> Result<SessionObserveDigest, ConfigError> {
    let named = session_id.as_str().to_owned();
    match client
        .request(RequestBody::SessionObserve {
            session_id,
            last_event_limit: 0,
            metadata_only: false,
        })
        .await
        .map_err(ConfigError::Client)?
    {
        ResponseBody::SessionObserve { digest } => Ok(digest),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(ConfigError::Rpc {
            // Keep the pre-#13 CLI wording, which named the session id.
            message: if code == haider_rpc::ERROR_CODE_NOT_FOUND {
                format!("session `{named}` was not found")
            } else {
                message
            },
            code,
            retryable,
        }),
        _ => Err(ConfigError::Protocol(
            "session.observe response method mismatch",
        )),
    }
}

async fn provider_summaries(
    client: &haider_client::RpcClient,
) -> Result<Vec<ProviderSummaryWire>, ConfigError> {
    match client
        .request(RequestBody::ProviderList { provider: None })
        .await
        .map_err(ConfigError::Client)?
    {
        ResponseBody::ProviderList { providers, .. } => Ok(providers),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(ConfigError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(ConfigError::Protocol(
            "provider.list response method mismatch",
        )),
    }
}

async fn control_attachment(
    client: &haider_client::RpcClient,
    session_id: SessionId,
    after_seq: u64,
) -> Result<(AttachmentId, u64), ConfigError> {
    match client
        .request(RequestBody::SessionAttach {
            session_id,
            after_seq,
            mode: AttachMode::Control,
            sealed_replay: false,
        })
        .await
        .map_err(ConfigError::Client)?
    {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } => Ok((attachment_id, attach_state.worker_generation)),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(ConfigError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(ConfigError::Protocol(
            "session.attach response method mismatch",
        )),
    }
}

async fn detach(client: &haider_client::RpcClient, attachment_id: AttachmentId) {
    let _ = client
        .request(RequestBody::SessionDetach { attachment_id })
        .await;
}

fn document(
    digest: SessionObserveDigest,
    providers: &[ProviderSummaryWire],
) -> Result<SessionConfigDocument, ConfigError> {
    let metadata = digest.metadata.ok_or(ConfigError::MissingMetadata)?;
    let context_window = providers
        .iter()
        .find(|provider| provider.provider == metadata.provider)
        .and_then(|provider| {
            provider
                .model_details
                .iter()
                .find(|detail| detail.name == metadata.model)
        })
        .and_then(|detail: &ModelDetailWire| detail.context_window);
    let footprint = digest
        .latest_context_footprint
        .map(|footprint| FootprintView {
            truth: match footprint.truth {
                ContextFootprintTruth::Exact => "exact",
                ContextFootprintTruth::Estimated => "estimated",
            },
            tokens: footprint.used_tokens,
        });
    Ok(SessionConfigDocument {
        schema: SESSION_CONFIG_SCHEMA,
        session_id: digest.session_id.as_str().to_owned(),
        title: digest.title,
        run_state: run_state_name(digest.run_state),
        provider: metadata.provider,
        model: metadata.model,
        effort: metadata.effort,
        speed: if metadata.fast { "fast" } else { "normal" },
        fast: metadata.fast,
        account_alias: None,
        agent_type: metadata.agent_type,
        context_window,
        workspace_cwd: metadata.cwd,
        max_tokens: metadata.max_tokens,
        created_at_ms: metadata.created_at_ms,
        head_seq: digest.head_seq,
        worker_generation: digest.worker_generation,
        turn_count: digest.turn_count,
        footprint,
        subagent_count: digest.subagents.len(),
        agent_metrics: digest.agent_metrics,
        updated_at_ms: digest.updated_at_ms,
    })
}

fn run_state_name(state: ObserveRunStateWire) -> &'static str {
    match state {
        ObserveRunStateWire::Idle => "idle",
        ObserveRunStateWire::Running => "running",
        ObserveRunStateWire::WaitingForRoute => "waiting_for_route",
        ObserveRunStateWire::EffectUnknown => "effect_unknown",
        ObserveRunStateWire::ParkedPermission => "parked_permission",
        ObserveRunStateWire::ParkedInput => "parked_input",
        ObserveRunStateWire::Errored => "errored",
        ObserveRunStateWire::Cancelled => "cancelled",
        ObserveRunStateWire::Unknown => "unknown",
        _ => "unknown",
    }
}

fn write_document(document: &SessionConfigDocument) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, document)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider session config: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn write_human(document: &SessionConfigDocument) -> ExitCode {
    let footprint = document.footprint.as_ref().map_or_else(
        || "unknown".to_owned(),
        |footprint| format!("{}:{}", footprint.truth, footprint.tokens),
    );
    let account = document.account_alias.as_deref().unwrap_or("unbound");
    let context_window = document
        .context_window
        .map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string());
    let text = format!(
        "{} — {}\nstate: {}\nprovider/model: {}/{}\neffort: {}\nspeed: {}\naccount: {}\ncontext_window: {}\nfootprint: {}\nworkspace: {}\n",
        document.session_id,
        document.title.replace('\n', " "),
        document.run_state,
        document.provider,
        document.model,
        document.effort.as_deref().unwrap_or("provider default"),
        document.speed,
        account,
        context_window,
        footprint,
        document.workspace_cwd,
    );
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
    {
        eprintln!("haider session config: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn failure(error: &ConfigError) -> ExitCode {
    eprintln!("haider session config: {error}");
    failure_code(error)
}

fn failure_code(error: &ConfigError) -> ExitCode {
    let code = match error {
        ConfigError::Ensure(
            EnsureError::ProtocolMismatch(_)
            | EnsureError::MissingFeatures { .. }
            | EnsureError::ProfileMismatch { .. },
        )
        | ConfigError::MissingFeatures(_)
        | ConfigError::AccountSelectionUnsupported
        | ConfigError::Protocol(_)
        | ConfigError::InvalidSelector(_) => EX_PROTOCOL,
        ConfigError::Ensure(_) => EX_UNAVAILABLE,
        ConfigError::Client(ClientError::Disconnected(_)) => EX_IOERR,
        ConfigError::Client(ClientError::Encode(_) | ClientError::MissingFeature(_))
        | ConfigError::MissingMetadata => EX_SOFTWARE,
        ConfigError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "provider_error"
                    | "provider_timeout"
                    | "credential_missing"
                    | "credential_limited"
                    | "unauthorized"
            ) =>
        {
            EX_PROVIDER
        }
        ConfigError::Rpc { code, .. }
            if matches!(code.as_str(), "permission_denied" | "input_required") =>
        {
            EX_BLOCKED
        }
        ConfigError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "protocol_mismatch" | "unknown_method" | "invalid_argument"
            ) =>
        {
            EX_PROTOCOL
        }
        ConfigError::Rpc { .. } => EX_SOFTWARE,
        ConfigError::Partial { error, .. } => return failure_code(error),
    };
    ExitCode::from(code)
}

fn command_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    )
}
