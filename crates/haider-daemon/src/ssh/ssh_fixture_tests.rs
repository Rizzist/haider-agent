#![allow(clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use haider_accounts::MemoryVault;
use haider_rpc::{ShellKindWire, ShellStatusWire, SshPtySizeWire};
use russh::keys::PublicKey;
use russh::server::{self, Server as _, Session};
use russh::{Channel, ChannelId, Pty};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

use super::*;
use crate::shell_registry::{ShellControl, ShellRegistry, ShellRegistryEvent};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const FIXTURE_USER: &str = "fixture";
const FIXTURE_PASSWORD: &str = "ssh-password-sentinel-never-persist";

const SERVER_KEY_A: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACCpXetK4xfLb1iuLFoc0xBxs9zqGeWFsRYv/NQr5upaPQAAAKi5mKJCuZii\n\
QgAAAAtzc2gtZWQyNTUxOQAAACCpXetK4xfLb1iuLFoc0xBxs9zqGeWFsRYv/NQr5upaPQ\n\
AAAECzPxwBmmfKZCPQhw0n65Y0okhvwDaF6IkJ8EEMNh8Lpqld60rjF8tvWK4sWhzTEHGz\n\
3OoZ5YWxFi/81Cvm6lo9AAAAH3Jpenppc3RAU3llZHMtTWFjQm9vay1BaXIubG9jYWwBAg\n\
MEBQY=\n\
-----END OPENSSH PRIVATE KEY-----\n";

const SERVER_KEY_B: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACAdDaTVEyeVwN761NUN8eqmAdWhAX0TupTR6/LRYKiA3gAAAKjslcGT7JXB\n\
kwAAAAtzc2gtZWQyNTUxOQAAACAdDaTVEyeVwN761NUN8eqmAdWhAX0TupTR6/LRYKiA3g\n\
AAAEDa6jgMy5MlOg8Pnxr8JWrJyonzsO6Rjalz4iSMmSWCCx0NpNUTJ5XA3vrU1Q3x6qYB\n\
1aEBfRO6lNHr8tFgqIDeAAAAH3Jpenppc3RAU3llZHMtTWFjQm9vay1BaXIubG9jYWwBAg\n\
MEBQY=\n\
-----END OPENSSH PRIVATE KEY-----\n";

const CLIENT_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACCImw4l+zW4wmUTzkNdbFI6X+cQx1PoIYKyKOzYk7/JMQAAAKgbMhg6GzIY\n\
OgAAAAtzc2gtZWQyNTUxOQAAACCImw4l+zW4wmUTzkNdbFI6X+cQx1PoIYKyKOzYk7/JMQ\n\
AAAEDvT7/yffktZDJNkLNpDpfTVkLTJDI+ryw0wlDOQdEyLIibDiX7NbjCZRPOQ11sUjpf\n\
5xDHU+ghgrIo7NiTv8kxAAAAH3Jpenppc3RAU3llZHMtTWFjQm9vay1BaXIubG9jYWwBAg\n\
MEBQY=\n\
-----END OPENSSH PRIVATE KEY-----\n";

#[derive(Default)]
struct JournalSink {
    deltas: Mutex<Vec<haider_protocol::item::ItemDelta>>,
}

