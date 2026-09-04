//! Provider-agnostic model selection — the ONE resolution/validation truth.
//!
//! CONTRACT (owner, F1): sessions are provider-agnostic. The user selects a
//! MODEL; the provider rides along implicitly as an attribute of the chosen
//! model row — each pickable row is a model×provider pair, which is what
//! keeps metering honest. The session's stored provider/model pair is
//! plumbing (the CURRENT selection), never session identity. Error copy
//! therefore speaks "model selection", not "changing the session's
//! provider", and future lanes must not reintroduce provider-as-identity.
//!
//! Both consumers resolve through this module so they cannot drift:
//!
//! - `session.select_model` (live-session switch, `session_hub/rpc.rs`)
//! - the `spawn_subagent` model selector (child pair resolution, `worker.rs`)
//!
//! Validation truths, in authority order:
//!
//! 1. **Creatability** — the D3-5 creatable-provider registry plus enabled
//!    custom chat-completions profiles, exactly the `session.create` rule.
//!    Selecting a row on the session's (or parent's) CURRENT provider never
//!    consults creatability: the session already runs it.
//! 2. **Known inventory** — a built-in provider with a non-empty discovered
//!    model list must contain the selected model. Custom compatible catalogs
//!    are advisory: a miss is typed `Unlisted` but the passthrough id remains
//!    admissible. A provider WITHOUT a discovered inventory accepts honestly;
//!    provider errors surface at turn time.

use haider_rpc::{
    ModelInventoryAuthorityWire, ModelInventoryStatusWire, ProviderApiFamilyWire,
    ProviderAvailabilityWire, ProviderSummaryWire,
};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
#[path = "model_select_tests.rs"]
mod model_select_tests;

/// Immutable snapshot of everything a selection decision may consult.
pub(crate) struct ModelSelectionAuthority {
    /// The installed `session.create` whitelist; `None` means the registry is
    /// not installed (startup edge) and no cross-provider row is creatable.
    creatable: Option<BTreeSet<String>>,
    /// Management-snapshot provider summaries (inventory + availability).
    /// Empty when no account facade is installed: every inventory is then
    /// unknown and selection stays honest rather than guessing.
    summaries: Vec<ProviderSummaryWire>,
}

/// One admitted explicit selection plus its honest relationship to the
/// provider's latest inventory. `Unlisted` never mutates the provider summary
/// or creates a pickable model row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedModelSelection {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) inventory_status: ModelInventoryStatusWire,
}

/// One agent-facing row projected from the same provider summary and model
/// detail that drive the TUI picker. Unknown declarations remain `None`
/// instead of being flattened into a false capability claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ModelCatalogRow {
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) availability: ProviderAvailabilityWire,
    pub(crate) inventory_age_ms: Option<u64>,
    pub(crate) capabilities: ModelCatalogCapabilities,
    pub(crate) aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ModelCatalogCapabilities {
    pub(crate) vision: Option<bool>,
    pub(crate) fast: bool,
    pub(crate) pdf: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ModelCatalogPage {
    pub(crate) models: Vec<ModelCatalogRow>,
    pub(crate) truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hint: Option<String>,
}

impl ValidatedModelSelection {
    fn into_pair(self) -> (String, String) {
        (self.provider, self.model)
    }
}

/// A typed G3 tuning refusal beside [`SelectionRefusal`]: `/effort` and
/// `/fast` refuse in pair-capability terms, never with a silent no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TuningRefusal {
    /// The requested effort is not in the CURRENT pair's declared ladder.
    /// `supported` is the exact ladder consulted — EMPTY means the pair
    /// declares no effort vocabulary at all.
    EffortUnsupported {
        provider: String,
        model: String,
        effort: String,
        supported: Vec<String>,
    },
    /// Fast mode was requested on a pair outside the static fast gate.
    FastUnsupported { provider: String, model: String },
}

impl TuningRefusal {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::EffortUnsupported {
                provider,
                model,
                effort,
                supported,
            } => {
                if supported.is_empty() {
                    format!(
                        "pair `{model}` · `{provider}` declares no effort ladder — \
                         effort `{effort}` cannot be validated"
                    )
                } else {
                    format!(
                        "effort `{effort}` is not in pair `{model}` · `{provider}`'s ladder ({})",
                        supported.join(", ")
                    )
                }
            }
            Self::FastUnsupported { provider, model } => {
                format!("pair `{model}` · `{provider}` does not support fast mode")
            }
        }
    }
}

