//! Durable provider-profile registry owned by the account/provider actor.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use haider_protocol::credential::AuthMethod;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_provider::{
    ANTHROPIC_API_URL, ANTHROPIC_OAUTH_BASE_URL, ANTHROPIC_OAUTH_PROVIDER_NAME,
    ANTHROPIC_PROVIDER_NAME, DiscoveredModel, OPENAI_COMPATIBLE_PROVIDER_NAME,
    OPENAI_OAUTH_PROVIDER_NAME, OPENAI_PROVIDER_NAME, OPENAI_RESPONSES_API_URL,
    OPENAI_SUBSCRIPTION_RESPONSES_URL, pickable,
};
use haider_rpc::{
    ModelDetailWire, ProviderApiFamilyWire, ProviderAuthRequirementWire, ProviderAvailabilityWire,
    ProviderSummaryWire,
};
use serde::{Deserialize, Serialize};

pub(crate) const PROVIDERS_FILE_NAME: &str = "providers.json";

#[async_trait::async_trait]
pub trait ProviderEndpointValidator: Send + Sync {
    async fn validate(&self, origin: &str) -> Result<String, HaiderError>;
}

#[derive(Debug, Default)]
pub struct ProductionProviderEndpointValidator;

#[async_trait::async_trait]
impl ProviderEndpointValidator for ProductionProviderEndpointValidator {
    async fn validate(&self, origin: &str) -> Result<String, HaiderError> {
        haider_provider::validate_openai_compatible_endpoint(origin)
            .await
            .map_err(|error| {
                HaiderError::new(ErrorCode::InvalidArgument, error.message, error.retryable)
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderProvenance {
    BuiltIn,
    Custom,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProviderProfileV1 {
    pub provider_id: String,
    pub display_name: String,
    pub api_family: ProviderApiFamilyWire,
    pub base_url: Option<String>,
    pub enabled: bool,
    pub auth_requirement: ProviderAuthRequirementWire,
    #[serde(default)]
    pub configured_models: Vec<String>,
    pub default_model: Option<String>,
    pub provenance: ProviderProvenance,
}

pub(crate) trait ProviderRegistryStoreLike: Send + Sync {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError>;
    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError>;
}

#[derive(Debug, Clone)]
pub(crate) struct JsonProviderRegistryStore {
    path: PathBuf,
}

impl JsonProviderRegistryStore {
    pub(crate) fn new(profile_dir: impl AsRef<Path>) -> Self {
        Self {
            path: profile_dir.as_ref().join(PROVIDERS_FILE_NAME),
        }
    }
}

impl ProviderRegistryStoreLike for JsonProviderRegistryStore {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(file_error("read", &self.path, error)),
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "provider registry `{}` is not valid JSON: {error}",
                    self.path.display()
                ),
                false,
            )
        })
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        let parent = self.path.parent().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "provider registry path has no parent directory",
                false,
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| file_error("create parent directory for", &self.path, error))?;
        let bytes = serde_json::to_vec_pretty(profiles).map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("could not serialize provider registry: {error}"),
                false,
            )
        })?;
        let temporary = self.path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|error| file_error("open temporary file for", &temporary, error))?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| file_error("write", &temporary, error))?;
        drop(file);
        fs::rename(&temporary, &self.path)
            .map_err(|error| file_error("replace", &self.path, error))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| file_error("sync parent directory of", &self.path, error))
    }
}

impl ProviderRegistryStoreLike for Arc<dyn ProviderRegistryStoreLike> {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        self.as_ref().load()
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.as_ref().save(profiles)
    }
}

impl ProviderRegistryStoreLike for Box<dyn ProviderRegistryStoreLike> {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        self.as_ref().load()
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.as_ref().save(profiles)
    }
}

pub(crate) trait ProviderModelSourceLike: Send + Sync {
    fn models(&self, provider: &str) -> Option<Vec<DiscoveredModel>>;
    fn replace(&self, provider: String, models: Vec<DiscoveredModel>);
}

