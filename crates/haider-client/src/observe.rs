//! Read-only daemon observation and raw-envelope streaming.
//!
//! Snapshot state comes from the daemon's journal-derived digest. Streams use
//! the existing view-only attachment lane: sequence is the sole cursor, raw
//! payload JSON is never narrowed, and every reconnect resumes each session
//! after the greatest envelope admitted to the lossless caller spool.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use haider_rpc::haider_protocol::credential::CredentialDescriptor;
use haider_rpc::haider_protocol::envelope::RawEnvelope;
use haider_rpc::haider_protocol::ids::SessionId;
use haider_rpc::{
    AttachMode, AttachmentId, Capability, CapabilitySet, ClientKind, DescendantReplayCursorWire,
    ERROR_CODE_NOT_FOUND, FEATURE_EFFECT_RECOVERY_V1, FEATURE_SESSION_DESCENDANT_STREAM_V1,
    FEATURE_SESSION_FLEET_V1, FEATURE_SESSION_LIST_WATCH_V1, FEATURE_SESSION_OBSERVE_BATCH_V1,
    FEATURE_SESSION_OBSERVE_V1, LifecyclePhase, ObserveRunStateWire, RequestBody, ResponseBody,
    SessionDescendantBaselineWire, SessionFleetSnapshot, SessionObserveDigest, SessionSummary,
    Welcome, WireFrame,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::client::{
    ClientCloseOutcome, ClientConfig, ClientError, ClientHealthWait, ConnectError, ConnectionState,
    ConnectionUsage, RpcClient, connect,
};
use crate::profile::ResolvedProfile;
use crate::spawn::{EnsureError, EnsureOptions, ensure_daemon};

const LIST_PAGE: u32 = 100;
const EVENT_SHARD_SIZE: usize = 16;
const STREAM_HEALTH_REPAIR_INTERVAL: Duration = Duration::from_secs(30);

/// Typed failure for the scriptable observation surface.
#[derive(Debug)]
pub enum ObserveError {
    NoDaemon(ConnectError),
    Connect(ConnectError),
    Ensure(EnsureError),
    NotReady(LifecyclePhase),
    ProfileMismatch {
        expected: String,
        actual: String,
    },
    MissingFeature(&'static str),
    UnknownSession(SessionId),
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
    OutputClosed,
    StreamTask(String),
}

impl std::fmt::Display for ObserveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDaemon(error) => write!(formatter, "daemon is unavailable: {error}"),
            Self::Connect(error) => write!(formatter, "cannot connect to daemon: {error}"),
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::NotReady(phase) => write!(formatter, "daemon is not ready ({phase:?})"),
            Self::ProfileMismatch { expected, actual } => write!(
                formatter,
                "daemon serves profile {actual}, expected {expected}"
            ),
            Self::MissingFeature(feature) => {
                write!(
                    formatter,
                    "daemon does not advertise required feature `{feature}`"
                )
            }
            Self::UnknownSession(session_id) => {
                write!(formatter, "session `{session_id}` was not found")
            }
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc {
                code,
                message,
                retryable: _,
            } => write!(formatter, "daemon rejected observation ({code}): {message}"),
            Self::Protocol(message) => formatter.write_str(message),
            Self::OutputClosed => formatter.write_str("observation output closed"),
            Self::StreamTask(message) => write!(formatter, "observation stream failed: {message}"),
        }
    }
}

impl std::error::Error for ObserveError {}

impl From<ClientError> for ObserveError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

/// One connected, view-only observation client.
pub struct ObserveClient {
    client: RpcClient,
    welcome: Welcome,
}

/// Scalar-only snapshot used by short-lived status clients.
pub struct ObserveStatusSnapshot {
    pub active_account: Option<CredentialDescriptor>,
    pub session_count: u64,
    pub waiting_for_route_count: u64,
    pub adoption_available: Vec<haider_rpc::AccountAdoptionAvailable>,
    pub daemon_pid: Option<u32>,
    pub socket_path: Option<String>,
    pub pid_file_path: Option<String>,
    /// The daemon's serving edge. For pre-feature daemons, a negotiated
    /// Ready Welcome is the compatible source of the same lifecycle fact.
    pub ready: bool,
}

/// Finite result from the event-driven durable-roster barrier.
///
/// `ready` is true only when the daemon is at `Ready` (enforced by
/// [`ObserveClient::connect`]) and the requested current-format session rows
/// have all been published. The barrier never requires a provider run to be
/// idle: a resource orchestrator can deliberately hold live running sessions.
pub struct SessionReadinessSnapshot {
    pub daemon_ready: bool,
    pub daemon_generation: u64,
    pub expected_count: usize,
    pub ready_count: usize,
    pub total_session_count: usize,
    pub timed_out: bool,
    pub summaries: Vec<SessionSummary>,
}

/// Finite result from waiting for one durable session to leave its running
/// state. Parked and recovery-required states are settled results, not hangs.
pub struct SessionResumeSnapshot {
    pub daemon_ready: bool,
    pub daemon_generation: u64,
    pub timed_out: bool,
    pub summary: Option<SessionSummary>,
}

/// Feature-negotiated descendant view. `Snapshot` is deliberately a separate
/// variant: it carries no event receiver and cannot be mistaken for live
/// lineage when an older daemon omits `session_descendant_stream_v1`.
pub enum DescendantView {
    Live(DescendantLiveAttachment),
    Snapshot(SessionFleetSnapshot),
}