/// A typed selection refusal. Every variant names WHY in selection terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectionRefusal {
    /// The selected row's provider attribute is not creatable on this daemon.
    ProviderUnavailable { provider: String },
    /// The provider has a KNOWN discovered inventory and the model is not in
    /// it.
    ModelUnknown {
        provider: String,
        model: String,
        inventory_age_ms: Option<u64>,
        suggestions: Vec<String>,
    },
    /// A bare model selector resolved to zero or several provider rows;
    /// `candidates` names every available row so the caller can retry with an
    /// explicit pair.
    ModelNotResolvable {
        model: String,
        candidates: Vec<String>,
        suggestions: Vec<String>,
    },
    /// The selector itself is malformed (empty model, provider without a
    /// model, …).
    InvalidSelector { message: String },
}

impl SelectionRefusal {
    /// Stable machine-readable kind, shared by wire error details and tool
    /// rejection previews.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::ProviderUnavailable { .. } => "provider_unavailable",
            Self::ModelUnknown { .. } => "model_unknown",
            Self::ModelNotResolvable { .. } => "model_not_resolvable",
            Self::InvalidSelector { .. } => "invalid_selector",
        }
    }

    /// Human copy — model-selection language, never provider-switch language.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::ProviderUnavailable { provider } => {
                format!("this model row's provider `{provider}` is not available on this daemon")
            }
            Self::ModelUnknown {
                provider,
                model,
                suggestions,
                ..
            } => {
                let mut message = format!(
                    "model `{model}` is not in provider `{provider}`'s discovered inventory"
                );
                append_suggestion_guidance(&mut message, suggestions);
                message
            }
            Self::ModelNotResolvable {
                model,
                candidates,
                suggestions,
            } => {
                if candidates.is_empty() {
                    let mut message = format!("no available provider serves model `{model}`");
                    append_suggestion_guidance(&mut message, suggestions);
                    message
                } else {
                    format!(
                        "model `{model}` is served by several providers ({}) — retry naming one \
                         explicitly",
                        candidates.join(", ")
                    )
                }
            }
            Self::InvalidSelector { message } => message.clone(),
        }
    }

    /// Structured coordinates for wire `details` / tool previews. Static
    /// vocabulary plus the caller's own selector strings — never provider
    /// response bytes.
    pub(crate) fn details(&self) -> serde_json::Value {
        match self {
            Self::ProviderUnavailable { provider } => serde_json::json!({
                "kind": self.kind(),
                "provider": provider,
                "suggestions": [],
            }),
            Self::ModelUnknown {
                provider,
                model,
                inventory_age_ms,
                suggestions,
            } => serde_json::json!({
                "kind": self.kind(),
                "provider": provider,
                "model": model,
                "inventory_age": inventory_age_ms,
                "suggestions": suggestions,
            }),
            Self::ModelNotResolvable {
                model,
                candidates,
                suggestions,
            } => serde_json::json!({
                "kind": self.kind(),
                "model": model,
                "candidates": candidates,
                "suggestions": suggestions,
            }),
            Self::InvalidSelector { message } => serde_json::json!({
                "kind": self.kind(),
                "message": message,
                "suggestions": [],
            }),
        }
    }
}

impl ModelSelectionAuthority {
    pub(crate) fn new(
        creatable: Option<BTreeSet<String>>,
        summaries: Vec<ProviderSummaryWire>,
    ) -> Self {
        Self {
            creatable,
            summaries,
        }
    }

    /// The `session.create` creatability rule: the installed static registry,
    /// or an enabled custom OpenAI/Anthropic profile.
    fn provider_is_creatable(&self, provider: &str) -> bool {
        let static_creatable = self
            .creatable
            .as_ref()
            .is_some_and(|providers| providers.contains(provider));
        static_creatable
            || self.summaries.iter().any(|summary| {
                summary.provider == provider
                    && summary.enabled
                    && matches!(
                        summary.api_family,
                        ProviderApiFamilyWire::OpenAiChatCompletions
                            | ProviderApiFamilyWire::AnthropicMessages
                    )
            })
    }

