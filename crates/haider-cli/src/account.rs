//! Scriptable account inventory and guarded durable removal.

use std::collections::BTreeSet;
use std::future::Future;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use haider_client::{ClientError, EnsureError, EnsureOptions, ProfileEnv, resolve_profile};
use haider_protocol::credential::{AuthMethod, CredentialDescriptor};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, CommandId, ErrorData, ProviderApiFamilyWire,
    ProviderAuthRequirementWire, ProviderProbeFailureWire, ProviderSummaryWire, ProviderTrustWire,
    RequestBody, ResponseBody, SecretWire, SnapshotAvailabilityWire, StagePurpose,
};
use serde::Serialize;
use zeroize::Zeroizing;

use super::run::{EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE};

const ACCOUNTS_SCHEMA: &str = "haider.accounts.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountCommand {
    List { json: bool },
    Import { source: String, confirm: bool },
    Refresh { alias: String },
    Remove { alias: String, confirm: bool },
    Add(CustomAccountOptions),
    Probe { alias: String, json: bool },
    Update(CustomAccountOptions),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SecretInput {
    // Use the transport's redacted, zeroize-on-drop wrapper immediately
    // after parsing. The original process argv cannot be made secret, but
    // the CLI must not retain an additional ordinary `String` copy.
    Direct(SecretWire),
    Environment(String),
    Stdin,
    NoAuth,
}

impl std::fmt::Debug for SecretInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct(_) => formatter.write_str("Direct([REDACTED])"),
            Self::Environment(name) => formatter.debug_tuple("Environment").field(name).finish(),
            Self::Stdin => formatter.write_str("Stdin"),
            Self::NoAuth => formatter.write_str("NoAuth"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CustomAccountOptions {
    alias: String,
    base_url: Option<String>,
    secret: Option<SecretInput>,
    api_family: Option<ProviderApiFamilyWire>,
    response_open_timeout_ms: Option<u64>,
    chunk_idle_timeout_ms: Option<u64>,
    semantic_progress_timeout_ms: Option<u64>,
    trust: Option<ProviderTrustWire>,
    json: bool,
}

#[derive(Serialize)]
struct AccountsDocument {
    schema: &'static str,
    accounts: Vec<AccountView>,
}

/// Deliberately narrower than `CredentialDescriptor`: adding a public field to
/// the RPC descriptor cannot silently widen either CLI output format.
#[derive(Serialize)]
struct AccountView {
    alias: String,
    provider: String,
    auth_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<haider_protocol::credential::AccountIdentity>,
    created: Option<u64>,
}

#[derive(Serialize)]
struct CustomAccountDocument {
    schema: &'static str,
    operation: &'static str,
    alias: String,
    base_url: Option<String>,
    api_family: &'static str,
    auth_state: &'static str,
    reachable: bool,
    latency_ms: u64,
    model_count: usize,
    models: Vec<String>,
}

#[derive(Serialize)]
struct AccountErrorDocument<'a> {
    schema: &'static str,
    code: &'a str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<&'static str>,
}

#[derive(Debug)]
enum AccountError {
    Ensure(EnsureError),
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
        data: Option<ErrorData>,
    },
    Protocol(&'static str),
    SnapshotUnavailable(String),
    MissingAlias(String),
    SecretInput(String),
}

trait AccountClient {
    fn request(
        &self,
        request: RequestBody,
    ) -> impl Future<Output = Result<ResponseBody, ClientError>> + Send;
}

impl AccountClient for haider_client::RpcClient {
    fn request(
        &self,
        request: RequestBody,
    ) -> impl Future<Output = Result<ResponseBody, ClientError>> + Send {
        haider_client::RpcClient::request(self, request)
    }
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc {
                code,
                message,
                retryable,
                data,
            } => {
                write!(
                    formatter,
                    "daemon rejected account command ({code}, retryable={retryable}"
                )?;
                if let Some(failure) = provider_probe_failure(data.as_ref()) {
                    write!(formatter, ", failure={failure}")?;
                }
                write!(formatter, "): {message}")
            }
            Self::Protocol(message) => write!(formatter, "{message}"),
            Self::SnapshotUnavailable(reason) => {
                write!(formatter, "account inventory is unavailable: {reason}")
            }
            Self::MissingAlias(alias) => {
                write!(formatter, "account alias `{alias}` does not exist")
            }
            Self::SecretInput(message) => write!(formatter, "{message}"),
        }
    }
}

