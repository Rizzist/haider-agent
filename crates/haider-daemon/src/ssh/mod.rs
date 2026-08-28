//! Profile-scoped SSH secrets and the cross-platform `russh` client pool.

mod runtime;
mod store;

use std::sync::Arc;

use haider_accounts::Vault;

pub(crate) use runtime::{SshExecRequest, SshOutput, SshPtyRequest, SshRuntime};
pub(crate) use store::{
    PinnedHostKey, SshAuth, SshError, SshProfile, SshProfileStore, SshScope, SshTarget,
    enforce_scope,
};

/// Profile-scoped SSH services installed when the account `FileVault` is
/// available. Both halves share the same vault-backed store and therefore the
/// same platform-specific at-rest protection as API keys.
#[derive(Clone)]
pub(crate) struct SshService {
    pub(crate) store: SshProfileStore,
    pub(crate) runtime: SshRuntime,
}

impl SshService {
    pub(crate) fn new(vault: Arc<dyn Vault>) -> Self {
        let store = SshProfileStore::new(vault);
        let runtime = SshRuntime::new(store.clone());
        Self { store, runtime }
    }
}

#[cfg(test)]
#[path = "ssh_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ssh_fixture_tests.rs"]
mod fixture_tests;
