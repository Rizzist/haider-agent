//! Durable provider-profile registry owned by the account/provider actor.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use haider_protocol::credential::AuthMethod;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_provider::{
    ANTHROPIC_API_URL, ANTHROPIC_OAUTH_BASE_URL, ANTHROPIC_OAUTH_PROVIDER_NAME,
    ANTHROPIC_PROVIDER_NAME, BEDROCK_MANTLE_DEFAULT_BASE_URL, BEDROCK_PROVIDER_NAME,
    BEDROCK_SEED_MODELS, DEEPSEEK_BASE_URL, DEEPSEEK_PROVIDER_NAME, DiscoveredModel,
    GEMINI_API_BASE_URL, GEMINI_PROVIDER_NAME, GOOGLE_ANTIGRAVITY_PROVIDER_NAME,
    GROK_OAUTH_BASE_URL, GROK_OAUTH_PROVIDER_NAME, HAIDER_CODE_BASE_URL, HAIDER_CODE_PROVIDER_NAME,
    KIMI_OAUTH_BASE_URL, KIMI_OAUTH_PROVIDER_NAME, OPENAI_COMPATIBLE_PROVIDER_NAME,
    OPENAI_OAUTH_PROVIDER_NAME, OPENAI_PROVIDER_NAME, OPENAI_RESPONSES_API_URL,
    OPENAI_SUBSCRIPTION_RESPONSES_URL, ProviderErrorKind, VERTEX_PROVIDER_NAME, VERTEX_SEED_MODELS,
    XAI_BASE_URL, XAI_PROVIDER_NAME, XAI_SEED_MODEL_CONTEXT_WINDOWS, azure_openai_origin, pickable,
};
use haider_rpc::{
    ModelDetailWire, ProviderApiFamilyWire, ProviderAuthRequirementWire, ProviderAvailabilityWire,
    ProviderSummaryWire, ProviderTrustWire,
};
use serde::{Deserialize, Serialize};

pub(crate) const PROVIDERS_FILE_NAME: &str = "providers.json";
pub(crate) const FALLBACK_CHAIN_ENV: &str = "HAIDER_FALLBACK_CHAIN";
pub(crate) const COMPACTION_PROMOTION_ENV: &str = "HAIDER_COMPACTION_PROMOTION";

#[async_trait::async_trait]
pub trait ProviderEndpointValidator: Send + Sync {
    async fn validate(&self, origin: &str) -> Result<String, HaiderError>;

    /// An explicitly chosen model inventory needs origin safety, not a
    /// second availability probe. Injected validators retain their policy.
    async fn validate_configured_origin(&self, origin: &str) -> Result<String, HaiderError> {
        self.validate(origin).await
    }
}

#[derive(Debug, Default)]
pub struct ProductionProviderEndpointValidator;

#[async_trait::async_trait]
impl ProviderEndpointValidator for ProductionProviderEndpointValidator {
    async fn validate_configured_origin(&self, origin: &str) -> Result<String, HaiderError> {
        haider_provider::validate_openai_compatible_origin(
            origin,
            haider_provider::CompatibleOriginPolicy::TrustedLan,
        )
        .await
        .map_err(|error| {
            let code = match error.kind {
                ProviderErrorKind::InvalidRequest => ErrorCode::InvalidArgument,
                ProviderErrorKind::NetworkUnavailable | ProviderErrorKind::Transport => {
                    ErrorCode::ProviderError
                }
                _ => ErrorCode::Internal,
            };
            HaiderError::new(code, error.message, error.retryable)
        })
    }

    async fn validate(&self, origin: &str) -> Result<String, HaiderError> {
        // This validator runs for brand-new and repointed custom
        // `provider.configure` profiles, so both paths share the scoped
        // TrustedLan matrix (G4a): RFC1918 LAN origins are legal for custom
        // local endpoints; link-local/metadata and public plain HTTP stay
        // refused. Builtin providers never route through here.
        haider_provider::validate_openai_compatible_endpoint(
            origin,
            haider_provider::CompatibleOriginPolicy::TrustedLan,
        )
        .await
        .map_err(|error| {
            let code = match error.kind {
                ProviderErrorKind::InvalidRequest => ErrorCode::InvalidArgument,
                ProviderErrorKind::NetworkUnavailable | ProviderErrorKind::Transport => {
                    ErrorCode::ProviderError
                }
                _ => ErrorCode::Internal,
            };
            HaiderError::new(code, error.message, error.retryable)
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
    /// OpenAI-family response-header budget; absent selects 60 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_open_timeout_ms: Option<u64>,
    /// Raw response-chunk idle budget; absent selects the adapter default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_idle_timeout_ms: Option<u64>,
    /// Active-route semantic-progress budget; absent selects the adapter
    /// default. Heartbeat/comment bytes do not reset it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_progress_timeout_ms: Option<u64>,
    pub enabled: bool,
    pub auth_requirement: ProviderAuthRequirementWire,
    #[serde(default)]
    pub configured_models: Vec<String>,
    pub default_model: Option<String>,
    /// Optional larger-context model on this provider. Runtime promotion
    /// still verifies the discovered context window before selecting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_model: Option<String>,
    pub provenance: ProviderProvenance,
    /// Missing in pre-lockdown records means Full so an upgrade cannot
    /// silently revoke capabilities from an existing custom provider. Later
    /// trust changes are authoritative in committed SQLite receipts, replayed
    /// over this seed before the account actor becomes ready.
    #[serde(default)]
    pub trust: ProviderTrustWire,
}

/// One validated provider/model coordinate from the profile's resilience
/// configuration. A fallback entry may omit `model`; promotion targets never
/// do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderTargetV1 {
    pub provider: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromotionOverrideV1 {
    provider: Option<String>,
    model: String,
}

/// Immutable, startup-resolved resilience settings. This is deliberately
/// cloneable so the provider factory can retain the same parsed snapshot after
/// the mutable registry moves into the account actor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProviderResilienceConfigV1 {
    fallback_chain: Vec<ProviderTargetV1>,
    promotion_models: HashMap<String, String>,
    compaction_promotion_override: Option<PromotionOverrideV1>,
}

impl ProviderResilienceConfigV1 {
    pub(crate) fn fallback_chain(&self) -> &[ProviderTargetV1] {
        &self.fallback_chain
    }

    pub(crate) fn compaction_promotion(&self, current_provider: &str) -> Option<ProviderTargetV1> {
        if let Some(target) = &self.compaction_promotion_override {
            if target
                .provider
                .as_deref()
                .is_some_and(|provider| provider != current_provider)
            {
                return None;
            }
            return Some(ProviderTargetV1 {
                provider: current_provider.to_owned(),
                model: Some(target.model.clone()),
            });
        }
        self.promotion_models
            .get(current_provider)
            .map(|model| ProviderTargetV1 {
                provider: current_provider.to_owned(),
                model: Some(model.clone()),
            })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProviderRegistryDocumentV1 {
    #[serde(default, alias = "profiles")]
    providers: Vec<ProviderProfileV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fallback_chain: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum ProviderRegistryFileV1 {
    Legacy(Vec<ProviderProfileV1>),
    Document(ProviderRegistryDocumentV1),
}

impl ProviderRegistryFileV1 {
    fn into_document(self) -> ProviderRegistryDocumentV1 {
        match self {
            Self::Legacy(providers) => ProviderRegistryDocumentV1 {
                providers,
                fallback_chain: Vec::new(),
            },
            Self::Document(document) => document,
        }
    }
}

pub(crate) trait ProviderRegistryStoreLike: Send + Sync {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError>;
    fn load_document(&self) -> Result<ProviderRegistryDocumentV1, HaiderError> {
        self.load().map(|providers| ProviderRegistryDocumentV1 {
            providers,
            fallback_chain: Vec::new(),
        })
    }
    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError>;
    fn save_bootstrap(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.save(profiles)
    }
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

    fn read_document(&self) -> Result<ProviderRegistryDocumentV1, HaiderError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProviderRegistryDocumentV1::default());
            }
            Err(error) => return Err(file_error("read", &self.path, error)),
        };
        serde_json::from_slice::<ProviderRegistryFileV1>(&bytes)
            .map(ProviderRegistryFileV1::into_document)
            .map_err(|error| {
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

    fn save_with_policy(
        &self,
        profiles: &[ProviderProfileV1],
        policy: haider_platform::SyncPolicy,
    ) -> Result<(), HaiderError> {
        let parent = self.path.parent().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "provider registry path has no parent directory",
                false,
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| file_error("create parent directory for", &self.path, error))?;
        // Provider management mutates only the provider rows. Preserve the
        // profile-level resilience configuration across every such write.
        let fallback_chain = self.read_document()?.fallback_chain;
        let document = ProviderRegistryDocumentV1 {
            providers: profiles.to_vec(),
            fallback_chain,
        };
        let mut bytes = serde_json::to_vec_pretty(&document).map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("could not serialize provider registry: {error}"),
                false,
            )
        })?;
        bytes.push(b'\n');
        match fs::read(&self.path) {
            Ok(current) if current == bytes => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(file_error("compare", &self.path, error)),
        }
        let temporary = self.path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|error| file_error("open temporary file for", &temporary, error))?;
        file.write_all(&bytes)
            .and_then(|()| haider_platform::fs::sync_file(&file, policy))
            .map_err(|error| file_error("write", &temporary, error))?;
        drop(file);
        haider_platform::replace_file(&temporary, &self.path)
            .map_err(|error| file_error("replace", &self.path, error))?;
        haider_platform::fs::sync_directory(parent, policy)
            .map_err(|error| file_error("sync parent directory of", &self.path, error))
    }
}

