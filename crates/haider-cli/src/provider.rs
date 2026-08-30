//! Provider trust and lifecycle commands.

#[cfg(test)]
#[path = "provider_tests.rs"]
mod provider_tests;

use std::collections::BTreeSet;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use haider_client::{EnsureOptions, ProfileEnv, ensure_daemon, provider_lockdown, resolve_profile};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, CommandId, ProviderSummaryWire, ProviderTrustWire,
    RequestBody, ResponseBody, SnapshotAvailabilityWire,
};
use serde::Serialize;

use super::run::{EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

struct ProviderListDocument {
    schema: &'static str,
    revision: u64,
    providers: Vec<ProviderSummaryWire>,
}

struct ProviderShowDocument {
    schema: &'static str,
    revision: u64,
    provider: ProviderSummaryWire,
    envelope: haider_rpc::LockdownStatusWire,
}

impl Serialize for ProviderListDocument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde_json::json!({
            "schema": self.schema,
            "revision": self.revision,
            "providers": self.providers.iter().map(provider_json_with_trust).collect::<Vec<_>>(),
        })
        .serialize(serializer)
    }
}

impl Serialize for ProviderShowDocument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde_json::json!({
            "schema": self.schema,
            "revision": self.revision,
            "provider": provider_json_with_trust(&self.provider),
            "envelope": self.envelope,
        })
        .serialize(serializer)
    }
}

fn provider_json_with_trust(provider: &ProviderSummaryWire) -> serde_json::Value {
    let mut value = match serde_json::to_value(provider) {
        Ok(value) => value,
        Err(_) => serde_json::Value::Null,
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "trust".to_owned(),
            serde_json::Value::String(trust_name(provider.trust).to_owned()),
        );
    }
    value
}

pub(crate) async fn provider_command(rest: &[String]) -> ExitCode {
    if matches!(rest.first().map(String::as_str), Some("add")) {
        let mut delegated = Vec::with_capacity(rest.len());
        delegated.push("add".to_owned());
        delegated.extend(rest.iter().skip(1).cloned());
        return super::account::account_command(&delegated).await;
    }
    let (operation, name, trust, json, confirm) = match parse(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("haider provider: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    if operation == "remove" && !confirm {
        eprintln!("haider provider: pass --confirm to remove provider `{name}`");
        return ExitCode::from(EX_USAGE);
    }
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider provider: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let mut options = EnsureOptions {
        required_features: BTreeSet::from([
            haider_rpc::FEATURE_PROVIDER_MANAGEMENT_V1.to_owned(),
            haider_rpc::FEATURE_PROVIDER_LOCKDOWN_V1.to_owned(),
        ]),
        ..EnsureOptions::default()
    };
    options.client = haider_client::ClientConfig {
        client_name: "haider-provider".to_owned(),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        ..options.client
    };
    let ensured = match ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("haider provider: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let result = execute(&ensured.client, operation, name, trust, json).await;
    let _ = ensured.client.close();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("haider provider: {message}");
            ExitCode::from(EX_PROTOCOL)
        }
    }
}

fn parse(
    rest: &[String],
) -> Result<(&'static str, String, Option<ProviderTrustWire>, bool, bool), String> {
    match rest {
        [command] if command == "list" => Ok(("list", String::new(), None, false, false)),
        [command, flag] if command == "list" && flag == "--json" => {
            Ok(("list", String::new(), None, true, false))
        }
        [command, name] if command == "show" && !name.is_empty() => {
            Ok(("show", name.clone(), None, false, false))
        }
        [command, name, flag] if command == "show" && flag == "--json" => {
            Ok(("show", name.clone(), None, true, false))
        }
        [command, name, flag] if command == "set" && flag == "--lockdown" => Ok((
            "set",
            name.clone(),
            Some(ProviderTrustWire::Lockdown),
            false,
            false,
        )),
        [command, name, flag] if command == "set" && flag == "--full" => Ok((
            "set",
            name.clone(),
            Some(ProviderTrustWire::Full),
            false,
            false,
        )),
        [command, name, flag] if command == "remove" && flag == "--confirm" => {
            Ok(("remove", name.clone(), None, false, true))
        }
        [command, name] if command == "remove" => {
            Ok(("remove", name.clone(), None, false, false))
        }
        _ => Err("usage: provider list [--json] | provider show <name> [--json] | provider add <name> ... [--lockdown|--full] | provider set <name> (--lockdown|--full) | provider remove <name> --confirm".to_owned()),
    }
}

