//! Scriptable SSH profile administration.

use std::io::{self, Read};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use haider_client::{EnsureOptions, ProfileEnv, ensure_daemon, resolve_profile, ssh_profiles};
use haider_rpc::{
    RequestBody, ResponseBody, SecretWire, SshAuthInputWire, SshProfileInputWire,
    SshProfileUpdateWire, StagePurpose,
};
use serde::Serialize;
use zeroize::Zeroizing;

use super::run::{
    EX_BLOCKED, EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_TIMEOUT, EX_UNAVAILABLE, EX_USAGE,
};

pub(crate) const SSH_LIST_SCHEMA: &str = "haider.ssh.list.v1";
pub(crate) const SSH_SHOW_SCHEMA: &str = "haider.ssh.profile.v1";

#[derive(Debug)]
enum SshCommand {
    Add(SshProfileInputWire),
    List {
        json: bool,
    },
    Show {
        name: String,
    },
    Edit {
        name: String,
        changes: SshProfileUpdateWire,
    },
    Remove {
        name: String,
    },
    Test {
        name: String,
    },
    Shell {
        name: String,
        command: Option<String>,
    },
}

#[derive(Serialize)]
struct SshListDocument<'a> {
    schema: &'static str,
    profiles: &'a [haider_rpc::SshProfileWire],
}

#[derive(Serialize)]
struct SshShowDocument<'a> {
    schema: &'static str,
    profile: &'a haider_rpc::SshProfileWire,
}

pub(crate) async fn ssh_command(rest: &[String]) -> ExitCode {
    let command = match parse(rest) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("haider ssh: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider ssh: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let mut options = EnsureOptions::default();
    options
        .required_features
        .insert(haider_rpc::FEATURE_SSH_PROFILES_V1.to_owned());
    let ensured = match ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("haider ssh: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let result = execute(&ensured.client, command).await;
    let _ = ensured.client.close();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliSshError::Io(error)) => {
            eprintln!("haider ssh: {error}");
            ExitCode::from(EX_IOERR)
        }
        Err(CliSshError::Refused { code, message }) => {
            eprintln!("haider ssh: {code}: {message}");
            ExitCode::from(exit_code_for_refusal(&code))
        }
        Err(error) => {
            eprintln!("haider ssh: {error}");
            ExitCode::from(EX_PROTOCOL)
        }
    }
}

#[derive(Debug)]
enum CliSshError {
    Client(haider_client::client::ClientError),
    Io(io::Error),
    Refused { code: String, message: String },
    FeatureAbsent,
    Protocol,
    Interactive(String),
}

impl std::fmt::Display for CliSshError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Refused { code, message } => write!(formatter, "{code}: {message}"),
            Self::FeatureAbsent => formatter.write_str("daemon does not advertise ssh_profiles_v1"),
            Self::Protocol => formatter.write_str("daemon answered with an unexpected body"),
            Self::Interactive(message) => formatter.write_str(message),
        }
    }
}

impl From<haider_client::client::ClientError> for CliSshError {
    fn from(error: haider_client::client::ClientError) -> Self {
        Self::Client(error)
    }
}

impl From<haider_client::SshProfilesClientError> for CliSshError {
    fn from(error: haider_client::SshProfilesClientError) -> Self {
        match error {
            haider_client::SshProfilesClientError::Client(error) => Self::Client(error),
            haider_client::SshProfilesClientError::Refused { code, message, .. } => {
                Self::Refused { code, message }
            }
            haider_client::SshProfilesClientError::UnexpectedBody => Self::Protocol,
        }
    }
}

