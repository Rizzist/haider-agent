//! The live IO shell (W3c3 M2) — the only part of live mode that awaits.
//!
//! [`LiveDriver`](crate::live::LiveDriver) is a pure state machine; this
//! module is the task that turns its [`LiveCommand`]s into real RPCs and
//! its inbound frames into [`LiveReply`]s. Splitting them is what makes the
//! working set, the reconnect cursors, the launcher order and the menu
//! coordinates testable without a daemon.
//!
//! ## Why the link owns the connection
//!
//! Reconnection is a CALLER primitive in `haider-client`: the client never
//! redials silently, it reports a typed [`DisconnectReason`] and the caller
//! dials again. This task is that caller. It holds the connection, redials
//! with bounded backoff, and reports both edges (`Disconnected`,
//! `Reconnected`) to the UI loop as ordinary replies — so the driver's
//! resume path is exercised by the same code in tests and in production.
//!
//! ## The two orderings this task exists to preserve
//!
//! A request has a SEND half and a WAIT half, and they have opposite needs.
//! The wait must be concurrent — a slow response must never stall event
//! forwarding, because events are how the transcript stays live while a
//! mutation is in flight. The send must be SEQUENTIAL, because the driver
//! emits `[Detach, Attach]` as one ordered vector for a 17th-slot eviction
//! and the daemon rejects the attach at `max_attachments_per_connection` if
//! it arrives first. So [`issue`] awaits `begin_request` INLINE (the frame
//! is on the wire before the next command is looked at) and spawns only
//! `PendingResponse::wait` (review W3c3 P1-3).
//!
//! The second ordering is in the REPLY stream: the attach response must
//! reach the driver before the first event for the attachment it names, or
//! the driver rejects that event as an unknown attachment id and it is lost
//! forever. The daemon's own register→replay seam gets this right on the
//! wire, but the response travelled through a spawned task while events
//! travelled through the select loop, so the two raced here. The link now
//! HOLDS attachment-scoped replies while any attach response is outstanding
//! and flushes them in order once none is (review W3c3 P1-2).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use haider_client::{ClientConfig, ClientError, DisconnectReason, ResolvedProfile, RpcClient};
use haider_rpc::{AttachMode, RequestBody, ResponseBody, WireFrame};
use tokio::sync::mpsc;

use crate::live::{LiveCommand, LiveReply};

/// Bounded redial backoff: the daemon may be draining, restarting, or being
/// replaced by a newer version. We keep trying, never faster than this.
const REDIAL_MIN: Duration = Duration::from_millis(200);
const REDIAL_MAX: Duration = Duration::from_secs(5);

/// Channel depth for commands and replies. Both are UI-paced.
const LINK_CAPACITY: usize = 256;

/// How many attachment-scoped replies the link will hold behind an
/// outstanding attach response before it gives up and reports a gap.
///
/// The barrier exists to fix an ordering race, not to become a second
/// unbounded buffer: an attach response that never lands (a wedged daemon)
/// must not grow this queue without limit. Overflowing it publishes
/// [`LiveReply::EventsLost`] and drops the queue, because a drop the driver
/// can see costs one reattach, and a drop it cannot see costs a transcript
/// that is silently wrong.
#[doc(hidden)]
pub const HELD_REPLY_CAP: usize = 512;

/// A live link: send [`LiveCommand`]s, receive [`LiveReply`]s.
pub struct Link {
    pub commands: mpsc::Sender<LiveCommand>,
    pub replies: mpsc::Receiver<LiveReply>,
    /// What the daemon advertised at handshake (W5e-1b feature gating).
    pub daemon_features: std::collections::BTreeSet<String>,
    pub daemon_version: String,
    task: tokio::task::JoinHandle<()>,
}