/// Typed, in-memory projection of the durable provider-model cache.
///
/// Production hydrates this before publishing the first management snapshot
/// and the account actor replaces entries only after the corresponding
/// SQLite write succeeds.
#[derive(Default)]
pub(crate) struct CachedProviderModelSource {
    models: std::sync::Mutex<HashMap<String, Vec<DiscoveredModel>>>,
}

impl ProviderModelSourceLike for CachedProviderModelSource {
    fn models(&self, provider: &str) -> Option<Vec<DiscoveredModel>> {
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
    }

    fn replace(&self, provider: String, models: Vec<DiscoveredModel>) {
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider, models);
    }
}

pub(crate) struct ProviderRegistry<S> {
    store: S,
    profiles: Vec<ProviderProfileV1>,
    model_source: Arc<dyn ProviderModelSourceLike>,
}

impl<S: ProviderRegistryStoreLike> ProviderRegistry<S> {
    pub(crate) fn new(
        store: S,
        initial: Vec<ProviderProfileV1>,
        model_source: Arc<dyn ProviderModelSourceLike>,
    ) -> Result<Self, HaiderError> {
        let mut profiles = store.load()?;
        if profiles.is_empty() {
            profiles = initial;
            validate_profiles(&profiles)?;
            store.save(&profiles)?;
        } else {
            validate_profiles(&profiles)?;
            for profile in initial {
                if !profiles
                    .iter()
                    .any(|existing| existing.provider_id == profile.provider_id)
                {
                    profiles.push(profile);
                }
            }
            validate_profiles(&profiles)?;
            store.save(&profiles)?;
        }
        Ok(Self {
            store,
            profiles,
            model_source,
        })
    }

    pub(crate) fn get(&self, provider: &str) -> Option<&ProviderProfileV1> {
        self.profiles
            .iter()
            .find(|profile| profile.provider_id == provider)
    }

    pub(crate) fn summaries(&self) -> Vec<ProviderSummaryWire> {
        self.profiles
            .iter()
            .map(|profile| self.summary_profile(profile))
            .collect()
    }

    pub(crate) fn summary(&self, provider: &str) -> Option<ProviderSummaryWire> {
        self.get(provider)
            .map(|profile| self.summary_profile(profile))
    }

    pub(crate) fn replace_models(&self, provider: String, models: Vec<DiscoveredModel>) {
        self.model_source.replace(provider, models);
    }

    pub(crate) fn configure(
        &mut self,
        input: ProviderConfigureInput,
    ) -> Result<ProviderProfileV1, HaiderError> {
        let (next, profile) = self.configured_profiles(input)?;
        self.store.save(&next)?;
        self.profiles = next;
        Ok(profile)
    }

    pub(crate) fn validate_configure(
        &self,
        input: ProviderConfigureInput,
    ) -> Result<(), HaiderError> {
        self.configured_profiles(input).map(|_| ())
    }

    fn configured_profiles(
        &self,
        input: ProviderConfigureInput,
    ) -> Result<(Vec<ProviderProfileV1>, ProviderProfileV1), HaiderError> {
        let discovered_models = self.discovered_slugs(&input.provider);
        self.configured_profiles_with_inventory(input, &discovered_models)
    }