#[async_trait::async_trait]
impl haider_tools::CommandOutputSink for JournalSink {
    async fn emit(
        &self,
        _call_id: &str,
        delta: haider_protocol::item::ItemDelta,
    ) -> haider_tools::ToolResult<()> {
        self.deltas
            .lock()
            .map_err(|_| haider_tools::ToolError::Runtime {
                message: "journal fixture lock poisoned".into(),
            })?
            .push(delta);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FixtureSize {
    cols: u32,
    rows: u32,
    pixel_width: u32,
    pixel_height: u32,
}

struct FixtureState {
    client_key: PublicKey,
    password: String,
    public_key_auths: AtomicUsize,
    password_auths: AtomicUsize,
    connections: AtomicUsize,
    sizes: watch::Sender<Vec<FixtureSize>>,
}

#[derive(Clone)]
struct FixtureHandler {
    state: Arc<FixtureState>,
}

struct FixtureFactory {
    state: Arc<FixtureState>,
}

impl server::Server for FixtureFactory {
    type Handler = FixtureHandler;

    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        self.state.connections.fetch_add(1, Ordering::Relaxed);
        FixtureHandler {
            state: Arc::clone(&self.state),
        }
    }
}

impl server::Handler for FixtureHandler {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if user == FIXTURE_USER && password == self.state.password {
            self.state.password_auths.fetch_add(1, Ordering::Relaxed);
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::reject())
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        // SSH transports key material, not the private key file's local
        // comment. Compare the cryptographic identity the server receives.
        if user == FIXTURE_USER && public_key.key_data() == self.state.client_key.key_data() {
            self.state.public_key_auths.fetch_add(1, Ordering::Relaxed);
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_success(channel);
        let command = String::from_utf8_lossy(command);
        if command.contains("fixture-drop") {
            return Err(russh::Error::Disconnect);
        }
        session.data(channel, format!("stdout:{command}\n").into_bytes())?;
        session.extended_data(channel, 1, format!("stderr:{command}\n").into_bytes())?;
        let code = u32::from(command.contains("exit-7")) * 7;
        session.exit_status_request(channel, code)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        cols: u32,
        rows: u32,
        pixel_width: u32,
        pixel_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        record_size(
            &self.state,
            FixtureSize {
                cols,
                rows,
                pixel_width,
                pixel_height,
            },
        );
        let _ = session.channel_success(channel);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_success(channel);
        session.data(channel, b"fixture-shell-ready\r\n".to_vec())?;
        session.extended_data(channel, 1, b"fixture-shell-stderr\r\n".to_vec())?;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Exercise the valid EOF-before-exit-status ordering. The client must
        // keep draining channel requests instead of recording an unknown
        // status as soon as EOF arrives.
        session.eof(channel)?;
        session.exit_status_request(channel, 0)?;
        session.close(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())?;
        if data
            .windows(b"exit\n".len())
            .any(|window| window == b"exit\n")
        {
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        pixel_width: u32,
        pixel_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        record_size(
            &self.state,
            FixtureSize {
                cols,
                rows,
                pixel_width,
                pixel_height,
            },
        );
        let _ = session.channel_success(channel);
        Ok(())
    }
}

fn record_size(state: &FixtureState, size: FixtureSize) {
    state.sizes.send_modify(|sizes| sizes.push(size));
}

struct FixtureServer {
    address: SocketAddr,
    state: Arc<FixtureState>,
    shutdown: Option<server::RunningServerHandle>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl FixtureServer {
    async fn start(server_key: &str, port: Option<u16>) -> Self {
        let private_key =
            russh::keys::decode_secret_key(CLIENT_KEY, None).expect("decode fixture client key");
        let (sizes, _) = watch::channel(Vec::new());
        let state = Arc::new(FixtureState {
            client_key: private_key.public_key().clone(),
            password: FIXTURE_PASSWORD.into(),
            public_key_auths: AtomicUsize::new(0),
            password_auths: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            sizes,
        });
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port.unwrap_or(0));
        let listener = TcpListener::bind(address)
            .await
            .expect("bind loopback fixture");
        let address = listener.local_addr().expect("fixture address");
        let host_key =
            russh::keys::decode_secret_key(server_key, None).expect("decode fixture host key");
        let config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_key],
            ..server::Config::default()
        });
        let mut factory = FixtureFactory {
            state: Arc::clone(&state),
        };
        let (handle_sender, handle_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let running = factory.run_on_socket(config, &listener);
            let _ = handle_sender.send(running.handle());
            running.await
        });
        let shutdown = handle_receiver.await.expect("fixture startup handle");
        Self {
            address,
            state,
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    fn port(&self) -> u16 {
        self.address.port()
    }

    fn sizes(&self) -> watch::Receiver<Vec<FixtureSize>> {
        self.state.sizes.subscribe()
    }

    async fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.shutdown("fixture stopped".into());
        }
        if let Some(task) = self.task.take() {
            let result = tokio::time::timeout(TEST_TIMEOUT, task)
                .await
                .expect("fixture shutdown deadline")
                .expect("fixture task join");
            result.expect("fixture server shutdown");
        }
    }
}

fn network_profile(name: &str, port: u16, auth: SshAuth) -> SshProfile {
    SshProfile {
        name: name.into(),
        description: Some("loopback russh fixture".into()),
        ssh: SshTarget {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
            user: FIXTURE_USER.into(),
            auth,
            default_cwd: None,
            host_key: None,
        },
        last_used_ms: None,
    }
}

fn exec_request(profile: &str, command: &str) -> SshExecRequest {
    SshExecRequest {
        profile: profile.into(),
        command: command.into(),
        cwd: None,
        timeout: Some(TEST_TIMEOUT),
        close: None,
        output: None,
    }
}