impl Link {
    /// Start the link over an already-negotiated connection.
    #[must_use]
    pub fn start(client: RpcClient, profile: ResolvedProfile, config: ClientConfig) -> Self {
        // Capture the handshake facts BEFORE the client moves into the task.
        let daemon_features = client.welcome().features.iter().cloned().collect();
        let daemon_version = client.welcome().daemon_version.clone();
        let (commands_tx, commands_rx) = mpsc::channel(LINK_CAPACITY);
        let (replies_tx, replies) = mpsc::channel(LINK_CAPACITY);
        let task = tokio::spawn(run_link(
            Arc::new(client),
            profile,
            config,
            commands_rx,
            replies_tx,
        ));
        Self {
            commands: commands_tx,
            replies,
            daemon_features,
            daemon_version,
            task,
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run_link(
    mut client: Arc<RpcClient>,
    profile: ResolvedProfile,
    config: ClientConfig,
    mut commands: mpsc::Receiver<LiveCommand>,
    replies: mpsc::Sender<LiveReply>,
) {
    let Some(mut events) = client.take_events() else {
        return;
    };
    // THE ATTACH BARRIER. Attach responses come back on their own channel so
    // the LINK publishes them, not the spawned task that waited for them:
    // that is the only place with a total order over both the response and
    // the events, and therefore the only place that can guarantee the
    // response goes first.
    let (mut attaches_tx, mut attaches_rx) = mpsc::channel::<Vec<LiveReply>>(LINK_CAPACITY);
    let mut outstanding_attaches: usize = 0;
    let mut held: VecDeque<LiveReply> = VecDeque::new();
    // Loss the barrier swallowed while an attach was outstanding; published
    // only when the barrier drains (review r2 NF-1).
    let mut deferred_loss: u64 = 0;
    // The client drops uncorrelated frames when its event channel is full
    // and counts them. Nothing used to read that counter, so a dropped final
    // envelope — the one with no later sequence to expose its hole — was
    // pure silence (review W3c3 P1-2).
    let mut lost_baseline = client.lost_events();
    loop {
        let lost = client.lost_events();
        if lost > lost_baseline {
            let reply = LiveReply::EventsLost {
                count: lost - lost_baseline,
            };
            lost_baseline = lost;
            if !deliver(
                reply,
                outstanding_attaches,
                &mut held,
                &mut deferred_loss,
                &replies,
            )
            .await
            {
                return;
            }
        }
        let dead = tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return };
                if issue(&client, command, &replies, &attaches_tx).await {
                    outstanding_attaches += 1;
                }
                None
            }
            settled = attaches_rx.recv() => {
                // `attaches_tx` is held right here, so the channel cannot
                // close; `None` is unreachable and harmless.
                for reply in settled.unwrap_or_default() {
                    if replies.send(reply).await.is_err() {
                        return;
                    }
                }
                outstanding_attaches = outstanding_attaches.saturating_sub(1);
                if outstanding_attaches == 0 {
                    while let Some(reply) = held.pop_front() {
                        if replies.send(reply).await.is_err() {
                            return;
                        }
                    }
                    // The barrier is drained and every attach outcome above
                    // is installed — NOW the swallowed loss is repairable:
                    // the driver's reattach walk includes the attachments
                    // those attaches just installed (review r2 NF-1).
                    if deferred_loss > 0 {
                        let count = deferred_loss;
                        deferred_loss = 0;
                        if replies.send(LiveReply::EventsLost { count }).await.is_err() {
                            return;
                        }
                    }
                }
                None
            }
            frame = events.recv() => match frame {
                Some(frame) => {
                    for reply in map_frame(frame) {
                        if !deliver(
                            reply,
                            outstanding_attaches,
                            &mut held,
                            &mut deferred_loss,
                            &replies,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    None
                }
                // The event channel closes with the connection.
                None => Some(client.disconnected().await),
            },
            reason = client.disconnected() => Some(reason),
        };
        let Some(reason) = dead else { continue };
        // A disconnect voids the barrier outright: every attachment died
        // with the socket, the reattach replays whatever the held replies
        // carried, and a stale attach response for the dead connection must
        // never be published as a live attachment. Replacing the channel
        // orphans the tasks still waiting on the old one.
        held.clear();
        outstanding_attaches = 0;
        // The disconnect reattach replays everything from applied cursors;
        // deferred loss is subsumed by it.
        deferred_loss = 0;
        let (fresh_tx, fresh_rx) = mpsc::channel::<Vec<LiveReply>>(LINK_CAPACITY);
        attaches_tx = fresh_tx;
        attaches_rx = fresh_rx;
        if matches!(reason, DisconnectReason::Closed) {
            return;
        }
        if replies
            .send(LiveReply::Disconnected {
                reason: reason.to_string(),
            })
            .await
            .is_err()
        {
            return;
        }
        // Redial. `ensure_daemon` is deliberately NOT used here: the daemon
        // was running a moment ago, and spawning a competitor while it
        // restarts is exactly what R8 forbids.
        let mut backoff = REDIAL_MIN;
        loop {
            match haider_client::connect(&profile.endpoint_path, config.clone()).await {
                Ok(connected) => {
                    let fresh = Arc::new(connected.client);
                    let Some(fresh_events) = fresh.take_events() else {
                        return;
                    };
                    client = fresh;
                    events = fresh_events;
                    // A fresh client owns a fresh loss counter.
                    lost_baseline = client.lost_events();
                    if replies.send(LiveReply::Reconnected).await.is_err() {
                        return;
                    }
                    break;
                }
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(REDIAL_MAX);
                }
            }
        }
    }
}

/// Whether a reply describes a specific ATTACHMENT and is therefore subject
/// to the attach barrier.
///
/// Connection-scoped news (`Draining`, a `ProtocolError` turned `Failed`)
/// is not: it names no attachment, so holding it behind one would delay a
/// drain notice for nothing. `EventsLost` IS attachment-scoped in effect —
/// it makes every held attachment reattach — so releasing it early would
/// make the driver drop the very attachments whose held events are about to
/// be flushed.
const fn attachment_scoped(reply: &LiveReply) -> bool {
    matches!(
        reply,
        LiveReply::Event { .. }
            | LiveReply::Lagged { .. }
            | LiveReply::CaughtUp { .. }
            | LiveReply::EventsLost { .. }
    )
}

/// Publish one inbound reply, or hold it behind an outstanding attach.
///
/// Returns `false` once the reply consumer is gone (the link is finished).
///
/// Public only so `tests/` can pin the overflow-deferral law at this seam
/// (the same precedent as [`CommandContext`]): a socket-driven flood cannot
/// deterministically overflow the barrier, because the client's own bounded
/// event channel drops first — a DIFFERENT loss class with its own report.
#[doc(hidden)]
pub async fn deliver(
    reply: LiveReply,
    outstanding_attaches: usize,
    held: &mut VecDeque<LiveReply>,
    deferred_loss: &mut u64,
    replies: &mpsc::Sender<LiveReply>,
) -> bool {
    if outstanding_attaches == 0 || !attachment_scoped(&reply) {
        return replies.send(reply).await.is_ok();
    }
    if held.len() >= HELD_REPLY_CAP {
        // The queue and the reply that overflowed it are both gone — but the
        // loss must NOT be published yet. An `EventsLost` sent while the
        // attach is still outstanding reaches a driver with no installed
        // attachment to repair, and the later `Attached` then paints a
        // surface whose replay and catch-up boundary were silently thrown
        // away (review r2 NF-1). Defer the count until the barrier drains:
        // the flush publishes it AFTER the attach outcome installs, so the
        // driver's reattach walk covers the fresh attachment too.
        *deferred_loss = deferred_loss.saturating_add(
            u64::try_from(held.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        );
        held.clear();
        return true;
    }
    held.push_back(reply);
    true
}

/// Perform one command, returning whether an ATTACH response is now
/// outstanding.
///
/// The write is awaited INLINE so commands reach the wire in the order the
/// driver emitted them; only the response wait is spawned, so a slow answer
/// still cannot stall event forwarding. An attach's replies are routed to
/// `attaches` instead of straight to `replies` so the link — the one place
/// with a total order over responses and events — publishes them itself.
async fn issue(
    client: &Arc<RpcClient>,
    command: LiveCommand,
    replies: &mpsc::Sender<LiveReply>,
    attaches: &mpsc::Sender<Vec<LiveReply>>,
) -> bool {
    // A menu answer is an UNCORRELATED frame by wire design (the durable
    // identity is its `command_id`). Its outcome arrives as the committed
    // `MenuAnswered` envelope, which is also what retires it from the
    // outbox — a correlated echo would be a second authority.
    if let LiveCommand::Answer {
        command_id,
        session,
        menu,
        request_seq,
        worker_generation,
        option_key,
        option_index,
        input,
    } = command
    {
        let frame = WireFrame::MenuAnswer {
            request_id: None,
            command_id,
            session_id: session,
            menu_id: menu,
            request_seq,
            worker_generation,
            option_key,
            option_index,
            input,
        };
        let _ = client.send_frame(frame).await;
        return false;
    }
    let context = CommandContext::of(&command);
    let is_attach = context.attach.is_some();
    let body = request_body(command);
    let pending = match client.begin_request(body).await {
        Ok(pending) => pending,
        // Nothing reached the wire, so nothing is outstanding. The outbox
        // resends under the same durable id on reconnect; reads are
        // reissued by the resume path.
        Err(ClientError::Disconnected(_)) => return false,
        Err(error) => {
            // An attach carries no command id, so the generic `Failed` can
            // never release the slot the driver claimed for it — a permanent
            // false in-flight latch (review r2 NF-2). The link knows which
            // session the attach was for; say so, permanently.
            let reply = if let Some(session) = context.attach.clone() {
                LiveReply::AttachFailed {
                    session,
                    code: "encode_failed".to_owned(),
                    message: error.to_string(),
                    retryable: false,
                }
            } else {
                LiveReply::Failed {
                    command_id: context.command_id.clone(),
                    code: "encode_failed".to_owned(),
                    message: error.to_string(),
                    retryable: false,
                }
            };
            let _ = replies.send(reply).await;
            return false;
        }
    };
    let replies = replies.clone();
    let attaches = attaches.clone();
    tokio::spawn(async move {
        let mapped = match pending.wait().await {
            Ok(response) => map_response(&context, response),
            // The disconnect edge is reported by the link itself.
            Err(ClientError::Disconnected(_)) => Vec::new(),
            Err(error) => vec![LiveReply::Failed {
                command_id: context.command_id.clone(),
                code: "encode_failed".to_owned(),
                message: error.to_string(),
                retryable: false,
            }],
        };
        if is_attach {
            // Even an EMPTY vector must be delivered: it is what retires
            // the barrier for a failed or disconnected attach.
            let _ = attaches.send(mapped).await;
        } else {
            for reply in mapped {
                let _ = replies.send(reply).await;
            }
        }
    });
    is_attach
}

/// What a response needs from its request to be interpretable (the wire
/// deliberately does not echo, e.g., which session an attach was for).
///
/// Public only so `tests/` can pin the request/response mapping without a
/// daemon — the workspace forbids inline test modules, and an unmapped
/// response body is a silently swallowed reply.
pub struct CommandContext {
    /// The OAuth start's attempt id, so the reply is identity-tagged.
    pub oauth_attempt: Option<String>,
    command_id: Option<haider_rpc::CommandId>,
    cwd: String,
    model: String,
    /// Carried across `vault.stage` so the login that follows knows which
    /// account it is committing — and, since TUI6.4, WHICH ATTEMPT asked:
    /// the reply is identity-tagged from this context, so stage
    /// correlation never depends on reply ordering. Never a secret.
    login: Option<(String, Option<String>, u64)>,
    /// The session an ATTACH was for. An attach carries no durable command
    /// id, so without this a failure cannot be correlated back to the
    /// session it wedged (review P1-5).
    attach: Option<haider_protocol::ids::SessionId>,
    /// The provider a `provider.models_refresh` was for — the request has
    /// no durable id, and its failure must land on the PROVIDER ROW, not
    /// the status-bar flash (owner bug: boot-time auto-refresh of a dead
    /// probe provider flashed `provider_error` at the launcher).
    models_provider: Option<String>,
}

impl CommandContext {
    /// Capture everything the response will not carry back.
    #[must_use]
    pub fn of(command: &LiveCommand) -> Self {
        let (cwd, model) = match command {
            LiveCommand::Create { cwd, model, .. } => (cwd.clone(), model.clone()),
            _ => (String::new(), String::new()),
        };
        let oauth_attempt = match command {
            LiveCommand::OAuthStart { attempt_id, .. } => Some(attempt_id.clone()),
            _ => None,
        };
        Self {
            command_id: command.command_id().cloned(),
            cwd,
            model,
            login: match command {
                LiveCommand::Stage {
                    provider,
                    alias,
                    attempt,
                    ..
                }
                | LiveCommand::LoginApi {
                    provider,
                    alias,
                    attempt,
                    ..
                } => Some((provider.clone(), alias.clone(), *attempt)),
                _ => None,
            },
            oauth_attempt,
            models_provider: match command {
                LiveCommand::RefreshProviderModels { provider } => Some(provider.clone()),
                _ => None,
            },
            attach: match command {
                LiveCommand::Attach { session, .. } => Some(session.clone()),
                _ => None,
            },
        }
    }
}

/// The wire request one driver command becomes.
#[must_use]
pub fn request_body(command: LiveCommand) -> RequestBody {
    match command {
        LiveCommand::List { cursor } => RequestBody::SessionList {
            cursor,
            limit: crate::live::LIST_PAGE,
        },
        LiveCommand::Attach { session, after_seq } => RequestBody::SessionAttach {
            session_id: session,
            after_seq,
            mode: AttachMode::Control,
        },
        LiveCommand::Detach { attachment } => RequestBody::SessionDetach {
            attachment_id: attachment,
        },
        LiveCommand::Create {
            command_id,
            cwd,
            provider,
            model,
            max_tokens,
            ..
        } => RequestBody::SessionCreateWithPermissionOverrides {
            command_id,
            cwd,
            provider,
            model,
            max_tokens,
            // OWNER DIRECTIVE: the interactive surface runs in AUTO mode
            // (like pi) — writes/exec never open approval menus. The W8
            // machinery stays for request_input and future policy modes.
            permission_overrides: Some(
                haider_rpc::haider_protocol::session::SessionPermissionOverridesV1 {
                    allow_writes: true,
                    allow_exec: true,
                },
            ),
        },
        // B2b encode-selection law: a captured branch rides the
        // branch-capable decode form; `None` keeps the LEGACY variant so
        // main-branch wire bytes stay byte-for-byte historical.
        LiveCommand::Submit {
            command_id,
            session,
            worker_generation,
            text,
            mode,
            branch,
        } => match branch {
            Some(branch_id) => RequestBody::TurnSubmitWithBranch {
                command_id,
                session_id: session,
                worker_generation,
                branch_id: Some(branch_id),
                text,
                attachments: vec![],
                mode,
            },
            None => RequestBody::TurnSubmit {
                command_id,
                session_id: session,
                worker_generation,
                text,
                attachments: vec![],
                mode,
            },
        },
        LiveCommand::Cancel {
            command_id,
            session,
            worker_generation,
            run_id,
            // Client-side capture only: the wire cancel pins the run by
            // `run_id`, which acceptance already branch-pinned.
            branch: _,
        } => RequestBody::TurnCancel {
            command_id,
            session_id: session,
            worker_generation,
            run_id,
        },
        LiveCommand::Compact {
            command_id,
            session,
            worker_generation,
            branch,
        } => match branch {
            Some(branch_id) => RequestBody::SessionCompactOnBranch {
                command_id,
                session_id: session,
                worker_generation,
                branch_id: Some(branch_id),
            },
            None => RequestBody::SessionCompact {
                command_id,
                session_id: session,
                worker_generation,
            },
        },
        LiveCommand::BranchCreate {
            command_id,
            session,
            worker_generation,
            source_branch,
            fork_node_id,
            fork_seq,
            name,
        } => RequestBody::BranchCreate {
            command_id,
            session_id: session,
            worker_generation,
            source_branch_id: source_branch,
            fork_node_id,
            fork_seq,
            name,
        },
        LiveCommand::ShellExec {
            command_id,
            session,
            worker_generation,
            command,
        } => RequestBody::ShellExec {
            command_id,
            session_id: session,
            worker_generation,
            command,
            cwd: None,
        },
        LiveCommand::ToolsInventory { session } => RequestBody::ToolsInventory {
            session_id: session,
        },
        LiveCommand::AccountRemove {
            command_id,
            alias,
            expected_revision,
        } => RequestBody::AccountRemove {
            command_id,
            alias,
            expected_revision,
        },
        LiveCommand::ProviderRemove {
            command_id,
            provider,
            expected_revision,
        } => RequestBody::ProviderRemove {
            command_id,
            provider,
            expected_revision,
        },
        LiveCommand::Stage {
            stage_id, secret, ..
        } => RequestBody::VaultStage {
            stage_id,
            purpose: haider_rpc::StagePurpose::ApiKey,
            secret,
        },
        LiveCommand::LoginApi {
            command_id,
            provider,
            alias,
            vault_reference,
            // The attempt is CLIENT-side identity (TUI6.4) — never sent.
            attempt: _,
        } => RequestBody::AccountLoginApi {
            command_id,
            provider,
            alias,
            vault_reference,
            // `None` means the release-owned full model id in the resolved
            // profile — the client never invents a validation model.
            validation_model: None,
        },
        LiveCommand::AccountList => RequestBody::AccountList { provider: None },
        LiveCommand::AccountSetActive { command_id, alias } => {
            RequestBody::AccountSetActive { command_id, alias }
        }
        LiveCommand::ProviderList => RequestBody::ProviderList { provider: None },
        LiveCommand::RefreshProviderModels { provider } => {
            RequestBody::ProviderModelsRefresh { provider }
        }
        LiveCommand::SetDefaultModel {
            command_id,
            provider,
            model,
            expected_revision,
        } => RequestBody::AccountSetDefaultModel {
            command_id,
            provider,
            model,
            expected_revision,
        },
        // W5g-4: the card CREATES — identity fields are fixed here, never
        // typed. The origin string is data on the wire only; it is never
        // interpolated into a shell or browser command (report §4.4).
        LiveCommand::ConfigureProvider {
            command_id,
            provider,
            origin,
            model,
            expected_revision,
        } => RequestBody::ProviderConfigure {
            command_id,
            provider,
            api_family: Some(haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some(origin),
            auth_requirement: Some(haider_rpc::ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec![model.clone()],
            default_model: Some(model),
            expected_revision,
        },
        LiveCommand::OAuthStart {
            provider,
            desired_alias,
            attempt_id,
        } => RequestBody::AccountOAuthStart {
            provider,
            desired_alias,
            attempt_id,
        },
        LiveCommand::OAuthStatus {
            flow_id,
            attempt_id,
        } => RequestBody::AccountOAuthStatus {
            flow_id,
            attempt_id,
        },
        LiveCommand::OAuthCancel {
            flow_id,
            attempt_id,
        } => RequestBody::AccountOAuthCancel {
            flow_id,
            attempt_id,
        },
        LiveCommand::AccountAddOAuth {
            command_id,
            provider,
            alias,
            flow_id,
            attempt_id,
            oauth_reference,
        } => RequestBody::AccountAdd {
            command_id,
            provider,
            alias,
            auth_method: haider_rpc::AccountAddMethod::OAuth,
            flow_id,
            attempt_id,
            oauth_reference,
        },
        LiveCommand::Answer { .. } => unreachable!("answers ride send_frame, not request"),
    }
}

/// The driver facts one response body carries, given what its request knew.
#[must_use]
pub fn map_response(context: &CommandContext, body: ResponseBody) -> Vec<LiveReply> {
    match body {
        ResponseBody::SessionList {
            sessions,
            next_cursor,
        } => vec![LiveReply::Listed {
            sessions,
            next_cursor,
        }],
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } => vec![LiveReply::Attached {
            session: attach_state.session_id,
            attachment: attachment_id,
            worker_generation: attach_state.worker_generation,
            replay_through_seq: attach_state.replay_through_seq,
        }],
        ResponseBody::SessionDetach { attachment_id } => vec![LiveReply::Detached {
            attachment: attachment_id,
        }],
        ResponseBody::SessionCreate {
            session_id,
            worker_generation,
            ..
        } => context.command_id.clone().map_or_else(Vec::new, |id| {
            vec![LiveReply::Created {
                command_id: id,
                session: session_id,
                worker_generation,
                cwd: context.cwd.clone(),
                model: context.model.clone(),
            }]
        }),
        // The branch-pinned response shape carries the same driver facts as
        // the legacy one — the branch pin is durable-receipt truth the
        // driver already holds from its own captured command.
        ResponseBody::TurnSubmit {
            session_id,
            worker_generation,
            disposition,
            ..
        }
        | ResponseBody::TurnSubmitOnBranch {
            session_id,
            worker_generation,
            disposition,
            ..
        } => context.command_id.clone().map_or_else(Vec::new, |id| {
            vec![LiveReply::Submitted {
                command_id: id,
                session: session_id,
                worker_generation,
                disposition,
            }]
        }),
        ResponseBody::TurnCancel { .. } => context
            .command_id
            .clone()
            .map_or_else(Vec::new, |id| vec![LiveReply::Cancelled { command_id: id }]),
        ResponseBody::SessionCompact { .. } | ResponseBody::SessionCompactOnBranch { .. } => {
            context
                .command_id
                .clone()
                .map_or_else(Vec::new, |id| vec![LiveReply::Compacted { command_id: id }])
        }
        ResponseBody::BranchCreate {
            session_id,
            branch_id,
            name,
            ..
        } => context.command_id.clone().map_or_else(Vec::new, |id| {
            vec![LiveReply::BranchForked {
                command_id: id,
                session: session_id,
                branch_id,
                name,
            }]
        }),
        ResponseBody::ShellExec { .. } => context.command_id.clone().map_or_else(Vec::new, |id| {
            vec![LiveReply::ShellAccepted { command_id: id }]
        }),
        ResponseBody::ToolsInventory {
            session_id,
            inventory,
        } => vec![LiveReply::ToolsInventory {
            session: session_id,
            snapshot: Box::new(inventory),
        }],
        ResponseBody::AccountRemove {
            removed_alias,
            replacement_active_alias,
            revision,
        } => context.command_id.clone().map_or_else(Vec::new, |id| {
            vec![LiveReply::AccountRemoved {
                command_id: id,
                removed_alias: removed_alias.as_str().to_owned(),
                replacement_active_alias: replacement_active_alias
                    .as_ref()
                    .map(|alias| alias.as_str().to_owned()),
                revision,
            }]
        }),
        ResponseBody::ProviderRemove { provider, revision } => {
            context.command_id.clone().map_or_else(Vec::new, |id| {
                vec![LiveReply::ProviderRemoved {
                    command_id: id,
                    provider,
                    revision,
                }]
            })
        }
        ResponseBody::VaultStage {
            vault_reference, ..
        } => context
            .login
            .clone()
            .map_or_else(Vec::new, |(provider, alias, attempt)| {
                vec![LiveReply::Staged {
                    vault_reference,
                    provider,
                    alias,
                    attempt,
                }]
            }),
        ResponseBody::AccountLoginApi { descriptor } => {
            context.command_id.clone().map_or_else(Vec::new, |id| {
                vec![LiveReply::LoggedIn {
                    command_id: id,
                    identity: descriptor.identity.clone(),
                }]
            })
        }
        ResponseBody::AccountList {
            descriptors,
            revision,
            ..
        } => vec![LiveReply::Accounts {
            descriptors,
            revision,
        }],
        ResponseBody::AccountSetActive {
            descriptor,
            revision,
            ..
        } => context.command_id.clone().map_or_else(Vec::new, |id| {
            vec![LiveReply::AccountSelected {
                command_id: id,
                descriptor,
                revision,
            }]
        }),
        ResponseBody::ProviderList {
            providers,
            revision,
        } => vec![LiveReply::Providers {
            providers,
            revision,
        }],
        ResponseBody::ProviderModelsRefresh { provider, revision } => {
            vec![LiveReply::ProviderModelsRefreshed { provider, revision }]
        }
        ResponseBody::AccountSetDefaultModel { provider, revision } => {
            context.command_id.clone().map_or_else(Vec::new, |id| {
                vec![LiveReply::DefaultModelSet {
                    command_id: id,
                    provider,
                    revision,
                }]
            })
        }
        ResponseBody::ProviderConfigure { provider, revision } => {
            context.command_id.clone().map_or_else(Vec::new, |id| {
                vec![LiveReply::ProviderConfigured {
                    command_id: id,
                    provider,
                    revision,
                }]
            })
        }
        ResponseBody::AccountOAuthStart {
            availability,
            flow_id,
            authorization_url,
            provider_origin,
            ..
        } => context
            .oauth_attempt
            .clone()
            .map_or_else(Vec::new, |attempt_id| {
                vec![LiveReply::OAuthStarted {
                    attempt_id,
                    availability,
                    flow_id,
                    authorization_url: authorization_url
                        .map(|url| url.expose_authorization_url().to_owned()),
                    provider_origin,
                }]
            }),
        ResponseBody::AccountOAuthStatus { flow_id, status } => {
            vec![LiveReply::OAuthFlowStatus { flow_id, status }]
        }
        // Cancel's answer needs no driver fact: the card already closed.
        ResponseBody::AccountOAuthCancel { .. } => Vec::new(),
        ResponseBody::AccountAdd { descriptor } => {
            context.command_id.clone().map_or_else(Vec::new, |id| {
                vec![LiveReply::AccountAdded {
                    command_id: id,
                    descriptor,
                }]
            })
        }
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => context.attach.clone().map_or_else(
            || {
                // TUI6.4: a STAGE request's error is identity-tagged from
                // this same context (stages carry no durable command id,
                // so `command_id` is None exactly when `login` context
                // exists without one — the login command itself has an
                // id and keeps the correlated `Failed` path).
                if context.command_id.is_none()
                    && let Some((_, _, attempt)) = context.login
                {
                    return vec![LiveReply::StageFailed {
                        attempt,
                        code: code.clone(),
                        message: message.clone(),
                    }];
                }
                // W5e-1b: `account.oauth_start` is NON-DURABLE, so its error
                // reply carries no command_id — the generic `Failed` path
                // cannot correlate it and the add card would wait at
                // "starting the loopback flow…" forever (observed live on
                // v0.0.18). Identity-tag it from the same context, exactly
                // as TUI6.4 does for a stage.
                if let Some(attempt_id) = context.oauth_attempt.clone() {
                    return vec![LiveReply::OAuthStartFailed {
                        attempt_id,
                        code: code.clone(),
                        message: message.clone(),
                    }];
                }
                if let Some(provider) = context.models_provider.clone() {
                    return vec![LiveReply::ModelsRefreshFailed {
                        provider,
                        message: message.clone(),
                    }];
                }
                vec![LiveReply::Failed {
                    command_id: context.command_id.clone(),
                    code: code.clone(),
                    message: message.clone(),
                    retryable,
                }]
            },
            |session| {
                vec![LiveReply::AttachFailed {
                    session,
                    code: code.clone(),
                    message: message.clone(),
                    retryable,
                }]
            },
        ),
        // Account/menu bodies belong to M3's login card; unknown bodies are
        // tolerated, never fatal (forward-compat law).
        _ => Vec::new(),
    }
}

/// The driver facts one uncorrelated frame carries.
///
/// Unknown frames map to nothing (forward-compat law); frames that carry
/// cursor authority must NEVER take that path.
#[must_use]
pub fn map_frame(frame: WireFrame) -> Vec<LiveReply> {
    match frame {
        WireFrame::Event {
            attachment_id,
            session_id,
            envelope,
        } => vec![LiveReply::Event {
            attachment: attachment_id,
            session: session_id,
            envelope: Box::new(envelope),
        }],
        WireFrame::Lagged { attachment_id, .. } => vec![LiveReply::Lagged {
            attachment: attachment_id,
        }],
        // THE CATCH-UP BOUNDARY. This used to be dropped by the catch-all
        // below, on the claim that it "carries no state the cursor law
        // needs". It is the opposite: a hole in the MIDDLE of a replay is
        // caught by the next sequence, but a hole at its END has no next
        // sequence at all, and this marker is the only thing that says how
        // far the replay was supposed to reach (review W3c3 P1-2).
        WireFrame::AttachCaughtUp {
            attachment_id,
            high_water_seq,
        } => vec![LiveReply::CaughtUp {
            attachment: attachment_id,
            high_water_seq,
        }],
        WireFrame::ServerDraining { reason, .. } => vec![LiveReply::Draining { reason }],
        WireFrame::ProtocolError(error) => vec![LiveReply::Failed {
            command_id: None,
            code: error.code.clone(),
            message: error.message.clone(),
            retryable: !error.fatal,
        }],
        // Handshake, correlated, and heartbeat frames never reach here; an
        // unknown frame from a newer daemon is tolerated, never fatal.
        _ => Vec::new(),
    }
}
