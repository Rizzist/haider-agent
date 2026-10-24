#![allow(clippy::expect_used)]

use super::*;
use crate::session_hub::session_hub_private_tests::CapturingFrameSink;
use haider_accounts::MemoryVault;
use haider_rpc::{ShellStatusWire, SshPtySizeWire};
use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::time::Duration;

#[tokio::test]
async fn cancelling_a_pty_open_does_not_block_the_rpc_dispatcher() {
    let deadline = Duration::from_secs(10);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("loopback listener");
    let root = tempfile::tempdir().expect("temporary store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let ssh = crate::ssh::SshService::new(Arc::new(MemoryVault::default()));
    ssh.store
        .add(crate::ssh::SshProfile {
            name: "pending-fixture".into(),
            description: None,
            ssh: crate::ssh::SshTarget {
                host: "127.0.0.1".into(),
                port: listener.local_addr().expect("loopback address").port(),
                user: "fixture".into(),
                auth: crate::ssh::SshAuth::Agent,
                default_cwd: None,
                host_key: None,
            },
            last_used_ms: None,
        })
        .expect("synthetic profile");
    *hub.inner.ssh.lock().expect("SSH slot") = Some(ssh);
    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            BTreeSet::from([Capability::Control, Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("fixture connection");
    let budget = Arc::clone(&connection.identity_lease.peer_requests);
    let capacity = budget.available_permits();
    let exhausted = Arc::clone(&budget)
        .try_acquire_many_owned(capacity as u32)
        .expect("exhaust pending request budget");
    connection
        .request(
            RequestId::new("overloaded-open"),
            RequestBody::SshShellOpen {
                name: "pending-fixture".into(),
                session_id: None,
                term: "xterm".into(),
                size: SshPtySizeWire {
                    cols: 80,
                    rows: 24,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            },
        )
        .await
        .expect("overload response");
    assert!(sink.0.lock().expect("responses").iter().any(|frame| {
        matches!(frame, WireFrame::Response {
            request_id,
            body: ResponseBody::Error { code, .. },
        } if request_id.0 == "overloaded-open" && code == ERROR_CODE_OVERLOADED)
    }));
    assert_eq!(hub.shell_registry().inventory().count, 0);
    drop(exhausted);
    tokio::time::timeout(
        deadline,
        connection.request(
            RequestId::new("pending-open"),
            RequestBody::SshShellOpen {
                name: "pending-fixture".into(),
                session_id: None,
                term: "xterm".into(),
                size: SshPtySizeWire {
                    cols: 80,
                    rows: 24,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            },
        ),
    )
    .await
    .expect("open admission must return before SSH negotiation")
    .expect("open admission");
    let shell = sink
        .0
        .lock()
        .expect("responses")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                body: ResponseBody::SshShellOpen { shell },
                ..
            } => Some(shell.clone()),
            _ => None,
        })
        .expect("pending shell coordinate is immediately available");
    assert_eq!(shell.status, ShellStatusWire::Starting);
    assert_eq!(budget.available_permits(), capacity - 1);
    let (peer, _) = tokio::time::timeout(deadline, listener.accept())
        .await
        .expect("connect deadline")
        .expect("loopback accepted");
    tokio::time::timeout(
        deadline,
        connection.request(
            RequestId::new("pending-close"),
            RequestBody::ShellClose {
                id: shell.id.clone(),
            },
        ),
    )
    .await
    .expect("close must reach the dispatcher during pending setup")
    .expect("close response");
    assert_eq!(hub.shell_registry().inventory().count, 0);
    assert_eq!(
        hub.shell_registry()
            .get(&shell.id)
            .expect("shell row")
            .status,
        ShellStatusWire::Closed
    );
    let returned = tokio::time::timeout(
        deadline,
        Arc::clone(&budget).acquire_many_owned(capacity as u32),
    )
    .await
    .expect("cancelled setup must return its admission permit")
    .expect("budget remains open");
    drop(returned);
    drop(connection);
    tokio::time::timeout(deadline, hub.shutdown())
        .await
        .expect("graceful hub shutdown deadline")
        .expect("hub shutdown");
    store.close().await.expect("store closes");
    drop(peer);
}