async fn wait_for_output(
    events: &mut tokio::sync::broadcast::Receiver<ShellRegistryEvent>,
    shell_id: &str,
    expected: &[u8],
) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut collected = Vec::new();
        loop {
            if let ShellRegistryEvent::Output { id, bytes, .. } =
                events.recv().await.expect("shell output event")
                && id == shell_id
            {
                collected.extend_from_slice(bytes.as_slice());
                if collected
                    .windows(expected.len())
                    .any(|window| window == expected)
                {
                    break;
                }
            }
        }
    })
    .await
    .expect("shell output deadline");
}

async fn wait_for_exit(
    events: &mut tokio::sync::broadcast::Receiver<ShellRegistryEvent>,
    shell_id: &str,
    code: Option<i32>,
) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if let ShellRegistryEvent::State(shell) =
                events.recv().await.expect("shell state event")
                && shell.id == shell_id
                && shell.status == (ShellStatusWire::Exited { code })
            {
                break;
            }
        }
    })
    .await
    .expect("shell exit deadline");
}

#[tokio::test]
async fn pure_russh_fixture_key_auth_tofu_exec_stream_and_exit_code() {
    let mut server = FixtureServer::start(SERVER_KEY_A, None).await;
    let store = SshProfileStore::new(Arc::new(MemoryVault::default()));
    let key_ref = store
        .put_auth_secret("key-fixture", CLIENT_KEY.as_bytes())
        .expect("store client key");
    store
        .add(network_profile(
            "key-fixture",
            server.port(),
            SshAuth::KeyMaterial { vault_ref: key_ref },
        ))
        .expect("add key profile");
    let runtime = SshRuntime::new(store.clone());

    let pinned = runtime
        .test("key-fixture", Some(TEST_TIMEOUT))
        .await
        .expect("TOFU connection");
    assert!(pinned);
    assert!(
        store
            .get("key-fixture")
            .expect("pinned profile")
            .ssh
            .host_key
            .is_some()
    );
    let result = runtime
        .exec(exec_request("key-fixture", "stream exit-7"))
        .await
        .expect("fixture exec");
    assert!(result.stdout.contains("stdout:stream exit-7"));
    assert!(result.stderr.contains("stderr:stream exit-7"));
    assert_eq!(result.exit_code, Some(7));
    assert_eq!(server.state.public_key_auths.load(Ordering::Relaxed), 1);
    server.stop().await;
}

#[tokio::test]
async fn password_auth_secret_sentinel_never_reaches_json_or_registry_bytes() {
    let mut server = FixtureServer::start(SERVER_KEY_A, None).await;
    let store = SshProfileStore::new(Arc::new(MemoryVault::default()));
    let password_ref = store
        .put_auth_secret("password-fixture", FIXTURE_PASSWORD.as_bytes())
        .expect("store password");
    store
        .add(network_profile(
            "password-fixture",
            server.port(),
            SshAuth::Password {
                vault_ref: password_ref,
            },
        ))
        .expect("add password profile");
    let runtime = SshRuntime::new(store.clone());
    let journal = Arc::new(JournalSink::default());
    let result = runtime
        .exec(SshExecRequest {
            output: Some(SshOutput {
                call_id: "password-fixture-call".into(),
                sink: journal.clone(),
            }),
            ..exec_request("password-fixture", "secret-hygiene")
        })
        .await
        .expect("password exec");
    assert_eq!(server.state.password_auths.load(Ordering::Relaxed), 1);

    let registry = ShellRegistry::default();
    let (shell, _controls) = registry
        .open_interactive(
            ShellKindWire::Ssh {
                profile: "password-fixture".into(),
            },
            "password-fixture",
            "127.0.0.1",
            None,
        )
        .expect("open registry row");
    shell
        .publish_output(
            haider_rpc::ShellOutputStreamWire::Stdout,
            result.stdout.as_bytes(),
        )
        .expect("publish safe output");
    let mut observable =
        serde_json::to_vec(&store.list().expect("list profiles")).expect("serialize profile list");
    observable.extend(serde_json::to_vec(&result).expect("serialize --json result"));
    observable.extend(
        serde_json::to_vec(&registry.list().expect("list shells")).expect("serialize registry"),
    );
    observable.extend(
        serde_json::to_vec(&*journal.deltas.lock().expect("journal deltas"))
            .expect("serialize journal deltas"),
    );
    let encoded_sentinel = base64::engine::general_purpose::STANDARD.encode(FIXTURE_PASSWORD);
    let input_frame = haider_rpc::WireFrame::Request {
        request_id: haider_rpc::RequestId::new("fixture-secret-log-probe"),
        body: haider_rpc::RequestBody::SshShellInput {
            id: shell.id().into(),
            data_b64: haider_rpc::SecretWire::new(encoded_sentinel.clone()),
        },
    };
    let output_frame = haider_rpc::WireFrame::ShellOutput {
        id: shell.id().into(),
        stream: haider_rpc::ShellOutputStreamWire::Stdout,
        chunk_b64: haider_rpc::TerminalOutputWire::new(encoded_sentinel.clone()),
    };
    observable.extend(format!("{input_frame:?}{output_frame:?}").as_bytes());
    assert!(
        !observable
            .windows(FIXTURE_PASSWORD.len())
            .any(|window| window == FIXTURE_PASSWORD.as_bytes())
    );
    assert!(
        !observable
            .windows(encoded_sentinel.len())
            .any(|window| window == encoded_sentinel.as_bytes())
    );
    assert!(
        !format!("{:?}", store.get("password-fixture").expect("profile"))
            .contains(FIXTURE_PASSWORD)
    );
    server.stop().await;
}

