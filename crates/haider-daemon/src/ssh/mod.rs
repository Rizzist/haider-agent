//! Profile-scoped SSH secrets and the cross-platform `russh` client pool.

mod runtime;
mod store;

use std::sync::Arc;

use haider_accounts::Vault;

pub(crate) use runtime::{SshExecRequest, SshOutput, SshRuntime};
pub(crate) use store::{
    PinnedHostKey, SshAuth, SshError, SshProfile, SshProfileStore, SshScope, SshTarget,
};

/// Profile-scoped SSH services installed when the owner-only account vault is
/// installed. Both halves share the same vault-backed store.
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