async fn execute(
    client: &haider_client::RpcClient,
    operation: &str,
    name: String,
    trust: Option<ProviderTrustWire>,
    json: bool,
) -> Result<(), String> {
    let (providers, revision) = provider_snapshot(client).await?;
    match operation {
        "list" => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ProviderListDocument {
                        schema: "haider.providers.v1",
                        revision,
                        providers,
                    })
                    .map_err(|error| error.to_string())?
                );
            } else {
                println!("PROVIDER\tTRUST\tENABLED\tDEFAULT MODEL");
                for provider in providers {
                    println!(
                        "{}\t{}\t{}\t{}",
                        provider.provider,
                        trust_name(provider.trust),
                        provider.enabled,
                        provider.default_model.as_deref().unwrap_or("-")
                    );
                }
            }
            Ok(())
        }
        "show" => {
            let provider = providers
                .into_iter()
                .find(|provider| provider.provider == name)
                .ok_or_else(|| format!("provider `{name}` was not found"))?;
            let lockdown = provider_lockdown(client)
                .ok_or_else(|| "daemon does not advertise provider_lockdown_v1".to_owned())?;
            let envelope = lockdown
                .status(Some(name))
                .await
                .map_err(|error| error.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ProviderShowDocument {
                        schema: "haider.provider.v1",
                        revision,
                        provider,
                        envelope,
                    })
                    .map_err(|error| error.to_string())?
                );
            } else {
                println!("provider: {}", provider.provider);
                println!("trust: {}", trust_name(provider.trust));
                println!("endpoint: {}", provider.endpoint.as_deref().unwrap_or("-"));
                println!(
                    "default model: {}",
                    provider.default_model.as_deref().unwrap_or("-")
                );
                if let Some(activation) = envelope.activation {
                    let (label, value) = match activation {
                        haider_rpc::LockdownActivationWire::Configured => {
                            ("enforcement", "configured lockdown")
                        }
                        haider_rpc::LockdownActivationWire::AutoHermetic => {
                            ("enforcement", "auto-hermetic")
                        }
                        haider_rpc::LockdownActivationWire::AutoHermeticEligible => {
                            ("policy", "auto-hermetic when active")
                        }
                        haider_rpc::LockdownActivationWire::Unknown => ("enforcement", "unknown"),
                        _ => ("enforcement", "unknown"),
                    };
                    println!("{label}: {value}");
                }
                if let Some(reason) = envelope.reason.as_deref() {
                    println!("reason: {reason}");
                }
                println!("allowed tools: {}", envelope.tools_allowed.join(", "));
                println!(
                    "quota: {} / {} bytes",
                    envelope.quota_used, envelope.quota_limit
                );
            }
            Ok(())
        }
        "set" => {
            let trust = trust.ok_or_else(|| "provider trust is missing".to_owned())?;
            let lockdown = provider_lockdown(client)
                .ok_or_else(|| "daemon does not advertise provider_lockdown_v1".to_owned())?;
            let (provider, _) = lockdown
                .set_trust(command_id("provider-trust"), name, trust, revision)
                .await
                .map_err(|error| error.to_string())?;
            println!("{}\t{}", provider.provider, trust_name(provider.trust));
            Ok(())
        }
        "remove" => match client
            .request(RequestBody::ProviderRemove {
                command_id: command_id("provider-remove"),
                provider: name,
                expected_revision: revision,
            })
            .await
            .map_err(|error| error.to_string())?
        {
            ResponseBody::ProviderRemove { provider, .. } => {
                println!("removed {provider}");
                Ok(())
            }
            ResponseBody::Error { code, message, .. } => Err(format!("{code}: {message}")),
            _ => Err("daemon answered provider.remove with an unexpected body".to_owned()),
        },
        _ => Err("unsupported provider operation".to_owned()),
    }
}

async fn provider_snapshot(
    client: &haider_client::RpcClient,
) -> Result<(Vec<ProviderSummaryWire>, u64), String> {
    match client
        .request(RequestBody::ProviderList { provider: None })
        .await
        .map_err(|error| error.to_string())?
    {
        ResponseBody::ProviderList {
            providers,
            revision,
            availability: Some(SnapshotAvailabilityWire::Available) | None,
        } => Ok((providers, revision)),
        ResponseBody::ProviderList {
            availability: Some(SnapshotAvailabilityWire::Unavailable { reason }),
            ..
        } => Err(reason),
        ResponseBody::Error { code, message, .. } => Err(format!("{code}: {message}")),
        _ => Err("daemon answered provider.list with an unexpected body".to_owned()),
    }
}

fn command_id(prefix: &str) -> CommandId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    CommandId::new(format!("{prefix}-{}-{nanos}", std::process::id()))
}

fn trust_name(trust: ProviderTrustWire) -> &'static str {
    match trust {
        ProviderTrustWire::Full => "full",
        ProviderTrustWire::Lockdown => "lockdown",
        ProviderTrustWire::Unknown => "unknown",
        _ => "unknown",
    }
}