#[tokio::test]
async fn tofu_mismatch_is_typed_and_a_dropped_connection_reconnects() {
    let mut first = FixtureServer::start(SERVER_KEY_A, None).await;
    let port = first.port();
    let store = SshProfileStore::new(Arc::new(MemoryVault::default()));
    let key_ref = store
        .put_auth_secret("reconnect", CLIENT_KEY.as_bytes())
        .expect("store client key");
    store
        .add(network_profile(
            "reconnect",
            port,
            SshAuth::KeyMaterial { vault_ref: key_ref },
        ))
        .expect("add reconnect profile");
    let runtime = SshRuntime::new(store.clone());
    runtime
        .test("reconnect", Some(TEST_TIMEOUT))
        .await
        .expect("initial pin");
    let _ = runtime
        .exec(exec_request("reconnect", "fixture-drop"))
        .await;
    let result = runtime
        .exec(exec_request("reconnect", "after-drop"))
        .await
        .expect("reconnect exec");
    assert!(result.stdout.contains("after-drop"));
    assert!(first.state.connections.load(Ordering::Relaxed) >= 2);

    first.stop().await;
    let mut second = FixtureServer::start(SERVER_KEY_B, Some(port)).await;
    let mismatch = runtime
        .exec(exec_request("reconnect", "must-refuse"))
        .await
        .expect_err("changed host key must fail closed");
    assert!(matches!(mismatch, SshError::SshHostKeyChanged { .. }));
    second.stop().await;
}

