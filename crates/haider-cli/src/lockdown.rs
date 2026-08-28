//! Machine-user-wide lockdown quota/status commands.

#[cfg(test)]
#[path = "lockdown_tests.rs"]
mod lockdown_tests;

use std::collections::BTreeSet;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use haider_client::{EnsureOptions, ProfileEnv, ensure_daemon, provider_lockdown, resolve_profile};
use haider_rpc::{Capability, CapabilitySet, ClientKind, CommandId};
use serde::Serialize;

use super::run::{EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

pub(crate) async fn lockdown_command(rest: &[String]) -> ExitCode {
    let (set, json) = match parse(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("haider lockdown: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider lockdown: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let mut options = EnsureOptions {
        required_features: BTreeSet::from([haider_rpc::FEATURE_PROVIDER_LOCKDOWN_V1.to_owned()]),
        ..EnsureOptions::default()
    };
    options.client = haider_client::ClientConfig {
        client_name: "haider-lockdown".to_owned(),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        ..options.client
    };
    let ensured = match ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("haider lockdown: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let result = match provider_lockdown(&ensured.client) {
        Some(lockdown) => match set {
            Some(bytes) => lockdown.set_quota(command_id(), bytes).await,
            None => lockdown.status(None).await,
        },
        None => {
            eprintln!("haider lockdown: daemon does not advertise provider_lockdown_v1");
            ensured.client.close();
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    ensured.client.close();
    match result {
        Ok(status) => {
            match render_status(&status, json) {
                Ok(output) => println!("{output}"),
                Err(error) => {
                    eprintln!("haider lockdown: {error}");
                    return ExitCode::from(EX_PROTOCOL);
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("haider lockdown: {error}");
            ExitCode::from(EX_PROTOCOL)
        }
    }
}

fn parse(rest: &[String]) -> Result<(Option<u64>, bool), String> {
    match rest {
        [command] if command == "status" || command == "quota" => Ok((None, false)),
        [command, flag] if (command == "status" || command == "quota") && flag == "--json" => {
            Ok((None, true))
        }
        [command, flag, bytes] if command == "quota" && flag == "--set" => {
            match bytes.parse::<u64>() {
                Ok(bytes) => Ok((Some(bytes), false)),
                Err(_) => Err("--set requires an unsigned byte count".to_owned()),
            }
        }
        [command, flag, bytes, json]
            if command == "quota" && flag == "--set" && json == "--json" =>
        {
            match bytes.parse::<u64>() {
                Ok(bytes) => Ok((Some(bytes), true)),
                Err(_) => Err("--set requires an unsigned byte count".to_owned()),
            }
        }
        _ => Err(
            "usage: lockdown status [--json] | lockdown quota [--set <bytes>] [--json]".to_owned(),
        ),
    }
}

#[derive(Serialize)]
struct LockdownDocument<'a> {
    schema: &'static str,
    status: &'a haider_rpc::LockdownStatusWire,
}

fn render_status(status: &haider_rpc::LockdownStatusWire, json: bool) -> Result<String, String> {
    if json {
        serde_json::to_string_pretty(&LockdownDocument {
            schema: "haider.lockdown.v1",
            status,
        })
        .map_err(|error| error.to_string())
    } else {
        Ok(format!(
            "quota {} / {} bytes\nallowed tools: {}",
            status.quota_used,
            status.quota_limit,
            status.tools_allowed.join(", ")
        ))
    }
}

fn command_id() -> CommandId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    CommandId::new(format!("lockdown-quota-{}-{nanos}", std::process::id()))
}