/// One real `session.descendants.attach` result. The receiver carries
/// `SessionDescendantStream` and system-lane repair/drain frames for this
/// connection; callers advance child cursors only after applying envelopes.
pub struct DescendantLiveAttachment {
    pub attachment_id: AttachmentId,
    pub baseline: SessionDescendantBaselineWire,
    pub events: mpsc::Receiver<WireFrame>,
    /// Loss counter sampled before the request. A later value from
    /// [`ObserveClient::lost_events`] requires reconnect from applied cursors.
    pub lost_events_at_attach: u64,
}

impl ObserveClient {
    /// Connects to the resolved profile, optionally using the standard safe
    /// connect-or-spawn path. `spawn == false` never unlinks or starts anything.
    pub async fn connect(profile: &ResolvedProfile, spawn: bool) -> Result<Self, ObserveError> {
        Self::connect_with_usage(profile, spawn, ConnectionUsage::LongLived).await
    }

    /// Connects without allocating an event queue or arming heartbeat work.
    pub async fn connect_one_shot(
        profile: &ResolvedProfile,
        spawn: bool,
    ) -> Result<Self, ObserveError> {
        Self::connect_with_usage(profile, spawn, ConnectionUsage::OneShot).await
    }

    async fn connect_with_usage(
        profile: &ResolvedProfile,
        spawn: bool,
        connection_usage: ConnectionUsage,
    ) -> Result<Self, ObserveError> {
        let config = ClientConfig {
            connection_usage,
            ..observe_client_config()
        };
        let (client, welcome) = if spawn {
            let options = EnsureOptions {
                required_features: BTreeSet::new(),
                client: config,
                ..EnsureOptions::default()
            };
            let ensured = ensure_daemon(profile, options)
                .await
                .map_err(ObserveError::Ensure)?;
            (ensured.client, ensured.welcome)
        } else {
            match connect(&profile.endpoint_path, config).await {
                Ok(connected) => (connected.client, connected.welcome),
                Err(error) if error.is_spawnable() => return Err(ObserveError::NoDaemon(error)),
                Err(error) => return Err(ObserveError::Connect(error)),
            }
        };
        if !welcome.profile_id.is_empty() && welcome.profile_id != profile.profile_id {
            let _ = client.close();
            return Err(ObserveError::ProfileMismatch {
                expected: profile.profile_id.clone(),
                actual: welcome.profile_id,
            });
        }
        if welcome.lifecycle_phase != LifecyclePhase::Ready {
            let phase = welcome.lifecycle_phase;
            let _ = client.close();
            return Err(ObserveError::NotReady(phase));
        }
        Ok(Self { client, welcome })
    }

    #[must_use]
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Consumes a one-shot observer without cloning its negotiated feature
    /// set. Dropping the client half still performs the ordinary typed close.
    #[must_use]
    pub fn into_welcome(self) -> Welcome {
        let Self { client, welcome } = self;
        drop(client);
        welcome
    }

    pub fn close(&self) -> ClientCloseOutcome {
        self.client.close()
    }

    /// Number of uncorrelated frames this connection could not spool. Any
    /// increase invalidates a live descendant view until cursor-based repair.
    #[must_use]
    pub fn lost_events(&self) -> u64 {
        self.client.lost_events()
    }

    /// Reads the daemon-authorized status scalars without materializing the
    /// account roster or session summaries.
    pub async fn status_snapshot(&self) -> Result<ObserveStatusSnapshot, ObserveError> {
        if !self
            .welcome
            .features
            .contains(haider_rpc::FEATURE_STATUS_SNAPSHOT_V1)
        {
            return Err(ObserveError::MissingFeature(
                haider_rpc::FEATURE_STATUS_SNAPSHOT_V1,
            ));
        }
        match self.client.request(RequestBody::StatusSnapshot {}).await? {
            ResponseBody::StatusSnapshot {
                active_account,
                session_count,
                waiting_for_route_count,
                adoption_available,
                daemon_pid,
                socket_path,
                pid_file_path,
                ready,
            } => Ok(ObserveStatusSnapshot {
                active_account,
                session_count,
                waiting_for_route_count,
                adoption_available,
                daemon_pid,
                socket_path,
                pid_file_path,
                ready: if self
                    .welcome
                    .features
                    .contains(haider_rpc::FEATURE_STATUS_RUNTIME_V1)
                {
                    ready
                } else {
                    self.welcome.lifecycle_phase == haider_rpc::LifecyclePhase::Ready
                },
            }),
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => Err(ObserveError::Rpc {
                code,
                message,
                retryable,
            }),
            _ => Err(ObserveError::Protocol(
                "status.snapshot response method mismatch",
            )),
        }
    }

    /// Lists every durable session in the daemon's stable byte order.
    pub async fn session_ids(&self) -> Result<Vec<SessionId>, ObserveError> {
        Ok(self
            .session_summaries()
            .await?
            .into_iter()
            .map(|summary| summary.session_id)
            .collect())
    }

