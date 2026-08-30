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
    FEATURE_SESSION_FLEET_V1, FEATURE_SESSION_OBSERVE_BATCH_V1, FEATURE_SESSION_OBSERVE_V1,
    LifecyclePhase, RequestBody, ResponseBody, SessionDescendantBaselineWire, SessionFleetSnapshot,
    SessionObserveDigest, SessionSummary, Welcome, WireFrame,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::client::{
    ClientCloseOutcome, ClientConfig, ClientError, ConnectError, ConnectionUsage, RpcClient,
    connect,
};
use crate::profile::ResolvedProfile;
use crate::spawn::{EnsureError, EnsureOptions, ensure_daemon};

const LIST_PAGE: u32 = 100;
const EVENT_SHARD_SIZE: usize = 16;
const STREAM_HEALTH_POLL: Duration = Duration::from_millis(25);
const SESSION_DISCOVERY_POLL: Duration = Duration::from_secs(1);

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
    pub adoption_available: Vec<haider_rpc::AccountAdoptionAvailable>,
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
                adoption_available,
            } => Ok(ObserveStatusSnapshot {
                active_account,
                session_count,
                adoption_available,
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
/// per-connection cap. Follow mode polls session.list so later sessions join.
pub async fn observe_stream_all(
    profile: &ResolvedProfile,
    spawn: bool,
    follow: bool,
    output: mpsc::UnboundedSender<RawEnvelope>,
) -> Result<(), ObserveError> {
    let initial = list_session_ids(profile, spawn).await?;
    if !follow {
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

    let mut known = initial
        .iter()
        .map(|session_id| session_id.as_str().to_owned())
        .collect::<HashSet<_>>();
    let mut streams = JoinSet::new();
    for shard in initial.chunks(EVENT_SHARD_SIZE) {
        streams.spawn(stream_shard(
            profile.clone(),
            spawn,
            shard.to_vec(),
            true,
            output.clone(),
            0,
        ));
    }
    let mut discovery = tokio::time::interval(SESSION_DISCOVERY_POLL);
    discovery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            joined = streams.join_next(), if !streams.is_empty() => {
                match joined {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => return Err(error),
                    Some(Err(error)) => return Err(ObserveError::StreamTask(error.to_string())),
                    None => {}
                }
            }
            _ = discovery.tick() => {
                let ids = list_session_ids(profile, spawn).await?;
                let new = ids
                    .into_iter()
                    .filter(|session_id| known.insert(session_id.as_str().to_owned()))
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
        }
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
        let mut health = tokio::time::interval(STREAM_HEALTH_POLL);
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let reconnect = loop {
            tokio::select! {
                frame = events.recv() => {
                    let Some(frame) = frame else { break true; };
                    match frame {
                        WireFrame::Event {
                            attachment_id,
                            session_id,
                            envelope,
                        } => {
                            let Some((_, expected_session)) = attachments.get(attachment_id.as_str()) else {
                                continue;
                            };
                            if *expected_session != session_id || envelope.session_id != session_id {
                                let _ = observer.close();
                                return Err(ObserveError::Protocol("attachment event session mismatch"));
                            }
                            let cursor = cursors.entry(session_id.as_str().to_owned()).or_default();
                            if envelope.seq <= *cursor {
                                continue;
                            }
                            if envelope.seq != cursor.saturating_add(1) {
                                break true;
                            }
                            let seq = envelope.seq;
                            output.send(envelope).map_err(|_| ObserveError::OutputClosed)?;
                            *cursor = seq;
                        }
                        WireFrame::AttachCaughtUp {
                            attachment_id,
                            high_water_seq,
                        } => {
                            let Some((_, session_id)) = attachments.get(attachment_id.as_str()) else {
                                continue;
                            };
                            if cursors.get(session_id.as_str()).copied().unwrap_or(0) < high_water_seq {
                                break true;
                            }
                            caught_up.insert(session_id.as_str().to_owned());
                            if !follow && caught_up.len() == sessions.len() {
                                let _ = observer.close();
                                return Ok(());
                            }
                        }
                        WireFrame::Lagged { attachment_id, .. }
                            if attachments.contains_key(attachment_id.as_str()) => break true,
                        WireFrame::ServerDraining { .. } => break true,
                        _ => {}
                    }
                }
                _ = health.tick() => {
                    if observer.client.lost_events() != baseline_lost {
                        break true;
                    }
                }
            }
        };
        let _ = observer.close();
        if !reconnect {
            return Ok(());
        }
    }
}
