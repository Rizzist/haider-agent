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
//! 2. **Known inventory** — a provider with a non-empty discovered model list
//!    must contain the selected model. A provider WITHOUT a discovered
//!    inventory accepts honestly; provider errors surface at turn time.

use haider_rpc::{ProviderApiFamilyWire, ProviderAvailabilityWire, ProviderSummaryWire};
use std::collections::BTreeSet;

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
    ModelUnknown { provider: String, model: String },
    /// A bare model selector resolved to zero or several provider rows;
    /// `candidates` names every available row so the caller can retry with an
    /// explicit pair.
    ModelNotResolvable {
        model: String,
        candidates: Vec<String>,
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
            Self::ModelUnknown { provider, model } => {
                format!("model `{model}` is not in provider `{provider}`'s discovered inventory")
            }
            Self::ModelNotResolvable { model, candidates } => {
                if candidates.is_empty() {
                    format!(
                        "no available provider serves model `{model}` — retry with an explicit \
                         provider alongside the model"
                    )
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
            }),
            Self::ModelUnknown { provider, model } => serde_json::json!({
                "kind": self.kind(),
                "provider": provider,
                "model": model,
            }),
            Self::ModelNotResolvable { model, candidates } => serde_json::json!({
                "kind": self.kind(),
                "model": model,
                "candidates": candidates,
            }),
            Self::InvalidSelector { message } => serde_json::json!({
                "kind": self.kind(),
                "message": message,
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
    /// or an ENABLED custom chat-completions profile.
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
        let model = model.trim();
        if model.is_empty() {
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
        if let Some(inventory) = self.known_inventory(provider)
            && !inventory.iter().any(|known| known == model)
        {
            return Err(SelectionRefusal::ModelUnknown {
                provider: provider.to_owned(),
                model: model.to_owned(),
            });
        }
        Ok((provider.to_owned(), model.to_owned()))
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
            haider_provider::ANTHROPIC_PROVIDER_NAME
            | haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME => {
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
                // Parent-first preference: the spawning session's own provider
                // wins whenever its KNOWN inventory serves the model.
                if self
                    .known_inventory(parent_provider)
                    .is_some_and(|inventory| inventory.iter().any(|known| known == model))
                {
                    return Ok((parent_provider.to_owned(), model.to_owned()));
                }
                let candidates = self
                    .summaries
                    .iter()
                    .filter(|summary| {
                        matches!(summary.availability, ProviderAvailabilityWire::Available)
                            && self.provider_is_creatable(&summary.provider)
                            && summary.models.iter().any(|known| known == model)
                    })
                    .map(|summary| summary.provider.clone())
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [only] => Ok((only.clone(), model.to_owned())),
                    _ => Err(SelectionRefusal::ModelNotResolvable {
                        model: model.to_owned(),
                        candidates,
                    }),
                }
            }
        }
    }
}