pub(crate) fn parse_account_command(rest: &[String]) -> Result<AccountCommand, String> {
    match rest {
        [command] if command == "list" => Ok(AccountCommand::List { json: false }),
        [command, flag] if command == "list" && flag == "--json" => {
            Ok(AccountCommand::List { json: true })
        }
        [command, source]
            if command == "import" && matches!(source.as_str(), "codex" | "claude-code") =>
        {
            Ok(AccountCommand::Import {
                source: source.clone(),
                confirm: false,
            })
        }
        [command, source, flag]
            if command == "import"
                && matches!(source.as_str(), "codex" | "claude-code")
                && flag == "--confirm" =>
        {
            Ok(AccountCommand::Import {
                source: source.clone(),
                confirm: true,
            })
        }
        [command, alias]
            if command == "refresh" && !alias.is_empty() && !alias.starts_with('-') =>
        {
            Ok(AccountCommand::Refresh {
                alias: alias.clone(),
            })
        }
        [command, alias] if command == "remove" && !alias.is_empty() && !alias.starts_with('-') => {
            Ok(AccountCommand::Remove {
                alias: alias.clone(),
                confirm: false,
            })
        }
        [command, alias, flag]
            if command == "remove"
                && !alias.is_empty()
                && !alias.starts_with('-')
                && flag == "--confirm" =>
        {
            Ok(AccountCommand::Remove {
                alias: alias.clone(),
                confirm: true,
            })
        }
        [command, alias, options @ ..]
            if matches!(command.as_str(), "add" | "update")
                && !alias.is_empty()
                && !alias.starts_with('-') =>
        {
            let options = parse_custom_options(alias, options, command == "add")?;
            if command == "add" {
                Ok(AccountCommand::Add(options))
            } else {
                Ok(AccountCommand::Update(options))
            }
        }
        [command, alias] if command == "probe" && !alias.is_empty() && !alias.starts_with('-') => {
            Ok(AccountCommand::Probe {
                alias: alias.clone(),
                json: false,
            })
        }
        [command, alias, flag]
            if command == "probe"
                && !alias.is_empty()
                && !alias.starts_with('-')
                && flag == "--json" =>
        {
            Ok(AccountCommand::Probe {
                alias: alias.clone(),
                json: true,
            })
        }
        [] => Err(account_usage()),
        _ => Err(account_usage()),
    }
}

fn account_usage() -> String {
    "expected list [--json], import <codex|claude-code> [--confirm], refresh <alias>, remove <alias> --confirm, add <alias> --base-url <url> [--api-key <key> | --api-key-env <VAR> | --api-key-stdin | --no-auth] [--api-family openai|anthropic] [--response-open-timeout <duration>] [--chunk-idle-timeout <duration>] [--semantic-progress-timeout <duration>] [--lockdown|--full] [--json], probe <alias> [--json], or update <alias> [mutable options] [--json]".into()
}

