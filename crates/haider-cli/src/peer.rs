//! Scriptable peer-messaging commands.

use std::io::{self, Read};
use std::process::ExitCode;

use haider_client::{
    EnsureOptions, PeerClientError, PeerDelivery, PeerDeliveryReason, PeerDescriptor, PeerEvent,
    PeerKind, PeerReceipt, PeerState, ProfileEnv, ensure_daemon, peer_messaging, resolve_profile,
};
use haider_protocol::peer::{PeerSendOptions, PeerStatusQuery};
use serde::Serialize;

use super::run::{EX_BLOCKED, EX_IOERR, EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

pub(crate) const PEER_LIST_SCHEMA: &str = "haider.peer.list.v1";
pub(crate) const PEER_EVENT_SCHEMA: &str = "haider.peer.event.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerCommand {
    List {
        json: bool,
    },
    Send {
        options: PeerSendOptions,
        session: Option<String>,
        to: String,
        message: String,
    },
    Status {
        session: Option<String>,
        msg_id: Option<String>,
        after_seq: u64,
        watch: bool,
    },
    Name {
        session: Option<String>,
        name: String,
    },
    Watch,
    WaitIdle {
        to: String,
    },
}

enum PeerCommandOutcome {
    Complete,
    RefusedDelivery(PeerReceipt),
}

#[derive(Serialize)]
pub(crate) struct PeerListDocument<'a> {
    pub schema: &'static str,
    pub agents: &'a [PeerDescriptor],
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PeerEventDocument<'a> {
    Received {
        schema: &'static str,
        speaker: &'static str,
        authority: &'static str,
        message: &'a haider_client::PeerMessage,
    },
    DeliveryChanged {
        schema: &'static str,
        receipt: &'a haider_client::PeerReceipt,
    },
}

pub(crate) fn parse_peer_command(rest: &[String]) -> Result<PeerCommand, String> {
    if rest.first().is_some_and(|command| {
        command == "send" || command == "status" || (command == "watch" && rest.len() > 1)
    }) {
        return parse_delivery_command(rest);
    }
    match rest {
        [command] if command == "list" => Ok(PeerCommand::List { json: false }),
        [command, flag] if command == "list" && flag == "--json" => {
            Ok(PeerCommand::List { json: true })
        }
        [command, name] if command == "name" && name != "--session" && !name.is_empty() => {
            Ok(PeerCommand::Name { session: None, name: name.clone() })
        }
        [command, flag, session, name] if command == "name" && flag == "--session" && !session.is_empty() && !name.is_empty() => {
            Ok(PeerCommand::Name { session: Some(session.clone()), name: name.clone() })
        }
        [command] if command == "watch" => Ok(PeerCommand::Watch),
        [command, to] if command == "wait-idle" && !to.is_empty() => {
            Ok(PeerCommand::WaitIdle { to: to.clone() })
        }
        _ => Err(
            "usage: peer list [--json] | peer send [--session <id>] [--id <msg-id>] [--ttl-ms <ms>] <address> <message|-> | peer status [--session <id>] [--after <seq>] [msg-id] | peer status [--session <id>] --cancel <msg-id> | peer name [--session <id>] <new-name> | peer watch | peer wait-idle <address>"
                .into(),
        ),
    }
}

fn parse_delivery_command(rest: &[String]) -> Result<PeerCommand, String> {
    let mut session = None;
    let mut options = PeerSendOptions::default();
    let mut after_seq = 0;
    let mut index = 1;
    while rest.get(index).is_some_and(|value| value.starts_with("--")) {
        let flag = &rest[index];
        let value = rest
            .get(index + 1)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--session" if session.is_none() => session = Some(value.clone()),
            "--id" if rest[0] == "send" && options.msg_id.is_none() => {
                options.msg_id = Some(value.clone())
            }
            "--ttl-ms" if rest[0] == "send" && options.ttl_ms.is_none() => {
                options.ttl_ms = Some(
                    value
                        .parse()
                        .map_err(|_| "--ttl-ms requires milliseconds")?,
                )
            }
            "--after" if rest[0] != "send" => {
                after_seq = value
                    .parse()
                    .map_err(|_| "--after requires a journal sequence")?
            }
            "--cancel" if rest[0] == "status" && options.msg_id.is_none() => {
                options.cancel = true;
                options.msg_id = Some(value.clone());
            }
            _ => return Err(format!("unknown or repeated peer option {flag}")),
        }
        index += 2;
    }
    let args = &rest[index..];
    if options.cancel {
        if !args.is_empty() {
            return Err("peer status --cancel does not accept extra arguments".into());
        }
        return Ok(PeerCommand::Send {
            session,
            options,
            to: String::new(),
            message: String::new(),
        });
    }
    if rest[0] == "send" {
        if let [to, message] = args
            && !to.is_empty()
            && !message.is_empty()
        {
            return Ok(PeerCommand::Send {
                session,
                options,
                to: to.clone(),
                message: message.clone(),
            });
        }
        return Err("peer send requires an address and message".into());
    }
    if args.len() > 1 || args.first().is_some_and(|id| id.is_empty()) {
        return Err("peer status/watch accepts at most one msg-id".into());
    }
    Ok(PeerCommand::Status {
        session,
        msg_id: args.first().cloned(),
        after_seq,
        watch: rest[0] == "watch",
    })
}