    /// Lists every durable session summary in the daemon's stable byte order.
    pub async fn session_summaries(&self) -> Result<Vec<SessionSummary>, ObserveError> {
        let mut cursor = None;
        let mut sessions = Vec::new();
        loop {
            let response = self
                .client
                .request(RequestBody::SessionList {
                    cursor,
                    limit: LIST_PAGE,
                })
                .await?;
            match response {
                ResponseBody::SessionList {
                    sessions: page,
                    next_cursor,
                } => {
                    sessions.extend(page);
                    let Some(next) = next_cursor else {
                        return Ok(sessions);
                    };
                    cursor = Some(next);
                }
                ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    ..
                } => {
                    return Err(ObserveError::Rpc {
                        code,
                        message,
                        retryable,
                    });
                }
                _ => {
                    return Err(ObserveError::Protocol(
                        "session.list response method mismatch",
                    ));
                }
            }
        }
    }

    /// Reads the secret-free daemon projection for one session.
    pub async fn session(
        &self,
        session_id: SessionId,
        last_event_limit: u32,
    ) -> Result<SessionObserveDigest, ObserveError> {
        self.session_observe(session_id, last_event_limit, false)
            .await
    }

    /// Reads only the authoritative digest fields (`metadata`, `title`,
    /// `head_seq`, `worker_generation`) for one session. A current daemon
    /// skips the full-replay projection; an older daemon ignores the flag
    /// and serves the full digest — the authoritative fields are identical
    /// either way.
    pub async fn session_metadata_only(
        &self,
        session_id: SessionId,
    ) -> Result<SessionObserveDigest, ObserveError> {
        self.session_observe(session_id, 0, true).await
    }

    async fn session_observe(
        &self,
        session_id: SessionId,
        last_event_limit: u32,
        metadata_only: bool,
    ) -> Result<SessionObserveDigest, ObserveError> {
        if !self.welcome.features.contains(FEATURE_SESSION_OBSERVE_V1) {
            return Err(ObserveError::MissingFeature(FEATURE_SESSION_OBSERVE_V1));
        }
        match self
            .client
            .request(RequestBody::SessionObserve {
                session_id,
                last_event_limit,
                metadata_only,
            })
            .await?
        {
            ResponseBody::SessionObserve { digest } => Ok(digest),
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => Err(ObserveError::Rpc {
                code,
                message,
                retryable,
            }),
            _ => Err(ObserveError::Protocol(
                "session.observe response method mismatch",
            )),
        }
    }

    /// Reads the bounded durable descendant tree and daemon-side rollup for
    /// one live or terminal session.
    pub async fn fleet(&self, session_id: SessionId) -> Result<SessionFleetSnapshot, ObserveError> {
        self.require_fleet_feature()?;
        let requested_session = session_id.clone();
        match self
            .client
            .request(RequestBody::SessionFleet { session_id })
            .await?
        {
            ResponseBody::SessionFleet { snapshot } => Ok(snapshot),
            ResponseBody::Error { code, .. } if code == ERROR_CODE_NOT_FOUND => {
                Err(ObserveError::UnknownSession(requested_session))
            }
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => Err(ObserveError::Rpc {
                code,
                message,
                retryable,
            }),
            _ => Err(ObserveError::Protocol(
                "session.fleet response method mismatch",
            )),
        }
    }

    /// Opens the reconnectable descendant view when advertised. If and only
    /// if that feature is absent, falls back to the separately feature-gated
    /// point-in-time fleet snapshot and returns the `Snapshot` variant.
    pub async fn descendants_attach(
        &self,
        session_id: SessionId,
        cursors: Vec<DescendantReplayCursorWire>,
        max_children: u32,
    ) -> Result<DescendantView, ObserveError> {
        if !self
            .welcome
            .features
            .contains(FEATURE_SESSION_DESCENDANT_STREAM_V1)
        {
            return self.fleet(session_id).await.map(DescendantView::Snapshot);
        }
        let Some(events) = self.client.take_events() else {
            return Err(ObserveError::Protocol(
                "descendant attachment event stream was already taken",
            ));
        };
        let requested_session = session_id.clone();
        let lost_events_at_attach = self.client.lost_events();
        let response = match self
            .client
            .request(RequestBody::SessionDescendantsAttach {
                session_id,
                cursors,
                max_children,
            })
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.restore_descendant_events(events)?;
                return Err(error.into());
            }
        };
        match response {
            ResponseBody::SessionDescendantsAttach {
                attachment_id,
                baseline,
            } if baseline.session_id == requested_session => {
                Ok(DescendantView::Live(DescendantLiveAttachment {
                    attachment_id,
                    baseline,
                    events,
                    lost_events_at_attach,
                }))
            }
            ResponseBody::Error { code, .. } if code == ERROR_CODE_NOT_FOUND => {
                self.restore_descendant_events(events)?;
                Err(ObserveError::UnknownSession(requested_session))
            }
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => {
                self.restore_descendant_events(events)?;
                Err(ObserveError::Rpc {
                    code,
                    message,
                    retryable,
                })
            }
            _ => {
                self.restore_descendant_events(events)?;
                Err(ObserveError::Protocol(
                    "session.descendants.attach response method mismatch",
                ))
            }
        }
    }

    fn restore_descendant_events(
        &self,
        events: mpsc::Receiver<WireFrame>,
    ) -> Result<(), ObserveError> {
        self.client.restore_events(events).map_err(|_| {
            ObserveError::Protocol("descendant attachment event stream could not be restored")
        })
    }

    /// Fails with the same typed feature error as [`Self::fleet`] before a
    /// list-mode caller performs any per-session reads.
    pub fn require_fleet_feature(&self) -> Result<(), ObserveError> {
        if self.welcome.features.contains(FEATURE_SESSION_FLEET_V1) {
            Ok(())
        } else {
            Err(ObserveError::MissingFeature(FEATURE_SESSION_FLEET_V1))
        }
    }

    /// Fails before a recovery sweep can misread an older daemon's omitted
    /// additive state as an empty fleet.
    pub fn require_effect_recovery_feature(&self) -> Result<(), ObserveError> {
        if self.welcome.features.contains(FEATURE_EFFECT_RECOVERY_V1) {
            Ok(())
        } else {
            Err(ObserveError::MissingFeature(FEATURE_EFFECT_RECOVERY_V1))
        }
    }

    /// Reads all session digests in stable session-id order.
    pub async fn sessions(
        &self,
        last_event_limit: u32,
    ) -> Result<Vec<SessionObserveDigest>, ObserveError> {
        let ids = self.session_ids().await?;
        if self
            .welcome
            .features
            .contains(FEATURE_SESSION_OBSERVE_BATCH_V1)
        {
            let mut digests = Vec::with_capacity(ids.len());
            for session_ids in ids.chunks(64) {
                match self
                    .client
                    .request(RequestBody::SessionObserveBatch {
                        session_ids: session_ids.to_vec(),
                        last_event_limit,
                        metadata_only: false,
                    })
                    .await?
                {
                    ResponseBody::SessionObserveBatch {
                        digests: batch_digests,
                    } => digests.extend(batch_digests),
                    ResponseBody::Error {
                        code,
                        message,
                        retryable,
                        ..
                    } => {
                        return Err(ObserveError::Rpc {
                            code,
                            message,
                            retryable,
                        });
                    }
                    _ => {
                        return Err(ObserveError::Protocol(
                            "session.observe_batch response method mismatch",
                        ));
                    }
                }
            }
            return Ok(digests);
        }
        let mut digests = Vec::with_capacity(ids.len());
        for session_id in ids {
            digests.push(self.session(session_id, last_event_limit).await?);
        }
        Ok(digests)
    }

    /// Returns the existing headless convention's first active descriptor.
    pub async fn active_account(&self) -> Result<Option<CredentialDescriptor>, ObserveError> {
        match self
            .client
            .request(RequestBody::AccountList { provider: None })
            .await?
        {
            ResponseBody::AccountList { descriptors, .. } => {
                Ok(descriptors.into_iter().find(|descriptor| descriptor.active))
            }
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => Err(ObserveError::Rpc {
                code,
                message,
                retryable,
            }),
            _ => Err(ObserveError::Protocol(
                "account.list response method mismatch",
            )),
        }
    }

    /// Detects first-party CLI logins that are not represented in Haider.
    /// This read returns only public identity metadata and never imports.
    pub async fn account_adoption_available(
        &self,
    ) -> Result<Vec<haider_rpc::AccountAdoptionAvailable>, ObserveError> {
        if !self
            .welcome
            .features
            .contains(haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1)
        {
            return Ok(Vec::new());
        }
        match self
            .client
            .request(RequestBody::AccountDeviceCandidates)
            .await?
        {
            ResponseBody::AccountDeviceCandidates {
                adoption_available, ..
            } => Ok(adoption_available),
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => Err(ObserveError::Rpc {
                code,
                message,
                retryable,
            }),
            _ => Err(ObserveError::Protocol(
                "account.device_candidates response method mismatch",
            )),
        }
    }
}