impl ProviderRegistryStoreLike for JsonProviderRegistryStore {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        self.read_document().map(|document| document.providers)
    }

    fn load_document(&self) -> Result<ProviderRegistryDocumentV1, HaiderError> {
        self.read_document()
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.save_with_policy(profiles, haider_platform::SyncPolicy::Full)
    }

    fn save_bootstrap(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        // Release-owned defaults and newly introduced built-ins are
        // reconstructible on the next boot. Keep the atomic replacement and
        // plain fsync ordering without paying Apple's device-wide full-cache
        // flush twice on the readiness path. User-issued mutations continue
        // through `save`, whose policy remains Full.
        self.save_with_policy(profiles, haider_platform::SyncPolicy::Plain)
    }
}

impl ProviderRegistryStoreLike for Arc<dyn ProviderRegistryStoreLike> {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        self.as_ref().load()
    }

    fn load_document(&self) -> Result<ProviderRegistryDocumentV1, HaiderError> {
        self.as_ref().load_document()
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.as_ref().save(profiles)
    }

    fn save_bootstrap(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.as_ref().save_bootstrap(profiles)
    }
}

impl ProviderRegistryStoreLike for Box<dyn ProviderRegistryStoreLike> {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        self.as_ref().load()
    }

    fn load_document(&self) -> Result<ProviderRegistryDocumentV1, HaiderError> {
        self.as_ref().load_document()
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.as_ref().save(profiles)
    }

    fn save_bootstrap(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        self.as_ref().save_bootstrap(profiles)
    }
}

/// One atomic inventory value. A missing fetch cannot carry model rows.
#[derive(Debug, Clone, Default)]
pub(crate) enum ProviderInventory {
    #[default]
    NeverFetched,
    Fetched {
        models: Vec<DiscoveredModel>,
        fetched_at_ms: u64,
    },
    Stale {
        models: Vec<DiscoveredModel>,
        fetched_at_ms: u64,
        reason: String,
    },
    Unavailable {
        reason: String,
    },
    /// Test fixtures state their provenance explicitly without inventing a fetch time.
    #[cfg(test)]
    Configured {
        models: Vec<DiscoveredModel>,
    },
}

impl ProviderInventory {
    fn models(&self) -> Option<Vec<DiscoveredModel>> {
        match self {
            Self::Fetched { models, .. } | Self::Stale { models, .. } => Some(models.clone()),
            #[cfg(test)]
            Self::Configured { models } => Some(models.clone()),
            Self::NeverFetched | Self::Unavailable { .. } => None,
        }
    }

    fn provenance(&self) -> haider_rpc::ModelInventoryWire {
        use haider_rpc::ModelInventoryWire;
        match self {
            Self::NeverFetched => ModelInventoryWire::NeverFetched,
            Self::Fetched { fetched_at_ms, .. } => ModelInventoryWire::Fetched {
                fetched_at_ms: *fetched_at_ms,
            },
            Self::Stale {
                fetched_at_ms,
                reason,
                ..
            } => ModelInventoryWire::Stale {
                fetched_at_ms: *fetched_at_ms,
                reason: reason.clone(),
            },
            Self::Unavailable { reason } => ModelInventoryWire::Unavailable {
                reason: reason.clone(),
            },
            #[cfg(test)]
            Self::Configured { .. } => ModelInventoryWire::Configured,
        }
    }
}

pub(crate) trait ProviderModelSourceLike: Send + Sync {
    fn inventory(&self, provider: &str) -> ProviderInventory;
    fn models(&self, provider: &str) -> Option<Vec<DiscoveredModel>> {
        self.inventory(provider).models()
    }
    fn replace(&self, provider: String, inventory: ProviderInventory);
    fn unavailable(&self, provider: &str, reason: String);
    fn touch(&self, provider: &str, fetched_at_ms: u64);
    fn remove(&self, provider: &str);
}