pub(crate) async fn peer_command(rest: &[String]) -> ExitCode {
    let command = match parse_peer_command(rest) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("haider peer: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let command = match read_stdin_message(command) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("haider peer: cannot read message from stdin: {error}");
            return ExitCode::from(EX_IOERR);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider peer: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let mut options = EnsureOptions::default();
    options
        .required_features
        .insert(haider_rpc::FEATURE_PEER_MESSAGING_V1.to_owned());
    if matches!(command, PeerCommand::WaitIdle { .. }) {
        options
            .required_features
            .insert(haider_rpc::FEATURE_PEER_AGENT_INJECTION_V1.to_owned());
    }
    let ensured = match ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("haider peer: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let Some(peers) = peer_messaging(&ensured.client) else {
        eprintln!("haider peer: daemon does not advertise peer_messaging_v1");
        let _ = ensured.client.close();
        return ExitCode::from(EX_UNAVAILABLE);
    };

    // Sender attribution comes from a control attachment, never from the
    // target address or a guessed roster entry. Drain replay on this short-lived
    // command connection so an established sender's history cannot stall RPC.
    let replay_drain = if matches!(command, PeerCommand::Send { .. } | PeerCommand::Name { .. }) {
        let sender = match &command {
            PeerCommand::Send { session, .. } | PeerCommand::Name { session, .. } => session
                .clone()
                .or_else(|| std::env::var("HAIDER_SESSION_ID").ok()),
            _ => None,
        };
        let Some(sender) = sender.filter(|value| !value.trim().is_empty()) else {
            eprintln!(
                "haider peer: sending or renaming requires --session <id> or HAIDER_SESSION_ID for the sender session"
            );
            let _ = ensured.client.close();
            return ExitCode::from(EX_USAGE);
        };
        let drain = ensured
            .client
            .take_events()
            .map(|mut events| tokio::spawn(async move { while events.recv().await.is_some() {} }));
        if let Err(error) = attach_sender(&ensured.client, sender).await {
            eprintln!("haider peer: {error}");
            let _ = ensured.client.close();
            if let Some(drain) = drain {
                drain.abort();
            }
            return ExitCode::from(peer_error_exit(&error));
        }
        drain
    } else {
        None
    };

    let result = match command {
        PeerCommand::List { json } => peers.list().await.and_then(|agents| {
            if json {
                print_json(&PeerListDocument {
                    schema: PEER_LIST_SCHEMA,
                    agents: &agents,
                })?;
            } else {
                print_peer_table(&agents);
            }
            Ok(PeerCommandOutcome::Complete)
        }),
        PeerCommand::Send {
            to,
            message,
            options,
            ..
        } => peers
            .send_with_options(to, message, None, options)
            .await
            .and_then(|receipt| {
                print_json(&receipt)?;
                if receipt.delivery == PeerDelivery::Refused {
                    Ok(PeerCommandOutcome::RefusedDelivery(receipt))
                } else {
                    Ok(PeerCommandOutcome::Complete)
                }
            }),
        PeerCommand::Status {
            session,
            msg_id,
            after_seq,
            watch,
        } => {
            let session = session.or_else(|| std::env::var("HAIDER_SESSION_ID").ok());
            if let Some(session) = session.filter(|value| !value.trim().is_empty()) {
                replay_status(
                    &peers,
                    PeerStatusQuery {
                        session_id: haider_protocol::ids::SessionId::new(session),
                        msg_id,
                        after_seq,
                    },
                    watch,
                )
                .await
            } else {
                Err(PeerClientError::Refused {
                    code: "peer_invalid".into(),
                    message: "peer status/watch requires --session <id> or HAIDER_SESSION_ID"
                        .into(),
                    retryable: false,
                    data: None,
                })
            }
        }
        PeerCommand::Name { name, .. } => peers.set_name(name).await.map(|agent| {
            println!("{}", agent.name);
            PeerCommandOutcome::Complete
        }),
        PeerCommand::WaitIdle { to } => peers.notify_when_idle(to).await.map(|agent| {
            println!("{} idle", agent.address());
            PeerCommandOutcome::Complete
        }),
        PeerCommand::Watch => match peers.subscribe().await {
            Ok(mut events) => {
                let mut result = Ok(PeerCommandOutcome::Complete);
                while let Some(event) = events.next().await {
                    let encoded = match &event {
                        PeerEvent::Received(message) => print_json(&PeerEventDocument::Received {
                            schema: PEER_EVENT_SCHEMA,
                            speaker: "agent",
                            authority: haider_rpc::haider_protocol::peer::PEER_AUTHORITY_STATEMENT,
                            message,
                        }),
                        PeerEvent::DeliveryChanged(receipt) => {
                            print_json(&PeerEventDocument::DeliveryChanged {
                                schema: PEER_EVENT_SCHEMA,
                                receipt,
                            })
                        }
                    };
                    if let Err(error) = encoded {
                        result = Err(error);
                        break;
                    }
                }
                result
            }
            Err(error) => Err(error),
        },
    };
    let _ = ensured.client.close();
    if let Some(drain) = replay_drain {
        drain.abort();
    }
    match result {
        Ok(PeerCommandOutcome::Complete) => ExitCode::SUCCESS,
        Ok(PeerCommandOutcome::RefusedDelivery(receipt)) => {
            let reason = receipt
                .reason
                .map(reason_label)
                .unwrap_or("unspecified reason");
            eprintln!("haider peer: delivery refused: {reason}");
            ExitCode::from(EX_BLOCKED)
        }
        Err(error) => {
            eprintln!("haider peer: {error}");
            ExitCode::from(peer_error_exit(&error))
        }
    }
}

async fn replay_status(
    peers: &haider_client::peer::PeerMessaging<'_>,
    mut query: PeerStatusQuery,
    watch: bool,
) -> Result<PeerCommandOutcome, PeerClientError> {
    loop {
        let page = peers.status(query.clone()).await?;
        if !watch || !page.receipts.is_empty() {
            print_json(&serde_json::json!({"schema": "haider.peer.status.v1", "status": page}))?;
        }
        query.after_seq = page.next_seq;
        if page.has_more {
            continue;
        }
        if !watch {
            return Ok(PeerCommandOutcome::Complete);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn attach_sender(
    client: &haider_client::RpcClient,
    sender: String,
) -> Result<(), PeerClientError> {
    match client
        .request(haider_rpc::RequestBody::SessionAttach {
            session_id: haider_protocol::ids::SessionId::new(sender),
            after_seq: 0,
            mode: haider_rpc::AttachMode::Control,
            sealed_replay: false,
        })
        .await?
    {
        haider_rpc::ResponseBody::SessionAttach { .. } => Ok(()),
        haider_rpc::ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(PeerClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(PeerClientError::UnexpectedBody),
    }
}

fn read_stdin_message(command: PeerCommand) -> io::Result<PeerCommand> {
    match command {
        PeerCommand::Send {
            session,
            to,
            message,
            options,
        } if message == "-" => {
            let mut message = String::new();
            io::stdin().read_to_string(&mut message)?;
            if message.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stdin message is empty",
                ));
            }
            Ok(PeerCommand::Send {
                session,
                to,
                message,
                options,
            })
        }
        command => Ok(command),
    }
}

fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<(), PeerClientError> {
    let line = serde_json::to_string(value).map_err(|_| PeerClientError::UnexpectedBody)?;
    println!("{line}");
    Ok(())
}

fn print_peer_table(agents: &[PeerDescriptor]) {
    println!("ADDRESS\tNAME\tKIND\tWORKSPACE\tSTATE\tLAST SEEN");
    for agent in agents {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            agent.address(),
            agent.name,
            kind_label(agent.kind),
            agent.workspace,
            state_label(agent.state),
            agent.last_seen
        );
    }
}

const fn kind_label(kind: PeerKind) -> &'static str {
    match kind {
        PeerKind::HaiderSession => "haider_session",
        PeerKind::External => "external",
    }
}