fn observe_client_config() -> ClientConfig {
    ClientConfig {
        client_name: "haider-observe".into(),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View]),
        ..ClientConfig::default()
    }
}

/// Streams one session as lossless raw-envelope JSON values. The caller owns
/// JSONL formatting; `follow` false stops at the first complete replay barrier.
pub async fn observe_stream_session(
    profile: &ResolvedProfile,
    spawn: bool,
    session_id: SessionId,
    follow: bool,
    output: mpsc::UnboundedSender<RawEnvelope>,
) -> Result<(), ObserveError> {
    stream_shard(profile.clone(), spawn, vec![session_id], follow, output, 0).await
}

/// [`observe_stream_session`] with a starting cursor: replay begins strictly
/// after `after_seq`. The incremental-export windowing seam — a bounded
/// collector plus a moving cursor reaches EVERY suffix across calls, so
/// truncation can never strand history.
pub async fn observe_stream_session_after(
    profile: &ResolvedProfile,
    spawn: bool,
    session_id: SessionId,
    follow: bool,
    output: mpsc::UnboundedSender<RawEnvelope>,
    after_seq: u64,
) -> Result<(), ObserveError> {
    stream_shard(
        profile.clone(),
        spawn,
        vec![session_id],
        follow,
        output,
        after_seq,
    )
    .await
}

