//! Scriptable convergence-graph control surface.

use std::process::ExitCode;

use haider_protocol::graph::{GraphNodeName, GraphPhase};
use haider_protocol::ids::SessionId;
use haider_rpc::{AttachMode, AttachmentId, RequestBody, ResponseBody};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphDispatch {
    Status { session_id: String, json: bool },
    Pin { session_id: String },
    Abandon { session_id: String, why: String },
}

pub(crate) fn parse_graph_dispatch(rest: &[String]) -> Result<GraphDispatch, String> {
    match rest {
        [command, session_id] if command == "status" => Ok(GraphDispatch::Status {
            session_id: session_id.clone(),
            json: false,
        }),
        [command, session_id, flag] if command == "status" && flag == "--json" => {
            Ok(GraphDispatch::Status {
                session_id: session_id.clone(),
                json: true,
            })
        }
        [command, session_id] if command == "pin" => Ok(GraphDispatch::Pin {
            session_id: session_id.clone(),
        }),
        [command, session_id] if command == "abandon" => Ok(GraphDispatch::Abandon {
            session_id: session_id.clone(),
            why: "abandoned from CLI".to_owned(),
        }),
        [command, session_id, why @ ..] if command == "abandon" && !why.is_empty() => {
            Ok(GraphDispatch::Abandon {
                session_id: session_id.clone(),
                why: why.join(" "),
            })
        }
        [] => Err("expected status, pin, or abandon".into()),
        [command, ..] if !matches!(command.as_str(), "status" | "pin" | "abandon") => {
            Err(format!("unknown graph command `{command}`"))
        }
        _ => Err(
            "usage: graph status <session-id> [--json] | graph pin <session-id> | graph abandon <session-id> [why]"
                .into(),
        ),
    }
}

pub(crate) async fn graph_command(rest: &[String]) -> ExitCode {
    let dispatch = match parse_graph_dispatch(rest) {
        Ok(dispatch) => dispatch,
        Err(message) => {
            eprintln!("haider graph: {message}");
            return ExitCode::from(2);
        }
    };
    let session_id = SessionId::new(match &dispatch {
        GraphDispatch::Status { session_id, .. }
        | GraphDispatch::Pin { session_id }
        | GraphDispatch::Abandon { session_id, .. } => session_id.clone(),
    });
    let profile = match haider_client::resolve_profile(&haider_client::ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider graph: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut options = haider_client::EnsureOptions::default();
    options
        .required_features
        .insert(haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned());
    let ensured = match haider_client::ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("haider graph: {error}");
            return ExitCode::FAILURE;
        }
    };

    let result = match dispatch {
        GraphDispatch::Status { json, .. } => {
            match haider_client::graph_status(&ensured.client, session_id).await {
                Ok(status) if json => serde_json::to_string(&status)
                    .map(|value| println!("{value}"))
                    .map_err(|error| error.to_string()),
                Ok(Some(status)) => {
                    let lifecycle = match status.phase {
                        GraphPhase::Active => "active",
                        GraphPhase::Blocked => "blocked",
                        GraphPhase::Completed => "completed",
                        GraphPhase::Abandoned => "abandoned",
                        GraphPhase::Superseded => "superseded",
                    };
                    let node = status
                        .current_node
                        .as_ref()
                        .map_or("-", GraphNodeName::label);
                    println!(
                        "{} {} node={} attempt={}/{}",
                        status.graph_id,
                        lifecycle,
                        node,
                        status.attempt,
                        haider_protocol::graph::GRAPH_MAX_ATTEMPTS
                    );
                    Ok(())
                }
                Ok(None) => {
                    println!("no graph pinned");
                    Ok(())
                }
                Err(error) => Err(error.to_string()),
            }
        }
        GraphDispatch::Pin { .. } => {
            match control_attachment(&ensured.client, session_id.clone()).await {
                Ok((attachment_id, worker_generation)) => {
                    let outcome = haider_client::graph_pin(
                        &ensured.client,
                        haider_rpc::CommandId::new(command_id("graph-pin")),
                        session_id,
                        worker_generation,
                    )
                    .await
                    .map(|result| {
                        println!(
                            "pinned {} {} digest={} attempt=1",
                            result.graph_id, result.template, result.digest
                        );
                    })
                    .map_err(|error| error.to_string());
                    detach(&ensured.client, attachment_id).await;
                    outcome
                }
                Err(error) => Err(error),
            }
        }
        GraphDispatch::Abandon { why, .. } => {
            match control_attachment(&ensured.client, session_id.clone()).await {
                Ok((attachment_id, worker_generation)) => {
                    let outcome = haider_client::graph_abandon(
                        &ensured.client,
                        haider_rpc::CommandId::new(command_id("graph-abandon")),
                        session_id,
                        worker_generation,
                        why,
                    )
                    .await
                    .map(|result| println!("abandoned {}", result.graph_id))
                    .map_err(|error| error.to_string());
                    detach(&ensured.client, attachment_id).await;
                    outcome
                }
                Err(error) => Err(error),
            }
        }
    };
    ensured.client.close();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("haider graph: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn control_attachment(
    client: &haider_client::RpcClient,
    session_id: SessionId,
) -> Result<(AttachmentId, u64), String> {
    match client
        .request(RequestBody::SessionAttach {
            session_id,
            after_seq: 0,
            mode: AttachMode::Control,
        })
        .await
        .map_err(|error| error.to_string())?
    {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } => Ok((attachment_id, attach_state.worker_generation)),
        ResponseBody::Error { code, message, .. } => Err(format!("{code}: {message}")),
        _ => Err("session.attach response method mismatch".into()),
    }
}

async fn detach(client: &haider_client::RpcClient, attachment_id: AttachmentId) {
    let _ = client
        .request(RequestBody::SessionDetach { attachment_id })
        .await;
}

fn command_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    )
}

#[cfg(test)]
mod tests {
    use super::{GraphDispatch, parse_graph_dispatch};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_all_graph_cli_surfaces() {
        assert_eq!(
            parse_graph_dispatch(&args(&["status", "s1", "--json"])),
            Ok(GraphDispatch::Status {
                session_id: "s1".into(),
                json: true
            })
        );
        assert_eq!(
            parse_graph_dispatch(&args(&["pin", "s1"])),
            Ok(GraphDispatch::Pin {
                session_id: "s1".into()
            })
        );
        assert_eq!(
            parse_graph_dispatch(&args(&["abandon", "s1", "waiting", "for", "release"])),
            Ok(GraphDispatch::Abandon {
                session_id: "s1".into(),
                why: "waiting for release".into()
            })
        );
    }
}
