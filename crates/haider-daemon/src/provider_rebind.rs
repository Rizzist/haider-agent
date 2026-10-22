//! Request-boundary pickup of durable per-session routing changes.
#[cfg(test)]
#[path = "provider_rebind_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run_route_recovery_tests.rs"]
mod run_route_recovery_tests;

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A headless spec pins run tuning, while an explicit durable rebind owns the
/// provider route. Restoring only the spec provider would combine its adapter
/// with another provider's rebound endpoint and account after recovery.
pub(crate) async fn pin_headless_turn_metadata(
    metadata: &mut SessionMetadataV1,
    spec: &HeadlessRunSpecV1,
    store: &HubStoreHandle,
    run_id: &RunId,
) -> Result<(), HaiderError> {
    let pair = store
        .hub()
        .run_output_pair(store.session_id(), run_id)
        .await?
        .ok_or_else(|| {
            haider_protocol::session::run_route_history_unavailable(
                "headless run has no admission boundary",
            )
        })?;
    pair.restore_route(metadata)?;
    metadata.provider = pair.provider;
    metadata.model = pair.model;
    metadata.max_tokens = pair.max_tokens;
    metadata.max_tokens_source = pair.source;
    metadata.effort.clone_from(&spec.effort);
    metadata.fast = spec.fast;
    Ok(())
}

/// Recovery retains the accepted run's authority identity. An explicit Full
/// route change does not create a new lockdown binding for that same run.
#[cfg(test)]
pub(super) fn rebound_turn_lockdown_snapshot(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    explicitly_rebound: bool,
    provider: &str,
    policy: crate::auto_hermetic::ProviderLockdownPolicy,
) -> Result<Option<crate::lockdown::LockdownTurn>, HaiderError> {
    if explicitly_rebound
        && let Some((bound_provider, bound_policy)) = hub
            .bound_lockdown_run(session_id, run_id)
            .map_err(hub_error)?
    {
        if bound_policy.binding_bits() != policy.binding_bits()
            || (bound_policy.is_lockdown() && bound_provider != provider)
        {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "provider rebind would change the recovered run's frozen provider trust",
                true,
            ));
        }
        if !bound_policy.is_lockdown() {
            return Ok(None);
        }
    }
    lockdown_turn_snapshot(hub, session_id, run_id, provider, policy)
}

/// Recovery performs the same frozen-policy check, then publishes the binding
/// and active boundary with one durable replacement before the turn proceeds.
pub(super) fn rebound_active_turn_lockdown_snapshot(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    explicitly_rebound: bool,
    provider: &str,
    policy: crate::auto_hermetic::ProviderLockdownPolicy,
) -> Result<Option<crate::lockdown::LockdownTurn>, HaiderError> {
    if explicitly_rebound
        && let Some((bound_provider, bound_policy)) = hub
            .bound_lockdown_run(session_id, run_id)
            .map_err(hub_error)?
    {
        if bound_policy.binding_bits() != policy.binding_bits()
            || (bound_policy.is_lockdown() && bound_provider != provider)
        {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "provider rebind would change the recovered run's frozen provider trust",
                true,
            ));
        }
        if !bound_policy.is_lockdown() {
            // A recovered explicit Full rebind intentionally keeps the
            // accepted run's original authority identity. Its binding is
            // already durable, so only activation remains to persist.
            hub.activate_lockdown_turn(session_id, run_id)
                .map_err(hub_error)?;
            return Ok(None);
        }
    }
    active_lockdown_turn_snapshot(hub, session_id, run_id, provider, policy)
}

pub(crate) struct DaemonProviderRebindResolver {
    store: HubStoreHandle,
    run_id: RunId,
    factory: Arc<dyn ProviderFactory>,
    metadata: std::sync::Mutex<SessionMetadataV1>,
    web_degrade: WebCapabilityDegrade,
    seen_revision: AtomicU64,
    initialized: AtomicBool,
}

impl std::fmt::Debug for DaemonProviderRebindResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonProviderRebindResolver")
            .finish_non_exhaustive()
    }
}