/// Streams every session, sharding view attachments at the daemon's default
/// per-connection cap. Follow mode uses `session.list_watch`, whose daemon-side
/// 30 s full-roster audit repairs a lost publication.
pub async fn observe_stream_all(
    profile: &ResolvedProfile,
    spawn: bool,
    follow: bool,
    output: mpsc::UnboundedSender<RawEnvelope>,
) -> Result<(), ObserveError> {
    if !follow {
        let initial = list_session_ids(profile, spawn).await?;
        for shard in initial.chunks(EVENT_SHARD_SIZE) {
            stream_shard(
                profile.clone(),
                spawn,
                shard.to_vec(),
                false,
                output.clone(),
                0,
            )
            .await?;
        }
        return Ok(());
    }

    let mut known = HashSet::new();
    let mut streams = JoinSet::new();
    loop {
        let (watcher, mut roster_events, baseline_lost) =
            open_session_list_watch(profile, spawn).await?;
        let health = watcher.client.health_watch();
        let mut health_wait =
            Box::pin(health.wait("follow-all roster health", STREAM_HEALTH_REPAIR_INTERVAL));
        loop {
            tokio::select! {
                biased;
                joined = streams.join_next(), if !streams.is_empty() => {
                    match joined {
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(error))) => return Err(error),
                        Some(Err(error)) => {
                            return Err(ObserveError::StreamTask(error.to_string()));
                        }
                        None => {}
                    }
                }
                frame = roster_events.recv() => {
                    match frame {
                        Some(WireFrame::SessionRosterDelta { summaries }) => {
                            let new = summaries
                                .into_iter()
                                .map(|summary| summary.session_id)
                                .filter(|session_id| {
                                    known.insert(session_id.as_str().to_owned())
                                })
                                .collect::<Vec<_>>();
                            for shard in new.chunks(EVENT_SHARD_SIZE) {
                                streams.spawn(stream_shard(
                                    profile.clone(),
                                    spawn,
                                    shard.to_vec(),
                                    true,
                                    output.clone(),
                                    0,
                                ));
                            }
                        }
                        Some(WireFrame::ServerDraining { .. }) | None => break,
                        Some(_) => {}
                    }
                    // Roster frames retain priority so a final buffered delta
                    // is not discarded on disconnect. Under saturation the
                    // retained loss generation still forces repair after this
                    // one consumed frame, so health cannot be starved.
                    if watcher.client.lost_events() != baseline_lost {
                        break;
                    }
                }
                outcome = &mut health_wait => {
                    let (health, outcome) = outcome;
                    if health_requires_reconnect(&outcome, baseline_lost) {
                        break;
                    }
                    health_wait = Box::pin(health.wait(
                        "follow-all roster health",
                        STREAM_HEALTH_REPAIR_INTERVAL,
                    ));
                }
            }
        }
        let _ = watcher.close();
    }
}

async fn open_session_list_watch(
    profile: &ResolvedProfile,
    spawn: bool,
) -> Result<(ObserveClient, mpsc::Receiver<WireFrame>, u64), ObserveError> {
    let watcher = ObserveClient::connect(profile, spawn).await?;
    if !watcher
        .welcome
        .features
        .contains(FEATURE_SESSION_LIST_WATCH_V1)
    {
        let _ = watcher.close();
        return Err(ObserveError::MissingFeature(FEATURE_SESSION_LIST_WATCH_V1));
    }
    let Some(events) = watcher.client.take_events() else {
        let _ = watcher.close();
        return Err(ObserveError::Protocol(
            "roster watch event stream was already taken",
        ));
    };
    let baseline_lost = watcher.client.lost_events();
    match watcher
        .client
        .request(RequestBody::SessionListWatch {})
        .await?
    {
        ResponseBody::SessionListWatch { accepted: true } => {}
        ResponseBody::SessionListWatch { accepted: false } => {
            let _ = watcher.close();
            return Err(ObserveError::Protocol(
                "session.list_watch was not accepted",
            ));
        }
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => {
            let _ = watcher.close();
            return Err(ObserveError::Rpc {
                code,
                message,
                retryable,
            });
        }
        _ => {
            let _ = watcher.close();
            return Err(ObserveError::Protocol(
                "session.list_watch response method mismatch",
            ));
        }
    }
    Ok((watcher, events, baseline_lost))
}

