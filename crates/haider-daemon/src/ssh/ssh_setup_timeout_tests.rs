#![allow(clippy::expect_used)]

use super::*;
use crate::shell_registry::ShellRegistry;
use haider_accounts::MemoryVault;
use haider_rpc::{ShellKindWire, ShellStatusWire, SshPtySizeWire};
use russh::server::{self, Server as _};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

#[derive(Clone)]
struct StalledServer {
    auth: bool,
    reached: tokio::sync::mpsc::UnboundedSender<()>,
}

impl server::Server for StalledServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl server::Handler for StalledServer {
    type Error = russh::Error;
    async fn auth_password(&mut self, _: &str, _: &str) -> Result<server::Auth, Self::Error> {
        if self.auth {
            let _ = self.reached.send(());
            std::future::pending().await
        } else {
            Ok(server::Auth::Accept)
        }
    }
    async fn channel_open_session(
        &mut self,
        _: russh::Channel<server::Msg>,
        _: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let _ = self.reached.send(());
        std::future::pending().await
    }
}

pub(super) fn fixture(port: u16) -> (SshRuntime, ShellRegistry) {
    let store = SshProfileStore::new(Arc::new(MemoryVault::default()));
    let vault_ref = store
        .put_auth_secret("stall", b"synthetic-password")
        .expect("fake secret");
    store
        .add(SshProfile {
            name: "stall".into(),
            description: None,
            ssh: SshTarget {
                host: Ipv4Addr::LOCALHOST.to_string(),
                port,
                user: "fixture".into(),
                auth: SshAuth::Password { vault_ref },
                default_cwd: None,
                host_key: None,
            },
            last_used_ms: None,
        })
        .expect("fake profile");
    (SshRuntime::new(store), ShellRegistry::default())
}

pub(super) fn opening(
    runtime: &SshRuntime,
    registry: &ShellRegistry,
) -> (
    String,
    tokio::task::JoinHandle<Result<haider_rpc::ShellWire, SshError>>,
) {
    let (shell, controls) = registry
        .open_interactive(
            ShellKindWire::Ssh {
                profile: "stall".into(),
            },
            "stall",
            "127.0.0.1",
            Some("owner".into()),
        )
        .expect("starting shell");
    let id = shell.id().to_owned();
    let runtime = runtime.clone();
    let task = tokio::spawn(async move {
        runtime
            .start_pty(SshPtyRequest {
                profile: "stall".into(),
                term: "xterm".into(),
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
    (id, task)
}

pub(super) async fn assert_timeout(
    registry: &ShellRegistry,
    id: &str,
    task: tokio::task::JoinHandle<Result<haider_rpc::ShellWire, SshError>>,
) {
    let error = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("daemon setup deadline must fire")
        .expect("opening task")
        .expect_err("typed timeout");
    assert_eq!(error.code(), "ssh_timeout");
    assert!(error.retryable());
    let row = registry.get(id).expect("timeout row");
    assert_eq!(row.status, ShellStatusWire::Exited { code: None });
    assert_eq!(row.setup_error.expect("wire timeout").code, "ssh_timeout");
    assert_eq!(registry.inventory().count, 0);
}

#[tokio::test]
async fn stalled_banner_has_a_typed_deadline_and_releases_setup_inventory() {
    for banner in ["", "SSH-2.0-synthetic", "SSH-2.0-synthetic\r\n"] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let (runtime, registry) = fixture(listener.local_addr().expect("address").port());
        let (id, task) = opening(&runtime, &registry);
        let (mut peer, _) = listener.accept().await.expect("accepted real TCP");
        // Synchronize on the client's real banner so its negotiation timer
        // is registered before advancing the test clock.
        let mut client_banner = [0; 255];
        let read = tokio::time::timeout(Duration::from_secs(10), peer.read(&mut client_banner))
            .await
            .expect("client banner deadline")
            .expect("client banner");
        assert!(read > 0);
        peer.write_all(banner.as_bytes())
            .await
            .expect("fake banner");
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_timeout(&registry, &id, task).await;
        assert_eq!(runtime.active_channels("stall").await, 0);
        drop(peer);
        tokio::time::resume();
    }
}

#[tokio::test]
async fn stalled_auth_and_channel_admission_are_bounded_and_release_leases() {
    for auth in [true, false] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let (runtime, registry) = fixture(listener.local_addr().expect("address").port());
        let mut seed = [0; 32];
        getrandom::fill(&mut seed).expect("synthetic seed");
        let key = russh::keys::PrivateKey::new(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&seed).into(),
            "synthetic fixture",
        )
        .expect("synthetic key");
        let config = Arc::new(server::Config {
            keys: vec![key],
            ..server::Config::default()
        });
        let (reached, mut progress) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            StalledServer { auth, reached }
                .run_on_socket(config, &listener)
                .await
        });
        let (id, task) = opening(&runtime, &registry);
        tokio::time::timeout(Duration::from_secs(10), progress.recv())
            .await
            .expect("fixture reached stall")
            .expect("stall notification");
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(if auth { 30 } else { 50 })).await;
        assert_timeout(&registry, &id, task).await;
        assert_eq!(runtime.active_channels("stall").await, 0);
        server.abort();
        let _ = server.await;
        tokio::time::resume();
    }
}