fn exit_code_for_refusal(code: &str) -> u8 {
    match code {
        "ssh_timeout" => EX_TIMEOUT,
        "ssh_profile_out_of_scope" | "ssh_host_key_changed" | "ssh_authentication_failed" => {
            EX_BLOCKED
        }
        "ssh_agent_unavailable" | "ssh_connection_failed" | "ssh_vault_error" => EX_UNAVAILABLE,
        "ssh_channel_closed" | "ssh_channel_quota" => EX_BLOCKED,
        "ssh_command_failed" | "ssh_output_limit" => EX_SOFTWARE,
        "ssh_profile_not_found"
        | "ssh_profile_exists"
        | "ssh_profile_invalid_name"
        | "ssh_profile_invalid"
        | "ssh_key_invalid" => EX_USAGE,
        _ => EX_PROTOCOL,
    }
}

async fn execute(
    client: &haider_client::RpcClient,
    command: SshCommand,
) -> Result<(), CliSshError> {
    let profiles = ssh_profiles(client).ok_or(CliSshError::FeatureAbsent)?;
    match command {
        SshCommand::List { json } => {
            let items = profiles.list(None).await?;
            if json {
                print_json(&SshListDocument {
                    schema: SSH_LIST_SCHEMA,
                    profiles: &items,
                })?;
            } else {
                print_profiles(&items);
            }
        }
        SshCommand::Show { name } => {
            let item = profiles
                .list(None)
                .await?
                .into_iter()
                .find(|item| item.name == name)
                .ok_or_else(|| CliSshError::Refused {
                    code: "ssh_profile_not_found".into(),
                    message: format!("SSH profile `{name}` was not found"),
                })?;
            print_json(&SshShowDocument {
                schema: SSH_SHOW_SCHEMA,
                profile: &item,
            })?;
        }
        SshCommand::Add(mut input) => {
            stage_auth(client, &mut input.auth).await?;
            let profile = profiles.add(input).await?;
            println!("{}", profile.name);
        }
        SshCommand::Edit { name, mut changes } => {
            if let Some(auth) = changes.auth.as_mut() {
                stage_auth(client, auth).await?;
            }
            let profile = profiles.update(name, changes).await?;
            println!("{}", profile.name);
        }
        SshCommand::Remove { name } => println!("{}", profiles.remove(name).await?),
        SshCommand::Test { name } => {
            let result = profiles.test(name, None).await?;
            println!(
                "{} connected host_key_pinned={}",
                result.profile.name, result.host_key_pinned
            );
        }
        SshCommand::Shell {
            name,
            command: None,
        } => {
            let exit_code = haider_tui::ssh_terminal::run_ssh_terminal(client, &name)
                .await
                .map_err(|error| match error {
                    haider_tui::ssh_terminal::SshTerminalError::Client(error) => error.into(),
                    haider_tui::ssh_terminal::SshTerminalError::Io(error) => CliSshError::Io(error),
                    other => CliSshError::Interactive(other.to_string()),
                })?;
            if let Some(code) = exit_code
                && code != 0
            {
                return Err(CliSshError::Refused {
                    code: "ssh_command_failed".into(),
                    message: format!("remote shell exited with {code}"),
                });
            }
        }
        SshCommand::Shell {
            name,
            command: Some(command),
        } => {
            let result = profiles.shell(name, command, None, None).await?;
            print!("{}", result.stdout);
            eprint!("{}", result.stderr);
            if result.timed_out
                || result.stdout_truncated
                || result.stderr_truncated
                || result.exit_code.is_some_and(|code| code != 0)
            {
                return Err(CliSshError::Refused {
                    code: if result.timed_out {
                        "ssh_timeout"
                    } else if result.stdout_truncated || result.stderr_truncated {
                        "ssh_output_limit"
                    } else {
                        "ssh_command_failed"
                    }
                    .into(),
                    message: if result.timed_out {
                        "remote command timed out".into()
                    } else if result.stdout_truncated || result.stderr_truncated {
                        "remote command reached the shell output cap".into()
                    } else {
                        result.exit_code.map_or_else(
                            || "remote command failed".into(),
                            |code| format!("remote command exited with {code}"),
                        )
                    },
                });
            }
        }
    }
    Ok(())
}