/// Waits without polling until at least `count` current-format durable session
/// summaries are roster-visible, or until `timeout` expires. When `expected`
/// is nonempty, every named id must be ready and `count` must equal its length.
pub async fn wait_for_sessions_ready(
    profile: &ResolvedProfile,
    spawn: bool,
    count: usize,
    expected: &[SessionId],
    timeout: Duration,
) -> Result<SessionReadinessSnapshot, ObserveError> {
    let deadline = Instant::now() + timeout;
    let expected_ids = expected
        .iter()
        .map(|session_id| session_id.as_str().to_owned())
        .collect::<HashSet<_>>();
    let (watcher, mut events, mut baseline_lost) =
        match tokio::time::timeout_at(deadline, open_session_list_watch(profile, spawn)).await {
            Ok(result) => result?,
            Err(_) => {
                return Ok(readiness_snapshot(
                    false,
                    0,
                    count,
                    &expected_ids,
                    0,
                    true,
                    HashMap::new(),
                ));
            }
        };
    let daemon_generation = watcher.welcome.daemon_generation;
    let Some(mut summaries) = roster_before_deadline(&watcher, deadline).await? else {
        let _ = watcher.close();
        return Ok(readiness_snapshot(
            true,
            daemon_generation,
            count,
            &expected_ids,
            0,
            true,
            HashMap::new(),
        ));
    };
    let health = watcher.client.health_watch();
    let mut health_wait = Box::pin(health.wait(
        "session-readiness roster health",
        STREAM_HEALTH_REPAIR_INTERVAL,
    ));
    loop {
        let ready_count = readiness_count(&summaries, &expected_ids);
        let satisfied = if expected_ids.is_empty() {
            ready_count >= count
        } else {
            ready_count == expected_ids.len()
        };
        if satisfied {
            let snapshot = readiness_snapshot(
                true,
                daemon_generation,
                count,
                &expected_ids,
                ready_count,
                false,
                summaries,
            );
            let _ = watcher.close();
            return Ok(snapshot);
        }
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => {
                let ready_count = readiness_count(&summaries, &expected_ids);
                let snapshot = readiness_snapshot(
                    true,
                    daemon_generation,
                    count,
                    &expected_ids,
                    ready_count,
                    true,
                    summaries,
                );
                let _ = watcher.close();
                return Ok(snapshot);
            }
            outcome = &mut health_wait => {
                let (health, outcome) = outcome;
                if outcome.channel_closed()
                    || matches!(outcome.snapshot().state, ConnectionState::Disconnected(_))
                {
                    let _ = watcher.close();
                    return Err(ObserveError::Protocol(
                        "daemon disconnected before the session-readiness barrier completed",
                    ));
                }
                if outcome.snapshot().lost_events != baseline_lost {
                    let Some(refreshed) = roster_before_deadline(&watcher, deadline).await? else {
                        let ready_count = readiness_count(&summaries, &expected_ids);
                        let snapshot = readiness_snapshot(
                            true,
                            daemon_generation,
                            count,
                            &expected_ids,
                            ready_count,
                            true,
                            summaries,
                        );
                        let _ = watcher.close();
                        return Ok(snapshot);
                    };
                    summaries = refreshed;
                    baseline_lost = outcome.snapshot().lost_events;
                }
                health_wait = Box::pin(health.wait(
                    "session-readiness roster health",
                    STREAM_HEALTH_REPAIR_INTERVAL,
                ));
            }
            frame = events.recv() => match frame {
                Some(WireFrame::SessionRosterDelta { summaries: changed }) => {
                    for summary in changed {
                        summaries.insert(summary.session_id.as_str().to_owned(), summary);
                    }
                    if watcher.lost_events() != baseline_lost {
                        let Some(refreshed) = roster_before_deadline(&watcher, deadline).await? else {
                            let ready_count = readiness_count(&summaries, &expected_ids);
                            let snapshot = readiness_snapshot(
                                true,
                                daemon_generation,
                                count,
                                &expected_ids,
                                ready_count,
                                true,
                                summaries,
                            );
                            let _ = watcher.close();
                            return Ok(snapshot);
                        };
                        summaries = refreshed;
                        baseline_lost = watcher.lost_events();
                    }
                }
                Some(WireFrame::ServerDraining { .. }) | None => {
                    let _ = watcher.close();
                    return Err(ObserveError::Protocol(
                        "daemon stopped before the session-readiness barrier completed",
                    ));
                }
                Some(_) => {
                    if watcher.lost_events() != baseline_lost {
                        let Some(refreshed) = roster_before_deadline(&watcher, deadline).await? else {
                            let ready_count = readiness_count(&summaries, &expected_ids);
                            let snapshot = readiness_snapshot(
                                true,
                                daemon_generation,
                                count,
                                &expected_ids,
                                ready_count,
                                true,
                                summaries,
                            );
                            let _ = watcher.close();
                            return Ok(snapshot);
                        };
                        summaries = refreshed;
                        baseline_lost = watcher.lost_events();
                    }
                }
            }
        }
    }
}

