//! Per-session provider routing for automation and proxy-backed benchmarks.
use super::run::{EX_IOERR, EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};
use haider_client::{ClientConfig, EnsureOptions, ProfileEnv, resolve_profile};
use haider_protocol::ids::SessionId;
use haider_rpc::{
    AttachMode, Capability, CapabilitySet, ClientKind, CommandId, RequestBody, ResponseBody,
};
use std::process::ExitCode;

const USAGE: &str = "usage: haider session provider rebind --session <id> --provider <id> [--base-url <url>] [--account <name>]";

#[derive(Debug, PartialEq, Eq)]
struct Options {
    session: String,
    provider: String,
    base_url: Option<String>,
    account: Option<String>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut session = None;
    let mut provider = None;
    let mut base_url = None;
    let mut account = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let slot = match flag.as_str() {
            "--session" => &mut session,
            "--provider" => &mut provider,
            "--base-url" => &mut base_url,
            "--account" => &mut account,
            _ => return Err(format!("unknown flag {flag}")),
        };
        if slot.is_some() {
            return Err(format!("duplicate {flag}"));
        }
        let value = iter
            .next()
            .filter(|s| !s.trim().is_empty() && !s.starts_with("--"))
            .ok_or_else(|| format!("{flag} requires a value"))?;
        *slot = Some(value.clone());
    }
    Ok(Options {
        session: session.ok_or("--session is required")?,
        provider: provider.ok_or("--provider is required")?,
        base_url,
        account,
    })
}

pub(crate) async fn command(args: &[String]) -> ExitCode {
    if matches!(args, [flag] if flag == "--help" || flag == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let options = match parse(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("haider session provider rebind: {error}\n{USAGE}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let ensure = EnsureOptions {
        required_features: [haider_rpc::FEATURE_SESSION_PROVIDER_REBIND_V1.to_owned()].into(),
        client: ClientConfig {
            client_name: "haider-session-provider-rebind".into(),
            client_kind: ClientKind::Headless,
            capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
            ..Default::default()
        },
        ..Default::default()
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let result = execute(&ensured.client, options).await;
    let _ = ensured.client.close();
    match result {
        Ok(value) => {
            use std::io::Write;
            let mut output = std::io::stdout().lock();
            if serde_json::to_writer(&mut output, &value).is_err() || writeln!(output).is_err() {
                ExitCode::from(EX_IOERR)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("haider session provider rebind: {error}");
            ExitCode::from(EX_PROTOCOL)
        }
    }
}

async fn execute(
    client: &haider_client::RpcClient,
    options: Options,
) -> Result<serde_json::Value, String> {
    let session_id = SessionId::new(options.session);
    let response = client
        .request(RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        })
        .await
        .map_err(|e| e.to_string())?;
    let (attachment_id, worker_generation) = match response {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } => (attachment_id, attach_state.worker_generation),
        ResponseBody::Error { code, message, .. } => return Err(format!("{code}: {message}")),
        _ => return Err("unexpected session.attach response".into()),
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let result = client
        .request(RequestBody::SessionProviderRebind {
            command_id: CommandId::new(format!("provider-rebind-{}-{nonce}", std::process::id())),
            session_id: session_id.clone(),
            worker_generation,
            provider: options.provider,
            base_url: options.base_url,
            account: options.account,
        })
        .await
        .map_err(|e| e.to_string());
    let _ = client
        .request(RequestBody::SessionDetach { attachment_id })
        .await;
    match result? {
        ResponseBody::SessionProviderRebind {
            session_id: returned,
            provider,
            base_url,
            account,
            selected_seq,
            worker_generation,
        } if returned == session_id => Ok(serde_json::json!({
            "schema": "haider.session_provider_rebind.v1", "session_id": returned,
            "provider": provider, "base_url": base_url, "account": account,
            "selected_seq": selected_seq, "worker_generation": worker_generation,
        })),
        ResponseBody::Error { code, message, .. } => Err(format!("{code}: {message}")),
        _ => Err("unexpected session.provider.rebind response".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }
    #[test]
    fn provider_rebind_cli_requires_explicit_target_and_rejects_ambiguous_flags() {
        for invalid in [
            vec![],
            vec!["--session", "a"],
            vec!["--provider", "p"],
            vec!["--session", "a", "--provider", "p", "--account"],
            vec!["--session", "a", "--provider", "p", "--session", "b"],
        ] {
            assert!(parse(&args(&invalid)).is_err(), "{invalid:?}");
        }
        assert_eq!(
            parse(&args(&[
                "--session",
                "a",
                "--provider",
                "proxy",
                "--base-url",
                "http://127.0.0.1:8000",
                "--account",
                "bench"
            ])),
            Ok(Options {
                session: "a".into(),
                provider: "proxy".into(),
                base_url: Some("http://127.0.0.1:8000".into()),
                account: Some("bench".into())
            })
        );
    }
}