    fn configured_profiles_with_inventory(
        &self,
        input: ProviderConfigureInput,
        discovered_models: &[String],
    ) -> Result<(Vec<ProviderProfileV1>, ProviderProfileV1), HaiderError> {
        validate_provider_id(&input.provider)?;
        let mut next = self.profiles.clone();
        let profile = if let Some(existing) = next
            .iter_mut()
            .find(|profile| profile.provider_id == input.provider)
        {
            require_matching_identity(existing, &input)?;
            existing.enabled = input.enabled;
            existing.configured_models = normalized_models(input.models)?;
            existing.default_model = validated_default(discovered_models, input.default_model)?;
            existing.clone()
        } else {
            let api_family = input
                .api_family
                .ok_or_else(|| invalid("new provider configuration requires an API family"))?;
            let base_url = input
                .origin
                .filter(|origin| !origin.trim().is_empty())
                .ok_or_else(|| invalid("new provider configuration requires an origin"))?;
            let auth_requirement = input.auth_requirement.ok_or_else(|| {
                invalid("new provider configuration requires an auth requirement")
            })?;
            if !matches!(api_family, ProviderApiFamilyWire::OpenAiChatCompletions) {
                return Err(invalid(
                    "custom providers must use the openai_chat_completions API family",
                ));
            }
            if !matches!(
                auth_requirement,
                ProviderAuthRequirementWire::ApiKey | ProviderAuthRequirementWire::None
            ) {
                return Err(invalid(
                    "custom providers may require api_key authentication or none",
                ));
            }
            let configured_models = normalized_models(input.models)?;
            let default_model = validated_default(discovered_models, input.default_model)?;
            let profile = ProviderProfileV1 {
                provider_id: input.provider.clone(),
                display_name: input.provider,
                api_family,
                base_url: Some(base_url),
                enabled: input.enabled,
                auth_requirement,
                configured_models,
                default_model,
                provenance: ProviderProvenance::Custom,
            };
            next.push(profile.clone());
            profile
        };
        if profile.enabled && (discovered_models.is_empty() || profile.default_model.is_none()) {
            return Err(invalid(
                "an enabled provider requires a discovered model inventory and a default model",
            ));
        }
        validate_profiles(&next)?;
        Ok((next, profile))
    }

    /// Replays a pre-v8 pending configure receipt. A missing discovered
    /// inventory identifies the legacy recovery case: v8 commands cannot be
    /// claimed until discovery validation has succeeded, and cache rows are
    /// never deleted.
    pub(crate) fn reconcile_configure(
        &mut self,
        input: ProviderConfigureInput,
    ) -> Result<ProviderProfileV1, HaiderError> {
        let discovered_models = self.discovered_slugs(&input.provider);
        let inventory = if discovered_models.is_empty() {
            normalized_models(input.models.clone())?
        } else {
            discovered_models
        };
        let (next, profile) = self.configured_profiles_with_inventory(input, &inventory)?;
        self.store.save(&next)?;
        self.profiles = next;
        Ok(profile)
    }

    pub(crate) fn validate_default_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<(), HaiderError> {
        if self.get(provider).is_none() {
            return Err(invalid(format!("provider `{provider}` is not registered")));
        }
        let model = model.trim();
        if !self
            .discovered_slugs(provider)
            .iter()
            .any(|discovered| discovered == model)
        {
            return Err(invalid(format!(
                "model `{model}` is not in the discovered inventory for provider `{provider}`"
            )));
        }
        Ok(())
    }

    pub(crate) fn set_default_model(
        &mut self,
        provider: &str,
        model: &str,
    ) -> Result<ProviderProfileV1, HaiderError> {
        self.validate_default_model(provider, model)?;
        let mut next = self.profiles.clone();
        let profile = next
            .iter_mut()
            .find(|profile| profile.provider_id == provider)
            .ok_or_else(|| invalid(format!("provider `{provider}` is not registered")))?;
        let model = model.trim();
        profile.default_model = Some(model.to_owned());
        let result = profile.clone();
        validate_profiles(&next)?;
        self.store.save(&next)?;
        self.profiles = next;
        Ok(result)
    }

    /// Replays a pre-v8 pending default-model receipt against its durable
    /// legacy inventory when no discovered cache existed at migration time.
    pub(crate) fn reconcile_set_default_model(
        &mut self,
        provider: &str,
        model: &str,
    ) -> Result<ProviderProfileV1, HaiderError> {
        if !self.discovered_slugs(provider).is_empty() {
            return self.set_default_model(provider, model);
        }
        let profile = self
            .get(provider)
            .ok_or_else(|| invalid(format!("provider `{provider}` is not registered")))?;
        let model = model.trim();
        if !profile
            .configured_models
            .iter()
            .any(|configured| configured == model)
        {
            return Err(invalid(format!(
                "legacy model `{model}` is not registered for provider `{provider}`"
            )));
        }
        let mut next = self.profiles.clone();
        let profile = next
            .iter_mut()
            .find(|profile| profile.provider_id == provider)
            .ok_or_else(|| invalid(format!("provider `{provider}` is not registered")))?;
        profile.default_model = Some(model.to_owned());
        let result = profile.clone();
        validate_profiles(&next)?;
        self.store.save(&next)?;
        self.profiles = next;
        Ok(result)
    }

