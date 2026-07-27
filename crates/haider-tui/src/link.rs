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
//! Requests are performed on SPAWNED tasks with a cloned client handle: a
//! slow response must never stall event forwarding, because events are how
//! the transcript stays live while a mutation is in flight.

use std::sync::Arc;
use std::time::Duration;

use haider_client::{ClientConfig, ClientError, DisconnectReason, ResolvedProfile, RpcClient};
use haider_rpc::{AttachMode, RequestBody, ResponseBody, SeqRange, SessionSummary, WireFrame};
use tokio::sync::mpsc;

use crate::live::{LiveCommand, LiveReply};

/// Bounded redial backoff: the daemon may be draining, restarting, or being
/// replaced by a newer version. We keep trying, never faster than this.
const REDIAL_MIN: Duration = Duration::from_millis(200);
const REDIAL_MAX: Duration = Duration::from_secs(5);

/// Channel depth for commands and replies. Both are UI-paced.
const LINK_CAPACITY: usize = 256;

/// A live link: send [`LiveCommand`]s, receive [`LiveReply`]s.
pub struct Link {
    pub commands: mpsc::Sender<LiveCommand>,
    pub replies: mpsc::Receiver<LiveReply>,
    task: tokio::task::JoinHandle<()>,
}

impl Link {
    /// Start the link over an already-negotiated connection.
    #[must_use]
    pub fn start(client: RpcClient, profile: ResolvedProfile, config: ClientConfig) -> Self {
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
    loop {
        let dead = tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return };
                issue(&client, command, &replies);
                None
            }
            frame = events.recv() => match frame {
                Some(frame) => {
                    for reply in map_frame(frame) {
                        if replies.send(reply).await.is_err() {
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

/// Perform one command on a spawned task so the link keeps forwarding
/// events while it is in flight.
fn issue(client: &Arc<RpcClient>, command: LiveCommand, replies: &mpsc::Sender<LiveReply>) {
    let client = Arc::clone(client);
    let replies = replies.clone();
    tokio::spawn(async move {
        // A menu answer is an UNCORRELATED frame by wire design (the
        // durable identity is its `command_id`). Its outcome arrives as the
        // committed `MenuAnswered` envelope, which is also what retires it
        // from the outbox — a correlated echo would be a second authority.
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
            return;
        }
        let context = CommandContext::of(&command);
        let body = request_body(command);
        match client.request(body).await {
            Ok(response) => {
                for reply in map_response(&context, response) {
                    let _ = replies.send(reply).await;
                }
            }
            Err(ClientError::Disconnected(_)) => {
                // The outbox resends under the same durable id on reconnect;
                // reads are reissued by the resume path.
            }
            Err(error) => {
                let _ = replies
                    .send(LiveReply::Failed {
                        command_id: context.command_id.clone(),
                        code: "encode_failed".to_owned(),
                        message: error.to_string(),
                        retryable: false,
                    })
                    .await;
            }
        }
    });
}

/// What a response needs from its request to be interpretable (the wire
/// deliberately does not echo, e.g., which session an attach was for).
struct CommandContext {
    command_id: Option<haider_rpc::CommandId>,
    cwd: String,
    model: String,
}

impl CommandContext {
    fn of(command: &LiveCommand) -> Self {
        let (cwd, model) = match command {
            LiveCommand::Create { cwd, model, .. } => (cwd.clone(), model.clone()),
            _ => (String::new(), String::new()),
        };
        Self {
            command_id: command.command_id().cloned(),
            cwd,
            model,
        }
    }
}

fn request_body(command: LiveCommand) -> RequestBody {
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
        LiveCommand::Read { session, range } => RequestBody::SessionRead {
            session_id: session,
            range,
        },
        LiveCommand::Create {
            command_id,
            cwd,
            provider,
            model,
            max_tokens,
            ..
        } => RequestBody::SessionCreate {
            command_id,
            cwd,
            provider,
            model,
            max_tokens,
        },
        LiveCommand::Submit {
            command_id,
            session,
            worker_generation,
            text,
            mode,
        } => RequestBody::TurnSubmit {
            command_id,
            session_id: session,
            worker_generation,
            text,
            attachments: vec![],
            mode,
        },
        LiveCommand::Cancel {
            command_id,
            session,
            worker_generation,
            run_id,
        } => RequestBody::TurnCancel {
            command_id,
            session_id: session,
            worker_generation,
            run_id,
        },
        LiveCommand::Answer { .. } => unreachable!("answers ride send_frame, not request"),
    }
}

fn map_response(context: &CommandContext, body: ResponseBody) -> Vec<LiveReply> {
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
        ResponseBody::TurnSubmit {
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
        ResponseBody::SessionRead { result } => read_replies(result),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => vec![LiveReply::Failed {
            command_id: context.command_id.clone(),
            code,
            message,
            retryable,
        }],
        // Account/menu bodies belong to M3's login card; unknown bodies are
        // tolerated, never fatal (forward-compat law).
        _ => Vec::new(),
    }
}

/// A cold read replays as ordinary events for the session's own reducer —
/// the SAME reduction path an attachment uses, so a cold session's
/// transcript can never be built by a second, divergent projector.
fn read_replies(result: haider_rpc::SessionReadResult) -> Vec<LiveReply> {
    let session = result.session_id;
    result
        .envelopes
        .into_iter()
        .map(|envelope| LiveReply::ColdRead {
            session: session.clone(),
            envelope: Box::new(envelope),
        })
        .collect()
}

fn map_frame(frame: WireFrame) -> Vec<LiveReply> {
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
        WireFrame::ServerDraining { reason, .. } => vec![LiveReply::Draining { reason }],
        WireFrame::ProtocolError(error) => vec![LiveReply::Failed {
            command_id: None,
            code: error.code.clone(),
            message: error.message.clone(),
            retryable: !error.fatal,
        }],
        // AttachCaughtUp carries no state the cursor law needs: events
        // deduplicate by seq alone, and the marker may repeat.
        _ => Vec::new(),
    }
}

/// Session summaries the launcher can render before any attach.
#[must_use]
pub fn summary_ids(sessions: &[SessionSummary]) -> Vec<String> {
    sessions
        .iter()
        .map(|summary| summary.session_id.as_str().to_owned())
        .collect()
}

/// The full range of a cold session, for a metadata read.
#[must_use]
pub const fn full_range(head_seq: u64) -> SeqRange {
    SeqRange {
        start_seq: 1,
        end_seq: head_seq,
    }
}
