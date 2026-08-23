//! Scriptable full provider/model library projection.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::process::ExitCode;

use haider_client::{ClientError, EnsureError, EnsureOptions, ProfileEnv, resolve_profile};
use haider_protocol::credential::{CredentialDescriptor, CredentialStatus};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, ModelDetailWire, ProviderApiFamilyWire,
    ProviderAvailabilityWire, ProviderSummaryWire, RequestBody, ResponseBody,
};
use serde::Serialize;

use super::run::{EX_IOERR, EX_PROTOCOL, EX_PROVIDER, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE};

const MODELS_SCHEMA: &str = "haider.models.v1";
const SNAPSHOT_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ModelsOptions {
    pub(crate) json: bool,
}

#[derive(Serialize)]
struct ModelsDocument {
    schema: &'static str,
    revision: u64,
    providers: Vec<ProviderView>,
}

#[derive(Serialize)]
struct ProviderView {
    provider: String,
    api_family: ProviderApiFamilyWire,
    endpoint: Option<String>,
    enabled: bool,
    availability: &'static str,
    availability_reason: Option<String>,
    auth_state: &'static str,
    has_credential: bool,
    auth_methods: Vec<haider_protocol::credential::AuthMethod>,
    default_model: Option<String>,
    models: Vec<ModelView>,
}

#[derive(Serialize)]
struct ModelView {
    model: String,
    context_window: Option<u64>,
    supported_efforts: Vec<String>,
    default_effort: Option<String>,
    supported_speeds: Vec<String>,
    supports_thinking_type: Option<bool>,
}

#[derive(Debug)]
enum ModelsError {
    Ensure(EnsureError),
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
    SnapshotChanged,
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc {
                code,
                message,
                retryable,
            } => write!(
                formatter,
                "daemon rejected model listing ({code}, retryable={retryable}): {message}"
            ),
            Self::Protocol(message) => write!(formatter, "{message}"),
            Self::SnapshotChanged => write!(
                formatter,
                "provider/account management changed during model listing; retry the command"
            ),
        }
    }
}

pub(crate) async fn models_command(rest: &[String]) -> ExitCode {
    let options = match parse_options(rest) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("usage: haider models [--json]");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("haider models: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider models: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let mut ensure = EnsureOptions::default();
    ensure.required_features = BTreeSet::from([haider_rpc::FEATURE_MODELS_LIST_V1.to_owned()]);
    ensure.client = haider_client::ClientConfig {
        client_name: "haider-models".into(),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View]),
        ..ensure.client
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(&ModelsError::Ensure(error)),
    };
    let result = read_document(&ensured.client).await;
    ensured.client.close();
    let document = match result {
        Ok(document) => document,
        Err(error) => return failure(&error),
    };
    if options.json {
        write_json(&document)
    } else {
        write_human(&document)
    }
}

pub(crate) fn parse_options(rest: &[String]) -> Result<Option<ModelsOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(None);
    }
    let mut options = ModelsOptions::default();
    for value in rest {
        match value.as_str() {
            "--json" if !options.json => options.json = true,
            "--json" => return Err("duplicate --json flag".into()),
            _ => return Err("usage: haider models [--json]".into()),
        }
    }
    Ok(Some(options))
}

async fn read_document(client: &haider_client::RpcClient) -> Result<ModelsDocument, ModelsError> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let (descriptors, account_revision) = account_snapshot(client).await?;
        let (providers, revision) = provider_snapshot(client).await?;
        if account_revision.is_some_and(|account_revision| account_revision != revision) {
            continue;
        }
        let providers = providers
            .into_iter()
            .map(|provider| provider_view(provider, &descriptors))
            .collect();
        return Ok(ModelsDocument {
            schema: MODELS_SCHEMA,
            revision,
            providers,
        });
    }
    Err(ModelsError::SnapshotChanged)
}

async fn account_snapshot(
    client: &haider_client::RpcClient,
) -> Result<
    (
        Vec<haider_protocol::credential::CredentialDescriptor>,
        Option<u64>,
    ),
    ModelsError,
> {
    match client
        .request(RequestBody::AccountList { provider: None })
        .await
        .map_err(ModelsError::Client)?
    {
        ResponseBody::AccountList {
            descriptors,
            revision,
            ..
        } => Ok((descriptors, revision)),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(ModelsError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(ModelsError::Protocol(
            "account.list response method mismatch",
        )),
    }
}

async fn provider_snapshot(
    client: &haider_client::RpcClient,
) -> Result<(Vec<ProviderSummaryWire>, u64), ModelsError> {
    match client
        .request(RequestBody::ProviderList { provider: None })
        .await
        .map_err(ModelsError::Client)?
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
            ..
        } => Err(ModelsError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(ModelsError::Protocol(
            "provider.list response method mismatch",
        )),
    }
}

