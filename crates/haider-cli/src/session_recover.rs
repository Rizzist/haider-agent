//! Headless durable crash-window recovery door.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use haider_client::{
    ClientError, EnsureError, EnsureOptions, MenuAnswerRequest, ProfileEnv, resolve_profile,
};
use haider_protocol::ids::{MenuId, SessionId};
use haider_rpc::{
    AttachMode, AttachmentId, Capability, CapabilitySet, ClientKind, CommandId, ObserveMenuWire,
    ObserveRunStateWire, RequestBody, ResponseBody, SessionObserveDigest,
};
use serde::Serialize;

use super::run::{EX_BLOCKED, EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE};

const RECOVERY_SCHEMA: &str = "haider.session_recovery.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryAction {
    Probe,
    MarkDone,
    Retry,
    Abandon,
}

impl RecoveryAction {
    fn key(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::MarkDone => "mark_done",
            Self::Retry => "retry",
            Self::Abandon => "abandon",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RecoverOptions {
    json: bool,
    action: Option<RecoveryAction>,
}

#[derive(Serialize)]
struct RecoveryCardDocument {
    schema: &'static str,
    session_id: String,
    run_state: &'static str,
    menu_id: String,
    title: String,
    body: Vec<String>,
    options: Vec<RecoveryOptionDocument>,
    parked_since: Option<u64>,
}

#[derive(Serialize)]
struct RecoveryOptionDocument {
    key: String,
    label: String,
    detail: Option<String>,
}

#[derive(Serialize)]
struct RecoveryReceiptDocument {
    schema: &'static str,
    session_id: String,
    menu_id: String,
    chosen_option: &'static str,
    resolution_seq: u64,
    resulting_run_state: &'static str,
}

#[derive(Debug)]
enum RecoverError {
    Ensure(EnsureError),
    MissingFeatures(BTreeSet<String>),
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
    NoRecovery {
        state: &'static str,
    },
}

impl std::fmt::Display for RecoverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::MissingFeatures(features) => write!(
                formatter,
                "missing_feature: daemon does not advertise {}",
                features.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc {
                code,
                message,
                retryable,
            } => write!(
                formatter,
                "daemon rejected recovery ({code}, retryable={retryable}): {message}"
            ),
            Self::Protocol(message) => formatter.write_str(message),
            Self::NoRecovery { state } => {
                write!(
                    formatter,
                    "no crash window to reconcile — run_state is {state}"
                )
            }
        }
    }
}

pub(crate) async fn session_recover_command(session_id: &str, rest: &[String]) -> ExitCode {
    let options = match parse_options(rest) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!(
                "usage: haider session <session-id> recover [--json] [--probe|--mark-done|--retry|--abandon]"
            );
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("haider session recover: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider session recover: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let mut ensure = EnsureOptions::default();
    ensure.required_features.clear();
    ensure.client = haider_client::ClientConfig {
        client_name: "haider-session-recover".into(),
        client_kind: ClientKind::Headless,
        capabilities: if options.action.is_some() {
            CapabilitySet::from([Capability::View, Capability::Control])
        } else {
            CapabilitySet::from([Capability::View])
        },
        ..ensure.client
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(&RecoverError::Ensure(error)),
    };
    let required = BTreeSet::from([haider_rpc::FEATURE_EFFECT_RECOVERY_V1.to_owned()]);
    let missing = required
        .difference(&ensured.welcome.features)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        ensured.client.close();
        return failure(&RecoverError::MissingFeatures(missing));
    }
    let result = execute(&ensured.client, SessionId::new(session_id), options).await;
    ensured.client.close();
    match result {
        Ok(RecoveryOutput::Card(document)) if options.json => write_json(&document),
        Ok(RecoveryOutput::Card(document)) => write_card(&document),
        Ok(RecoveryOutput::Receipt(document)) if options.json => write_json(&document),
        Ok(RecoveryOutput::Receipt(document)) => write_receipt(&document),
        Err(error) => failure(&error),
    }
}

