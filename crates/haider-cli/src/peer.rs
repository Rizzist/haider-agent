//! Scriptable peer-messaging commands.

use std::io::{self, Read};
use std::process::ExitCode;

use haider_client::{
    EnsureOptions, PeerClientError, PeerDelivery, PeerDeliveryReason, PeerDescriptor, PeerEvent,
    PeerKind, PeerReceipt, PeerState, ProfileEnv, ensure_daemon, peer_messaging, resolve_profile,
};
use serde::Serialize;

use super::run::{EX_BLOCKED, EX_IOERR, EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

pub(crate) const PEER_LIST_SCHEMA: &str = "haider.peer.list.v1";
pub(crate) const PEER_EVENT_SCHEMA: &str = "haider.peer.event.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerCommand {
    List { json: bool },
    Send { to: String, message: String },
    Name { name: String },
    Watch,
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
        message: &'a haider_client::PeerMessage,
    },
    DeliveryChanged {
        schema: &'static str,
        receipt: &'a haider_client::PeerReceipt,
    },
}

pub(crate) fn parse_peer_command(rest: &[String]) -> Result<PeerCommand, String> {
    match rest {
        [command] if command == "list" => Ok(PeerCommand::List { json: false }),
        [command, flag] if command == "list" && flag == "--json" => {
            Ok(PeerCommand::List { json: true })
        }
        [command, to, message] if command == "send" && !to.is_empty() && !message.is_empty() => {
            Ok(PeerCommand::Send {
                to: to.clone(),
                message: message.clone(),
            })
        }
        [command, name] if command == "name" && !name.is_empty() => {
            Ok(PeerCommand::Name { name: name.clone() })
        }
        [command] if command == "watch" => Ok(PeerCommand::Watch),
        _ => Err(
            "usage: peer list [--json] | peer send <name> <message|-> | peer name <new-name> | peer watch"
                .into(),
        ),
    }
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
        PeerCommand::Send { to, message } => peers.send(to, message, None).await.map(|receipt| {
            println!("{} {}", receipt.msg_id, delivery_label(receipt.delivery));
            if receipt.delivery == PeerDelivery::Refused {
                PeerCommandOutcome::RefusedDelivery(receipt)
            } else {
                PeerCommandOutcome::Complete
            }
        }),
        PeerCommand::Name { name } => peers.set_name(name).await.map(|agent| {
            println!("{}", agent.name);
            PeerCommandOutcome::Complete
        }),
        PeerCommand::Watch => match peers.subscribe().await {
            Ok(mut events) => {
                let mut result = Ok(PeerCommandOutcome::Complete);
                while let Some(event) = events.next().await {
                    let encoded = match &event {
                        PeerEvent::Received(message) => print_json(&PeerEventDocument::Received {
                            schema: PEER_EVENT_SCHEMA,
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

fn read_stdin_message(command: PeerCommand) -> io::Result<PeerCommand> {
    match command {
        PeerCommand::Send { to, message } if message == "-" => {
            let mut message = String::new();
            io::stdin().read_to_string(&mut message)?;
            if message.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stdin message is empty",
                ));
            }
            Ok(PeerCommand::Send { to, message })
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
    println!("NAME\tKIND\tWORKSPACE\tSTATE\tLAST SEEN");
    for agent in agents {
        println!(
            "{}\t{}\t{}\t{}\t{}",
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

const fn delivery_label(delivery: PeerDelivery) -> &'static str {
    match delivery {
        PeerDelivery::Queued => "queued",
        PeerDelivery::Delivered => "delivered",
        PeerDelivery::Expired => "expired",
        PeerDelivery::Refused => "refused",
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
        PeerClientError::Client(_) => EX_UNAVAILABLE,
        PeerClientError::Refused { .. } | PeerClientError::UnexpectedBody => EX_PROTOCOL,
        PeerClientError::EventsAlreadyTaken => EX_UNAVAILABLE,
    }
}

#[cfg(test)]
#[path = "peer_tests.rs"]
mod tests;
