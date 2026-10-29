#![allow(clippy::expect_used)]
use super::*;
use crate::shell_registry::{ShellControl, ShellRegistry, ShellRegistryEvent};
use haider_accounts::MemoryVault;
use haider_rpc::{ShellKindWire, ShellStatusWire, SshPtySizeWire};
use russh::server::{self, Server as _};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::net::TcpListener;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const FIXTURE_PASSWORD: &str = "generated-server-synthetic-password";

#[derive(Clone)]
struct FixtureServer;
impl server::Server for FixtureServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}
impl server::Handler for FixtureServer {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        Ok(if user == "fixture" && password == FIXTURE_PASSWORD {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
}

fn network_profile(name: &str, port: u16, auth: SshAuth) -> SshProfile {
    SshProfile {
        name: name.into(),
        description: None,
        ssh: SshTarget {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
            user: "fixture".into(),
            auth,
            default_cwd: None,
            host_key: None,
        },
        last_used_ms: None,
    }
}

#[tokio::test]
async fn asynchronous_pty_setup_refusals_keep_the_typed_error_in_terminal_state() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("SSH fixture listener");
    let port = listener.local_addr().expect("SSH fixture address").port();
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).expect("synthetic host key seed");
    let key = russh::keys::PrivateKey::new(
        russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&seed).into(),
        "synthetic fixture",
    )
    .expect("synthetic host key");
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..server::Config::default()
    });
    let mut factory = FixtureServer;
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let running = factory.run_on_socket(config, &listener);
        let _ = started.send(running.handle());
        running.await
    });
    let shutdown = ready.await.expect("SSH fixture startup");
    let store = SshProfileStore::new(Arc::new(MemoryVault::default()));
    let secret = store
        .put_auth_secret("wrong-auth", b"synthetic-wrong-password")
        .expect("fixture password");
    store
        .add(network_profile(
            "wrong-auth",
            port,
            SshAuth::Password { vault_ref: secret },
        ))
        .expect("auth profile");
    let good = store
        .put_auth_secret("changed-host", FIXTURE_PASSWORD.as_bytes())
        .expect("fixture password");
    let mut changed = network_profile("changed-host", port, SshAuth::Password { vault_ref: good });
    changed.ssh.host_key = Some(PinnedHostKey {
        algorithm: "ssh-ed25519".into(),
        fingerprint: "synthetic-different-host-key".into(),
        pinned_at_ms: 0,
    });
    store.add(changed).expect("changed host profile");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("refused port");
    let port = listener.local_addr().expect("refused address").port();
    drop(listener);
    store
        .add(network_profile("refused", port, SshAuth::Agent))
        .expect("refused profile");
    let runtime = SshRuntime::new(store);
    let registry = ShellRegistry::default();
    for (profile, code) in [
        ("refused", "ssh_connection_failed"),
        ("wrong-auth", "ssh_authentication_failed"),
        ("changed-host", "ssh_host_key_changed"),
    ] {
        let mut events = registry.subscribe();
        let (shell, controls) = registry
            .open_interactive(
                ShellKindWire::Ssh {
                    profile: profile.into(),
                },
                profile,
                "127.0.0.1",
                Some("owned-fixture".into()),
            )
            .expect("starting row");
        let id = shell.id().to_owned();
        let error = tokio::time::timeout(
            TEST_TIMEOUT,
            runtime.start_pty(SshPtyRequest {
                profile: profile.into(),
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
            }),
        )
        .await
        .expect("setup deadline")
        .expect_err("fixture setup refusal");
        assert_eq!(error.code(), code);
        let row = registry.get(&id).expect("terminal state");
        assert_eq!(row.status, ShellStatusWire::Exited { code: None });
        assert_eq!(
            row.setup_error,
            Some(haider_rpc::ShellSetupErrorWire {
                code: code.into(),
                message: error.to_string(),
                retryable: error.retryable()
            })
        );
        assert_eq!(registry.inventory().count, 0);
        let denied = registry
            .control(&id, Some("other-connection"), ShellControl::Eof)
            .expect_err("other owner");
        assert!(registry.setup_error_for_control(&denied).is_none());
        let closed = registry
            .control(&id, Some("owned-fixture"), ShellControl::Eof)
            .expect_err("setup failed");
        assert_eq!(registry.setup_error_for_control(&closed), row.setup_error);

        let mut published = false;
        while let Ok(event) = events.try_recv() {
            match event {
                ShellRegistryEvent::State(state) if state.setup_error.is_some() => {
                    assert_eq!(state, row);
                    published = true;
                }
                ShellRegistryEvent::Output { .. } => {
                    panic!("setup refusal must be printed only by the client's error path")
                }
                _ => {}
            }
        }
        assert!(published, "typed refusal must reach the event stream");
    }
    shutdown.shutdown("fixture complete".into());
    tokio::time::timeout(TEST_TIMEOUT, task)
        .await
        .expect("SSH fixture shutdown deadline")
        .expect("SSH fixture task")
        .expect("SSH fixture shutdown");
}