fn parse_custom_options(
    alias: &str,
    rest: &[String],
    create: bool,
) -> Result<CustomAccountOptions, String> {
    let mut base_url = None;
    let mut secret = None;
    let mut api_family = None;
    let mut response_open_timeout_ms = None;
    let mut chunk_idle_timeout_ms = None;
    let mut semantic_progress_timeout_ms = None;
    let mut trust = None;
    let mut json = false;
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index].as_str();
        let value = |index: &mut usize, name: &str| -> Result<&String, String> {
            *index += 1;
            rest.get(*index)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag {
            "--base-url" if base_url.is_none() => base_url = Some(value(&mut index, flag)?.clone()),
            "--api-key" if secret.is_none() => {
                secret = Some(SecretInput::Direct(SecretWire::new(
                    value(&mut index, flag)?.clone(),
                )));
            }
            "--api-key-env" if secret.is_none() => {
                let name = value(&mut index, flag)?;
                if name.contains('=') || name.chars().any(char::is_control) {
                    return Err("--api-key-env requires an environment variable name".into());
                }
                secret = Some(SecretInput::Environment(name.clone()));
            }
            "--api-key-stdin" if secret.is_none() => secret = Some(SecretInput::Stdin),
            "--no-auth" if secret.is_none() => secret = Some(SecretInput::NoAuth),
            "--api-family" if api_family.is_none() => {
                api_family = Some(match value(&mut index, flag)?.as_str() {
                    "openai" => ProviderApiFamilyWire::OpenAiChatCompletions,
                    "anthropic" => ProviderApiFamilyWire::AnthropicMessages,
                    _ => return Err("--api-family requires openai or anthropic".into()),
                });
            }
            "--response-open-timeout" if response_open_timeout_ms.is_none() => {
                response_open_timeout_ms = Some(parse_duration_ms(flag, value(&mut index, flag)?)?);
            }
            "--chunk-idle-timeout" if chunk_idle_timeout_ms.is_none() => {
                chunk_idle_timeout_ms = Some(parse_duration_ms(flag, value(&mut index, flag)?)?);
            }
            "--semantic-progress-timeout" if semantic_progress_timeout_ms.is_none() => {
                semantic_progress_timeout_ms =
                    Some(parse_duration_ms(flag, value(&mut index, flag)?)?);
            }
            "--lockdown" if trust.is_none() => trust = Some(ProviderTrustWire::Lockdown),
            "--full" if trust.is_none() => trust = Some(ProviderTrustWire::Full),
            "--json" if !json => json = true,
            _ if matches!(
                flag,
                "--api-key" | "--api-key-env" | "--api-key-stdin" | "--no-auth"
            ) =>
            {
                return Err("choose exactly one API-key source or --no-auth".into());
            }
            _ => return Err(format!("unknown or repeated account option `{flag}`")),
        }
        index += 1;
    }
    if create && base_url.is_none() {
        return Err("account add requires --base-url".into());
    }
    if create && secret.is_none() {
        return Err("account add requires an API-key source or --no-auth".into());
    }
    if !create
        && base_url.is_none()
        && secret.is_none()
        && response_open_timeout_ms.is_none()
        && chunk_idle_timeout_ms.is_none()
        && semantic_progress_timeout_ms.is_none()
    {
        return Err(
            "account update requires --base-url, a key option, or a transport timeout".into(),
        );
    }
    if !create && api_family.is_some() {
        return Err(
            "account update does not change --api-family; remove and re-add the provider".into(),
        );
    }
    if !create && trust.is_some() {
        return Err(
            "account update does not change provider trust; use `haider provider set`".into(),
        );
    }
    Ok(CustomAccountOptions {
        alias: alias.to_owned(),
        base_url,
        secret,
        api_family: api_family.or(create.then_some(ProviderApiFamilyWire::OpenAiChatCompletions)),
        response_open_timeout_ms,
        chunk_idle_timeout_ms,
        semantic_progress_timeout_ms,
        trust,
        json,
    })
}

