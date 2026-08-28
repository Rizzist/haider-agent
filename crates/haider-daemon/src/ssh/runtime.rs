//! Cross-platform SSH runtime backed exclusively by `russh`.
//!
//! One authenticated client connection is retained per profile. Commands are
//! channels on that connection on every supported platform; no
//! process, argv, environment variable, global known-hosts file, or runtime
//! socket ever carries authentication material.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_protocol::item::{ItemDelta, OutputStream};
use haider_rpc::SshShellResultWire;
use haider_tools::CommandOutputSink;
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tokio::sync::Mutex;
use tokio::sync::watch;
use zeroize::Zeroizing;

use super::{PinnedHostKey, SshAuth, SshError, SshProfile, SshProfileStore};

const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_COMMAND_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const OUTPUT_LIMIT: usize = haider_tools::PROCESS_MAX_OUTPUT_BYTES;

#[derive(Clone)]
pub(crate) struct SshExecRequest {
    pub(crate) profile: String,
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) close: Option<watch::Receiver<bool>>,
    pub(crate) output: Option<SshOutput>,
}

#[derive(Clone)]
pub(crate) struct SshOutput {
    pub(crate) call_id: String,
    pub(crate) sink: Arc<dyn CommandOutputSink>,
}

#[derive(Clone)]
pub(crate) struct SshRuntime {
    store: SshProfileStore,
    sessions: Arc<Mutex<SessionPool>>,
}

type SessionSlot = Arc<Mutex<Option<LiveSession>>>;
type SessionPool = BTreeMap<String, SessionSlot>;

struct LiveSession {
    handle: Arc<client::Handle<HostKeyHandler>>,
    activity: Arc<StdMutex<SessionActivity>>,
    identity: SshConnectionIdentity,
}

#[derive(Clone, PartialEq, Eq)]
struct SshConnectionIdentity {
    host: String,
    port: u16,
    user: String,
    auth: SshAuth,
}

impl From<&SshProfile> for SshConnectionIdentity {
    fn from(profile: &SshProfile) -> Self {
        Self {
            host: profile.ssh.host.clone(),
            port: profile.ssh.port,
            user: profile.ssh.user.clone(),
            auth: profile.ssh.auth.clone(),
        }
    }
}

struct SessionActivity {
    active_channels: usize,
    idle_generation: u64,
}

struct SessionLease {
    slot: SessionSlot,
    activity: Arc<StdMutex<SessionActivity>>,
}

impl SessionLease {
    fn begin(slot: SessionSlot, activity: Arc<StdMutex<SessionActivity>>) -> Self {
        let mut state = activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active_channels = state.active_channels.saturating_add(1);
        state.idle_generation = state.idle_generation.wrapping_add(1);
        drop(state);
        Self { slot, activity }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let generation = {
            let mut state = self
                .activity
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.active_channels = state.active_channels.saturating_sub(1);
            if state.active_channels != 0 {
                return;
            }
            state.idle_generation = state.idle_generation.wrapping_add(1);
            state.idle_generation
        };
        let slot = Arc::clone(&self.slot);
        let activity = Arc::clone(&self.activity);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep(SESSION_IDLE_TIMEOUT).await;
            let handle = {
                let mut live = slot.lock().await;
                let should_disconnect = live.as_ref().is_some_and(|session| {
                    if !Arc::ptr_eq(&session.activity, &activity) {
                        return false;
                    }
                    let state = activity
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.active_channels == 0 && state.idle_generation == generation
                });
                if should_disconnect {
                    live.take().map(|session| session.handle)
                } else {
                    None
                }
            };
            if let Some(handle) = handle {
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "SSH session idle timeout", "en")
                    .await;
            }
        });
    }
}

#[derive(Debug)]
enum HandlerError {
    Russh(russh::Error),
    HostKeyChanged { expected: String, actual: String },
    HostKeyState,
}

impl fmt::Display for HandlerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Russh(error) => error.fmt(formatter),
            Self::HostKeyChanged { expected, actual } => {
                write!(
                    formatter,
                    "SSH host key changed (expected {expected}, actual {actual})"
                )
            }
            Self::HostKeyState => formatter.write_str("SSH host-key state is unavailable"),
        }
    }
}

impl std::error::Error for HandlerError {}

impl From<russh::Error> for HandlerError {
    fn from(error: russh::Error) -> Self {
        Self::Russh(error)
    }
}

#[derive(Clone)]
struct HostKeyHandler {
    expected: Option<PinnedHostKey>,
    observed: Arc<StdMutex<Option<PinnedHostKey>>>,
}