fn provider_view(
    provider: ProviderSummaryWire,
    descriptors: &[CredentialDescriptor],
) -> ProviderView {
    let auth_state = auth_state(&provider, descriptors);
    let has_credential = descriptors
        .iter()
        .any(|descriptor| descriptor.provider == provider.provider);
    let mut details = provider
        .model_details
        .into_iter()
        .map(|detail| (detail.name.clone(), detail))
        .collect::<BTreeMap<_, _>>();
    let mut models = provider
        .models
        .into_iter()
        .map(|model| {
            details
                .remove(&model)
                .map_or_else(|| model_view(model), model_detail_view)
        })
        .collect::<Vec<_>>();
    models.extend(details.into_values().map(model_detail_view));
    ProviderView {
        provider: provider.provider,
        api_family: provider.api_family,
        endpoint: provider.endpoint,
        enabled: provider.enabled,
        availability: availability_name(provider.availability),
        availability_reason: provider.availability_reason,
        auth_state,
        has_credential,
        auth_methods: provider.auth_methods,
        default_model: provider.default_model,
        models,
    }
}

pub(crate) fn auth_state(
    provider: &ProviderSummaryWire,
    descriptors: &[CredentialDescriptor],
) -> &'static str {
    if provider.auth_methods.is_empty() {
        return match provider.api_family {
            ProviderApiFamilyWire::Unknown => "unknown",
            _ => "not_required",
        };
    }
    let mut matching = descriptors
        .iter()
        .filter(|descriptor| descriptor.provider == provider.provider);
    let active = matching.clone().find(|descriptor| descriptor.active);
    match active.map(|descriptor| &descriptor.status) {
        Some(CredentialStatus::Ok) => "authenticated",
        Some(CredentialStatus::Limited { .. }) => "limited",
        Some(CredentialStatus::Expired) => "expired",
        Some(CredentialStatus::Revoked) => "revoked",
        Some(CredentialStatus::NeedsAttention { .. }) => "needs_attention",
        None if matching.next().is_some() => "inactive",
        None => "missing",
    }
}

fn model_view(model: String) -> ModelView {
    ModelView {
        model,
        context_window: None,
        supported_efforts: Vec::new(),
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
    }
}

fn model_detail_view(detail: ModelDetailWire) -> ModelView {
    ModelView {
        model: detail.name,
        context_window: detail.context_window,
        supported_efforts: detail.supported_efforts,
        default_effort: detail.default_effort,
        supported_speeds: detail.supported_speeds,
        supports_thinking_type: detail.supports_thinking_type,
    }
}

pub(crate) fn availability_name(availability: ProviderAvailabilityWire) -> &'static str {
    match availability {
        ProviderAvailabilityWire::Available => "available",
        ProviderAvailabilityWire::Unavailable => "unavailable",
        ProviderAvailabilityWire::Unknown => "unknown",
        _ => "unknown",
    }
}

fn write_json(document: &ModelsDocument) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, document)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider models: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn write_human(document: &ModelsDocument) -> ExitCode {
    let mut text = String::new();
    for provider in &document.providers {
        let credential = if provider.has_credential { "yes" } else { "no" };
        text.push_str(&format!(
            "{}  availability={}  auth_state={}  credential={}  default={}\n",
            provider.provider,
            provider.availability,
            provider.auth_state,
            credential,
            provider.default_model.as_deref().unwrap_or("-")
        ));
        for model in &provider.models {
            let context = model
                .context_window
                .map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string());
            text.push_str(&format!("  {}  context_window={}\n", model.model, context));
        }
    }
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
    {
        eprintln!("haider models: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn failure(error: &ModelsError) -> ExitCode {
    eprintln!("haider models: {error}");
    let code = match error {
        ModelsError::Ensure(
            EnsureError::ProtocolMismatch(_)
            | EnsureError::MissingFeatures { .. }
            | EnsureError::ProfileMismatch { .. },
        )
        | ModelsError::Protocol(_) => EX_PROTOCOL,
        ModelsError::Ensure(_) => EX_UNAVAILABLE,
        ModelsError::Client(ClientError::Disconnected(_)) => EX_IOERR,
        ModelsError::Client(ClientError::Encode(_)) | ModelsError::SnapshotChanged => EX_SOFTWARE,
        ModelsError::Rpc { code, .. }
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
        ModelsError::Rpc { .. } => EX_SOFTWARE,
    };
    ExitCode::from(code)
}
