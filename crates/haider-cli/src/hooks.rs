//! Scriptable hook discovery and digest trust commands.

use haider_client::{EnsureOptions, ProfileEnv, ensure_daemon, resolve_profile};
use haider_rpc::{CommandId, HookSummaryWire, RequestBody, ResponseBody};
use serde_json::json;
use std::process::ExitCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HooksCommand {
    List { json: bool },
    Trust { digest: String },
    Revoke { digest: String },
}

pub(crate) fn parse_hooks_command(rest: &[String]) -> Result<HooksCommand, String> {
    match rest {
        [command] if command == "list" => Ok(HooksCommand::List { json: false }),
        [command, flag] if command == "list" && flag == "--json" => {
            Ok(HooksCommand::List { json: true })
        }
        [command, digest] if command == "trust" && !digest.is_empty() => Ok(HooksCommand::Trust {
            digest: digest.clone(),
        }),
        [command, digest] if command == "revoke" && !digest.is_empty() => {
            Ok(HooksCommand::Revoke {
                digest: digest.clone(),
            })
        }
        [] => Err("expected list [--json], trust <digest>, or revoke <digest>".into()),
        _ => Err("expected list [--json], trust <digest>, or revoke <digest>".into()),
    }
}

pub(crate) async fn hooks_command(rest: &[String]) -> ExitCode {
    let command = match parse_hooks_command(rest) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("haider hooks: {message}");
            return ExitCode::from(2);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider hooks: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut options = EnsureOptions::default();
    options
        .required_features
        .insert(haider_rpc::FEATURE_HOOKS_V1.to_owned());
    let ensured = match ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("haider hooks: {error}");
            return ExitCode::FAILURE;
        }
    };
    let request = match &command {
        HooksCommand::List { .. } => {
            let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
                Ok(cwd) => cwd,
                Err(error) => {
                    eprintln!("haider hooks: current directory is unavailable: {error}");
                    ensured.client.close();
                    return ExitCode::FAILURE;
                }
            };
            let Some(cwd) = cwd.to_str() else {
                eprintln!("haider hooks: current directory is not valid UTF-8");
                ensured.client.close();
                return ExitCode::FAILURE;
            };
            RequestBody::HooksList {
                cwd: cwd.to_owned(),
            }
        }
        HooksCommand::Trust { digest } => RequestBody::HooksTrust {
            command_id: CommandId::new(command_id("trust")),
            digest: digest.clone(),
        },
        HooksCommand::Revoke { digest } => RequestBody::HooksRevoke {
            command_id: CommandId::new(command_id("revoke")),
            digest: digest.clone(),
        },
    };
    let response = ensured.client.request(request).await;
    ensured.client.close();
    match (command, response) {
        (HooksCommand::List { json: machine }, Ok(ResponseBody::HooksList { policy, hooks })) => {
            if machine {
                let value = json!({
                    "schema": haider_protocol::hook::HOOKS_LIST_SCHEMA,
                    "kind": "hooks",
                    "policy": policy,
                    "hooks": hooks,
                });
                match serde_json::to_string(&value) {
                    Ok(line) => println!("{line}"),
                    Err(error) => {
                        eprintln!("haider hooks: cannot encode response: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                print_hooks(&policy, &hooks);
            }
            ExitCode::SUCCESS
        }
        (HooksCommand::Trust { .. }, Ok(ResponseBody::HooksTrust { digest, trusted }))
        | (HooksCommand::Revoke { .. }, Ok(ResponseBody::HooksRevoke { digest, trusted })) => {
            let status = if trusted { "trusted" } else { "revoked" };
            println!("{status} {digest}");
            ExitCode::SUCCESS
        }
        (_, Ok(ResponseBody::Error { message, .. })) => {
            eprintln!("haider hooks: {message}");
            ExitCode::FAILURE
        }
        (_, Ok(_)) => {
            eprintln!("haider hooks: daemon returned an unexpected response");
            ExitCode::FAILURE
        }
        (_, Err(error)) => {
            eprintln!("haider hooks: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_hooks(policy: &str, hooks: &[HookSummaryWire]) {
    println!("policy: {policy}");
    if hooks.is_empty() {
        println!("no hooks discovered");
        return;
    }
    for hook in hooks {
        let trust = if hook.trusted { "trusted" } else { "untrusted" };
        let decision = if hook.decision { " decision" } else { "" };
        println!(
            "{}  {}  {}:{}{}  {}  {}",
            hook.digest, hook.name, hook.kind, hook.event, decision, trust, hook.source
        );
    }
}

fn command_id(operation: &str) -> String {
    format!(
        "hooks-{operation}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    )
}
