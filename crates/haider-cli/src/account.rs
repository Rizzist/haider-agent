//! Scriptable account inventory and guarded durable removal.

use std::collections::BTreeSet;
use std::future::Future;
use std::io::{self, Write};
use std::process::ExitCode;

use haider_client::{ClientError, EnsureError, EnsureOptions, ProfileEnv, resolve_profile};
use haider_protocol::credential::{AuthMethod, CredentialDescriptor};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, CommandId, RequestBody, ResponseBody,
    SnapshotAvailabilityWire,
};
use serde::Serialize;

use super::run::{EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE};

const ACCOUNTS_SCHEMA: &str = "haider.accounts.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountCommand {
    List { json: bool },
    Remove { alias: String, confirm: bool },
}

#[derive(Serialize)]
struct AccountsDocument {
    schema: &'static str,
    accounts: Vec<AccountView>,
}

/// Deliberately narrower than `CredentialDescriptor`: adding a public field to
/// the RPC descriptor cannot silently widen either CLI output format.
#[derive(Serialize)]
struct AccountView {
    alias: String,
    provider: String,
    auth_kind: &'static str,
    /// `account.list` does not publish account creation time. `null` is an
    /// honest machine-readable value; the human projection renders `unknown`.
    created: Option<u64>,
}

#[derive(Debug)]
enum AccountError {
    Ensure(EnsureError),
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
    SnapshotUnavailable(String),
    MissingAlias(String),
}

trait AccountClient {
    fn request(
        &self,
        request: RequestBody,
    ) -> impl Future<Output = Result<ResponseBody, ClientError>> + Send;
}

impl AccountClient for haider_client::RpcClient {
    fn request(
        &self,
        request: RequestBody,
    ) -> impl Future<Output = Result<ResponseBody, ClientError>> + Send {
        haider_client::RpcClient::request(self, request)
    }
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc {
                code,
                message,
                retryable,
            } => write!(
                formatter,
                "daemon rejected account command ({code}, retryable={retryable}): {message}"
            ),
            Self::Protocol(message) => write!(formatter, "{message}"),
            Self::SnapshotUnavailable(reason) => {
                write!(formatter, "account inventory is unavailable: {reason}")
            }
            Self::MissingAlias(alias) => {
                write!(formatter, "account alias `{alias}` does not exist")
            }
        }
    }
}

pub(crate) fn parse_account_command(rest: &[String]) -> Result<AccountCommand, String> {
    match rest {
        [command] if command == "list" => Ok(AccountCommand::List { json: false }),
        [command, flag] if command == "list" && flag == "--json" => {
            Ok(AccountCommand::List { json: true })
        }
        [command, alias] if command == "remove" && !alias.is_empty() && !alias.starts_with('-') => {
            Ok(AccountCommand::Remove {
                alias: alias.clone(),
                confirm: false,
            })
        }
        [command, alias, flag]
            if command == "remove"
                && !alias.is_empty()
                && !alias.starts_with('-')
                && flag == "--confirm" =>
        {
            Ok(AccountCommand::Remove {
                alias: alias.clone(),
                confirm: true,
            })
        }
        [] => Err("expected list [--json] or remove <alias> --confirm".into()),
        _ => Err("expected list [--json] or remove <alias> --confirm".into()),
    }
}