fn parse_duration_ms(flag: &str, value: &str) -> Result<u64, String> {
    let (digits, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else {
        return Err(format!(
            "{flag} requires an integer followed by ms, s, m, or h"
        ));
    };
    let amount = digits
        .parse::<u64>()
        .map_err(|_| format!("{flag} requires a positive integer duration"))?;
    let millis = amount
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{flag} is too large"))?;
    if millis == 0 {
        return Err(format!("{flag} must be greater than zero"));
    }
    Ok(millis)
}

pub(crate) async fn account_command(rest: &[String]) -> ExitCode {
    let command = match parse_account_command(rest) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("haider account: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    // This gate intentionally runs before profile resolution, daemon startup,
    // or any RPC. An unconfirmed invocation cannot reach a mutation surface.
    if let AccountCommand::Remove {
        alias,
        confirm: false,
    } = &command
    {
        eprintln!("haider account: would remove account `{alias}`; pass --confirm to proceed");
        return ExitCode::from(EX_USAGE);
    }
    let json_errors = matches!(
        &command,
        AccountCommand::List { json: true }
            | AccountCommand::Add(CustomAccountOptions { json: true, .. })
            | AccountCommand::Probe { json: true, .. }
            | AccountCommand::Update(CustomAccountOptions { json: true, .. })
    );

    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider account: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let capabilities = match &command {
        AccountCommand::List { .. } | AccountCommand::Import { confirm: false, .. } => {
            CapabilitySet::from([Capability::View])
        }
        AccountCommand::Import { confirm: true, .. }
        | AccountCommand::Refresh { .. }
        | AccountCommand::Remove { .. }
        | AccountCommand::Add(_)
        | AccountCommand::Probe { .. }
        | AccountCommand::Update(_) => CapabilitySet::from([Capability::View, Capability::Control]),
    };
    let mut required_features =
        BTreeSet::from([haider_rpc::FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned()]);
    if matches!(
        &command,
        AccountCommand::Import { .. } | AccountCommand::Refresh { .. }
    ) {
        required_features.insert(haider_rpc::FEATURE_ACCOUNT_IDENTITY_V1.to_owned());
    }
    if matches!(&command, AccountCommand::Import { .. }) {
        required_features.insert(haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1.to_owned());
    }
    if matches!(&command, AccountCommand::Add(_) | AccountCommand::Update(_)) {
        required_features.insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
        required_features.insert(haider_rpc::FEATURE_PROVIDER_MODELS_V1.to_owned());
    }
    if matches!(&command, AccountCommand::Add(_)) {
        required_features.insert(haider_rpc::FEATURE_PROVIDER_LOCKDOWN_V1.to_owned());
    }
    if matches!(&command, AccountCommand::Probe { .. }) {
        required_features.insert(haider_rpc::FEATURE_PROVIDER_MODELS_V1.to_owned());
    }
    if matches!(
        &command,
        AccountCommand::Add(CustomAccountOptions {
            secret: Some(secret),
            ..
        }) | AccountCommand::Update(CustomAccountOptions {
            secret: Some(secret),
            ..
        }) if !matches!(secret, SecretInput::NoAuth)
    ) {
        required_features.insert(haider_rpc::FEATURE_VAULT_STAGE_V1.to_owned());
        required_features.insert(haider_rpc::FEATURE_ACCOUNT_LOGIN_API_V1.to_owned());
    }
    let ensure = EnsureOptions {
        required_features,
        client: haider_client::ClientConfig {
            client_name: "haider-account".into(),
            client_kind: ClientKind::Headless,
            capabilities,
            ..haider_client::ClientConfig::default()
        },
        ..EnsureOptions::default()
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(&AccountError::Ensure(error), json_errors),
    };
    let result = execute(&ensured.client, command).await;
    let _ = ensured.client.close();
    match result {
        Ok(output) => output,
        Err(error) => failure(&error, json_errors),
    }
}

async fn execute(
    client: &impl AccountClient,
    command: AccountCommand,
) -> Result<ExitCode, AccountError> {
    match command {
        AccountCommand::List { json } => {
            let (descriptors, _) = account_snapshot(client).await?;
            let document = AccountsDocument {
                schema: ACCOUNTS_SCHEMA,
                accounts: descriptors.into_iter().map(account_view).collect(),
            };
            Ok(if json {
                write_json(&document)
            } else {
                write_human(&document)
            })
        }
        AccountCommand::Import { source, confirm } => {
            execute_import(client, &source, confirm).await
        }
        AccountCommand::Refresh { alias } => execute_refresh(client, alias).await,
        AccountCommand::Remove {
            alias,
            confirm: true,
        } => {
            // Mirror the TUI's revision-fenced account.remove path: snapshot,
            // resolve the exact global alias, then mutate at that revision.
            let (descriptors, revision) = account_snapshot(client).await?;
            let revision = revision.ok_or(AccountError::Protocol(
                "account.list omitted the revision required for removal",
            ))?;
            if !descriptors
                .iter()
                .any(|descriptor| descriptor.alias.as_str() == alias)
            {
                return Err(AccountError::MissingAlias(alias));
            }
            match client
                .request(RequestBody::AccountRemove {
                    command_id: CommandId::new(command_id("account-remove")),
                    alias: alias.clone(),
                    expected_revision: Some(revision),
                })
                .await
                .map_err(AccountError::Client)?
            {
                ResponseBody::AccountRemove { removed_alias, .. }
                    if removed_alias.as_str() == alias =>
                {
                    println!("removed account `{removed_alias}`");
                    Ok(ExitCode::SUCCESS)
                }
                ResponseBody::AccountRemove { .. } => Err(AccountError::Protocol(
                    "account.remove response named a different alias",
                )),
                ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    data,
                } => Err(AccountError::Rpc {
                    code,
                    message,
                    retryable,
                    data,
                }),
                _ => Err(AccountError::Protocol(
                    "account.remove response method mismatch",
                )),
            }
        }
        AccountCommand::Remove { confirm: false, .. } => {
            unreachable!("unconfirmed removal is gated before daemon startup")
        }
        AccountCommand::Add(options) => execute_custom(client, options, true).await,
        AccountCommand::Update(options) => execute_custom(client, options, false).await,
        AccountCommand::Probe { alias, json } => execute_probe(client, alias, json).await,
    }
}

async fn execute_refresh(
    client: &impl AccountClient,
    alias: String,
) -> Result<ExitCode, AccountError> {
    match client
        .request(RequestBody::AccountRefresh { alias })
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::AccountRefresh { descriptor, .. } => {
            let summary = descriptor.account_identity.as_ref().map_or_else(
                || "identity unavailable".to_owned(),
                |identity| identity.summary(),
            );
            println!(
                "refreshed {} ({}) — {summary}",
                descriptor.alias, descriptor.provider
            );
            Ok(ExitCode::SUCCESS)
        }
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(AccountError::Rpc {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(AccountError::Protocol("account.refresh response mismatch")),
    }
}

async fn execute_import(
    client: &impl AccountClient,
    source: &str,
    confirm: bool,
) -> Result<ExitCode, AccountError> {
    let candidates = match client
        .request(RequestBody::AccountDeviceCandidates)
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::AccountDeviceCandidates { candidates, .. } => candidates,
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => {
            return Err(AccountError::Rpc {
                code,
                message,
                retryable,
                data,
            });
        }
        _ => {
            return Err(AccountError::Protocol(
                "account.device_candidates response mismatch",
            ));
        }
    };
    let candidate = candidates
        .into_iter()
        .find(|candidate| candidate.source == source)
        .ok_or_else(|| AccountError::MissingAlias(format!("{source} local login")))?;
    if !candidate.import_supported {
        return Err(AccountError::Protocol(
            "the discovered local login cannot be imported safely",
        ));
    }
    let summary = candidate.identity.as_ref().map_or_else(
        || {
            candidate
                .account_label
                .clone()
                .unwrap_or_else(|| "unknown account".to_owned())
        },
        haider_protocol::credential::AccountIdentity::summary,
    );
    println!("found {source} login: {summary}");
    if !confirm {
        eprintln!(
            "haider account: review the identity, then run `haider account import {source} --confirm`"
        );
        return Ok(ExitCode::from(EX_USAGE));
    }
    match client
        .request(RequestBody::AccountImportDevice {
            command_id: CommandId::new(command_id("account-import")),
            candidate: candidate.candidate,
        })
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::AccountImportDevice { descriptor, .. } => {
            println!(
                "imported {} ({}) — {}",
                descriptor.alias,
                descriptor.provider,
                descriptor.account_identity.as_ref().map_or(
                    descriptor.identity.as_str(),
                    |identity| {
                        identity
                            .email
                            .as_deref()
                            .unwrap_or(descriptor.identity.as_str())
                    }
                )
            );
            Ok(ExitCode::SUCCESS)
        }
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(AccountError::Rpc {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(AccountError::Protocol(
            "account.import_device response mismatch",
        )),
    }
}

async fn execute_custom(
    client: &impl AccountClient,
    options: CustomAccountOptions,
    create: bool,
) -> Result<ExitCode, AccountError> {
    let (providers, revision) = provider_snapshot(client).await?;
    let existing = providers
        .iter()
        .find(|provider| provider.provider == options.alias)
        .cloned();
    if create && existing.is_some() {
        return Err(AccountError::Protocol(
            "account add alias already names a provider; use account update",
        ));
    }
    if !create && existing.is_none() {
        return Err(AccountError::MissingAlias(options.alias));
    }
    let existing = existing.as_ref();
    let api_family = options
        .api_family
        .or_else(|| existing.map(|provider| provider.api_family))
        .ok_or(AccountError::Protocol("provider has no API family"))?;
    let auth_requirement = match options.secret.as_ref() {
        Some(SecretInput::NoAuth) => Some(ProviderAuthRequirementWire::None),
        Some(_) => Some(ProviderAuthRequirementWire::ApiKey),
        None => create.then_some(ProviderAuthRequirementWire::ApiKey),
    };
    let secret = match options.secret.as_ref() {
        Some(SecretInput::NoAuth) | None => None,
        Some(input) => Some(resolve_secret(input)?),
    };
    let vault_reference = if let Some(secret) = secret {
        Some(stage_secret(client, secret).await?)
    } else {
        None
    };
    let began = Instant::now();
    let response = client
        .request(RequestBody::ProviderConfigure {
            command_id: CommandId::new(command_id("provider-configure")),
            provider: options.alias.clone(),
            api_family: create.then_some(api_family),
            origin: options.base_url.clone(),
            auth_requirement,
            enabled: existing.is_none_or(|provider| provider.enabled),
            // Empty means "discover now" at this door. Every add/update
            // success therefore proves current reachability and returns the
            // live inventory instead of relabelling stale cached rows.
            models: Vec::new(),
            default_model: existing.and_then(|provider| provider.default_model.clone()),
            response_open_timeout_ms: options.response_open_timeout_ms,
            chunk_idle_timeout_ms: options.chunk_idle_timeout_ms,
            semantic_progress_timeout_ms: options.semantic_progress_timeout_ms,
            probe_vault_reference: vault_reference.clone(),
            trust: options.trust,
            expected_revision: revision,
        })
        .await
        .map_err(AccountError::Client)?;
    let (configured, configured_revision) = match response {
        ResponseBody::ProviderConfigure { provider, revision } => (provider, revision),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => {
            return Err(AccountError::Rpc {
                code,
                message,
                retryable,
                data,
            });
        }
        _ => {
            return Err(AccountError::Protocol(
                "provider.configure response method mismatch",
            ));
        }
    };
    if !create && matches!(options.secret.as_ref(), Some(SecretInput::NoAuth)) {
        let (descriptors, revision) = account_snapshot(client).await?;
        let revision = revision.unwrap_or(configured_revision);
        if descriptors
            .iter()
            .any(|descriptor| descriptor.alias.as_str() == options.alias)
        {
            match client
                .request(RequestBody::AccountRemove {
                    command_id: CommandId::new(command_id("account-auth-none")),
                    alias: options.alias.clone(),
                    expected_revision: Some(revision),
                })
                .await
                .map_err(AccountError::Client)?
            {
                ResponseBody::AccountRemove { removed_alias, .. }
                    if removed_alias.as_str() == options.alias => {}
                ResponseBody::AccountRemove { .. } => {
                    return Err(AccountError::Protocol(
                        "account.remove response named a different alias",
                    ));
                }
                ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    data,
                } => {
                    return Err(AccountError::Rpc {
                        code,
                        message,
                        retryable,
                        data,
                    });
                }
                _ => {
                    return Err(AccountError::Protocol(
                        "account.remove response method mismatch",
                    ));
                }
            }
        }
    } else if let Some(vault_reference) = vault_reference {
        match client
            .request(RequestBody::AccountLoginApi {
                command_id: CommandId::new(command_id("account-login-api")),
                provider: options.alias.clone(),
                alias: Some(options.alias.clone()),
                vault_reference,
                validation_model: configured.default_model.clone(),
                replace_existing: !create,
            })
            .await
            .map_err(AccountError::Client)?
        {
            ResponseBody::AccountLoginApi { .. } => {}
            ResponseBody::Error {
                code,
                message,
                retryable,
                data,
            } => {
                return Err(AccountError::Rpc {
                    code,
                    message,
                    retryable,
                    data,
                });
            }
            _ => {
                return Err(AccountError::Protocol(
                    "account.login_api response method mismatch",
                ));
            }
        }
    }
    let document = custom_document(
        if create { "add" } else { "update" },
        &options.alias,
        configured,
        began.elapsed(),
    );
    Ok(write_custom(&document, options.json))
}

async fn execute_probe(
    client: &impl AccountClient,
    alias: String,
    json: bool,
) -> Result<ExitCode, AccountError> {
    let began = Instant::now();
    let provider = match client
        .request(RequestBody::ProviderModelsRefresh {
            provider: alias.clone(),
        })
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::ProviderModelsRefresh { provider, .. } => provider,
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => {
            return Err(AccountError::Rpc {
                code,
                message,
                retryable,
                data,
            });
        }
        _ => {
            return Err(AccountError::Protocol(
                "provider.models_refresh response method mismatch",
            ));
        }
    };
    let document = custom_document("probe", &alias, provider, began.elapsed());
    Ok(write_custom(&document, json))
}

