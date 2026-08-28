//! Scriptable unified shell registry commands.

use std::process::ExitCode;

use haider_client::{EnsureOptions, ProfileEnv, ensure_daemon, resolve_profile, shell_registry};
use serde::Serialize;

use crate::run::{EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

pub(crate) const SHELL_LIST_SCHEMA: &str = "haider.shell.list.v1";

#[derive(Serialize)]
struct ShellListDocument<'a> {
    schema: &'static str,
    shells: &'a [haider_rpc::ShellWire],
}

#[derive(Debug)]
enum ShellCommandError {
    Client(haider_client::ShellRegistryClientError),
    Json(serde_json::Error),
}

impl std::fmt::Display for ShellCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Json(error) => write!(formatter, "cannot encode JSON: {error}"),
        }
    }
}

impl From<haider_client::ShellRegistryClientError> for ShellCommandError {
    fn from(error: haider_client::ShellRegistryClientError) -> Self {
        Self::Client(error)
    }
}

impl From<serde_json::Error> for ShellCommandError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub(crate) async fn shell_command(rest: &[String]) -> ExitCode {
    let (close, json) = match rest {
        [command] if command == "list" => (None, false),
        [command, flag] if command == "list" && flag == "--json" => (None, true),
        [command, id] if command == "close" => (Some(id.clone()), false),
        _ => {
            eprintln!("usage: haider shell list [--json] | haider shell close <id>");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider shell: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let mut options = EnsureOptions::default();
    options
        .required_features
        .insert(haider_rpc::FEATURE_SHELL_REGISTRY_V1.to_owned());
    let ensured = match ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("haider shell: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let result: Result<(), ShellCommandError> = match shell_registry(&ensured.client) {
        Some(shells) => {
            async {
                match close {
                    Some(id) => {
                        let shell = shells.close(id).await?;
                        println!("{}", shell.id);
                    }
                    None => {
                        let shells = shells.list().await?;
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string(&ShellListDocument {
                                    schema: SHELL_LIST_SCHEMA,
                                    shells: &shells,
                                })?
                            );
                        } else {
                            println!("ID\tKIND\tSTATUS\tTITLE\tHOST/CWD");
                            for shell in shells {
                                println!(
                                    "{}\t{:?}\t{:?}\t{}\t{}",
                                    shell.id,
                                    shell.kind,
                                    shell.status,
                                    shell.title,
                                    shell.cwd_or_host
                                );
                            }
                        }
                    }
                }
                Ok(())
            }
            .await
        }
        None => {
            ensured.client.close();
            eprintln!("haider shell: daemon does not advertise shell_registry_v1");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    ensured.client.close();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("haider shell: {error}");
            ExitCode::from(EX_PROTOCOL)
        }
    }
}

#[cfg(test)]
#[path = "shell_registry_tests.rs"]
mod tests;