    fn discovered_slugs(&self, provider: &str) -> Vec<String> {
        self.discovered_details(provider)
            .into_iter()
            .map(|(slug, _context_window)| slug)
            .collect()
    }

    fn discovered_details(&self, provider: &str) -> Vec<(String, Option<u64>)> {
        self.model_source
            .models(provider)
            .map(|models| {
                pickable(&models)
                    .into_iter()
                    .map(|model| (model.slug, model.context_window))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn summary_profile(&self, profile: &ProviderProfileV1) -> ProviderSummaryWire {
        let model_details = self
            .discovered_details(&profile.provider_id)
            .into_iter()
            .map(|(name, context_window)| ModelDetailWire {
                name,
                context_window,
            })
            .collect();
        provider_summary(profile, model_details)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProviderConfigureInput {
    pub provider: String,
    pub api_family: Option<ProviderApiFamilyWire>,
    pub origin: Option<String>,
    pub auth_requirement: Option<ProviderAuthRequirementWire>,
    pub enabled: bool,
    pub models: Vec<String>,
    pub default_model: Option<String>,
}

pub(crate) fn initial_provider_profiles(
    provider_names: &std::collections::BTreeSet<String>,
    anthropic_default_model: &str,
) -> Vec<ProviderProfileV1> {
    provider_names
        .iter()
        .map(|provider| builtin_or_unknown(provider, anthropic_default_model))
        .collect()
}

fn provider_summary(
    profile: &ProviderProfileV1,
    model_details: Vec<ModelDetailWire>,
) -> ProviderSummaryWire {
    let discovered_models = model_details
        .iter()
        .map(|detail| detail.name.clone())
        .collect::<Vec<_>>();
    let auth_methods = match profile.auth_requirement {
        ProviderAuthRequirementWire::ApiKey => vec![AuthMethod::ApiKey],
        ProviderAuthRequirementWire::OAuth => vec![AuthMethod::OAuth],
        ProviderAuthRequirementWire::None | ProviderAuthRequirementWire::Unknown => Vec::new(),
        _ => Vec::new(),
    };
    let available = profile.enabled
        && !matches!(profile.api_family, ProviderApiFamilyWire::Unknown)
        && !discovered_models.is_empty();
    let default_model = profile
        .default_model
        .as_ref()
        .filter(|default| discovered_models.iter().any(|model| model == *default))
        .cloned();
    ProviderSummaryWire {
        provider: profile.provider_id.clone(),
        api_family: profile.api_family,
        endpoint: profile.base_url.clone(),
        models: discovered_models,
        model_details,
        auth_methods,
        availability: if available {
            ProviderAvailabilityWire::Available
        } else {
            ProviderAvailabilityWire::Unavailable
        },
        availability_reason: (!available).then(|| {
            if matches!(profile.provenance, ProviderProvenance::Unknown) {
                "provider adapter is not registered".to_owned()
            } else if !profile.enabled {
                "provider is disabled".to_owned()
            } else if matches!(profile.api_family, ProviderApiFamilyWire::Unknown) {
                "provider API family is unavailable".to_owned()
            } else {
                "provider model inventory is unavailable".to_owned()
            }
        }),
        default_model,
        enabled: profile.enabled,
    }
}

fn builtin_or_unknown(provider: &str, anthropic_default_model: &str) -> ProviderProfileV1 {
    let _ = anthropic_default_model;
    let (api_family, base_url, auth_requirement, enabled, provenance) = match provider {
        ANTHROPIC_PROVIDER_NAME => (
            ProviderApiFamilyWire::AnthropicMessages,
            Some(ANTHROPIC_API_URL.to_owned()),
            ProviderAuthRequirementWire::ApiKey,
            true,
            ProviderProvenance::BuiltIn,
        ),
        ANTHROPIC_OAUTH_PROVIDER_NAME => (
            ProviderApiFamilyWire::AnthropicMessages,
            Some(ANTHROPIC_OAUTH_BASE_URL.to_owned()),
            ProviderAuthRequirementWire::OAuth,
            true,
            ProviderProvenance::BuiltIn,
        ),
        OPENAI_PROVIDER_NAME => (
            ProviderApiFamilyWire::OpenAiResponses,
            Some(OPENAI_RESPONSES_API_URL.to_owned()),
            ProviderAuthRequirementWire::ApiKey,
            true,
            ProviderProvenance::BuiltIn,
        ),
        OPENAI_OAUTH_PROVIDER_NAME => (
            ProviderApiFamilyWire::OpenAiResponses,
            Some(OPENAI_SUBSCRIPTION_RESPONSES_URL.to_owned()),
            ProviderAuthRequirementWire::OAuth,
            true,
            ProviderProvenance::BuiltIn,
        ),
        OPENAI_COMPATIBLE_PROVIDER_NAME => (
            ProviderApiFamilyWire::OpenAiChatCompletions,
            None,
            ProviderAuthRequirementWire::ApiKey,
            false,
            ProviderProvenance::BuiltIn,
        ),
        _ => (
            ProviderApiFamilyWire::Unknown,
            None,
            ProviderAuthRequirementWire::Unknown,
            false,
            ProviderProvenance::Unknown,
        ),
    };
    ProviderProfileV1 {
        provider_id: provider.to_owned(),
        display_name: provider.to_owned(),
        api_family,
        base_url,
        enabled,
        auth_requirement,
        configured_models: Vec::new(),
        default_model: None,
        provenance,
    }
}

fn require_matching_identity(
    existing: &ProviderProfileV1,
    input: &ProviderConfigureInput,
) -> Result<(), HaiderError> {
    if input
        .api_family
        .is_some_and(|family| family != existing.api_family)
        || input
            .origin
            .as_ref()
            .is_some_and(|origin| Some(origin) != existing.base_url.as_ref())
        || input
            .auth_requirement
            .is_some_and(|requirement| requirement != existing.auth_requirement)
    {
        return Err(invalid(format!(
            "provider `{}` identity fields are create-only; use a new provider id for a different API family, origin, or auth requirement",
            existing.provider_id
        )));
    }
    Ok(())
}

fn normalized_models(models: Vec<String>) -> Result<Vec<String>, HaiderError> {
    let mut normalized = Vec::with_capacity(models.len());
    for model in models {
        let model = model.trim();
        if model.is_empty() {
            return Err(invalid("provider models must not be empty"));
        }
        if !normalized.iter().any(|existing| existing == model) {
            normalized.push(model.to_owned());
        }
    }
    Ok(normalized)
}

fn validated_default(
    models: &[String],
    default_model: Option<String>,
) -> Result<Option<String>, HaiderError> {
    let default_model = default_model
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty());
    if let Some(default_model) = &default_model
        && !models.iter().any(|model| model == default_model)
    {
        return Err(invalid(format!(
            "default model `{default_model}` is not in the configured model inventory"
        )));
    }
    Ok(default_model)
}

fn validate_profiles(profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
    let mut ids = std::collections::HashSet::new();
    for profile in profiles {
        validate_provider_id(&profile.provider_id)?;
        if !ids.insert(profile.provider_id.as_str()) {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "provider registry contains duplicate id `{}`",
                    profile.provider_id
                ),
                false,
            ));
        }
    }
    Ok(())
}

fn validate_provider_id(provider: &str) -> Result<(), HaiderError> {
    let bytes = provider.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase()
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
    {
        return Err(invalid("provider id must match [a-z0-9][a-z0-9._-]{0,63}"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> HaiderError {
    HaiderError::new(ErrorCode::InvalidArgument, message, false)
}

fn file_error(action: &str, path: &Path, error: std::io::Error) -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        format!(
            "could not {action} provider registry `{}`: {error}",
            path.display()
        ),
        error.kind() == std::io::ErrorKind::Interrupted,
    )
}

#[cfg(test)]
#[path = "provider_registry_tests.rs"]
mod provider_registry_tests;
