//! Receipt-backed per-session endpoint routing. Registry state is never mutated.
#[cfg(test)]
#[path = "provider_rebind_tests.rs"]
mod tests;

use super::*;
use crate::provider_registry::{ProductionProviderEndpointValidator, ProviderEndpointValidator};

impl HubConnection {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn session_provider_rebind(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        provider: String,
        base_url: Option<String>,
        account: Option<String>,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || provider.trim() != provider
            || account
                .as_ref()
                .is_some_and(|s| s.trim().is_empty() || s.trim() != s)
        {
            return self.respond_error(request_id, ERROR_CODE_INVALID_ARGUMENT,
                "provider rebind needs a command id, provider id, and nonempty account when supplied", false, None);
        }
        let request_json = serde_json::json!({
            "session_id": session_id, "worker_generation": worker_generation,
            "provider": provider, "base_url": base_url, "account": account,
        })
        .to_string();
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        // Lost-response replay is independent of later registry/account edits.
        match self
            .hub
            .inner
            .store
            .session_provider_rebind_receipt(
                command_id.0.clone(),
                request_digest.clone(),
                request_json.clone(),
            )
            .await
        {
            Ok(Some(selected)) => return self.respond_provider_rebound(request_id, selected),
            Ok(None) => {}
            Err(error) => return self.respond_turn_error(request_id, error),
        }
        let _selection = self.hub.lock_workflow_selection(&session_id).await;
        let Some(current) = (match self.hub.session_metadata(&session_id).await {
            Ok(value) => value,
            Err(error) => return self.respond_turn_error(request_id, error),
        }) else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "provider rebind requires a session with typed metadata",
                false,
                None,
            );
        };
        let view = self
            .hub
            .accounts()?
            .and_then(|facade| facade.management.read());
        let Some(view) = view else {
            return self.respond_error(
                request_id,
                "provider_unknown",
                "provider registry is unavailable",
                false,
                None,
            );
        };
        let Some(profile) = view.providers.iter().find(|p| p.provider == provider) else {
            return self.respond_error(
                request_id,
                "provider_unknown",
                "provider is not registered",
                false,
                None,
            );
        };
        if !profile.enabled {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_PROVIDER_UNAVAILABLE,
                "provider is disabled",
                false,
                None,
            );
        }
        if let Some(alias) = account.as_deref() {
            let Some(descriptor) = view.descriptors.iter().find(|d| d.alias.as_str() == alias)
            else {
                return self.respond_error(
                    request_id,
                    "account_unknown",
                    "account is not registered",
                    false,
                    None,
                );
            };
            if descriptor.provider != provider {
                return self.respond_error(
                    request_id,
                    "account_provider_mismatch",
                    "account does not belong to the requested provider",
                    false,
                    None,
                );
            }
        }
        let base_url = match base_url {
            None => None,
            Some(url) => match validate_rebind_endpoint(&provider, &url).await {
                Ok(url) => Some(url),
                Err(error) => return self.respond_turn_error(request_id, error),
            },
        };
        let authority = crate::model_select::ModelSelectionAuthority::new(
            self.hub.creatable_providers()?,
            view.providers.clone(),
        );
        if let Err(refusal) = authority.validate_selection_with_status(
            &current.provider,
            Some(&provider),
            &current.model,
        ) {
            return self.respond_selection_refusal(request_id, &refusal);
        }
        // Admission shares this lock. Compare against the ACTIVE run's
        // frozen ceiling: session.select_model can already describe the next
        // turn, and is not evidence of the current turn's authority.
        let new_no_auth = !view.descriptors.iter().any(|d| {
            d.provider == provider && account.as_ref().map_or(d.active, |a| d.alias.as_str() == a)
        });
        let new_policy = self
            .hub
            .provider_lockdown_policy_for_active(&provider, new_no_auth)?;
        if self.hub.session_has_nonterminal_runs(&session_id).await? {
            let frozen = self.hub.bound_session_lockdown(&session_id)?;
            if !frozen.as_ref().is_some_and(|(old_provider, old_policy)| {
                rebind_matches_frozen_policy(old_provider, *old_policy, &provider, new_policy)
            }) {
                return self.respond_error(request_id, haider_rpc::ERROR_CODE_BUSY,
                    "provider rebind must preserve the active run's frozen provider trust; retry when idle", true, None);
            }
        }
        let command = haider_store::SessionProviderRebindCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id,
            worker_generation,
            provider,
            base_url,
            account,
            event_id: EventId::new(random_id("session-provider-rebound")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let selected = match self.hub.rebind_session_provider(command).await {
            Ok(haider_store::SessionProviderRebindOutcome::Committed { selected, .. })
            | Ok(haider_store::SessionProviderRebindOutcome::IdempotentReplay { selected }) => {
                selected
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.respond_provider_rebound(request_id, selected)
    }

    fn respond_provider_rebound(
        &self,
        request_id: RequestId,
        selected: haider_store::ReboundSessionProvider,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionProviderRebind {
                session_id: selected.session_id,
                provider: selected.provider,
                base_url: selected.base_url,
                account: selected.account,
                selected_seq: selected.selected_seq,
                worker_generation: selected.worker_generation,
            },
        })
    }
}

/// Same origin policies as registry configuration. Explicit proxy identity
/// `openai-compatible` also permits a session-only endpoint. Fixed first-party,
/// OAuth and agent-owned adapters cannot redirect their credentials this way.
async fn validate_rebind_endpoint(provider: &str, url: &str) -> Result<String, HaiderError> {
    match provider {
        haider_provider::BEDROCK_PROVIDER_NAME => {
            haider_provider::validate_bedrock_mantle_base_url(url)
                .map_err(|e| HaiderError::new(ErrorCode::InvalidArgument, e.message, false))
        }
        haider_provider::VERTEX_PROVIDER_NAME => {
            haider_provider::validate_vertex_models_base_url(url)
                .map_err(|e| HaiderError::new(ErrorCode::InvalidArgument, e.message, false))
        }
        haider_provider::OPENAI_COMPATIBLE_PROVIDER_NAME => {
            ProductionProviderEndpointValidator.validate(url).await
        }
        id if !haider_provider::BUILTIN_PROVIDER_NAMES.contains(&id) => {
            ProductionProviderEndpointValidator.validate(url).await
        }
        _ => Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "base_url override is permitted only for registered custom providers, openai-compatible proxies, bedrock and vertex",
            false,
        )),
    }
}

fn rebind_matches_frozen_policy(
    old_provider: &str,
    old_policy: crate::auto_hermetic::ProviderLockdownPolicy,
    provider: &str,
    policy: crate::auto_hermetic::ProviderLockdownPolicy,
) -> bool {
    old_policy.binding_bits() == policy.binding_bits()
        && (!old_policy.is_lockdown() || old_provider == provider)
}