const fn state_label(state: PeerState) -> &'static str {
    match state {
        PeerState::Idle => "idle",
        PeerState::Busy => "busy",
    }
}

const fn reason_label(reason: PeerDeliveryReason) -> &'static str {
    match reason {
        PeerDeliveryReason::DeadlineElapsed => "deadline_elapsed",
        PeerDeliveryReason::TargetNeverReturned => "target_never_returned",
        PeerDeliveryReason::TargetUnavailable => "target_unavailable",
        PeerDeliveryReason::TargetRefused => "target_refused",
        PeerDeliveryReason::InvalidMessage => "invalid_message",
    }
}

fn peer_error_exit(error: &PeerClientError) -> u8 {
    match error {
        PeerClientError::Refused { code, .. }
            if code.contains("permission") || code.contains("refused") =>
        {
            EX_BLOCKED
        }
        PeerClientError::Refused { code, .. }
            if code == haider_rpc::ERROR_CODE_PEER_UNAVAILABLE =>
        {
            EX_UNAVAILABLE
        }
        PeerClientError::Client(_) => EX_UNAVAILABLE,
        PeerClientError::Refused { .. } | PeerClientError::UnexpectedBody => EX_PROTOCOL,
        PeerClientError::EventsAlreadyTaken => EX_UNAVAILABLE,
    }
}

#[cfg(test)]
#[path = "peer_tests.rs"]
mod tests;