/// Reconnects through the ordinary daemon-ready seam and waits without
/// polling until `session_id` is durably non-running, or until `timeout`.
pub async fn wait_for_session_resume(
    profile: &ResolvedProfile,
    spawn: bool,
    session_id: SessionId,
    timeout: Duration,
) -> Result<SessionResumeSnapshot, ObserveError> {
    let deadline = Instant::now() + timeout;
    let (watcher, mut events, mut baseline_lost) =
        match tokio::time::timeout_at(deadline, open_session_list_watch(profile, spawn)).await {
            Ok(result) => result?,
            Err(_) => {
                return Ok(SessionResumeSnapshot {
                    daemon_ready: false,
                    daemon_generation: 0,
                    timed_out: true,
                    summary: None,
                });
            }
        };
    let daemon_generation = watcher.welcome.daemon_generation;
    let key = session_id.as_str().to_owned();
    let Some(mut summaries) = roster_before_deadline(&watcher, deadline).await? else {
        let _ = watcher.close();
        return Ok(SessionResumeSnapshot {
            daemon_ready: true,
            daemon_generation,
            timed_out: true,
            summary: None,
        });
    };
    let health = watcher.client.health_watch();
    let mut health_wait = Box::pin(health.wait(
        "session-resume roster health",
        STREAM_HEALTH_REPAIR_INTERVAL,
    ));
    loop {
        let summary = summaries.get(&key).cloned();
        if summary.as_ref().is_some_and(session_summary_is_settled) {
            let _ = watcher.close();
            return Ok(SessionResumeSnapshot {
                daemon_ready: true,
                daemon_generation,
                timed_out: false,
                summary,
            });
        }
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => {
                let summary = summaries.get(&key).cloned();
                let _ = watcher.close();
                return Ok(SessionResumeSnapshot {
                    daemon_ready: true,
                    daemon_generation,
                    timed_out: true,
                    summary,
                });
            }
            outcome = &mut health_wait => {
                let (health, outcome) = outcome;
                if outcome.channel_closed()
                    || matches!(outcome.snapshot().state, ConnectionState::Disconnected(_))
                {
                    let _ = watcher.close();
                    return Err(ObserveError::Protocol(
                        "daemon disconnected before the resume barrier completed",
                    ));
                }
                if outcome.snapshot().lost_events != baseline_lost {
                    let Some(refreshed) = roster_before_deadline(&watcher, deadline).await? else {
                        let summary = summaries.get(&key).cloned();
                        let _ = watcher.close();
                        return Ok(SessionResumeSnapshot {
                            daemon_ready: true,
                            daemon_generation,
                            timed_out: true,
                            summary,
                        });
                    };
                    summaries = refreshed;
                    baseline_lost = outcome.snapshot().lost_events;
                }
                health_wait = Box::pin(health.wait(
                    "session-resume roster health",
                    STREAM_HEALTH_REPAIR_INTERVAL,
                ));
            }
            frame = events.recv() => match frame {
                Some(WireFrame::SessionRosterDelta { summaries: changed }) => {
                    for changed_summary in changed {
                        summaries.insert(
                            changed_summary.session_id.as_str().to_owned(),
                            changed_summary,
                        );
                    }
                    if watcher.lost_events() != baseline_lost {
                        let Some(refreshed) = roster_before_deadline(&watcher, deadline).await? else {
                            let summary = summaries.get(&key).cloned();
                            let _ = watcher.close();
                            return Ok(SessionResumeSnapshot {
                                daemon_ready: true,
                                daemon_generation,
                                timed_out: true,
                                summary,
                            });
                        };
                        summaries = refreshed;
                        baseline_lost = watcher.lost_events();
                    }
                }
                Some(WireFrame::ServerDraining { .. }) | None => {
                    let _ = watcher.close();
                    return Err(ObserveError::Protocol(
                        "daemon stopped before the resume barrier completed",
                    ));
                }
                Some(_) => {
                    if watcher.lost_events() != baseline_lost {
                        let Some(refreshed) = roster_before_deadline(&watcher, deadline).await? else {
                            let summary = summaries.get(&key).cloned();
                            let _ = watcher.close();
                            return Ok(SessionResumeSnapshot {
                                daemon_ready: true,
                                daemon_generation,
                                timed_out: true,
                                summary,
                            });
                        };
                        summaries = refreshed;
                        baseline_lost = watcher.lost_events();
                    }
                }
            }
        }
    }
}

async fn roster_before_deadline(
    watcher: &ObserveClient,
    deadline: Instant,
) -> Result<Option<HashMap<String, SessionSummary>>, ObserveError> {
    match tokio::time::timeout_at(deadline, watcher.session_summaries()).await {
        Ok(result) => result.map(summaries_by_id).map(Some),
        Err(_) => Ok(None),
    }
}

fn summaries_by_id(summaries: Vec<SessionSummary>) -> HashMap<String, SessionSummary> {
    summaries
        .into_iter()
        .map(|summary| (summary.session_id.as_str().to_owned(), summary))
        .collect()
}

fn session_summary_is_ready(summary: &SessionSummary) -> bool {
    summary.head_seq > 0 && summary.metadata.is_some() && summary.run_state.is_some()
}

fn session_summary_is_settled(summary: &SessionSummary) -> bool {
    session_run_state_is_settled(summary.run_state)
}

fn session_run_state_is_settled(run_state: Option<ObserveRunStateWire>) -> bool {
    !matches!(
        run_state,
        None | Some(
            ObserveRunStateWire::Running
                | ObserveRunStateWire::WaitingForRoute
                | ObserveRunStateWire::Unknown,
        )
    )
}

fn readiness_count(
    summaries: &HashMap<String, SessionSummary>,
    expected_ids: &HashSet<String>,
) -> usize {
    summaries
        .iter()
        .filter(|(session_id, summary)| {
            (expected_ids.is_empty() || expected_ids.contains(*session_id))
                && session_summary_is_ready(summary)
        })
        .count()
}

fn readiness_snapshot(
    daemon_ready: bool,
    daemon_generation: u64,
    count: usize,
    expected_ids: &HashSet<String>,
    ready_count: usize,
    timed_out: bool,
    summaries: HashMap<String, SessionSummary>,
) -> SessionReadinessSnapshot {
    let total_session_count = summaries.len();
    let mut summaries = summaries.into_values().collect::<Vec<_>>();
    if !expected_ids.is_empty() {
        summaries.retain(|summary| expected_ids.contains(summary.session_id.as_str()));
    }
    summaries.sort_by(|left, right| left.session_id.as_str().cmp(right.session_id.as_str()));
    SessionReadinessSnapshot {
        daemon_ready,
        daemon_generation,
        expected_count: if expected_ids.is_empty() {
            count
        } else {
            expected_ids.len()
        },
        ready_count,
        total_session_count,
        timed_out,
        summaries,
    }
}

async fn list_session_ids(
    profile: &ResolvedProfile,
    spawn: bool,
) -> Result<Vec<SessionId>, ObserveError> {
    let client = ObserveClient::connect(profile, spawn).await?;
    let result = client.session_ids().await;
    let _ = client.close();
    result
}