pub(crate) async fn account_command(rest: &[String]) -> ExitCode {
    let command = match parse_account_command(rest) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("haider account: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    // This gate intentionally runs before profile resolution, daemon startup,
    // or any RPC. An unconfirmed invocation cannot reach a mutation surface.
    if let AccountCommand::Remove {
        alias,
        confirm: false,
    } = &command
    {
        eprintln!("haider account: would remove account `{alias}`; pass --confirm to proceed");
        return ExitCode::from(EX_USAGE);
    }

    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider account: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let capabilities = match &command {
        AccountCommand::List { .. } => CapabilitySet::from([Capability::View]),
        AccountCommand::Remove { .. } => {
            CapabilitySet::from([Capability::View, Capability::Control])
        }
    };
    let ensure = EnsureOptions {
        required_features: BTreeSet::from([haider_rpc::FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned()]),
        client: haider_client::ClientConfig {
            client_name: "haider-account".into(),
            client_kind: ClientKind::Headless,
            capabilities,
            ..haider_client::ClientConfig::default()
        },
        ..EnsureOptions::default()
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(&AccountError::Ensure(error)),
    };
    let result = execute(&ensured.client, command).await;
    ensured.client.close();
    match result {
        Ok(output) => output,
        Err(error) => failure(&error),
    }
}

async fn execute(
    client: &impl AccountClient,
    command: AccountCommand,
) -> Result<ExitCode, AccountError> {
    match command {
        AccountCommand::List { json } => {
            let (descriptors, _) = account_snapshot(client).await?;
            let document = AccountsDocument {
                schema: ACCOUNTS_SCHEMA,
                accounts: descriptors.into_iter().map(account_view).collect(),
            };
            Ok(if json {
                write_json(&document)
            } else {
                write_human(&document)
            })
        }
        AccountCommand::Remove {
            alias,
            confirm: true,
        } => {
            // Mirror the TUI's revision-fenced account.remove path: snapshot,
            // resolve the exact global alias, then mutate at that revision.
            let (descriptors, revision) = account_snapshot(client).await?;
            let revision = revision.ok_or(AccountError::Protocol(
                "account.list omitted the revision required for removal",
            ))?;
            if !descriptors
                .iter()
                .any(|descriptor| descriptor.alias.as_str() == alias)
            {
                return Err(AccountError::MissingAlias(alias));
            }
            match client
                .request(RequestBody::AccountRemove {
                    command_id: CommandId::new(command_id()),
                    alias: alias.clone(),
                    expected_revision: Some(revision),
                })
                .await
                .map_err(AccountError::Client)?
            {
                ResponseBody::AccountRemove { removed_alias, .. }
                    if removed_alias.as_str() == alias =>
                {
                    println!("removed account `{removed_alias}`");
                    Ok(ExitCode::SUCCESS)
                }
                ResponseBody::AccountRemove { .. } => Err(AccountError::Protocol(
                    "account.remove response named a different alias",
                )),
                ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    ..
                } => Err(AccountError::Rpc {
                    code,
                    message,
                    retryable,
                }),
                _ => Err(AccountError::Protocol(
                    "account.remove response method mismatch",
                )),
            }
        }
        AccountCommand::Remove { confirm: false, .. } => {
            unreachable!("unconfirmed removal is gated before daemon startup")
        }
    }
}