fn parse_options(rest: &[String]) -> Result<Option<RecoverOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(None);
    }
    let mut options = RecoverOptions::default();
    for flag in rest {
        match flag.as_str() {
            "--json" if !options.json => options.json = true,
            "--json" => return Err("duplicate --json flag".into()),
            "--probe" => set_action(&mut options, RecoveryAction::Probe, flag)?,
            "--mark-done" => set_action(&mut options, RecoveryAction::MarkDone, flag)?,
            "--retry" => set_action(&mut options, RecoveryAction::Retry, flag)?,
            "--abandon" => set_action(&mut options, RecoveryAction::Abandon, flag)?,
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    Ok(Some(options))
}

fn set_action(
    options: &mut RecoverOptions,
    action: RecoveryAction,
    flag: &str,
) -> Result<(), String> {
    if options.action.is_some() {
        return Err(format!(
            "exactly one recovery action is allowed; duplicate or combined `{flag}`"
        ));
    }
    options.action = Some(action);
    Ok(())
}

enum RecoveryOutput {
    Card(RecoveryCardDocument),
    Receipt(RecoveryReceiptDocument),
}

async fn execute(
    client: &haider_client::RpcClient,
    session_id: SessionId,
    options: RecoverOptions,
) -> Result<RecoveryOutput, RecoverError> {
    let digest = session_digest(client, session_id.clone()).await?;
    let menu = recovery_menu(&digest)?.clone();
    let menu_id = menu.menu_id.clone().ok_or(RecoverError::Protocol(
        "effect_recovery_v1 menu omitted menu_id",
    ))?;
    if options.action.is_none() {
        return Ok(RecoveryOutput::Card(card_document(digest, menu, menu_id)));
    }
    let action = options.action.ok_or(RecoverError::Protocol(
        "effect recovery action disappeared after parsing",
    ))?;
    let (option_index, _) = menu
        .options
        .iter()
        .enumerate()
        .find(|(_, option)| option.key == action.key())
        .ok_or(RecoverError::Protocol(
            "effect recovery menu omitted the requested durable option",
        ))?;
    let request_seq = menu.request_seq.ok_or(RecoverError::Protocol(
        "effect_recovery_v1 menu omitted request_seq",
    ))?;
    let opening_generation = menu.worker_generation.ok_or(RecoverError::Protocol(
        "effect_recovery_v1 menu omitted worker_generation",
    ))?;
    let attachment_id = control_attachment(client, session_id.clone(), digest.head_seq).await?;
    let answer = client
        .answer_menu(MenuAnswerRequest {
            command_id: CommandId::new(command_id("session-recover")),
            session_id: session_id.clone(),
            menu_id: menu_id.clone(),
            request_seq,
            worker_generation: opening_generation,
            option_key: action.key().into(),
            option_index: u32::try_from(option_index)
                .map_err(|_| RecoverError::Protocol("recovery option index exceeds u32"))?,
            input: None,
        })
        .await
        .map_err(RecoverError::Client);
    let resolution_seq = match answer {
        Ok(ResponseBody::MenuAnswer { resolution_seq }) => resolution_seq,
        Ok(ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        }) => {
            detach(client, attachment_id).await;
            return Err(RecoverError::Rpc {
                code,
                message,
                retryable,
            });
        }
        Ok(_) => {
            detach(client, attachment_id).await;
            return Err(RecoverError::Protocol(
                "menu.answer response method mismatch",
            ));
        }
        Err(error) => {
            detach(client, attachment_id).await;
            return Err(error);
        }
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let resulting = session_digest(client, session_id.clone()).await;
    detach(client, attachment_id).await;
    let resulting = resulting?;
    Ok(RecoveryOutput::Receipt(RecoveryReceiptDocument {
        schema: RECOVERY_SCHEMA,
        session_id: session_id.as_str().to_owned(),
        menu_id: menu_id.as_str().to_owned(),
        chosen_option: action.key(),
        resolution_seq,
        resulting_run_state: run_state_name(resulting.run_state),
    }))
}

/// The crash-window reconciliation menu is `MenuKind::Recovery` — wire kind
/// `"recovery"` ("effect_outcome_unknown reconciliation" in protocol
/// menu.rs). `"error_recovery"` is the SEPARATE provider/account/stream card
/// and never carries the probe/retry/mark-done/abandon options this door
/// answers; the daemon pins its side of this string in observe_menu tests.
const RECOVERY_MENU_WIRE_KIND: &str = "recovery";

fn recovery_menu(digest: &SessionObserveDigest) -> Result<&ObserveMenuWire, RecoverError> {
    digest
        .pending_menus
        .iter()
        .filter(|menu| menu.kind == RECOVERY_MENU_WIRE_KIND)
        .min_by_key(|menu| menu.request_seq.unwrap_or(u64::MAX))
        .ok_or(RecoverError::NoRecovery {
            state: run_state_name(digest.run_state),
        })
}

fn card_document(
    digest: SessionObserveDigest,
    menu: ObserveMenuWire,
    menu_id: MenuId,
) -> RecoveryCardDocument {
    RecoveryCardDocument {
        schema: RECOVERY_SCHEMA,
        session_id: digest.session_id.as_str().to_owned(),
        run_state: run_state_name(digest.run_state),
        menu_id: menu_id.as_str().to_owned(),
        title: menu.title,
        body: menu.body,
        options: menu
            .options
            .into_iter()
            .map(|option| RecoveryOptionDocument {
                key: option.key,
                label: option.label,
                detail: option.detail,
            })
            .collect(),
        parked_since: menu.opened_at_ms,
    }
}

async fn session_digest(
    client: &haider_client::RpcClient,
    session_id: SessionId,
) -> Result<SessionObserveDigest, RecoverError> {
    match client
        .request(RequestBody::SessionObserve {
            session_id,
            last_event_limit: 0,
            metadata_only: false,
        })
        .await
        .map_err(RecoverError::Client)?
    {
        ResponseBody::SessionObserve { digest } => Ok(digest),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(RecoverError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(RecoverError::Protocol(
            "session.observe response method mismatch",
        )),
    }
}

async fn control_attachment(
    client: &haider_client::RpcClient,
    session_id: SessionId,
    after_seq: u64,
) -> Result<AttachmentId, RecoverError> {
    match client
        .request(RequestBody::SessionAttach {
            session_id,
            after_seq,
            mode: AttachMode::Control,
            sealed_replay: false,
        })
        .await
        .map_err(RecoverError::Client)?
    {
        ResponseBody::SessionAttach { attachment_id, .. } => Ok(attachment_id),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(RecoverError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(RecoverError::Protocol(
            "session.attach response method mismatch",
        )),
    }
}

async fn detach(client: &haider_client::RpcClient, attachment_id: AttachmentId) {
    let _ = client
        .request(RequestBody::SessionDetach { attachment_id })
        .await;
}

fn run_state_name(state: ObserveRunStateWire) -> &'static str {
    match state {
        ObserveRunStateWire::Idle => "idle",
        ObserveRunStateWire::Running => "running",
        ObserveRunStateWire::EffectUnknown => "effect_unknown",
        ObserveRunStateWire::ParkedPermission => "parked_permission",
        ObserveRunStateWire::ParkedInput => "parked_input",
        ObserveRunStateWire::Errored => "errored",
        ObserveRunStateWire::Cancelled => "cancelled",
        ObserveRunStateWire::Unknown => "unknown",
        _ => "unknown",
    }
}

fn write_json(document: &impl Serialize) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, document)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider session recover: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn write_card(document: &RecoveryCardDocument) -> ExitCode {
    let mut text = format!("{}\n", document.title);
    for line in &document.body {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("options:\n");
    for option in &document.options {
        text.push_str(&format!("  [{}] {}", option.key, option.label));
        if let Some(detail) = &option.detail {
            text.push_str(&format!(" — {detail}"));
        }
        text.push('\n');
    }
    write_human(text)
}

fn write_receipt(document: &RecoveryReceiptDocument) -> ExitCode {
    write_human(format!(
        "menu {} · option {} · resolution_seq={} · run_state={}\n",
        document.menu_id,
        document.chosen_option,
        document.resolution_seq,
        document.resulting_run_state
    ))
}

fn write_human(text: String) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
    {
        eprintln!("haider session recover: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn failure(error: &RecoverError) -> ExitCode {
    eprintln!("haider session recover: {error}");
    let code = match error {
        RecoverError::Ensure(EnsureError::MissingFeatures { .. })
        | RecoverError::MissingFeatures(_)
        | RecoverError::Protocol(_) => EX_PROTOCOL,
        RecoverError::Ensure(_) => EX_UNAVAILABLE,
        RecoverError::Client(ClientError::Disconnected(_)) => EX_UNAVAILABLE,
        RecoverError::Client(_) => EX_SOFTWARE,
        RecoverError::NoRecovery { .. } => EX_BLOCKED,
        RecoverError::Rpc { code, .. }
            if matches!(code.as_str(), "capability_denied" | "input_required") =>
        {
            EX_BLOCKED
        }
        RecoverError::Rpc { .. } => EX_SOFTWARE,
    };
    ExitCode::from(code)
}

fn command_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// MUTATION CHECK: let `set_action` overwrite instead of refusing, or
    /// stop treating `--help` as show. Expected runtime failure: a combined
    /// `--probe --retry` is accepted, or the parser refuses one action.
    #[test]
    fn parse_accepts_one_action_and_refuses_combinations() {
        let one = parse_options(&["--probe".into()])
            .expect("valid")
            .expect("options");
        assert!(matches!(one.action, Some(RecoveryAction::Probe)));
        assert!(!one.json);

        let with_json = parse_options(&["--retry".into(), "--json".into()])
            .expect("valid")
            .expect("options");
        assert!(matches!(with_json.action, Some(RecoveryAction::Retry)));
        assert!(with_json.json);

        // Bare (no action) = show the card.
        let show = parse_options(&["--json".into()])
            .expect("valid")
            .expect("options");
        assert!(show.action.is_none());

        // Exactly-one law: two actions, or a repeat, refuse.
        assert!(parse_options(&["--probe".into(), "--retry".into()]).is_err());
        assert!(parse_options(&["--abandon".into(), "--abandon".into()]).is_err());
        assert!(parse_options(&["--json".into(), "--json".into()]).is_err());
        assert!(parse_options(&["--bogus".into()]).is_err());

        // --help/-h short-circuits to "no options" (show help).
        assert!(parse_options(&["--help".into()]).expect("ok").is_none());
    }

    fn digest_with_menus(menus: Vec<ObserveMenuWire>) -> SessionObserveDigest {
        SessionObserveDigest {
            session_id: SessionId::new("session-recover-test"),
            head_seq: 9,
            worker_generation: 1,
            metadata: None,
            title: "parked".into(),
            run_state: ObserveRunStateWire::EffectUnknown,
            run_id: None,
            active_branch_id: None,
            branches: Vec::new(),
            main_head_node_id: None,
            main_head_seq: 9,
            latest_context_footprint: None,
            pending_menus: menus,
            subagents: Vec::new(),
            updated_at_ms: 0,
            last_event_kinds: Vec::new(),
            turn_count: None,
            agent_metrics: None,
            needs_input: None,
        }
    }

    fn wire_menu(kind: &str, seq: u64) -> ObserveMenuWire {
        ObserveMenuWire {
            kind: kind.into(),
            title: format!("{kind} menu"),
            menu_id: Some(MenuId::new(format!("menu-{kind}-{seq}"))),
            request_seq: Some(seq),
            worker_generation: Some(1),
            opened_at_ms: Some(1),
            body: vec!["evidence".into()],
            options: Vec::new(),
            permission_description: None,
            presentation: None,
        }
    }

    /// MUTATION CHECK: flipping `RECOVERY_MENU_WIRE_KIND` back to
    /// `"error_recovery"` (the v0.0.935 shipped defect) must fail both
    /// halves: the door must select the crash-window `"recovery"` menu and
    /// must NOT treat the provider/account `"error_recovery"` card as one.
    #[test]
    fn recovery_menu_selects_the_crash_window_kind_only() {
        let digest = digest_with_menus(vec![
            wire_menu("error_recovery", 3),
            wire_menu("recovery", 7),
        ]);
        let menu = recovery_menu(&digest).expect("crash-window menu");
        assert_eq!(menu.kind, "recovery");
        assert_eq!(menu.request_seq, Some(7));

        let digest = digest_with_menus(vec![wire_menu("error_recovery", 3)]);
        assert!(matches!(
            recovery_menu(&digest),
            Err(RecoverError::NoRecovery { .. })
        ));
    }
}