/// Model rows and their provenance are updated under the same lock.
#[derive(Default)]
pub(crate) struct CachedProviderModelSource {
    inventories: std::sync::Mutex<HashMap<String, ProviderInventory>>,
}

impl ProviderModelSourceLike for CachedProviderModelSource {
    fn inventory(&self, provider: &str) -> ProviderInventory {
        let mut inventory = self
            .inventories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
            .unwrap_or_default();
        if let ProviderInventory::Fetched {
            models,
            fetched_at_ms,
        } = &inventory
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                });
            if now.saturating_sub(*fetched_at_ms) >= haider_rpc::MODEL_INVENTORY_TTL_MS {
                inventory = ProviderInventory::Stale {
                    models: models.clone(),
                    fetched_at_ms: *fetched_at_ms,
                    reason: "catalog refresh due".to_owned(),
                };
            }
        }
        inventory
    }

    fn replace(&self, provider: String, inventory: ProviderInventory) {
        self.inventories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider, inventory);
    }

    fn unavailable(&self, provider: &str, reason: String) {
        let mut inventories = self
            .inventories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inventory = match inventories.remove(provider).unwrap_or_default() {
            ProviderInventory::Fetched {
                models,
                fetched_at_ms,
            }
            | ProviderInventory::Stale {
                models,
                fetched_at_ms,
                ..
            } if !models.is_empty() => ProviderInventory::Stale {
                models,
                fetched_at_ms,
                reason,
            },
            ProviderInventory::NeverFetched
            | ProviderInventory::Unavailable { .. }
            | ProviderInventory::Fetched { .. }
            | ProviderInventory::Stale { .. } => ProviderInventory::Unavailable { reason },
            #[cfg(test)]
            ProviderInventory::Configured { .. } => ProviderInventory::Unavailable { reason },
        };
        inventories.insert(provider.to_owned(), inventory);
    }

    fn touch(&self, provider: &str, fetched_at_ms: u64) {
        let mut inventories = self
            .inventories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(
            ProviderInventory::Fetched { models, .. } | ProviderInventory::Stale { models, .. },
        ) = inventories.get(provider)
        {
            let models = models.clone();
            inventories.insert(
                provider.to_owned(),
                ProviderInventory::Fetched {
                    models,
                    fetched_at_ms,
                },
            );
        }
    }

    fn remove(&self, provider: &str) {
        self.inventories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider);
    }
}

#[allow(dead_code)]
pub(crate) struct ProviderRegistry<S> {
    store: S,
    profiles: Vec<ProviderProfileV1>,
    model_source: Arc<dyn ProviderModelSourceLike>,
    resilience: ProviderResilienceConfigV1,
}

impl<S: ProviderRegistryStoreLike> ProviderRegistry<S> {
    pub(crate) fn new(
        store: S,
        initial: Vec<ProviderProfileV1>,
        model_source: Arc<dyn ProviderModelSourceLike>,
    ) -> Result<Self, HaiderError> {
        // Environment overrides are captured exactly once for this registry
        // instance. Long-running turns never reread mutable process state.
        let fallback_chain_override = read_env_override(FALLBACK_CHAIN_ENV)?;
        let compaction_promotion_override = read_env_override(COMPACTION_PROMOTION_ENV)?;
        let document = store.load_document()?;
        let mut profiles = document.providers;
        if profiles.is_empty() {
            profiles = initial;
            validate_profiles(&profiles)?;
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
        }
        for profile in &mut profiles {
            if matches!(profile.provenance, ProviderProvenance::BuiltIn)
                && matches!(
                    haider_provider::provider_catalog_definition(&profile.provider_id),
                    haider_provider::ProviderCatalogDefinition::Public { .. }
                        | haider_provider::ProviderCatalogDefinition::Authenticated { .. }
                )
            {
                profile.configured_models.clear();
                if profile.provider_id == HAIDER_CODE_PROVIDER_NAME
                    && profile
                        .default_model
                        .as_deref()
                        .is_some_and(|id| matches!(id, "Go" | "Go Max"))
                {
                    profile.default_model = None;
                }
            }
        }
        let fallback_chain = resolve_fallback_chain(
            &document.fallback_chain,
            fallback_chain_override.as_deref(),
            &profiles,
        )?;
        let compaction_promotion_override = compaction_promotion_override
            .as_deref()
            .map(|value| parse_promotion_override(value, &profiles))
            .transpose()?;
        let promotion_models = profiles
            .iter()
            .filter_map(|profile| {
                profile
                    .promotion_model
                    .as_ref()
                    .map(|model| (profile.provider_id.clone(), model.clone()))
            })
            .collect();
        let resilience = ProviderResilienceConfigV1 {
            fallback_chain,
            promotion_models,
            compaction_promotion_override,
        };
        // Refuse invalid resilience configuration before rewriting the
        // registry projection during bootstrap/profile reconciliation.
        store.save_bootstrap(&profiles)?;
        Ok(Self {
            store,
            profiles,
            model_source,
            resilience,
        })
    }

    pub(crate) fn get(&self, provider: &str) -> Option<&ProviderProfileV1> {
        self.profiles
            .iter()
            .find(|profile| profile.provider_id == provider)
    }

    #[allow(dead_code)]
    pub(crate) fn fallback_chain(&self) -> &[ProviderTargetV1] {
        self.resilience.fallback_chain()
    }

    #[allow(dead_code)]
    pub(crate) fn compaction_promotion(&self, current_provider: &str) -> Option<ProviderTargetV1> {
        self.resilience.compaction_promotion(current_provider)
    }

    /// A frozen startup snapshot suitable for provider-factory construction.
    #[allow(dead_code)]
    pub(crate) fn resilience_config(&self) -> ProviderResilienceConfigV1 {
        self.resilience.clone()
    }

    /// Projects every profile into its wire summary. `has_credential`
    /// answers "does at least one account exist for this provider" — the
    /// G4b seeded-inventory availability rule consults it (decision 6), so
    /// every caller must state its account truth explicitly.
    pub(crate) fn summaries(
        &self,
        has_credential: &dyn Fn(&str) -> bool,
    ) -> Vec<ProviderSummaryWire> {
        self.profiles
            .iter()
            .map(|profile| self.summary_profile(profile, has_credential))
            .collect()
    }

    pub(crate) fn summary(
        &self,
        provider: &str,
        has_credential: &dyn Fn(&str) -> bool,
    ) -> Option<ProviderSummaryWire> {
        self.get(provider)
            .map(|profile| self.summary_profile(profile, has_credential))
    }