async fn account_snapshot(
    client: &impl AccountClient,
) -> Result<(Vec<CredentialDescriptor>, Option<u64>), AccountError> {
    match client
        .request(RequestBody::AccountList { provider: None })
        .await
        .map_err(AccountError::Client)?
    {
        ResponseBody::AccountList {
            descriptors,
            revision,
            availability,
            ..
        } => match availability {
            Some(SnapshotAvailabilityWire::Unavailable { reason }) => {
                Err(AccountError::SnapshotUnavailable(reason))
            }
            Some(SnapshotAvailabilityWire::Unknown) => Err(AccountError::SnapshotUnavailable(
                "daemon returned an unknown availability state".into(),
            )),
            _ => Ok((descriptors, revision)),
        },
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(AccountError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(AccountError::Protocol(
            "account.list response method mismatch",
        )),
    }
}

fn account_view(descriptor: CredentialDescriptor) -> AccountView {
    AccountView {
        alias: descriptor.alias.as_str().to_owned(),
        provider: descriptor.provider,
        auth_kind: match descriptor.auth_method {
            AuthMethod::ApiKey => "api_key",
            AuthMethod::OAuth => "oauth",
        },
        created: None,
    }
}

fn write_json(document: &AccountsDocument) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, document)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider account: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn write_human(document: &AccountsDocument) -> ExitCode {
    let mut text = String::new();
    for account in &document.accounts {
        text.push_str(&format!(
            "{}  provider={}  auth_kind={}  created={}\n",
            account.alias,
            account.provider,
            account.auth_kind,
            account
                .created
                .map_or_else(|| "unknown".to_owned(), |created| created.to_string())
        ));
    }
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
    {
        eprintln!("haider account: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn command_id() -> String {
    format!(
        "account-remove-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    )
}

fn failure(error: &AccountError) -> ExitCode {
    eprintln!("haider account: {error}");
    let code = match error {
        AccountError::Ensure(
            EnsureError::ProtocolMismatch(_)
            | EnsureError::MissingFeatures { .. }
            | EnsureError::ProfileMismatch { .. },
        )
        | AccountError::Protocol(_) => EX_PROTOCOL,
        AccountError::Ensure(_) | AccountError::SnapshotUnavailable(_) => EX_UNAVAILABLE,
        AccountError::Client(ClientError::Disconnected(_)) => EX_IOERR,
        AccountError::Client(ClientError::Encode(_))
        | AccountError::Rpc { .. }
        | AccountError::MissingAlias(_) => EX_SOFTWARE,
    };
    ExitCode::from(code)
}

#[cfg(test)]
#[allow(clippy::expect_used)] // fake-client assertions may panic on a broken fixture
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use haider_protocol::credential::CredentialStatus;
    use haider_protocol::ids::CredentialAlias;

    use super::*;

    struct FakeAccountClient {
        requests: Mutex<Vec<RequestBody>>,
        responses: Mutex<VecDeque<ResponseBody>>,
    }

    impl FakeAccountClient {
        fn new(responses: impl IntoIterator<Item = ResponseBody>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
            }
        }

        fn requests(&self) -> Vec<RequestBody> {
            self.requests.lock().expect("request lock").clone()
        }
    }

    impl AccountClient for FakeAccountClient {
        fn request(
            &self,
            request: RequestBody,
        ) -> impl Future<Output = Result<ResponseBody, ClientError>> + Send {
            self.requests.lock().expect("request lock").push(request);
            let response = self
                .responses
                .lock()
                .expect("response lock")
                .pop_front()
                .expect("fake response");
            std::future::ready(Ok(response))
        }
    }

    fn descriptor(alias: &str) -> CredentialDescriptor {
        CredentialDescriptor {
            alias: CredentialAlias::new(alias),
            provider: "anthropic".into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
        }
    }

    fn list_response(alias: &str, revision: Option<u64>) -> ResponseBody {
        ResponseBody::AccountList {
            descriptors: vec![descriptor(alias)],
            revision,
            provider_active: Vec::new(),
            provider_defaults: Vec::new(),
            availability: Some(SnapshotAvailabilityWire::Available),
        }
    }

    /// MUTATION CHECK: remove the account.list preflight or stop propagating
    /// its revision. Expected RUNTIME failure: request count/order or the
    /// exact `expected_revision` assertion changes.
    #[tokio::test]
    async fn confirmed_remove_is_list_first_and_revision_fenced() {
        let client = FakeAccountClient::new([
            list_response("probe", Some(41)),
            ResponseBody::AccountRemove {
                removed_alias: CredentialAlias::new("probe"),
                replacement_active_alias: None,
                revision: 42,
            },
        ]);
        let result = execute(
            &client,
            AccountCommand::Remove {
                alias: "probe".into(),
                confirm: true,
            },
        )
        .await;
        assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
        let requests = client.requests();
        assert_eq!(requests.len(), 2);
        assert!(matches!(
            &requests[0],
            RequestBody::AccountList { provider: None }
        ));
        assert!(matches!(
            &requests[1],
            RequestBody::AccountRemove {
                alias,
                expected_revision: Some(41),
                ..
            } if alias == "probe"
        ));
    }

    #[tokio::test]
    async fn confirmed_remove_refuses_to_mutate_without_a_snapshot_revision() {
        let client = FakeAccountClient::new([list_response("probe", None)]);
        let result = execute(
            &client,
            AccountCommand::Remove {
                alias: "probe".into(),
                confirm: true,
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(AccountError::Protocol(
                "account.list omitted the revision required for removal"
            ))
        ));
        assert!(matches!(
            client.requests().as_slice(),
            [RequestBody::AccountList { provider: None }]
        ));
    }
}