async fn stage_auth(
    client: &haider_client::RpcClient,
    auth: &mut SshAuthInputWire,
) -> Result<(), CliSshError> {
    let purpose = match auth {
        SshAuthInputWire::KeyFile {
            passphrase_vault_reference: Some(vault_reference),
            ..
        } if vault_reference == "-" => StagePurpose::SshPassword,
        SshAuthInputWire::KeyMaterial { vault_reference } if vault_reference == "-" => {
            StagePurpose::SshKeyMaterial
        }
        SshAuthInputWire::Password { vault_reference } if vault_reference == "-" => {
            StagePurpose::SshPassword
        }
        _ => return Ok(()),
    };
    if matches!(auth, SshAuthInputWire::KeyMaterial { .. }) {
        eprintln!(
            "haider ssh: notice: pasted key material has the same FileVault protection as API keys (Windows relies on the enclosing profile ACL); Haider does not cryptographically encrypt it"
        );
    }
    let mut value = Zeroizing::new(String::new());
    io::stdin()
        .read_to_string(&mut value)
        .map_err(CliSshError::Io)?;
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    if value.is_empty() {
        return Err(CliSshError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stdin secret is empty",
        )));
    }
    let body = client
        .request(RequestBody::VaultStage {
            stage_id: coordinate("ssh-stage"),
            purpose,
            secret: SecretWire::new(std::mem::take(&mut *value)),
        })
        .await?;
    let reference = match body {
        ResponseBody::VaultStage {
            vault_reference, ..
        } => vault_reference,
        ResponseBody::Error { code, message, .. } => {
            return Err(CliSshError::Refused { code, message });
        }
        _ => return Err(CliSshError::Protocol),
    };
    match auth {
        SshAuthInputWire::KeyFile {
            passphrase_vault_reference,
            ..
        } => *passphrase_vault_reference = Some(reference),
        SshAuthInputWire::KeyMaterial { vault_reference }
        | SshAuthInputWire::Password { vault_reference } => *vault_reference = reference,
        _ => {}
    }
    Ok(())
}

fn parse(rest: &[String]) -> Result<SshCommand, String> {
    match rest {
        [command] if command == "list" => Ok(SshCommand::List { json: false }),
        [command, flag] if command == "list" && flag == "--json" => Ok(SshCommand::List { json: true }),
        [command, name] if command == "show" => Ok(SshCommand::Show { name: name.clone() }),
        [command, name] if command == "rm" => Ok(SshCommand::Remove { name: name.clone() }),
        [command, name] if command == "test" => Ok(SshCommand::Test { name: name.clone() }),
        [command, name, separator, tail @ ..] if command == "shell" && separator == "--" && !tail.is_empty() => {
            Ok(SshCommand::Shell { name: name.clone(), command: Some(tail.join(" ")) })
        }
        [command, name] if command == "shell" => Ok(SshCommand::Shell {
            name: name.clone(),
            command: None,
        }),
        [command, name, flags @ ..] if command == "add" => parse_add(name, flags),
        [command, name, flags @ ..] if command == "edit" => parse_edit(name, flags),
        _ => Err("usage: haider ssh add <name> --host H --user U [--port P] [--key PATH|--agent|--password-stdin|--key-stdin] [--description TEXT] [--cwd PATH] | list [--json] | show <name> | edit <name> ... | rm <name> | test <name> | shell <name> [-- CMD...]".into()),
    }
}

fn parse_add(name: &str, flags: &[String]) -> Result<SshCommand, String> {
    let parsed = parse_fields(flags, true)?;
    Ok(SshCommand::Add(SshProfileInputWire {
        name: name.to_owned(),
        description: parsed.description.flatten(),
        host: parsed.host.ok_or("ssh add requires --host")?,
        port: parsed.port.unwrap_or(22),
        user: parsed.user.ok_or("ssh add requires --user")?,
        auth: parsed
            .auth
            .ok_or("ssh add requires exactly one authentication option")?,
        default_cwd: parsed.cwd.flatten(),
    }))
}