impl DaemonProviderRebindResolver {
    pub(crate) fn new(
        store: HubStoreHandle,
        run_id: RunId,
        factory: Arc<dyn ProviderFactory>,
        metadata: SessionMetadataV1,
        web_degrade: WebCapabilityDegrade,
    ) -> Self {
        Self {
            store,
            run_id,
            factory,
            metadata: std::sync::Mutex::new(metadata),
            web_degrade,
            // The first boundary also consults durable state after a restart,
            // where the process-local revision counter starts at zero.
            seen_revision: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl haider_core::ProviderRebindResolver for DaemonProviderRebindResolver {
    async fn refresh(
        &self,
        current_model: &str,
        reasoning_settings: &str,
    ) -> Result<Option<haider_core::ProviderRebindTarget>, HaiderError> {
        let revision = self.store.hub().provider_rebind_revision();
        if self.initialized.load(Ordering::Acquire)
            && revision == self.seen_revision.load(Ordering::Acquire)
        {
            return Ok(None);
        }
        let mut metadata = fresh_turn_metadata(&self.store).await?;
        let previous = self
            .metadata
            .lock()
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "session provider binding is unavailable",
                    false,
                )
            })?
            .clone();
        // Model/effort selection retains its existing next-turn contract.
        // This lever changes only the explicitly bound route coordinates.
        metadata.model = current_model.to_owned();
        let reasoning: serde_json::Value =
            serde_json::from_str(reasoning_settings).unwrap_or(serde_json::Value::Null);
        metadata.effort = reasoning
            .get("effort")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        metadata.fast = reasoning
            .get("fast")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let active_pair = self
            .store
            .hub()
            .run_output_pair(self.store.session_id(), &self.run_id)
            .await?;
        // The durable run route survives ordinary selections for the next
        // turn, including selections that clear the session's rebind id.
        if let Some(pair) = active_pair.as_ref() {
            pair.restore_route(&mut metadata)?;
            metadata.provider.clone_from(&pair.provider);
        }
        if metadata.provider_rebind_id.is_none()
            || previous.provider_rebind_id == metadata.provider_rebind_id
        {
            self.seen_revision.store(revision, Ordering::Release);
            self.initialized.store(true, Ordering::Release);
            return Ok(None);
        }
        let rebind_seq = active_pair
            .as_ref()
            .map_or(0, |pair| pair.latest_rebind_seq);
        let recorded_active = active_pair
            .filter(|pair| pair.model == current_model)
            .map(
                |pair| haider_protocol::output_budget::SessionOutputBudgetV1 {
                    max_tokens: pair.max_tokens,
                    source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::classify(
                        pair.source,
                        pair.max_tokens,
                    ),
                    clamped: None,
                },
            );
        if let Some(budget) = recorded_active {
            metadata.max_tokens = budget.max_tokens;
            metadata.max_tokens_source = Some(budget.source);
        } else if let Some(view) = self
            .store
            .hub()
            .accounts()
            .map_err(hub_error)?
            .and_then(|accounts| accounts.management.read())
        {
            let selection = crate::model_select::ModelSelectionAuthority::new(
                self.store.hub().creatable_providers().map_err(hub_error)?,
                view.providers,
            )
            .validate_selection_with_status(
                &previous.provider,
                Some(&metadata.provider),
                current_model,
            )
            .map_err(|refusal| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "provider rebind rejected the active request model: {}",
                        refusal.message()
                    ),
                    false,
                )
            })?;
            // A pending ordinary model selection may differ from this run's
            // active model. The receipt budgets the durable next-turn pair;
            // the refreshed adapter must budget the pair it will dispatch.
            let budget = haider_protocol::output_budget::SessionOutputBudgetSourceV1::classify(
                metadata.max_tokens_source,
                metadata.max_tokens,
            )
            .apply(
                selection.max_output_tokens,
                selection.explicit_max_output_tokens,
            );
            metadata.max_tokens = budget.max_tokens;
            metadata.max_tokens_source = Some(budget.source);
        }
        let resolved = self
            .factory
            .resolve_for_turn_with_web(&metadata, self.web_degrade)
            .await?;
        if resolved.provider_name != metadata.provider {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "provider rebind resolved a different provider",
                false,
            ));
        }
        // Account/registry state can change after the RPC validated it.
        // Recheck the ACTUAL resolved adapter against the active run ceiling
        // before returning any replacement to core.
        let policy = self
            .store
            .hub()
            .provider_lockdown_policy_for_active(&resolved.provider_name, resolved.active_no_auth)
            .map_err(hub_error)?;
        let frozen = self
            .store
            .hub()
            .bound_session_lockdown(self.store.session_id())
            .map_err(hub_error)?;
        if !frozen.as_ref().is_some_and(|(provider, frozen)| {
            frozen.binding_bits() == policy.binding_bits()
                && (!frozen.is_lockdown() || provider == &resolved.provider_name)
        }) {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "provider rebind would change the active run's frozen provider trust",
                true,
            ));
        }
        let epoch = self
            .store
            .commit_turn_resolved_route(&self.run_id, &metadata, resolved.account_alias.as_deref())
            .await?;
        metadata.selection_epoch = Some(epoch);
        metadata
            .resolved_route_alias
            .clone_from(&resolved.account_alias);
        metadata.resolved_route_seen = true;
        let capabilities = resolved.provider.capabilities().await;
        let request_state = provider_derived_request_state(
            &resolved.provider_name,
            &capabilities,
            self.web_degrade,
        );
        let auth_scope = credential_surface_name(resolved.provider.credential_surface()).to_owned();
        let route_epoch = metadata.provider_rebind_id.clone().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                "rebind identity disappeared",
                false,
            )
        })?;
        let target = haider_core::ProviderRebindTarget {
            provider: resolved.provider,
            provider_name: resolved.provider_name,
            account: resolved
                .account_alias
                .map(haider_protocol::ids::CredentialAlias::new),
            account_incarnation: resolved.account_incarnation,
            context_window: resolved.context_window,
            max_tokens: metadata.max_tokens,
            cached_input_is_subset: cached_input_is_subset_for_provider(&metadata.provider),
            provider_request_state: request_state,
            auth_scope,
            // Preserve the factory's retry policy: default-account binding
            // may rotate, while an explicitly pinned route stays pinned.
            attempt_resolver: resolved.attempt_resolver,
            rebind_seq,
            route_epoch,
            initial_rotation: resolved.initial_rotation,
            rotation_budget_consumed: resolved.rotation_budget_consumed,
        };
        *self.metadata.lock().map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "session provider binding is unavailable",
                false,
            )
        })? = metadata;
        // Store the revision captured BEFORE resolution. A concurrent newer
        // commit therefore remains visible to the following request.
        self.seen_revision.store(revision, Ordering::Release);
        self.initialized.store(true, Ordering::Release);
        Ok(Some(target))
    }
}
