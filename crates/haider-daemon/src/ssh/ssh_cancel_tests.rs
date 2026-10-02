#![allow(clippy::expect_used)]

use super::*;
use crate::shell_registry::ShellRegistry;
use haider_accounts::MemoryVault;
use haider_rpc::{ShellKindWire, ShellStatusWire, SshPtySizeWire};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::net::TcpListener;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn cancelling_a_pty_open_interrupts_stalled_negotiation_and_clears_inventory() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("loopback listener");
    let store = SshProfileStore::new(Arc::new(MemoryVault::default()));
    store
        .add(SshProfile {
            name: "cancel-open".into(),
            description: None,
            ssh: SshTarget {
                host: "127.0.0.1".into(),
                port: listener.local_addr().expect("listener address").port(),
                user: "fixture".into(),
                auth: SshAuth::Agent,
                default_cwd: None,
                host_key: None,
            },
            last_used_ms: None,
        })
        .expect("synthetic profile");
    let runtime = SshRuntime::new(store);
    let registry = ShellRegistry::default();
    for cancel_before_start in [false, true] {
        let (shell, controls) = registry
            .open_interactive(
                ShellKindWire::Ssh {
                    profile: "cancel-open".into(),
                },
                "cancel-open",
                "127.0.0.1",
                Some("owned-connection".into()),
            )
            .expect("pending shell");
        let id = shell.id().to_owned();
        if cancel_before_start {
            registry
                .close_control(&id, Some("owned-connection"))
                .expect("early cancel");
        }
        let runtime = runtime.clone();
        let opening = tokio::spawn(async move {
            runtime
                .start_pty(SshPtyRequest {
                    profile: "cancel-open".into(),
                    term: "xterm-256color".into(),
                    size: SshPtySizeWire {
                        cols: 80,
                        rows: 24,
                        pixel_width: 0,
                        pixel_height: 0,
                    },
                    shell,
                    controls,
                    activation: None,
                })
                .await
        });
        let peer = if cancel_before_start {
            None
        } else {
            let (peer, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
                .await
                .expect("connect deadline")
                .expect("loopback accepted");
            assert!(
                !opening.is_finished(),
                "peer deliberately never sends an SSH banner"
            );
            assert!(
                registry
                    .close_control(&id, Some("other-connection"))
                    .is_err()
            );
            registry
                .close_control(&id, Some("owned-connection"))
                .expect("cancel pending open");
            Some(peer)
        };
        let result = tokio::time::timeout(TEST_TIMEOUT, opening)
            .await
            .expect("cancel must interrupt setup")
            .expect("opening task");
        assert!(result.is_err());
        assert_eq!(
            registry.get(&id).expect("cancelled row").status,
            ShellStatusWire::Closed
        );
        assert_eq!(registry.inventory().count, 0);
        drop(peer);
    }
}
