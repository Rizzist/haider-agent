//! Shared direct-shell admission truth for inventory, RPC and pre-spawn checks.
use super::*;
use std::path::Path;

impl SessionHub {
    pub(crate) async fn android_shell_refusal(
        &self,
        session_id: &SessionId,
        require_idle: bool,
    ) -> Result<Option<&'static str>, HaiderError> {
        if !crate::android_policy::enabled() {
            return Ok(Some("process_exec_disabled"));
        }
        if self.inner.draining.load(Ordering::Acquire) || self.worker_manager().is_err() {
            return Ok(Some("daemon_unavailable"));
        }
        if lock(&self.inner.closing_sessions)
            .map_err(hub_error_as_store)?
            .contains(session_id)
            || lock(&self.inner.deleting_sessions)
                .map_err(hub_error_as_store)?
                .contains(session_id)
        {
            return Ok(Some("session_ineligible"));
        }
        let Some(metadata) = self.session_metadata(session_id).await? else {
            return Ok(Some("session_unavailable"));
        };
        if metadata
            .permission_overrides
            .is_some_and(|policy| policy.read_only)
        {
            return Ok(Some("read_only"));
        }
        // A direct human command must not bypass a delegated/typed tool grant.
        // Such sessions keep their normal brokered model-tool path only.
        if metadata.agent_type.is_some()
            || self
                .delegation_for_child_session(session_id.clone())
                .await?
                .is_some()
        {
            return Ok(Some("session_ineligible"));
        }
        let bound = self.bound_session_lockdown(session_id);
        let selected = self.provider_lockdown_policy_detail(&metadata.provider);
        match (bound, selected) {
            (Ok(bound), Ok(selected)) => {
                if selected.is_lockdown() || bound.is_some_and(|(_, policy)| policy.is_lockdown()) {
                    return Ok(Some("lockdown"));
                }
            }
            _ => return Ok(Some("policy_unavailable")),
        }
        if crate::android_workspace::validate(Path::new(&metadata.cwd)).is_err() {
            return Ok(Some("workspace_unavailable"));
        }
        if !haider_tools::android_shell_available() {
            return Ok(Some("system_shell_unavailable"));
        }
        if require_idle && self.session_has_nonterminal_runs(session_id).await? {
            return Ok(Some("session_busy"));
        }
        Ok(None)
    }
}