fn custom_document(
    operation: &'static str,
    alias: &str,
    provider: ProviderSummaryWire,
    latency: Duration,
) -> CustomAccountDocument {
    let api_family = match provider.api_family {
        ProviderApiFamilyWire::AnthropicMessages => "anthropic",
        ProviderApiFamilyWire::OpenAiChatCompletions => "openai",
        _ => "unknown",
    };
    let auth_state = if provider.auth_methods.is_empty() {
        "no_auth"
    } else {
        "authenticated"
    };
    let models = provider
        .models
        .iter()
        .map(|model| format!("{alias}/{model}"))
        .collect::<Vec<_>>();
    CustomAccountDocument {
        schema: "haider.account.custom.v1",
        operation,
        alias: alias.to_owned(),
        base_url: provider.endpoint,
        api_family,
        auth_state,
        reachable: true,
        latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
        model_count: models.len(),
        models,
    }
}

fn write_custom(document: &CustomAccountDocument, json: bool) -> ExitCode {
    if json {
        write_json(document)
    } else {
        println!(
            "{}: reachable={} latency_ms={} auth={} models={}",
            document.alias,
            document.reachable,
            document.latency_ms,
            document.auth_state,
            document.model_count
        );
        for model in &document.models {
            println!("{model}");
        }
        ExitCode::SUCCESS
    }
}