fn parse_edit(name: &str, flags: &[String]) -> Result<SshCommand, String> {
    let parsed = parse_fields(flags, false)?;
    Ok(SshCommand::Edit {
        name: name.to_owned(),
        changes: SshProfileUpdateWire {
            description: parsed.description,
            host: parsed.host,
            port: parsed.port,
            user: parsed.user,
            auth: parsed.auth,
            default_cwd: parsed.cwd,
        },
    })
}

#[derive(Default)]
struct ParsedFields {
    description: Option<Option<String>>,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    auth: Option<SshAuthInputWire>,
    cwd: Option<Option<String>>,
}

fn parse_fields(flags: &[String], require_auth: bool) -> Result<ParsedFields, String> {
    let mut parsed = ParsedFields::default();
    let mut index = 0;
    while index < flags.len() {
        let flag = flags[index].as_str();
        let value = |index: &mut usize| -> Result<String, String> {
            *index += 1;
            flags
                .get(*index)
                .filter(|value| !value.starts_with("--"))
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag {
            "--host" if parsed.host.is_none() => parsed.host = Some(value(&mut index)?),
            "--user" if parsed.user.is_none() => parsed.user = Some(value(&mut index)?),
            "--port" if parsed.port.is_none() => {
                parsed.port = Some(
                    value(&mut index)?
                        .parse()
                        .map_err(|_| "--port requires 1..=65535")?,
                )
            }
            "--description" if parsed.description.is_none() => {
                parsed.description = Some(Some(value(&mut index)?))
            }
            "--cwd" if parsed.cwd.is_none() => parsed.cwd = Some(Some(value(&mut index)?)),
            "--key" if parsed.auth.is_none() => {
                parsed.auth = Some(SshAuthInputWire::KeyFile {
                    path: value(&mut index)?,
                    passphrase_vault_reference: None,
                })
            }
            "--key-passphrase-stdin" => match parsed.auth.as_mut() {
                Some(SshAuthInputWire::KeyFile {
                    passphrase_vault_reference,
                    ..
                }) if passphrase_vault_reference.is_none() => {
                    *passphrase_vault_reference = Some("-".into());
                }
                _ => return Err("--key-passphrase-stdin requires one preceding --key".into()),
            },
            "--agent" if parsed.auth.is_none() => parsed.auth = Some(SshAuthInputWire::Agent),
            "--password-stdin" if parsed.auth.is_none() => {
                parsed.auth = Some(SshAuthInputWire::Password {
                    vault_reference: "-".into(),
                })
            }
            "--key-stdin" if parsed.auth.is_none() => {
                parsed.auth = Some(SshAuthInputWire::KeyMaterial {
                    vault_reference: "-".into(),
                })
            }
            "--clear-description" if parsed.description.is_none() => {
                parsed.description = Some(None)
            }
            "--clear-cwd" if parsed.cwd.is_none() => parsed.cwd = Some(None),
            _ => return Err(format!("unknown or duplicate SSH option `{flag}`")),
        }
        index += 1;
    }
    if require_auth && parsed.auth.is_none() {
        return Err("ssh add requires exactly one authentication option".into());
    }
    Ok(parsed)
}

fn print_json(value: &impl Serialize) -> Result<(), CliSshError> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(|_| CliSshError::Protocol)?
    );
    Ok(())
}

fn print_profiles(profiles: &[haider_rpc::SshProfileWire]) {
    println!("NAME\tHOST\tUSER\tPORT\tLAST USED\tMULTIPLEXING");
    for profile in profiles {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            profile.name,
            profile.host,
            profile.user,
            profile.port,
            profile
                .last_used_ms
                .map_or_else(|| "-".into(), |value| value.to_string()),
            profile.multiplexing
        );
    }
}

fn coordinate(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}-{now}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
#[path = "ssh_tests.rs"]
mod tests;
