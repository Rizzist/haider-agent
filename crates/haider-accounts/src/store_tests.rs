#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use crate::store::{AccountStore, JsonFileStore, StoreLike};
use crate::{AuthMethod, CredentialAlias, CredentialDescriptor, CredentialStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CounterSyncStage {
    File,
    Directory,
    Descriptor,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CounterSyncProbe {
    events: Arc<Mutex<Vec<CounterSyncStage>>>,
    fail_at: Option<CounterSyncStage>,
}

impl CounterSyncProbe {
    pub(crate) fn record(&self, stage: CounterSyncStage) -> std::io::Result<()> {
        self.events.lock().expect("sync probe").push(stage);
        if self.fail_at == Some(stage) {
            return Err(std::io::Error::other("injected counter sync failure"));
        }
        Ok(())
    }
}

#[test]
fn counter_file_and_directory_are_synced_before_descriptor_commit() {
    for failure in [
        None,
        Some(CounterSyncStage::File),
        Some(CounterSyncStage::Directory),
    ] {
        let dir = tempfile::tempdir().expect("profile");
        let mut store = JsonFileStore::new(dir.path());
        store.counter_sync.fail_at = failure;
        let observer = store.clone();
        let mut accounts = AccountStore::new(store).expect("accounts");
        let result = accounts.add(CredentialDescriptor {
            incarnation: 0,
            credential_digest: Some("synthetic".into()),
            alias: CredentialAlias::new("work"),
            provider: "custom".into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "synthetic".into(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
            account_identity: None,
            created_at_ms: None,
        });
        let events = observer.counter_sync.events.lock().expect("events").clone();
        match failure {
            None => {
                result.expect("commit after both syncs");
                assert_eq!(
                    events,
                    [
                        CounterSyncStage::File,
                        CounterSyncStage::Directory,
                        CounterSyncStage::Descriptor
                    ]
                );
                assert_eq!(observer.load().expect("committed snapshot").len(), 1);
            }
            Some(CounterSyncStage::File) => {
                assert!(result.is_err());
                assert_eq!(events, [CounterSyncStage::File]);
                assert!(observer.load().expect("uncommitted snapshot").is_empty());
            }
            Some(CounterSyncStage::Directory) => {
                assert!(result.is_err());
                assert_eq!(
                    events,
                    [CounterSyncStage::File, CounterSyncStage::Directory]
                );
                assert!(observer.load().expect("uncommitted snapshot").is_empty());
            }
            Some(CounterSyncStage::Descriptor) => unreachable!(),
        }
    }
}

/// Observe the REAL sync primitives, so deleting fsync inside the helper
/// cannot leave a passing test merely by retaining its completion probe.
#[test]
#[cfg(unix)]
fn counter_sync_propagates_real_file_and_directory_failures() {
    let dir = tempfile::tempdir().expect("profile");
    let store = JsonFileStore::new(dir.path());
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("private socket pair");
    let descriptor: std::os::fd::OwnedFd = socket.into();
    let file = std::fs::File::from(descriptor);
    assert!(
        store.sync_counter_file(&file).is_err(),
        "fsync of a socket is invalid"
    );
    assert!(
        store
            .sync_counter_directory(&dir.path().join("missing-directory"))
            .is_err(),
        "a missing counter directory cannot be synced"
    );
    assert!(
        store.counter_sync.events.lock().expect("events").is_empty(),
        "failed real syncs must never be reported as completed"
    );
}