async fn stage_secret(
    client: &impl AccountClient,
    secret: SecretWire,
) -> Result<String, AccountError> {
    match client
        .request(RequestBody::VaultStage {
            stage_id: command_id("account-key-stage"),
            purpose: StagePurpose::ApiKey,
            secret,
        })
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::VaultStage {
            vault_reference, ..
        } => Ok(vault_reference),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(AccountError::Rpc {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(AccountError::Protocol(
            "vault.stage response method mismatch",
        )),
    }
}

fn resolve_secret(input: &SecretInput) -> Result<SecretWire, AccountError> {
    const MAX_SECRET_BYTES: u64 = 4_096;
    if let SecretInput::Direct(value) = input {
        if value.is_empty() || value.expose_secret().chars().any(char::is_control) {
            return Err(AccountError::SecretInput(
                "API key is empty or invalid".into(),
            ));
        }
        return Ok(value.clone());
    }
    let mut value = match input {
        SecretInput::Environment(name) => Zeroizing::new(std::env::var(name).map_err(|_| {
            AccountError::SecretInput(format!("environment variable `{name}` is not set"))
        })?),
        SecretInput::Stdin => {
            let mut value = Zeroizing::new(String::new());
            io::stdin()
                .take(MAX_SECRET_BYTES + 1)
                .read_to_string(&mut value)
                .map_err(|error| {
                    AccountError::SecretInput(format!("could not read API key from stdin: {error}"))
                })?;
            if u64::try_from(value.len()).unwrap_or(u64::MAX) > MAX_SECRET_BYTES {
                return Err(AccountError::SecretInput(
                    "API key from stdin is too large".into(),
                ));
            }
            while matches!(value.as_bytes().last(), Some(b'\r' | b'\n')) {
                value.pop();
            }
            value
        }
        SecretInput::NoAuth => {
            return Err(AccountError::Protocol(
                "internal no-auth input reached secret staging",
            ));
        }
        SecretInput::Direct(_) => unreachable!("direct secret returned before string resolution"),
    };
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(AccountError::SecretInput(
            "API key is empty or invalid".into(),
        ));
    }
    Ok(SecretWire::new(std::mem::take(&mut *value)))
}