#[tokio::test]
async fn pty_shell_round_trip_window_change_exit_and_channel_quota() {
    let mut server = FixtureServer::start(SERVER_KEY_A, None).await;
    let store = SshProfileStore::new(Arc::new(MemoryVault::default()));
    let password_ref = store
        .put_auth_secret("pty-fixture", FIXTURE_PASSWORD.as_bytes())
        .expect("store password");
    store
        .add(network_profile(
            "pty-fixture",
            server.port(),
            SshAuth::Password {
                vault_ref: password_ref,
            },
        ))
        .expect("add PTY profile");
    let quota_password_ref = store
        .put_auth_secret("quota-fixture", FIXTURE_PASSWORD.as_bytes())
        .expect("store quota password");
    store
        .add(network_profile(
            "quota-fixture",
            server.port(),
            SshAuth::Password {
                vault_ref: quota_password_ref,
            },
        ))
        .expect("add quota profile");
    let runtime = SshRuntime::new(store);
    let registry = ShellRegistry::default();
    let mut events = registry.subscribe();
    let mut sizes = server.sizes();
    let initial = SshPtySizeWire {
        cols: 80,
        rows: 24,
        pixel_width: 640,
        pixel_height: 480,
    };
    let (shell, controls) = registry
        .open_interactive(
            ShellKindWire::Ssh {
                profile: "pty-fixture".into(),
            },
            "pty-fixture",
            "127.0.0.1",
            None,
        )
        .expect("open PTY row");
    let shell_id = shell.id().to_owned();
    let (activate, activation) = oneshot::channel();
    let opened = runtime
        .start_pty(SshPtyRequest {
            profile: "pty-fixture".into(),
            term: "xterm-256color".into(),
            size: initial,
            shell,
            controls,
            activation: Some(activation),
        })
        .await
        .expect("start PTY");
    assert_eq!(opened.status, ShellStatusWire::Running);
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, ShellRegistryEvent::Output { .. }),
            "PTY output must wait until the opening response is admitted"
        );
    }
    activate
        .send(())
        .expect("activate PTY output after response");
    wait_for_output(&mut events, &shell_id, b"fixture-shell-ready").await;
    wait_for_output(&mut events, &shell_id, b"fixture-shell-stderr").await;
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if sizes.borrow_and_update().contains(&FixtureSize {
                cols: initial.cols,
                rows: initial.rows,
                pixel_width: initial.pixel_width,
                pixel_height: initial.pixel_height,
            }) {
                break;
            }
            sizes.changed().await.expect("initial size observation");
        }
    })
    .await
    .expect("initial PTY size deadline");
    registry
        .control(
            &shell_id,
            None,
            ShellControl::Input(Zeroizing::new(b"round-trip\n".to_vec())),
        )
        .expect("send PTY input");
    wait_for_output(&mut events, &shell_id, b"round-trip").await;

    let resized = SshPtySizeWire {
        cols: 132,
        rows: 43,
        pixel_width: 1056,
        pixel_height: 860,
    };
    registry
        .control(&shell_id, None, ShellControl::Resize(resized))
        .expect("resize PTY");
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if sizes
                .borrow_and_update()
                .iter()
                .any(|size| size.cols == resized.cols && size.rows == resized.rows)
            {
                break;
            }
            sizes.changed().await.expect("size observation");
        }
    })
    .await
    .expect("resize deadline");
    registry
        .control(&shell_id, None, ShellControl::Eof)
        .expect("send PTY EOF");
    wait_for_exit(&mut events, &shell_id, Some(0)).await;
    let reused = runtime
        .exec(exec_request("pty-fixture", "after-pty"))
        .await
        .expect("reuse profile session after PTY exit");
    assert!(reused.stdout.contains("after-pty"));
    assert_eq!(server.state.connections.load(Ordering::Relaxed), 1);

    let mut quota_shells = Vec::new();
    for index in 0..super::runtime::MAX_CHANNELS_PER_PROFILE {
        let (shell, controls) = registry
            .open_interactive(
                ShellKindWire::Ssh {
                    profile: "quota-fixture".into(),
                },
                format!("quota-{index}"),
                "127.0.0.1",
                Some("quota-connection".into()),
            )
            .expect("open quota row");
        let running = runtime
            .start_pty(SshPtyRequest {
                profile: "quota-fixture".into(),
                term: "xterm".into(),
                size: initial,
                shell,
                controls,
                activation: None,
            })
            .await
            .expect("open quota channel");
        quota_shells.push(running.id);
    }
    let (overflow, controls) = registry
        .open_interactive(
            ShellKindWire::Ssh {
                profile: "quota-fixture".into(),
            },
            "quota-overflow",
            "127.0.0.1",
            None,
        )
        .expect("open overflow row");
    let quota = runtime
        .start_pty(SshPtyRequest {
            profile: "quota-fixture".into(),
            term: "xterm".into(),
            size: initial,
            shell: overflow,
            controls,
            activation: None,
        })
        .await
        .expect_err("ninth channel must be refused");
    assert!(matches!(
        quota,
        SshError::SshChannelQuota { limit, .. }
            if limit == super::runtime::MAX_CHANNELS_PER_PROFILE
    ));
    registry
        .close_owner("quota-connection")
        .expect("connection loss closes quota shells");
    for id in &quota_shells {
        assert_eq!(
            registry.get(id).expect("closed quota shell").status,
            ShellStatusWire::Closed
        );
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        while runtime.active_channels("quota-fixture").await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("client-loss quota release deadline");
    let (replacement, controls) = registry
        .open_interactive(
            ShellKindWire::Ssh {
                profile: "quota-fixture".into(),
            },
            "quota-replacement",
            "127.0.0.1",
            Some("quota-replacement-connection".into()),
        )
        .expect("open replacement quota row");
    let replacement = runtime
        .start_pty(SshPtyRequest {
            profile: "quota-fixture".into(),
            term: "xterm".into(),
            size: initial,
            shell: replacement,
            controls,
            activation: None,
        })
        .await
        .expect("released quota permits replacement channel");
    registry
        .close(&replacement.id)
        .expect("close replacement quota shell");
    server.stop().await;
}

#[tokio::test]
async fn narrowed_scope_refuses_the_fixture_profile_before_network_use() {
    let mut server = FixtureServer::start(SERVER_KEY_A, None).await;
    let session = haider_protocol::ids::SessionId::new("fixture-session");
    let scope = SshScope::from_wire(haider_rpc::SshScopeWire::Allow {
        names: vec!["allowed".into()],
    })
    .expect("fixture scope");
    let error = enforce_scope(&scope, &session, "pty-fixture")
        .expect_err("profile outside the narrowed session scope");
    assert_eq!(error.code(), "ssh_profile_out_of_scope");
    assert_eq!(server.state.connections.load(Ordering::Relaxed), 0);
    server.stop().await;
}