    /// `Some(models)` exactly when the provider has a KNOWN (non-empty)
    /// discovered inventory.
    fn known_inventory(&self, provider: &str) -> Option<&[String]> {
        self.summaries
            .iter()
            .find(|summary| summary.provider == provider)
            .map(|summary| summary.models.as_slice())
            .filter(|models| !models.is_empty())
    }

    fn inventory_age_ms(&self, provider: &str) -> Option<u64> {
        let fetched_at_ms = self
            .summaries
            .iter()
            .find(|summary| summary.provider == provider)?
            .inventory_fetched_at_ms?;
        Some(unix_time_ms().saturating_sub(fetched_at_ms))
    }

    /// Bounded local catalog projection. The management summaries were
    /// already discovered and published by the account actor; this path only
    /// reads them and can never refresh or probe a provider.
    pub(crate) fn model_catalog(&self, filter: Option<&str>, cap: usize) -> ModelCatalogPage {
        self.model_catalog_at(filter, cap, unix_time_ms())
    }

    fn model_catalog_at(&self, filter: Option<&str>, cap: usize, now_ms: u64) -> ModelCatalogPage {
        let filter = filter
            .map(str::trim)
            .filter(|filter| !filter.is_empty())
            .map(str::to_lowercase);
        let mut models = Vec::new();
        let mut truncated = false;
        'providers: for summary in &self.summaries {
            let inventory_age_ms = summary
                .inventory_fetched_at_ms
                .map(|fetched_at_ms| now_ms.saturating_sub(fetched_at_ms));
            for model in &summary.models {
                let detail = summary
                    .model_details
                    .iter()
                    .find(|detail| detail.name == *model);
                let aliases = detail
                    .and_then(|detail| detail.display_name.as_ref())
                    .filter(|display_name| {
                        !display_name.trim().is_empty() && *display_name != model
                    })
                    .cloned()
                    .into_iter()
                    .collect::<Vec<_>>();
                if filter.as_ref().is_some_and(|filter| {
                    !model.to_lowercase().contains(filter)
                        && !summary.provider.to_lowercase().contains(filter)
                        && !aliases
                            .iter()
                            .any(|alias| alias.to_lowercase().contains(filter))
                }) {
                    continue;
                }
                if models.len() == cap {
                    truncated = true;
                    break 'providers;
                }
                models.push(ModelCatalogRow {
                    model: model.clone(),
                    provider: summary.provider.clone(),
                    availability: summary.availability,
                    inventory_age_ms,
                    capabilities: ModelCatalogCapabilities {
                        vision: detail.and_then(|detail| detail.supports_vision),
                        fast: detail.is_some_and(|detail| {
                            detail.supported_speeds.iter().any(|speed| speed == "fast")
                        }),
                        pdf: !matches!(
                            haider_provider::pdf_document_capability(&summary.provider),
                            haider_protocol::provider::FeatureResolve::Unsupported
                        ),
                        context_window: detail.and_then(|detail| detail.context_window),
                    },
                    aliases,
                });
            }
        }
        ModelCatalogPage {
            models,
            truncated,
            hint: truncated.then(|| {
                format!(
                    "catalog truncated at {cap} rows; call list_models again with a model, provider, or alias filter"
                )
            }),
        }
    }

    fn suggestions_for_provider(&self, model: &str, provider: &str) -> Vec<String> {
        let rows = self
            .summaries
            .iter()
            .filter(|summary| summary.provider == provider)
            .flat_map(|summary| {
                summary
                    .models
                    .iter()
                    .map(move |known| (known.as_str(), summary.provider.as_str()))
            });
        nearest_model_rows(model, rows)
    }

    fn suggestions_for_bare(&self, model: &str) -> Vec<String> {
        // Guidance spans the visible catalog, including rows that cannot be
        // auto-selected. Showing such a row never widens creatability: the
        // actual resolver below still requires an available configured
        // provider, and an explicit retry retains provider_unavailable.
        let rows = self.summaries.iter().flat_map(|summary| {
            summary
                .models
                .iter()
                .map(move |known| (known.as_str(), summary.provider.as_str()))
        });
        nearest_model_rows(model, rows)
    }

    /// Validates one explicit selection for a session currently on
    /// `current_provider`. An absent `requested_provider` keeps today's
    /// behavior: the model is selected within the current provider.
    ///
    /// Returns the RESOLVED (provider, model) pair.
    pub(crate) fn validate_selection(
        &self,
        current_provider: &str,
        requested_provider: Option<&str>,
        model: &str,
    ) -> Result<(String, String), SelectionRefusal> {
        self.validate_selection_with_status(current_provider, requested_provider, model)
            .map(ValidatedModelSelection::into_pair)
    }

    /// The selection decision with the typed inventory telemetry retained for
    /// daemon/UI consumers. Only an explicitly advisory custom inventory may
    /// admit `Unlisted`; unknown and authoritative inventories keep the
    /// built-in `ModelUnknown` refusal.
    pub(crate) fn validate_selection_with_status(
        &self,
        current_provider: &str,
        requested_provider: Option<&str>,
        model: &str,
    ) -> Result<ValidatedModelSelection, SelectionRefusal> {
        let requested_model = model.trim();
        if requested_model.is_empty() {
            return Err(SelectionRefusal::InvalidSelector {
                message: "model selection must name a model".to_owned(),
            });
        }
        let provider = requested_provider
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .unwrap_or(current_provider);
        // Staying on the current provider is not a create; only a row on a
        // DIFFERENT provider consults the creatability authority.
        if provider != current_provider && !self.provider_is_creatable(provider) {
            return Err(SelectionRefusal::ProviderUnavailable {
                provider: provider.to_owned(),
            });
        }
        let summary = self
            .summaries
            .iter()
            .find(|summary| summary.provider == provider);
        let resolved_model = summary
            .and_then(|summary| {
                summary
                    .models
                    .iter()
                    .find(|known| known.as_str() == requested_model)
                    .or_else(|| {
                        let normalized = normalize_model_selector(requested_model);
                        let mut matches = summary
                            .models
                            .iter()
                            .filter(|known| normalize_model_selector(known) == normalized);
                        let only = matches.next()?;
                        matches.next().is_none().then_some(only)
                    })
            })
            .map_or_else(|| requested_model.to_owned(), Clone::clone);
        let inventory_status = summary.map_or(ModelInventoryStatusWire::Unknown, |summary| {
            summary.model_inventory_status(&resolved_model)
        });
        if matches!(inventory_status, ModelInventoryStatusWire::Unlisted)
            && !summary.is_some_and(|summary| {
                matches!(
                    summary.inventory_authority,
                    ModelInventoryAuthorityWire::Advisory
                )
            })
        {
            return Err(SelectionRefusal::ModelUnknown {
                provider: provider.to_owned(),
                model: requested_model.to_owned(),
                inventory_age_ms: self.inventory_age_ms(provider),
                suggestions: self.suggestions_for_provider(requested_model, provider),
            });
        }
        Ok(ValidatedModelSelection {
            provider: provider.to_owned(),
            model: resolved_model,
            inventory_status,
        })
    }

    /// The CURRENT pair's declared effort ladder (G3), in validation-truth
    /// order: the management projection's per-model detail (the registry
    /// enriches anthropic/gemini rows from the pinned static tables when
    /// their catalogs declare none), then — for the static-table families
    /// only — the table itself, so an anthropic/gemini pair validates even
    /// before a management snapshot exists. Everything else honestly gets
    /// the EMPTY ladder.
    pub(crate) fn effort_ladder(&self, provider: &str, model: &str) -> Vec<String> {
        let declared = self
            .summaries
            .iter()
            .find(|summary| summary.provider == provider)
            .and_then(|summary| {
                summary
                    .model_details
                    .iter()
                    .find(|detail| detail.name == model)
            })
            .map(|detail| detail.supported_efforts.clone());
        if let Some(ladder) = declared.filter(|ladder| !ladder.is_empty()) {
            return ladder;
        }
        match provider {
            // G4b: bedrock/vertex serve the same Claude families — the
            // static tables normalize their enterprise spellings
            // (`anthropic.` prefix, `@date` suffix), so `/effort`
            // validates on those pairs too.
            haider_provider::ANTHROPIC_PROVIDER_NAME
            | haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME
            | haider_provider::BEDROCK_PROVIDER_NAME
            | haider_provider::VERTEX_PROVIDER_NAME => {
                haider_provider::anthropic_supported_efforts(model)
                    .iter()
                    .map(|level| (*level).to_owned())
                    .collect()
            }
            haider_provider::GEMINI_PROVIDER_NAME => {
                haider_provider::gemini_supported_efforts(model)
                    .iter()
                    .map(|level| (*level).to_owned())
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Validates one explicit effort selection against the CURRENT pair's
    /// declared ladder (G3). `None` — revert to the provider default — is
    /// always valid; a present value must be IN the ladder, and an empty
    /// ladder refuses every value honestly.
    pub(crate) fn validate_effort(
        &self,
        provider: &str,
        model: &str,
        effort: Option<&str>,
    ) -> Result<(), TuningRefusal> {
        let Some(effort) = effort else {
            return Ok(());
        };
        let supported = self.effort_ladder(provider, model);
        if supported.iter().any(|level| level == effort) {
            Ok(())
        } else {
            Err(TuningRefusal::EffortUnsupported {
                provider: provider.to_owned(),
                model: model.to_owned(),
                effort: effort.to_owned(),
                supported,
            })
        }
    }

    /// Validates one fast-mode toggle (G3). Disabling is always accepted —
    /// recovery must never be gated. Enabling requires an anthropic-family
    /// pair inside the pinned static fast gate.
    pub(crate) fn validate_fast(
        &self,
        provider: &str,
        model: &str,
        enabled: bool,
    ) -> Result<(), TuningRefusal> {
        if !enabled {
            return Ok(());
        }
        let supported = matches!(
            provider,
            haider_provider::ANTHROPIC_PROVIDER_NAME
                | haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME
        ) && haider_provider::anthropic_fast_mode_supported(model);
        if supported {
            Ok(())
        } else {
            Err(TuningRefusal::FastUnsupported {
                provider: provider.to_owned(),
                model: model.to_owned(),
            })
        }
    }

    /// Resolves a `spawn_subagent` model selector to the child's pair.
    ///
    /// - both absent → the child inherits the parent's CURRENT pair;
    /// - bare model → the parent's own provider is preferred when its known
    ///   inventory serves the model; otherwise exactly one available provider
    ///   serving it wins; zero or several is a typed refusal naming the
    ///   candidates so the caller retries with an explicit pair;
    /// - model + provider → the explicit pair, validated like a live-session
    ///   selection;
    /// - provider without model → refused (the selector is the MODEL; the
    ///   provider only disambiguates it).
    pub(crate) fn resolve_child_selector(
        &self,
        parent_provider: &str,
        parent_model: &str,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> Result<(String, String), SelectionRefusal> {
        let model = model.map(str::trim).filter(|model| !model.is_empty());
        let provider = provider
            .map(str::trim)
            .filter(|provider| !provider.is_empty());
        match (model, provider) {
            (None, None) => Ok((parent_provider.to_owned(), parent_model.to_owned())),
            (None, Some(_)) => Err(SelectionRefusal::InvalidSelector {
                message: "spawn_subagent `provider` only disambiguates a `model` — name the model"
                    .to_owned(),
            }),
            (Some(model), Some(provider)) => {
                self.validate_selection(parent_provider, Some(provider), model)
            }
            (Some(model), None) => {
                // Literal equality is evaluated across the catalog before the
                // separator-insensitive form. This prevents a normalized
                // collision from shadowing an exact request slug.
                if let Some(exact) = self
                    .known_inventory(parent_provider)
                    .and_then(|inventory| inventory.iter().find(|known| known.as_str() == model))
                {
                    return Ok((parent_provider.to_owned(), exact.clone()));
                }
                let exact = self.child_matching_rows(|known| known == model);
                if !exact.is_empty() {
                    return resolve_child_rows(model, exact);
                }

                let normalized = normalize_model_selector(model);
                let parent_matches = self
                    .known_inventory(parent_provider)
                    .into_iter()
                    .flatten()
                    .filter(|known| normalize_model_selector(known) == normalized)
                    .collect::<Vec<_>>();
                if let [only] = parent_matches.as_slice() {
                    return Ok((parent_provider.to_owned(), (*only).clone()));
                }
                if parent_matches.len() > 1 {
                    return Err(SelectionRefusal::ModelNotResolvable {
                        model: model.to_owned(),
                        candidates: vec![parent_provider.to_owned()],
                        suggestions: Vec::new(),
                    });
                }
                let normalized_rows =
                    self.child_matching_rows(|known| normalize_model_selector(known) == normalized);
                if !normalized_rows.is_empty() {
                    return resolve_child_rows(model, normalized_rows);
                }
                Err(SelectionRefusal::ModelNotResolvable {
                    model: model.to_owned(),
                    candidates: Vec::new(),
                    suggestions: self.suggestions_for_bare(model),
                })
            }
        }
    }

    fn child_matching_rows(&self, matches_model: impl Fn(&str) -> bool) -> Vec<(String, String)> {
        self.summaries
            .iter()
            .filter(|summary| {
                matches!(summary.availability, ProviderAvailabilityWire::Available)
                    && self.provider_is_creatable(&summary.provider)
            })
            .flat_map(|summary| {
                summary
                    .models
                    .iter()
                    .filter(|known| matches_model(known))
                    .map(move |known| (summary.provider.clone(), known.clone()))
            })
            .collect()
    }
}

fn resolve_child_rows(
    requested_model: &str,
    rows: Vec<(String, String)>,
) -> Result<(String, String), SelectionRefusal> {
    if let [(provider, model)] = rows.as_slice() {
        return Ok((provider.clone(), model.clone()));
    }
    let mut candidates = Vec::new();
    for (provider, _) in rows {
        if !candidates.contains(&provider) {
            candidates.push(provider);
        }
    }
    Err(SelectionRefusal::ModelNotResolvable {
        model: requested_model.to_owned(),
        candidates,
        suggestions: Vec::new(),
    })
}

fn append_suggestion_guidance(message: &mut String, suggestions: &[String]) {
    if suggestions.is_empty() {
        message.push_str(" — call list_models to inspect the available catalog");
    } else {
        message.push_str(&format!(
            " — nearest catalog rows: {}; call list_models to inspect the available catalog",
            suggestions.join(", ")
        ));
    }
}

/// Unicode-safe model selector key: lower-case scalar expansion, dropping
/// only the punctuation/spacing family the public selector contract names.
pub(crate) fn normalize_model_selector(selector: &str) -> String {
    selector
        .chars()
        .filter(|character| !character.is_whitespace() && !matches!(character, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn nearest_model_rows<'a>(
    requested: &str,
    rows: impl Iterator<Item = (&'a str, &'a str)>,
) -> Vec<String> {
    const SUGGESTION_CAP: usize = 5;
    let requested_normalized = normalize_model_selector(requested);
    let requested_tokens = selector_tokens(requested);
    let mut ranked = rows
        .map(|(model, provider)| {
            let normalized = normalize_model_selector(model);
            let overlap = requested_tokens
                .intersection(&selector_tokens(model))
                .count();
            RankedModelRow {
                distance: levenshtein_chars(&requested_normalized, &normalized),
                overlap,
                length_delta: requested_normalized
                    .chars()
                    .count()
                    .abs_diff(normalized.chars().count()),
                model: model.to_owned(),
                provider: provider.to_owned(),
            }
        })
        .collect::<Vec<_>>();
    ranked.sort_by(RankedModelRow::cmp);
    let mut suggestions = Vec::new();
    for row in ranked {
        let suggestion = format!("{} · {}", row.model, row.provider);
        if !suggestions.contains(&suggestion) {
            suggestions.push(suggestion);
        }
        if suggestions.len() == SUGGESTION_CAP {
            break;
        }
    }
    suggestions
}

struct RankedModelRow {
    distance: usize,
    overlap: usize,
    length_delta: usize,
    model: String,
    provider: String,
}

impl RankedModelRow {
    fn cmp(left: &Self, right: &Self) -> Ordering {
        left.distance
            .cmp(&right.distance)
            .then_with(|| right.overlap.cmp(&left.overlap))
            .then_with(|| left.length_delta.cmp(&right.length_delta))
            .then_with(|| left.model.to_lowercase().cmp(&right.model.to_lowercase()))
            .then_with(|| {
                left.provider
                    .to_lowercase()
                    .cmp(&right.provider.to_lowercase())
            })
            .then_with(|| left.model.cmp(&right.model))
            .then_with(|| left.provider.cmp(&right.provider))
    }
}

fn selector_tokens(selector: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let mut token = String::new();
    for character in selector.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            token.push(character);
        } else if !token.is_empty() {
            tokens.insert(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.insert(token);
    }
    tokens
}

fn levenshtein_chars(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_character) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_character) in right.iter().enumerate() {
            let substitution =
                previous[right_index] + usize::from(left_character != *right_character);
            current[right_index + 1] = (current[right_index] + 1)
                .min(previous[right_index + 1] + 1)
                .min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}