async fn provider_snapshot(
    client: &impl AccountClient,
) -> Result<(Vec<ProviderSummaryWire>, u64), AccountError> {
    match client
        .request(RequestBody::ProviderList { provider: None })
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::ProviderList {
            providers,
            revision,
            ..
        } => Ok((providers, revision)),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(AccountError::Rpc {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(AccountError::Protocol(
            "provider.list response method mismatch",
        )),
    }
}

async fn account_snapshot(
    client: &impl AccountClient,
) -> Result<(Vec<CredentialDescriptor>, Option<u64>), AccountError> {
    match client
        .request(RequestBody::AccountList { provider: None })
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::AccountList {
            descriptors,
            revision,
            availability,
            ..
        } => match availability {
            Some(SnapshotAvailabilityWire::Unavailable { reason }) => {
                Err(AccountError::SnapshotUnavailable(reason))
            }
            Some(SnapshotAvailabilityWire::Unknown) => Err(AccountError::SnapshotUnavailable(
                "daemon returned an unknown availability state".into(),
            )),
            _ => Ok((descriptors, revision)),
        },
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(AccountError::Rpc {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(AccountError::Protocol(
            "account.list response method mismatch",
        )),
    }
}

fn account_view(descriptor: CredentialDescriptor) -> AccountView {
    AccountView {
        alias: descriptor.alias.as_str().to_owned(),
        provider: descriptor.provider,
        auth_kind: match descriptor.auth_method {
            AuthMethod::ApiKey => "api_key",
            AuthMethod::OAuth => "oauth",
        },
        identity: descriptor.account_identity,
        created: descriptor.created_at_ms,
    }
}

fn write_json(document: &impl Serialize) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, document)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider account: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn write_human(document: &AccountsDocument) -> ExitCode {
    let mut text = String::new();
    for account in &document.accounts {
        text.push_str(&format!(
            "{}  provider={}  identity={}  created={}\n",
            account.alias,
            account.provider,
            account
                .identity
                .as_ref()
                .map_or_else(|| "unknown".to_owned(), |identity| identity.summary()),
            account.created.map_or_else(
                || "unknown (added before 0.0.964)".to_owned(),
                |created| created.to_string(),
            )
        ));
    }
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
    {
        eprintln!("haider account: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn command_id(operation: &str) -> String {
    format!(
        "{operation}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    )
}

fn provider_probe_failure(data: Option<&ErrorData>) -> Option<&'static str> {
    let ErrorData::ProviderProbeFailed { failure, .. } = data? else {
        return None;
    };
    Some(match failure {
        ProviderProbeFailureWire::Unreachable => "unreachable",
        ProviderProbeFailureWire::Unauthorized => "unauthorized",
        ProviderProbeFailureWire::NonOpenAiCompatibleBody => "non_open_ai_compatible_body",
        ProviderProbeFailureWire::EmptyList => "empty_list",
        ProviderProbeFailureWire::Unavailable => "unavailable",
        ProviderProbeFailureWire::Unknown => "unknown",
        _ => "unknown",
    })
}

fn failure(error: &AccountError, json: bool) -> ExitCode {
    if json {
        let (code, retryable, failure) = match error {
            AccountError::Rpc {
                code,
                retryable,
                data,
                ..
            } => (
                code.as_str(),
                *retryable,
                provider_probe_failure(data.as_ref()),
            ),
            AccountError::Ensure(_) => ("daemon_unavailable", true, None),
            AccountError::Client(_) => ("client_error", true, None),
            AccountError::Protocol(_) => ("protocol_error", false, None),
            AccountError::SnapshotUnavailable(_) => ("snapshot_unavailable", true, None),
            AccountError::MissingAlias(_) => ("not_found", false, None),
            AccountError::SecretInput(_) => ("invalid_secret_input", false, None),
        };
        let _ = write_json(&AccountErrorDocument {
            schema: "haider.account.error.v1",
            code,
            message: error.to_string(),
            retryable,
            failure,
        });
    } else {
        eprintln!("haider account: {error}");
    }
    let code = match error {
        AccountError::Ensure(
            EnsureError::ProtocolMismatch(_)
            | EnsureError::MissingFeatures { .. }
            | EnsureError::ProfileMismatch { .. },
        )
        | AccountError::Protocol(_) => EX_PROTOCOL,
        AccountError::Ensure(_) | AccountError::SnapshotUnavailable(_) => EX_UNAVAILABLE,
        AccountError::Client(ClientError::Disconnected(_)) => EX_IOERR,
        AccountError::Client(ClientError::Encode(_))
        | AccountError::Rpc { .. }
        | AccountError::MissingAlias(_)
        | AccountError::SecretInput(_) => EX_SOFTWARE,
    };
    ExitCode::from(code)
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "account_custom_tests.rs"]
mod account_custom_tests;