impl client::Handler for HostKeyHandler {
    type Error = HandlerError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        let actual = PinnedHostKey {
            algorithm: key.algorithm().as_str().to_owned(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            pinned_at_ms: unix_ms(),
        };
        if let Some(expected) = &self.expected
            && expected.fingerprint != actual.fingerprint
        {
            return Err(HandlerError::HostKeyChanged {
                expected: expected.fingerprint.clone(),
                actual: actual.fingerprint,
            });
        }
        *self
            .observed
            .lock()
            .map_err(|_| HandlerError::HostKeyState)? = Some(actual);
        Ok(true)
    }
}

impl SshRuntime {
    pub(crate) fn new(store: SshProfileStore) -> Self {
        Self {
            store,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub(crate) async fn test(
        &self,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<bool, SshError> {
        let was_pinned = self.store.get(name)?.ssh.host_key.is_some();
        self.exec(SshExecRequest {
            profile: name.to_owned(),
            command: "true".into(),
            cwd: None,
            timeout,
            close: None,
            output: None,
        })
        .await?;
        Ok(!was_pinned && self.store.get(name)?.ssh.host_key.is_some())
    }

    pub(crate) async fn exec(
        &self,
        request: SshExecRequest,
    ) -> Result<SshShellResultWire, SshError> {
        let timeout = request
            .timeout
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT)
            .min(MAX_COMMAND_TIMEOUT);
        let future = self.exec_inner(&request);
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Ok(SshShellResultWire {
                profile: request.profile,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                exit_code: None,
                timed_out: true,
            }),
        }
    }

    pub(crate) async fn forget(&self, name: &str) {
        let session = self.sessions.lock().await.remove(name);
        if let Some(session) = session
            && let Some(live) = session.lock().await.take()
        {
            let _ = live
                .handle
                .disconnect(Disconnect::ByApplication, "profile removed", "en")
                .await;
        }
    }

    async fn exec_inner(&self, request: &SshExecRequest) -> Result<SshShellResultWire, SshError> {
        let slot = self.session_slot(&request.profile).await;
        let mut guard = slot.lock().await;
        let profile = self.store.get(&request.profile)?;
        let identity = SshConnectionIdentity::from(&profile);
        if guard
            .as_ref()
            .is_none_or(|session| session.handle.is_closed() || session.identity != identity)
        {
            *guard = Some(self.connect(&profile).await?);
        }
        let handle = Arc::clone(
            &guard
                .as_ref()
                .ok_or_else(|| SshError::SshConnection {
                    message: "authenticated session was not retained".into(),
                })?
                .handle,
        );
        let activity = Arc::clone(
            &guard
                .as_ref()
                .ok_or_else(|| SshError::SshConnection {
                    message: "authenticated session activity was not retained".into(),
                })?
                .activity,
        );
        let mut lease = Some(SessionLease::begin(Arc::clone(&slot), activity));
        drop(guard);
        let command = command_in_cwd(
            &request.command,
            request
                .cwd
                .as_deref()
                .or(profile.ssh.default_cwd.as_deref()),
        );
        let mut channel = match handle.channel_open_session().await {
            Ok(channel) => channel,
            Err(_) if handle.is_closed() => {
                // Opening a channel has not dispatched command bytes, so one
                // reconnect is safe. Never retry after `exec`, where the
                // server may already have acted despite a dropped reply.
                drop(lease.take());
                let mut guard = slot.lock().await;
                *guard = Some(self.connect(&profile).await?);
                let live = guard.as_ref().ok_or_else(|| SshError::SshConnection {
                    message: "reconnected session was not retained".into(),
                })?;
                let handle = Arc::clone(&live.handle);
                lease = Some(SessionLease::begin(
                    Arc::clone(&slot),
                    Arc::clone(&live.activity),
                ));
                drop(guard);
                handle
                    .channel_open_session()
                    .await
                    .map_err(connection_error)?
            }
            Err(error) => return Err(connection_error(error)),
        };
        let _lease = lease;
        channel
            .exec(true, command)
            .await
            .map_err(connection_error)?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;
        let mut exit_code = None;
        let mut closed_by_operator = false;
        let mut close = request.close.clone();
        loop {
            let message = tokio::select! {
                message = channel.wait() => message,
                changed = async {
                    match close.as_mut() {
                        Some(close) => close.changed().await,
                        None => std::future::pending().await,
                    }
                }, if close.is_some() => {
                    if changed.is_ok() && close.as_mut().is_some_and(|close| *close.borrow_and_update()) {
                        let _ = channel.close().await;
                        closed_by_operator = true;
                    }
                    None
                }
            };
            let Some(message) = message else {
                break;
            };
            let output_limit_reached = match message {
                ChannelMsg::Data { data } => {
                    let (limit_reached, retained) = append_bounded(
                        &mut stdout,
                        stderr.len(),
                        data.as_ref(),
                        &mut stdout_truncated,
                    );
                    emit_output(&request.output, OutputStream::Stdout, &data[..retained]).await?;
                    limit_reached
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    let (limit_reached, retained) = append_bounded(
                        &mut stderr,
                        stdout.len(),
                        data.as_ref(),
                        &mut stderr_truncated,
                    );
                    emit_output(&request.output, OutputStream::Stderr, &data[..retained]).await?;
                    limit_reached
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = i32::try_from(exit_status).ok();
                    false
                }
                _ => false,
            };
            if output_limit_reached {
                let _ = channel.close().await;
                break;
            }
        }
        if closed_by_operator {
            return Err(SshError::SshChannelClosed { name: profile.name });
        }
        if let Err(error) = self.store.mark_used(&profile.name, unix_ms()) {
            // Last-used is presentation metadata, never execution truth. A
            // failed metadata write after the server ran the command must not
            // turn success into a retryable failure and execute it twice.
            tracing::warn!(profile = %profile.name, error = %error, "could not persist SSH last-used time");
        }
        Ok(SshShellResultWire {
            profile: profile.name,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_truncated,
            stderr_truncated,
            exit_code,
            timed_out: false,
        })
    }

    async fn session_slot(&self, name: &str) -> SessionSlot {
        let mut sessions = self.sessions.lock().await;
        sessions
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    async fn connect(&self, profile: &SshProfile) -> Result<LiveSession, SshError> {
        let observed = Arc::new(StdMutex::new(None));
        let handler = HostKeyHandler {
            expected: profile.ssh.host_key.clone(),
            observed: Arc::clone(&observed),
        };
        let mut config = client::Config::default();
        config.keepalive_interval = Some(KEEPALIVE_INTERVAL);
        config.keepalive_max = 3;
        let config = Arc::new(config);
        let mut handle = client::connect(
            config,
            (profile.ssh.host.as_str(), profile.ssh.port),
            handler,
        )
        .await
        .map_err(handler_error)?;
        let authenticated = self.authenticate(&mut handle, profile).await?;
        if !authenticated {
            return Err(SshError::SshAuthenticationFailed {
                name: profile.name.clone(),
            });
        }
        let pinned = observed
            .lock()
            .map_err(|_| SshError::SshConnection {
                message: "SSH host-key state is unavailable".into(),
            })?
            .clone()
            .ok_or_else(|| SshError::SshConnection {
                message: "SSH server did not present a host key".into(),
            })?;
        self.store.pin_host_key(&profile.name, pinned)?;
        Ok(LiveSession {
            handle: Arc::new(handle),
            activity: Arc::new(StdMutex::new(SessionActivity {
                active_channels: 0,
                idle_generation: 0,
            })),
            identity: SshConnectionIdentity::from(profile),
        })
    }

    async fn authenticate(
        &self,
        handle: &mut client::Handle<HostKeyHandler>,
        profile: &SshProfile,
    ) -> Result<bool, SshError> {
        match &profile.ssh.auth {
            SshAuth::Password { vault_ref } => {
                let secret = self.store.resolve_auth_secret(&profile.name, vault_ref)?;
                let password = std::str::from_utf8(secret.expose_secret()).map_err(|_| {
                    SshError::SshAuthenticationFailed {
                        name: profile.name.clone(),
                    }
                })?;
                handle
                    .authenticate_password(&profile.ssh.user, password.to_owned())
                    .await
                    .map(|result| result.success())
                    .map_err(connection_error)
            }
            SshAuth::KeyFile {
                path,
                passphrase_vault_ref,
            } => {
                let key_text =
                    Zeroizing::new(tokio::fs::read_to_string(path).await.map_err(|_| {
                        SshError::SshKeyInvalid {
                            name: profile.name.clone(),
                        }
                    })?);
                let passphrase_secret = passphrase_vault_ref
                    .as_deref()
                    .map(|vault_ref| self.store.resolve_auth_secret(&profile.name, vault_ref))
                    .transpose()?;
                let passphrase = passphrase_secret
                    .as_ref()
                    .map(|secret| std::str::from_utf8(secret.expose_secret()))
                    .transpose()
                    .map_err(|_| SshError::SshKeyInvalid {
                        name: profile.name.clone(),
                    })?;
                self.authenticate_key(handle, profile, key_text.as_str(), passphrase)
                    .await
            }
            SshAuth::KeyMaterial { vault_ref } => {
                let secret = self.store.resolve_auth_secret(&profile.name, vault_ref)?;
                let key_text = std::str::from_utf8(secret.expose_secret()).map_err(|_| {
                    SshError::SshKeyInvalid {
                        name: profile.name.clone(),
                    }
                })?;
                self.authenticate_key(handle, profile, key_text, None).await
            }
            SshAuth::Agent => authenticate_agent(handle, profile).await,
        }
    }

    async fn authenticate_key(
        &self,
        handle: &mut client::Handle<HostKeyHandler>,
        profile: &SshProfile,
        key_text: &str,
        passphrase: Option<&str>,
    ) -> Result<bool, SshError> {
        let key = russh::keys::decode_secret_key(key_text, passphrase).map_err(|_| {
            SshError::SshKeyInvalid {
                name: profile.name.clone(),
            }
        })?;
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .map_err(connection_error)?
            .flatten();
        handle
            .authenticate_publickey(
                &profile.ssh.user,
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .map(|result| result.success())
            .map_err(connection_error)
    }
}

#[cfg(any(target_os = "android", not(any(unix, windows))))]
async fn authenticate_agent(
    _handle: &mut client::Handle<HostKeyHandler>,
    _profile: &SshProfile,
) -> Result<bool, SshError> {
    Err(SshError::SshAgentUnavailable)
}

#[cfg(all(unix, not(target_os = "android")))]
async fn authenticate_agent(
    handle: &mut client::Handle<HostKeyHandler>,
    profile: &SshProfile,
) -> Result<bool, SshError> {
    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|_| SshError::SshAgentUnavailable)?;
    authenticate_agent_identities(handle, profile, &mut agent).await
}

#[cfg(windows)]
async fn authenticate_agent(
    handle: &mut client::Handle<HostKeyHandler>,
    profile: &SshProfile,
) -> Result<bool, SshError> {
    const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    let mut agent = russh::keys::agent::client::AgentClient::connect_named_pipe(OPENSSH_AGENT_PIPE)
        .await
        .map_err(|_| SshError::SshAgentUnavailable)?;
    authenticate_agent_identities(handle, profile, &mut agent).await
}

#[cfg(any(all(unix, not(target_os = "android")), windows))]
async fn authenticate_agent_identities<S>(
    handle: &mut client::Handle<HostKeyHandler>,
    profile: &SshProfile,
    agent: &mut russh::keys::agent::client::AgentClient<S>,
) -> Result<bool, SshError>
where
    S: russh::keys::agent::client::AgentStream + Send + Unpin,
{
    let identities = agent
        .request_identities()
        .await
        .map_err(|_| SshError::SshAgentUnavailable)?;
    for identity in identities {
        let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .map_err(connection_error)?
            .flatten();
        let authenticated = handle
            .authenticate_publickey_with(&profile.ssh.user, key, hash, agent)
            .await
            .map_err(|_| SshError::SshAuthenticationFailed {
                name: profile.name.clone(),
            })?;
        if authenticated.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn append_bounded(
    output: &mut Vec<u8>,
    other_len: usize,
    bytes: &[u8],
    truncated: &mut bool,
) -> (bool, usize) {
    let combined = output.len().saturating_add(other_len);
    let remaining = OUTPUT_LIMIT.saturating_sub(combined);
    let take = remaining.min(bytes.len());
    output.extend_from_slice(&bytes[..take]);
    let limit_reached =
        take < bytes.len() || output.len().saturating_add(other_len) >= OUTPUT_LIMIT;
    *truncated |= limit_reached;
    (limit_reached, take)
}

async fn emit_output(
    output: &Option<SshOutput>,
    stream: OutputStream,
    bytes: &[u8],
) -> Result<(), SshError> {
    let Some(output) = output else {
        return Ok(());
    };
    if bytes.is_empty() {
        return Ok(());
    }
    output
        .sink
        .emit(
            &output.call_id,
            ItemDelta::CommandOutput {
                stream,
                chunk_b64: BASE64.encode(bytes),
            },
        )
        .await
        .map_err(|error| SshError::SshConnection {
            message: format!("cannot journal remote command output: {error}"),
        })
}

pub(super) fn command_in_cwd(command: &str, cwd: Option<&str>) -> String {
    cwd.map_or_else(
        || command.to_owned(),
        |cwd| format!("cd -- {} && {command}", shell_quote(cwd)),
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn handler_error(error: HandlerError) -> SshError {
    match error {
        HandlerError::HostKeyChanged { expected, actual } => {
            SshError::SshHostKeyChanged { expected, actual }
        }
        other => SshError::SshConnection {
            message: other.to_string(),
        },
    }
}

fn connection_error(error: russh::Error) -> SshError {
    SshError::SshConnection {
        message: error.to_string(),
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
