//! Scriptable full provider/model library projection.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelsOptions {
    pub(crate) json: bool,
    pub(crate) refresh: Option<ModelsRefresh>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelsRefresh {
    All,
    Provider(String),
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
    #[serde(rename = "fetched_at")]
    fetched_at_ms: Option<u64>,
    #[serde(rename = "inventory_age")]
    inventory_age_ms: Option<u64>,
    inventory: haider_rpc::ModelInventoryWire,
    catalog: haider_rpc::ProviderCatalogKindWire,
    models: Vec<ModelView>,
}

#[derive(Serialize)]
struct ModelView {
    model: String,
    context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<haider_rpc::ModelDetailSourceWire>,
    max_output_tokens: Option<u64>,
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
            println!("usage: haider models [--json] [--refresh [<alias>]]");
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
    ensure.required_features = BTreeSet::from([
        haider_rpc::FEATURE_MODELS_LIST_V1.to_owned(),
        haider_rpc::FEATURE_PROVIDER_MODELS_V1.to_owned(),
    ]);
    // A nominal read may discover a per-provider TTL expiry after connecting,
    // so the one connection negotiates Control up front. It still performs
    // no mutation unless a cached inventory is stale or refresh was explicit.
    ensure.client = haider_client::ClientConfig {
        client_name: "haider-models".into(),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        ..ensure.client
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(&ModelsError::Ensure(error)),
    };
    let result = read_document_with_refresh(&ensured.client, options.refresh.as_ref()).await;
    let _ = ensured.client.close();
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
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--json" if !options.json => options.json = true,
            "--json" => return Err("duplicate --json flag".into()),
            "--refresh" if options.refresh.is_none() => {
                let provider = rest
                    .get(index + 1)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                    .cloned();
                if provider.is_some() {
                    index += 1;
                }
                options.refresh =
                    Some(provider.map_or(ModelsRefresh::All, ModelsRefresh::Provider));
            }
            "--refresh" => return Err("duplicate --refresh flag".into()),
            _ => {
                return Err("usage: haider models [--json] [--refresh [<alias>]]".into());
            }
        }
        index += 1;
    }
    Ok(Some(options))
}

async fn read_document_with_refresh(
    client: &haider_client::RpcClient,
    requested: Option<&ModelsRefresh>,
) -> Result<ModelsDocument, ModelsError> {
    let document = read_document(client).await?;
    let targets = refresh_targets(&document, requested);
    if targets.is_empty() {
        return Ok(document);
    }
    let mut failures = BTreeMap::new();
    for provider in targets {
        if let Err(error) = refresh_provider(client, provider.clone()).await {
            failures.insert(provider, error.to_string());
        }
    }
    let mut document = read_document(client).await?;
    apply_refresh_failures(&mut document, failures)?;
    Ok(document)
}

fn apply_refresh_failures(
    document: &mut ModelsDocument,
    mut failures: BTreeMap<String, String>,
) -> Result<(), ModelsError> {
    for provider in &mut document.providers {
        if let Some(reason) = failures.remove(&provider.provider) {
            provider.inventory = match provider.inventory.fetched_at_ms() {
                Some(fetched_at_ms) => haider_rpc::ModelInventoryWire::Stale {
                    fetched_at_ms,
                    reason,
                },
                None => haider_rpc::ModelInventoryWire::Unavailable { reason },
            };
        }
    }
    // An explicit typo is a command error, never a failure attributed to a
    // different registered provider. Real provider failures stay on their row.
    if let Some((provider, reason)) = failures.into_iter().next() {
        return Err(ModelsError::Rpc {
            code: "provider_not_found".to_owned(),
            message: format!("{provider}: {reason}"),
            retryable: false,
        });
    }
    Ok(())
}

async fn read_document(client: &haider_client::RpcClient) -> Result<ModelsDocument, ModelsError> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let (descriptors, account_revision) = account_snapshot(client).await?;
        let (providers, revision) = provider_snapshot(client).await?;
        if account_revision.is_some_and(|account_revision| account_revision != revision) {
            continue;
        }
        let now_ms = unix_time_ms();
        let providers = providers
            .into_iter()
            .map(|provider| provider_view(provider, &descriptors, now_ms))
            .collect();
        return Ok(ModelsDocument {
            schema: MODELS_SCHEMA,
            revision,
            providers,
        });
    }
    Err(ModelsError::SnapshotChanged)
}

fn refresh_targets(document: &ModelsDocument, requested: Option<&ModelsRefresh>) -> Vec<String> {
    match requested {
        Some(ModelsRefresh::Provider(provider)) => vec![provider.clone()],
        Some(ModelsRefresh::All) => document
            .providers
            .iter()
            .filter(|provider| provider_supports_live_discovery(provider))
            .map(|provider| provider.provider.clone())
            .collect(),
        None => document
            .providers
            .iter()
            .filter(|provider| {
                matches!(
                    provider.inventory,
                    haider_rpc::ModelInventoryWire::Stale { .. }
                )
            })
            .map(|provider| provider.provider.clone())
            .collect(),
    }
}

fn provider_supports_live_discovery(provider: &ProviderView) -> bool {
    provider.catalog.supports_discovery()
}

async fn refresh_provider(
    client: &haider_client::RpcClient,
    provider: String,
) -> Result<(), ModelsError> {
    match client
        .request(RequestBody::ProviderModelsRefresh { provider })
        .await
        .map_err(ModelsError::Client)?
    {
        ResponseBody::ProviderModelsRefresh { .. } => Ok(()),
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
            "provider.models_refresh response method mismatch",
        )),
    }
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
    now_ms: u64,
) -> ProviderView {
    let mut provider = provider;
    provider.inventory = provider.inventory.at_time(now_ms);
    if !provider.has_known_models() {
        provider.models.clear();
        provider.model_details.clear();
        provider.default_model = None;
        provider.availability = ProviderAvailabilityWire::Unavailable;
    }
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
    let fetched_at_ms = provider.inventory.fetched_at_ms();
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
        fetched_at_ms,
        inventory_age_ms: fetched_at_ms.map(|fetched_at_ms| now_ms.saturating_sub(fetched_at_ms)),
        inventory: provider.inventory,
        catalog: provider.catalog,
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
        source: None,
        max_output_tokens: None,
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
        source: detail.source,
        max_output_tokens: detail.max_output_tokens,
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
            "{}  availability={}  auth_state={}  credential={}  default={}  inventory_age_ms={}\n",
            provider.provider,
            provider.availability,
            provider.auth_state,
            credential,
            provider.default_model.as_deref().unwrap_or("-"),
            provider
                .inventory_age_ms
                .map_or_else(|| "n/a".to_owned(), |age| age.to_string())
        ));
        text.push_str(&format!(
            "  inventory: {} (haider models --refresh {})\n",
            provider.inventory.description(),
            provider.provider
        ));
        for model in &provider.models {
            let context = model
                .context_window
                .map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string());
            let output = model
                .max_output_tokens
                .map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string());
            let source = model
                .source
                .filter(|source| *source != haider_rpc::ModelDetailSourceWire::Unknown)
                .map_or_else(String::new, |source| {
                    format!("  source={}", source.as_str())
                });
            text.push_str(&format!(
                "  {}  context_window={}  max_output_tokens={}{}\n",
                model.model, context, output, source
            ));
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

fn unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;

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
        ModelsError::Client(ClientError::Encode(_) | ClientError::MissingFeature(_))
        | ModelsError::SnapshotChanged => EX_SOFTWARE,
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