async fn stream_shard(
    profile: ResolvedProfile,
    spawn: bool,
    sessions: Vec<SessionId>,
    follow: bool,
    output: mpsc::UnboundedSender<RawEnvelope>,
    initial_after_seq: u64,
) -> Result<(), ObserveError> {
    if sessions.is_empty() {
        return Ok(());
    }
    let mut cursors = sessions
        .iter()
        .map(|session_id| (session_id.as_str().to_owned(), initial_after_seq))
        .collect::<HashMap<_, _>>();
    loop {
        let observer = ObserveClient::connect(&profile, spawn).await?;
        let Some(mut events) = observer.client.take_events() else {
            let _ = observer.close();
            return Err(ObserveError::Protocol(
                "observation client event stream was already taken",
            ));
        };
        // Replay starts as soon as each attach response is written. Snapshot
        // loss before the first attach so a fast replay cannot overflow the
        // bounded client event lane and hide the loss (including caught-up).
        let baseline_lost = observer.client.lost_events();
        let mut attachments = HashMap::<String, (AttachmentId, SessionId)>::new();
        for session_id in &sessions {
            let after_seq = cursors.get(session_id.as_str()).copied().unwrap_or(0);
            let response = observer
                .client
                .request(RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq,
                    mode: AttachMode::View,
                    sealed_replay: false,
                })
                .await?;
            match response {
                ResponseBody::SessionAttach {
                    attachment_id,
                    attach_state,
                } if attach_state.session_id == *session_id => {
                    attachments.insert(
                        attachment_id.as_str().to_owned(),
                        (attachment_id, session_id.clone()),
                    );
                }
                ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    ..
                } => {
                    let _ = observer.close();
                    return Err(ObserveError::Rpc {
                        code,
                        message,
                        retryable,
                    });
                }
                _ => {
                    let _ = observer.close();
                    return Err(ObserveError::Protocol(
                        "session.attach response method mismatch",
                    ));
                }
            }
        }

        let mut caught_up = HashSet::new();
        let health = observer.client.health_watch();
        let mut health_wait =
            Box::pin(health.wait("observation stream health", STREAM_HEALTH_REPAIR_INTERVAL));
        let reconnect = loop {
            tokio::select! {
                biased;
                frame = events.recv() => {
                    let Some(frame) = frame else { break true; };
                    match frame {
                        WireFrame::Event {
                            attachment_id,
                            session_id,
                            envelope,
                        } => {
                            if let Some((_, expected_session)) =
                                attachments.get(attachment_id.as_str())
                            {
                                if *expected_session != session_id
                                    || envelope.session_id != session_id
                                {
                                    let _ = observer.close();
                                    return Err(ObserveError::Protocol(
                                        "attachment event session mismatch",
                                    ));
                                }
                                let cursor =
                                    cursors.entry(session_id.as_str().to_owned()).or_default();
                                if envelope.seq > *cursor {
                                    if envelope.seq != cursor.saturating_add(1) {
                                        break true;
                                    }
                                    let seq = envelope.seq;
                                    output
                                        .send(envelope)
                                        .map_err(|_| ObserveError::OutputClosed)?;
                                    *cursor = seq;
                                }
                            }
                        }
                        WireFrame::AttachCaughtUp {
                            attachment_id,
                            high_water_seq,
                        } => {
                            if let Some((_, session_id)) = attachments.get(attachment_id.as_str()) {
                                if cursors.get(session_id.as_str()).copied().unwrap_or(0)
                                    < high_water_seq
                                {
                                    break true;
                                }
                                caught_up.insert(session_id.as_str().to_owned());
                                if !follow && caught_up.len() == sessions.len() {
                                    let _ = observer.close();
                                    return Ok(());
                                }
                            }
                        }
                        WireFrame::Lagged { attachment_id, .. }
                            if attachments.contains_key(attachment_id.as_str()) => break true,
                        WireFrame::ServerDraining { .. } => break true,
                        _ => {}
                    }
                    // A ready event lane precedes health to preserve buffered
                    // terminal facts. Probe the latest loss generation after
                    // each frame so an overloaded lane cannot starve recovery.
                    if observer.client.lost_events() != baseline_lost {
                        break true;
                    }
                }
                outcome = &mut health_wait => {
                    let (health, outcome) = outcome;
                    if health_requires_reconnect(&outcome, baseline_lost) {
                        break true;
                    }
                    health_wait = Box::pin(health.wait(
                        "observation stream health",
                        STREAM_HEALTH_REPAIR_INTERVAL,
                    ));
                }
            }
        };
        let _ = observer.close();
        if !reconnect {
            return Ok(());
        }
    }
}

fn health_requires_reconnect(outcome: &ClientHealthWait, baseline_lost: u64) -> bool {
    // A typed timeout is the explicit repair path: read the retained latest
    // values even if a watch wake were lost. Ordinary changes use the same
    // check, so protocol/reconnect semantics do not depend on the wake source.
    let _repair_timeout = outcome.repair_timeout();
    let health = outcome.snapshot();
    outcome.channel_closed()
        || health.lost_events != baseline_lost
        || matches!(health.state, ConnectionState::Disconnected(_))
}

#[cfg(test)]
mod tests {
    use super::session_run_state_is_settled;
    use haider_rpc::ObserveRunStateWire;

    /// MUTATION CHECK: classify a route wait as settled. A readiness caller
    /// would return while the same run is still alive and waiting to resume.
    #[test]
    fn waiting_for_route_is_not_a_settled_observe_state() {
        assert!(!session_run_state_is_settled(Some(
            ObserveRunStateWire::WaitingForRoute
        )));
        assert!(session_run_state_is_settled(Some(
            ObserveRunStateWire::Idle
        )));
    }
}
