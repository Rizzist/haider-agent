//! Request-boundary pickup of durable per-session routing changes.
#[cfg(test)]
#[path = "provider_rebind_tests.rs"]
mod tests;

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// A headless spec pins run tuning, while an explicit durable rebind owns the
/// provider route. Restoring only the spec provider would combine its adapter
/// with another provider's rebound endpoint and account after recovery.
pub(super) async fn pin_headless_turn_metadata(
    metadata: &mut SessionMetadataV1,
    spec: &HeadlessRunSpecV1,
    store: &HubStoreHandle,
    run_id: &RunId,
) -> Result<(), HaiderError> {
    metadata.model.clone_from(&spec.model);
    metadata.max_tokens = spec.max_output_tokens;
    metadata.effort.clone_from(&spec.effort);
    metadata.fast = spec.fast;
    if metadata.provider_rebind_id.is_none() {
        if metadata.provider != spec.provider {
            metadata.provider_base_url = None;
        }
        metadata.provider.clone_from(&spec.provider);
        return Ok(());
    }
    // An automatic promotion belongs to this run; a mutable session model
    // selection may instead be queued for the next turn. Scan only explicitly
    // rebound runs, leaving ordinary turn startup's reads unchanged.
    let mut cursor = 0;
    loop {
        let page = store.read(store.session_id(), cursor, 256).await?;
        let page_len = page.len();
        for envelope in page {
            cursor = envelope.seq;
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            let Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) = envelope.payload.decode_event()
            else {
                continue;
            };
            if kind == "provider_pair_switch_v1"
                && data
                    .get("to_provider")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|provider| !provider.is_empty())
                && let Some(model) = data
                    .get("to_model")
                    .and_then(serde_json::Value::as_str)
                    .filter(|model| !model.is_empty())
            {
                metadata.model = model.to_owned();
            }
        }
        if page_len < 256 {
            return Ok(());
        }
    }
}

/// Recovery retains the accepted run's authority identity. An explicit Full
/// route change does not create a new lockdown binding for that same run.
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

pub(super) struct DaemonProviderRebindResolver {
    store: HubStoreHandle,
    factory: Arc<dyn ProviderFactory>,
    metadata: std::sync::Mutex<SessionMetadataV1>,
    web_degrade: WebCapabilityDegrade,
    seen_revision: AtomicU64,
}

impl std::fmt::Debug for DaemonProviderRebindResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonProviderRebindResolver")
            .finish_non_exhaustive()
    }
}

impl DaemonProviderRebindResolver {
    pub(super) fn new(
        store: HubStoreHandle,
        factory: Arc<dyn ProviderFactory>,
        metadata: SessionMetadataV1,
        web_degrade: WebCapabilityDegrade,
    ) -> Self {
        Self {
            store,
            factory,
            metadata: std::sync::Mutex::new(metadata),
            web_degrade,
            // Start at zero: a commit during turn setup must be picked up
            // even if it preceded construction of this resolver.
            seen_revision: AtomicU64::new(0),
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
        if revision == self.seen_revision.load(Ordering::Acquire) {
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
        if metadata.provider_rebind_id.is_none()
            || previous.provider_rebind_id == metadata.provider_rebind_id
        {
            self.seen_revision.store(revision, Ordering::Release);
            return Ok(None);
        }
        if let Some(view) = self
            .store
            .hub()
            .accounts()
            .map_err(hub_error)?
            .and_then(|accounts| accounts.management.read())
        {
            crate::model_select::ModelSelectionAuthority::new(
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
            context_window: resolved.context_window,
            cached_input_is_subset: cached_input_is_subset_for_provider(&metadata.provider),
            provider_request_state: request_state,
            auth_scope,
            // Preserve the factory's retry policy: default-account binding
            // may rotate, while an explicitly pinned route stays pinned.
            attempt_resolver: resolved.attempt_resolver,
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
        Ok(Some(target))
    }
}