    pub(crate) fn replace_discovered_models(
        &self,
        provider: String,
        models: Vec<DiscoveredModel>,
        fetched_at_ms: u64,
    ) {
        self.model_source.replace(
            provider,
            ProviderInventory::Fetched {
                models,
                fetched_at_ms,
            },
        );
    }

    pub(crate) fn models_unavailable(&self, provider: &str, reason: String) {
        self.model_source.unavailable(provider, reason);
    }

    pub(crate) fn touch_models(&self, provider: &str, fetched_at_ms: u64) {
        self.model_source.touch(provider, fetched_at_ms);
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

    pub(crate) fn configure_with_inventory(
        &mut self,
        input: ProviderConfigureInput,
        discovered_models: &[String],
    ) -> Result<ProviderProfileV1, HaiderError> {
        let (next, profile) = self.configured_profiles_with_inventory(input, discovered_models)?;
        self.store.save(&next)?;
        self.profiles = next;
        Ok(profile)
    }

    pub(crate) fn preview_trust(
        &self,
        provider: &str,
        trust: ProviderTrustWire,
        has_credential: &dyn Fn(&str) -> bool,
    ) -> Result<ProviderSummaryWire, HaiderError> {
        if matches!(trust, ProviderTrustWire::Unknown) {
            return Err(invalid("provider trust must be full or lockdown"));
        }
        let mut profile = self
            .get(provider)
            .cloned()
            .ok_or_else(|| invalid(format!("provider `{provider}` is not registered")))?;
        profile.trust = trust;
        Ok(self.summary_profile(&profile, has_credential))
    }

    /// SQLite's committed trust receipt is authoritative. This infallible
    /// publication follows its commit; startup replays it before Ready.
    pub(crate) fn publish_trust(&mut self, provider: &str, trust: ProviderTrustWire) {
        if let Some(profile) = self.profiles.iter_mut().find(|p| p.provider_id == provider) {
            profile.trust = trust;
        }
    }

    pub(crate) fn validate_configure(
        &self,
        input: ProviderConfigureInput,
    ) -> Result<bool, HaiderError> {
        self.configured_profiles(input)
            .map(|(next, _)| next != self.profiles)
    }

    pub(crate) fn validate_configure_with_inventory(
        &self,
        input: ProviderConfigureInput,
        discovered_models: &[String],
    ) -> Result<bool, HaiderError> {
        self.configured_profiles_with_inventory(input, discovered_models)
            .map(|(next, _)| next != self.profiles)
    }

    /// Refuses a custom provider repoint to an origin already owned by another
    /// provider before any live endpoint probe. The repoint configure branch
    /// repeats this check after canonicalization; this preflight preserves the
    /// typed local rejection for an exact duplicate without touching the
    /// claimed remote endpoint.
    pub(crate) fn validate_repoint_origin_claim(
        &self,
        provider_id: &str,
        origin: &str,
    ) -> Result<(), HaiderError> {
        require_unique_repoint_origin(&self.profiles, provider_id, origin)
    }

    pub(crate) fn remove_custom(&mut self, provider: &str) -> Result<(), HaiderError> {
        let profile = self
            .get(provider)
            .ok_or_else(|| invalid(format!("provider `{provider}` is not registered")))?;
        if !matches!(profile.provenance, ProviderProvenance::Custom) {
            return Err(invalid(format!(
                "provider `{provider}` is release-owned and cannot be removed"
            )));
        }
        self.remove_profile(provider)
    }

    /// Reapplies an already-claimed removal after a crash. A missing profile
    /// means the durable JSON mutation completed before receipt finalization.
    pub(crate) fn reconcile_remove(&mut self, provider: &str) -> Result<(), HaiderError> {
        match self.get(provider) {
            None => {
                self.model_source.remove(provider);
                Ok(())
            }
            Some(profile) if matches!(profile.provenance, ProviderProvenance::Custom) => {
                self.remove_profile(provider)
            }
            Some(_) => Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!("provider-remove receipt targets release-owned provider `{provider}`"),
                false,
            )),
        }
    }

    fn remove_profile(&mut self, provider: &str) -> Result<(), HaiderError> {
        let mut next = self.profiles.clone();
        next.retain(|profile| profile.provider_id != provider);
        validate_profiles(&next)?;
        self.store.save(&next)?;
        self.profiles = next;
        self.model_source.remove(provider);
        Ok(())
    }

    fn configured_profiles(
        &self,
        input: ProviderConfigureInput,
    ) -> Result<(Vec<ProviderProfileV1>, ProviderProfileV1), HaiderError> {
        let discovered_models = self.discovered_slugs(&input.provider);
        // Discovery stays authoritative once it has run — but a BRAND-NEW
        // provider has nothing discovered yet, and requiring a discovered
        // default before the profile even exists is a chicken-and-egg no
        // create can escape (found live, W5g-5). Until discovery speaks,
        // the caller's STATED models are the inventory — the same rule the
        // legacy replay path has always applied.
        let inventory = if discovered_models.is_empty() {
            normalized_models(input.models.clone())?
        } else {
            discovered_models
        };
        self.configured_profiles_with_inventory(input, &inventory)
    }

    fn configured_profiles_with_inventory(
        &self,
        input: ProviderConfigureInput,
        discovered_models: &[String],
    ) -> Result<(Vec<ProviderProfileV1>, ProviderProfileV1), HaiderError> {
        validate_provider_id(&input.provider)?;
        if matches!(input.trust, Some(ProviderTrustWire::Unknown)) {
            return Err(invalid("provider trust must be full or lockdown"));
        }
        let mut next = self.profiles.clone();
        let profile = if let Some(existing) = next
            .iter_mut()
            .find(|profile| profile.provider_id == input.provider)
        {
            require_matching_identity(existing, &input)?;
            if let Some(auth_requirement) = input
                .auth_requirement
                .filter(|auth_requirement| *auth_requirement != existing.auth_requirement)
            {
                if !matches!(existing.provenance, ProviderProvenance::Custom) {
                    return Err(invalid(format!(
                        "provider `{}` authentication mode is release-owned",
                        existing.provider_id
                    )));
                }
                if !matches!(
                    auth_requirement,
                    ProviderAuthRequirementWire::ApiKey | ProviderAuthRequirementWire::None
                ) {
                    return Err(invalid(
                        "custom providers may require api_key authentication or none",
                    ));
                }
                existing.auth_requirement = auth_requirement;
            }
            // A custom provider's NAME is its stable identity. Its validated
            // origin is mutable metadata, but one endpoint cannot be claimed
            // by two different provider names during a repoint.
            if matches!(existing.provenance, ProviderProvenance::Custom)
                && let Some(origin) = input
                    .origin
                    .as_ref()
                    .filter(|origin| Some(*origin) != existing.base_url.as_ref())
            {
                require_unique_repoint_origin(&self.profiles, &existing.provider_id, origin)?;
                existing.base_url = Some(origin.clone());
            }
            // G4b: an enterprise builtin's origin change (region/project
            // re-configuration) applies only through its shape validator —
            // an off-template URL is refused, never stored.
            if let Some(validator) = enterprise_origin_validator(&existing.provider_id)
                && let Some(origin) = input
                    .origin
                    .as_ref()
                    .filter(|origin| Some(*origin) != existing.base_url.as_ref())
            {
                let validated = validator(origin).map_err(|error| invalid(error.message))?;
                existing.base_url = Some(validated);
            }
            existing.enabled = input.enabled;
            existing.configured_models = normalized_models(input.models)?;
            existing.default_model = validated_default(discovered_models, input.default_model)?;
            if let Some(timeout_ms) = input.response_open_timeout_ms {
                validate_response_open_timeout_ms(timeout_ms)?;
                existing.response_open_timeout_ms = Some(timeout_ms);
            }
            if let Some(timeout_ms) = input.chunk_idle_timeout_ms {
                validate_chunk_idle_timeout_ms(timeout_ms)?;
                existing.chunk_idle_timeout_ms = Some(timeout_ms);
            }
            if let Some(timeout_ms) = input.semantic_progress_timeout_ms {
                validate_semantic_progress_timeout_ms(timeout_ms)?;
                existing.semantic_progress_timeout_ms = Some(timeout_ms);
            }
            if let Some(trust) = input.trust
                && existing.trust != trust
            {
                return Err(invalid(format!(
                    "provider `{}` trust changes must use provider.set_trust",
                    existing.provider_id
                )));
            }
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
            if !matches!(
                api_family,
                ProviderApiFamilyWire::OpenAiChatCompletions
                    | ProviderApiFamilyWire::AnthropicMessages
            ) {
                return Err(invalid(
                    "custom providers must use the openai_chat_completions or anthropic_messages API family",
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
            if let Some(timeout_ms) = input.response_open_timeout_ms {
                validate_response_open_timeout_ms(timeout_ms)?;
            }
            if let Some(timeout_ms) = input.chunk_idle_timeout_ms {
                validate_chunk_idle_timeout_ms(timeout_ms)?;
            }
            if let Some(timeout_ms) = input.semantic_progress_timeout_ms {
                validate_semantic_progress_timeout_ms(timeout_ms)?;
            }
            let profile = ProviderProfileV1 {
                provider_id: input.provider.clone(),
                display_name: input.provider,
                api_family,
                base_url: Some(base_url),
                response_open_timeout_ms: input.response_open_timeout_ms,
                chunk_idle_timeout_ms: input.chunk_idle_timeout_ms,
                semantic_progress_timeout_ms: input.semantic_progress_timeout_ms,
                enabled: input.enabled,
                auth_requirement,
                configured_models,
                default_model,
                promotion_model: None,
                provenance: ProviderProvenance::Custom,
                trust: input.trust.unwrap_or(ProviderTrustWire::Full),
            };
            next.push(profile.clone());
            profile
        };
        // A custom compatible endpoint's catalog is advisory and arrives
        // after configuration/credential commit. It may therefore be empty,
        // or have rows while the user has not selected a default yet.
        let custom_catalog_is_advisory = matches!(profile.provenance, ProviderProvenance::Custom);
        if profile.enabled
            && (discovered_models.is_empty() || profile.default_model.is_none())
            && !custom_catalog_is_advisory
        {
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
            .selectable_slugs(provider)
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
            .map(|model| model.slug)
            .collect()
    }

    /// Selection accepts only known catalog entries: fetched remote rows or
    /// an offline provider's authoritative static/configured catalog.
    fn selectable_slugs(&self, provider: &str) -> Vec<String> {
        let discovered = self.discovered_slugs(provider);
        if discovered.is_empty()
            && let Some(profile) = self.get(provider)
            && offline_inventory(profile)
        {
            return profile.configured_models.clone();
        }
        discovered
    }

    fn discovered_details(&self, provider: &str) -> Vec<DiscoveredModel> {
        self.model_source
            .models(provider)
            .map(|models| pickable(&models))
            .unwrap_or_default()
    }

    fn summary_profile(
        &self,
        profile: &ProviderProfileV1,
        has_credential: &dyn Fn(&str) -> bool,
    ) -> ProviderSummaryWire {
        let inventory = self.model_source.inventory(&profile.provider_id);
        let discovered = inventory
            .models()
            .map(|models| pickable(&models))
            .unwrap_or_default();
        // Offline catalogs are authoritative without a network request.
        // An empty remote result never falls back to configured rows.
        let offline_catalog =
            matches!(inventory, ProviderInventory::NeverFetched) && offline_inventory(profile);
        let model_details = if offline_catalog {
            profile
                .configured_models
                .iter()
                .map(|slug| model_detail_wire(profile, offline_model(&profile.provider_id, slug)))
                .collect()
        } else {
            discovered
                .into_iter()
                .map(|model| model_detail_wire(profile, model))
                .collect()
        };
        provider_summary(
            profile,
            model_details,
            offline_catalog,
            has_credential(&profile.provider_id),
            if offline_catalog {
                haider_rpc::ModelInventoryWire::Static
            } else {
                inventory.provenance()
            },
        )
    }
}

/// Only offline catalogs and explicitly configured Azure deployments can
/// provide rows without a fetch. Remote definitions contain no static list.
fn offline_inventory(profile: &ProviderProfileV1) -> bool {
    matches!(
        haider_provider::provider_catalog_definition(&profile.provider_id),
        haider_provider::ProviderCatalogDefinition::Offline { .. }
    ) || (matches!(profile.provenance, ProviderProvenance::Custom)
        && profile.base_url.as_deref().is_some_and(azure_openai_origin))
}

/// Metadata for an offline catalog entry. Its name is catalog truth, not a
/// manufactured discovery response.
fn offline_model(_provider: &str, slug: &str) -> DiscoveredModel {
    DiscoveredModel {
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        context_window: None,
        max_output_tokens: None,
        description: None,
        default_effort: None,
        supported_efforts: Vec::new(),
        visible: true,
        priority: None,
        extensions: None,
    }
}

/// Projects one discovered model into its wire detail, enriching the G3
/// tuning fields for providers whose CATALOG declares none: anthropic and
/// gemini effort ladders come from the pinned static capability tables, and
/// the anthropic fast gate rides `supported_speeds`. The daemon is the ONE
/// source of this truth — clients hold no tables.
fn model_detail_wire(profile: &ProviderProfileV1, model: DiscoveredModel) -> ModelDetailWire {
    let provider = profile.provider_id.as_str();
    let static_limits = haider_provider::static_model_limits(provider, &model.slug);
    let static_ladder: &[&str] = if model.supported_efforts.is_empty() {
        match provider {
            // G4b: bedrock/vertex serve the same Claude families — the
            // pinned static tables normalize their enterprise spellings.
            ANTHROPIC_PROVIDER_NAME
            | ANTHROPIC_OAUTH_PROVIDER_NAME
            | BEDROCK_PROVIDER_NAME
            | VERTEX_PROVIDER_NAME => haider_provider::anthropic_supported_efforts(&model.slug),
            GEMINI_PROVIDER_NAME => haider_provider::gemini_supported_efforts(&model.slug),
            _ => &[],
        }
    } else {
        &[]
    };
    let supported_efforts = if model.supported_efforts.is_empty() {
        static_ladder
            .iter()
            .map(|level| (*level).to_owned())
            .collect()
    } else {
        model.supported_efforts.clone()
    };
    let default_effort = model.default_effort.clone().or_else(|| match provider {
        ANTHROPIC_PROVIDER_NAME
        | ANTHROPIC_OAUTH_PROVIDER_NAME
        | BEDROCK_PROVIDER_NAME
        | VERTEX_PROVIDER_NAME => {
            haider_provider::anthropic_default_effort(&model.slug).map(str::to_owned)
        }
        GEMINI_PROVIDER_NAME => {
            haider_provider::gemini_default_effort(&model.slug).map(str::to_owned)
        }
        _ => None,
    });
    // FAST stays Claude-API-only (G4b decision 4, LE-x): bedrock and vertex
    // details NEVER advertise `supported_speeds`, even for models inside
    // the static fast gate — the research found no fast mode on either.
    let supported_speeds = if matches!(
        provider,
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME
    ) && haider_provider::anthropic_fast_mode_supported(&model.slug)
    {
        vec!["fast".to_owned()]
    } else {
        Vec::new()
    };
    let context_window = model
        .context_window
        .or(static_limits.context_window)
        .or_else(|| {
            (provider == XAI_PROVIDER_NAME)
                .then(|| {
                    XAI_SEED_MODEL_CONTEXT_WINDOWS
                        .iter()
                        .find_map(|(slug, window)| (*slug == model.slug).then_some(*window))
                })
                .flatten()
        });
    let max_output_tokens = model
        .max_output_tokens
        .unwrap_or(static_limits.max_output_tokens);
    let max_output_tokens = max_output_tokens.min(haider_provider::MAX_OUTPUT_LIMIT);
    let max_output_tokens = context_window.map_or(max_output_tokens, |context_window| {
        max_output_tokens.min(context_window)
    });
    ModelDetailWire {
        name: model.slug.clone(),
        display_name: Some(model.display_name),
        context_window,
        max_output_tokens: Some(max_output_tokens),
        supported_efforts,
        default_effort,
        supported_speeds,
        supports_thinking_type: model
            .extensions
            .as_ref()
            .map(|extensions| extensions.supports_thinking_type),
        // 970 owner bug 2: project the adapters' own `vision` fact into the
        // catalog row so the composer can refuse a pasted image BEFORE it
        // costs a turn. The provider's per-model declaration wins; the
        // documented per-family list fills the rest. A family that only
        // serves USER-CONTROLLED slugs projects nothing at all — absence is
        // "undeclared", never a refusal, and the daemon stays the authority.
        supports_vision: haider_provider::vision_capability(
            provider,
            model
                .extensions
                .as_ref()
                .map(|extensions| extensions.supports_vision),
        )
        .map(|resolve| resolve != haider_protocol::provider::FeatureResolve::Unsupported),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_open_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_idle_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_progress_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<ProviderTrustWire>,
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
    offline_catalog: bool,
    credentialed: bool,
    inventory: haider_rpc::ModelInventoryWire,
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
    // G4b (decision 6, LA-x): a SEEDED inventory carries no proof the
    // endpoint answers — there is no discovery for these surfaces — so it
    // lights Available only once a credential exists AND the profile has an
    // endpoint to serve from (vertex seeds without one until its card runs).
    let offline_ready = !offline_catalog || (credentialed && profile.base_url.is_some());
    let inventory_is_current = matches!(
        &inventory,
        haider_rpc::ModelInventoryWire::Static
            | haider_rpc::ModelInventoryWire::Configured
            | haider_rpc::ModelInventoryWire::Fetched { .. }
    );
    let available = profile.enabled
        && !matches!(profile.api_family, ProviderApiFamilyWire::Unknown)
        && !discovered_models.is_empty()
        && offline_ready
        && inventory_is_current;
    let default_model = if matches!(profile.provenance, ProviderProvenance::Custom)
        && matches!(
            profile.api_family,
            ProviderApiFamilyWire::OpenAiChatCompletions
        ) {
        // A custom compatible endpoint owns its wire model vocabulary.
        // Discovery remains advisory inventory for pickers and availability;
        // it cannot rewrite or erase a configured passthrough id. When the
        // user has not selected one yet, the durable discovered order gives
        // the same first-row default the former synchronous create returned.
        profile
            .default_model
            .clone()
            .or_else(|| discovered_models.first().cloned())
    } else {
        profile
            .default_model
            .as_ref()
            .filter(|default| discovered_models.iter().any(|model| model == *default))
            .cloned()
            .or_else(|| {
                matches!(
                    haider_provider::provider_catalog_definition(&profile.provider_id),
                    haider_provider::ProviderCatalogDefinition::Public { .. }
                )
                .then(|| discovered_models.first().cloned())
                .flatten()
            })
    };
    let availability_reason = (!available).then(|| {
        if matches!(profile.provenance, ProviderProvenance::Unknown) {
            "provider adapter is not registered".to_owned()
        } else if !profile.enabled {
            "provider is disabled".to_owned()
        } else if matches!(profile.api_family, ProviderApiFamilyWire::Unknown) {
            "provider API family is unavailable".to_owned()
        } else if offline_catalog && profile.base_url.is_none() {
            "provider endpoint is not configured".to_owned()
        } else if offline_catalog && !credentialed {
            "provider has no credential".to_owned()
        } else if let haider_rpc::ModelInventoryWire::Stale { reason, .. }
        | haider_rpc::ModelInventoryWire::Unavailable { reason } = &inventory
        {
            reason.clone()
        } else {
            "provider model inventory is unavailable".to_owned()
        }
    });
    ProviderSummaryWire {
        provider: profile.provider_id.clone(),
        api_family: profile.api_family,
        endpoint: profile.base_url.clone(),
        response_open_timeout_ms: profile.response_open_timeout_ms,
        chunk_idle_timeout_ms: profile.chunk_idle_timeout_ms,
        semantic_progress_timeout_ms: profile.semantic_progress_timeout_ms,
        models: discovered_models,
        model_details,
        inventory,
        catalog: match haider_provider::provider_catalog_definition(&profile.provider_id) {
            haider_provider::ProviderCatalogDefinition::Public { .. } => {
                haider_rpc::ProviderCatalogKindWire::Public
            }
            haider_provider::ProviderCatalogDefinition::Authenticated { .. } => {
                haider_rpc::ProviderCatalogKindWire::Authenticated
            }
            haider_provider::ProviderCatalogDefinition::Offline { .. } => {
                haider_rpc::ProviderCatalogKindWire::Offline
            }
            haider_provider::ProviderCatalogDefinition::Adapter
                if matches!(profile.provenance, ProviderProvenance::Custom) =>
            {
                haider_rpc::ProviderCatalogKindWire::Custom
            }
            haider_provider::ProviderCatalogDefinition::Adapter => {
                haider_rpc::ProviderCatalogKindWire::Adapter
            }
        },
        inventory_authority: match profile.provenance {
            ProviderProvenance::BuiltIn => haider_rpc::ModelInventoryAuthorityWire::Authoritative,
            ProviderProvenance::Custom => haider_rpc::ModelInventoryAuthorityWire::Advisory,
            ProviderProvenance::Unknown => haider_rpc::ModelInventoryAuthorityWire::Unknown,
        },
        auth_methods,
        availability: if available {
            ProviderAvailabilityWire::Available
        } else {
            ProviderAvailabilityWire::Unavailable
        },
        availability_reason,
        default_model,
        enabled: profile.enabled,
        trust: profile.trust,
    }
}

fn builtin_or_unknown(provider: &str, anthropic_default_model: &str) -> ProviderProfileV1 {
    let _ = anthropic_default_model;
    // G4b enterprise builtins seed a CONFIGURED model inventory (neither
    // surface exposes a models API) plus, for bedrock, the default-region
    // mantle endpoint. Vertex seeds NO endpoint — its card must supply the
    // project/location before the profile can serve.
    if provider == BEDROCK_PROVIDER_NAME {
        return ProviderProfileV1 {
            provider_id: provider.to_owned(),
            display_name: provider.to_owned(),
            api_family: ProviderApiFamilyWire::AnthropicMessages,
            base_url: Some(BEDROCK_MANTLE_DEFAULT_BASE_URL.to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: BEDROCK_SEED_MODELS
                .iter()
                .map(|slug| (*slug).to_owned())
                .collect(),
            default_model: Some(BEDROCK_SEED_MODELS[0].to_owned()),
            promotion_model: None,
            provenance: ProviderProvenance::BuiltIn,
            trust: ProviderTrustWire::Full,
        };
    }
    if provider == VERTEX_PROVIDER_NAME {
        return ProviderProfileV1 {
            provider_id: provider.to_owned(),
            display_name: provider.to_owned(),
            api_family: ProviderApiFamilyWire::AnthropicMessages,
            base_url: None,
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: VERTEX_SEED_MODELS
                .iter()
                .map(|slug| (*slug).to_owned())
                .collect(),
            default_model: Some(VERTEX_SEED_MODELS[0].to_owned()),
            promotion_model: None,
            provenance: ProviderProvenance::BuiltIn,
            trust: ProviderTrustWire::Full,
        };
    }
    if provider == DEEPSEEK_PROVIDER_NAME {
        return ProviderProfileV1 {
            provider_id: provider.to_owned(),
            display_name: provider.to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some(DEEPSEEK_BASE_URL.to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: Vec::new(),
            default_model: None,
            promotion_model: None,
            provenance: ProviderProvenance::BuiltIn,
            trust: ProviderTrustWire::Full,
        };
    }
    if provider == XAI_PROVIDER_NAME {
        return ProviderProfileV1 {
            provider_id: provider.to_owned(),
            display_name: provider.to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some(XAI_BASE_URL.to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: Vec::new(),
            default_model: None,
            promotion_model: None,
            provenance: ProviderProvenance::BuiltIn,
            trust: ProviderTrustWire::Full,
        };
    }
    if provider == HAIDER_CODE_PROVIDER_NAME {
        return ProviderProfileV1 {
            provider_id: provider.to_owned(),
            display_name: provider.to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some(HAIDER_CODE_BASE_URL.to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: Vec::new(),
            default_model: None,
            promotion_model: None,
            provenance: ProviderProvenance::BuiltIn,
            trust: ProviderTrustWire::Full,
        };
    }
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
        KIMI_OAUTH_PROVIDER_NAME => (
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some(KIMI_OAUTH_BASE_URL.to_owned()),
            ProviderAuthRequirementWire::OAuth,
            true,
            ProviderProvenance::BuiltIn,
        ),
        // The Grok CLI lane follows the KIMI law, not the DeepSeek one: a
        // subscription proxy's catalog is the ONLY inventory truth (the CLI
        // itself is live-cataloged), so the profile boots inventory-empty
        // and the first signed-in connection's discovery fills it. A seed
        // list here would fill the summary and SUPPRESS the W5f-2d
        // auto-discovery trigger — the exact bug this arm replaces.
        GROK_OAUTH_PROVIDER_NAME => (
            ProviderApiFamilyWire::OpenAiChatCompletions,
            Some(GROK_OAUTH_BASE_URL.to_owned()),
            ProviderAuthRequirementWire::OAuth,
            true,
            ProviderProvenance::BuiltIn,
        ),
        GEMINI_PROVIDER_NAME => (
            ProviderApiFamilyWire::GeminiGenerateContent,
            Some(GEMINI_API_BASE_URL.to_owned()),
            ProviderAuthRequirementWire::ApiKey,
            true,
            ProviderProvenance::BuiltIn,
        ),
        // v0.0.970: the supervised Google Antigravity agent. This is an
        // EXECUTION KIND, not a transport — Haider launches Google's own
        // `antigravity-acp` executable and speaks newline-delimited JSON-RPC
        // to it over stdio — so there is NO base URL to seed and none is
        // invented. It follows the KIMI/Grok inventory law rather than the
        // DeepSeek one: a subscription agent's own catalog is the only
        // inventory truth, so the profile boots inventory-empty with no
        // default model and the first authenticated connection's discovery
        // fills it. A seed list here would fill the summary and SUPPRESS the
        // W5f-2d auto-discovery trigger. The API-key `gemini` arm directly
        // above is untouched: these are two separate provider classes.
        GOOGLE_ANTIGRAVITY_PROVIDER_NAME => (
            ProviderApiFamilyWire::AcpAgent,
            None,
            ProviderAuthRequirementWire::OAuth,
            true,
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
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        enabled,
        auth_requirement,
        configured_models: Vec::new(),
        default_model: None,
        promotion_model: None,
        provenance,
        trust: ProviderTrustWire::Full,
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
            // Custom-provider names are the stable identity, so their
            // origins are mutable after the actor validates them through
            // the same endpoint guard used on create. G4b enterprise
            // builtins likewise have mutable origins pinned by their
            // URL-shape validators.
            && !matches!(existing.provenance, ProviderProvenance::Custom)
            && enterprise_origin_validator(&existing.provider_id).is_none()
    {
        return Err(invalid(format!(
            "provider `{}` cannot change its API family, and its origin is mutable only when the provider supports repointing",
            existing.provider_id
        )));
    }
    Ok(())
}

fn require_unique_repoint_origin(
    profiles: &[ProviderProfileV1],
    provider_id: &str,
    origin: &str,
) -> Result<(), HaiderError> {
    if let Some(duplicate) = profiles.iter().find(|profile| {
        profile.provider_id != provider_id && profile.base_url.as_deref() == Some(origin)
    }) {
        return Err(invalid(format!(
            "provider origin `{origin}` is already registered to provider `{}`",
            duplicate.provider_id
        )));
    }
    Ok(())
}

type EnterpriseOriginValidator = fn(&str) -> Result<String, haider_provider::ProviderError>;

/// The shape validator that governs an enterprise builtin's mutable origin
/// (G4b): bedrock accepts only the mantle template (LB2's shape authority),
/// vertex only the publishers-models template. Every other provider gets
/// `None` — origins stay create-only for them.
fn enterprise_origin_validator(provider_id: &str) -> Option<EnterpriseOriginValidator> {
    match provider_id {
        BEDROCK_PROVIDER_NAME => Some(haider_provider::validate_bedrock_mantle_base_url),
        VERTEX_PROVIDER_NAME => Some(haider_provider::validate_vertex_models_base_url),
        _ => None,
    }
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

fn read_env_override(name: &str) -> Result<Option<String>, HaiderError> {
    std::env::var_os(name)
        .map(|value| {
            value.into_string().map_err(|_| {
                invalid(format!(
                    "environment override `{name}` must contain valid UTF-8"
                ))
            })
        })
        .transpose()
}

fn parse_fallback_chain(
    raw_entries: &[String],
    comma_separated: bool,
    profiles: &[ProviderProfileV1],
) -> Result<Vec<ProviderTargetV1>, HaiderError> {
    let entries = if comma_separated {
        let raw = raw_entries.first().map_or("", String::as_str);
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        raw.split(',').collect::<Vec<_>>()
    } else {
        raw_entries.iter().map(String::as_str).collect::<Vec<_>>()
    };
    entries
        .into_iter()
        .map(|entry| parse_fallback_target(entry, profiles))
        .collect()
}

fn resolve_fallback_chain(
    persisted: &[String],
    environment: Option<&str>,
    profiles: &[ProviderProfileV1],
) -> Result<Vec<ProviderTargetV1>, HaiderError> {
    match environment {
        Some(value) => parse_fallback_chain(&[value.to_owned()], true, profiles),
        None => parse_fallback_chain(persisted, false, profiles),
    }
}

fn parse_fallback_target(
    entry: &str,
    profiles: &[ProviderProfileV1],
) -> Result<ProviderTargetV1, HaiderError> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err(invalid("fallback chain entries must not be empty"));
    }
    let (provider, model) = match entry.split_once('/') {
        Some((provider, model)) => {
            let model = model.trim();
            if model.is_empty() {
                return Err(invalid(format!(
                    "fallback chain entry `{entry}` has an empty model"
                )));
            }
            (provider.trim(), Some(model.to_owned()))
        }
        None => (entry, None),
    };
    require_known_provider(provider, profiles, "fallback chain")?;
    Ok(ProviderTargetV1 {
        provider: provider.to_owned(),
        model,
    })
}

fn parse_promotion_override(
    value: &str,
    profiles: &[ProviderProfileV1],
) -> Result<PromotionOverrideV1, HaiderError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid(format!(
            "environment override `{COMPACTION_PROMOTION_ENV}` must name a model"
        )));
    }
    let (provider, model) = match value.split_once('/') {
        Some((provider, model)) => {
            let provider = provider.trim();
            require_known_provider(provider, profiles, "compaction promotion")?;
            (Some(provider.to_owned()), model.trim())
        }
        None => (None, value),
    };
    if model.is_empty() {
        return Err(invalid(format!(
            "environment override `{COMPACTION_PROMOTION_ENV}` has an empty model"
        )));
    }
    Ok(PromotionOverrideV1 {
        provider,
        model: model.to_owned(),
    })
}

fn require_known_provider(
    provider: &str,
    profiles: &[ProviderProfileV1],
    setting: &str,
) -> Result<(), HaiderError> {
    if profiles
        .iter()
        .any(|profile| profile.provider_id == provider)
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "{setting} provider `{provider}` is not registered"
        )))
    }
}

fn validate_profiles(profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
    let mut ids = std::collections::HashSet::new();
    for profile in profiles {
        validate_provider_id(&profile.provider_id)?;
        if matches!(profile.trust, ProviderTrustWire::Unknown) {
            return Err(invalid(format!(
                "provider `{}` trust must be full or lockdown",
                profile.provider_id
            )));
        }
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
        if profile
            .promotion_model
            .as_deref()
            .is_some_and(|model| model.is_empty() || model.trim() != model)
        {
            return Err(invalid(format!(
                "provider `{}` promotion model must be non-empty and trimmed",
                profile.provider_id
            )));
        }
        if let Some(timeout_ms) = profile.response_open_timeout_ms {
            validate_response_open_timeout_ms(timeout_ms)?;
        }
        if let Some(timeout_ms) = profile.chunk_idle_timeout_ms {
            validate_chunk_idle_timeout_ms(timeout_ms)?;
        }
        if let Some(timeout_ms) = profile.semantic_progress_timeout_ms {
            validate_semantic_progress_timeout_ms(timeout_ms)?;
        }
    }
    Ok(())
}

fn validate_response_open_timeout_ms(timeout_ms: u64) -> Result<(), HaiderError> {
    if timeout_ms == 0 {
        return Err(invalid(
            "provider response_open_timeout_ms must be greater than zero",
        ));
    }
    Ok(())
}

fn validate_chunk_idle_timeout_ms(timeout_ms: u64) -> Result<(), HaiderError> {
    if timeout_ms == 0 {
        return Err(invalid(
            "provider chunk_idle_timeout_ms must be greater than zero",
        ));
    }
    Ok(())
}

fn validate_semantic_progress_timeout_ms(timeout_ms: u64) -> Result<(), HaiderError> {
    if timeout_ms == 0 {
        return Err(invalid(
            "provider semantic_progress_timeout_ms must be greater than zero",
        ));
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
