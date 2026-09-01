//! Deterministic same-session workload used by the daemon footprint guard.
//!
//! The driver deliberately launches `haiderd` through the ephemeral
//! readiness/liveness path: that is the path used by a spawned CLI daemon and
//! therefore the path whose settled footprint the guard must cover.

use haider_client::{ClientConfig, ConnectionUsage, connect, endpoint_path_for};
use haider_platform::{DaemonSpawn, spawn_daemon_with_readiness_and_liveness};
use haider_protocol::{
    DeliveryMode, EventPayload,
    session::{SessionInteractionModeV1, SessionPermissionOverridesV1},
    state::{RunState, SessionState},
};
use haider_rpc::{AttachMode, CommandId, RequestBody, ResponseBody, WireFrame};
use serde_json::json;
use std::{
    error::Error,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

const PROFILE_ID: &str = "memdaemon-footprint";

struct Args {
    daemon: PathBuf,
    root: PathBuf,
    turns: u32,
    settle_seconds: u64,
    checkpoint_acks: bool,
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn protocol_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut daemon = None;
    let mut root = None;
    let mut turns = 40_u32;
    let mut settle_seconds = 60_u64;
    let mut checkpoint_acks = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| invalid_input(format!("missing value for {argument}")))
        };
        match argument.as_str() {
            "--daemon" => daemon = Some(PathBuf::from(value()?)),
            "--root" => root = Some(PathBuf::from(value()?)),
            "--turns" => turns = value()?.parse()?,
            "--settle-seconds" => settle_seconds = value()?.parse()?,
            "--checkpoint-acks" => checkpoint_acks = true,
            _ => return Err(invalid_input(format!("unknown argument: {argument}")).into()),
        }
    }
    if turns == 0 {
        return Err(invalid_input("--turns must be positive").into());
    }
    Ok(Args {
        daemon: daemon.ok_or_else(|| invalid_input("--daemon is required"))?,
        root: root.ok_or_else(|| invalid_input("--root is required"))?,
        turns,
        settle_seconds,
        checkpoint_acks,
    })
}

fn emit(value: serde_json::Value) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

async fn wait_settle(seconds: u64, phase: &'static str) -> Result<(), Box<dyn Error>> {
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    emit(json!({"phase": phase}))
}

fn wait_checkpoint_ack(enabled: bool) -> Result<(), Box<dyn Error>> {
    if !enabled {
        return Ok(());
    }
    let mut acknowledgement = String::new();
    if io::stdin().read_line(&mut acknowledgement)? == 0 {
        return Err(protocol_error("checkpoint controller closed without acknowledgement").into());
    }
    Ok(())
}

async fn settle_checkpoint(
    seconds: u64,
    phase: &'static str,
    checkpoint_acks: bool,
) -> Result<(), Box<dyn Error>> {
    wait_settle(seconds, phase).await?;
    wait_checkpoint_ack(checkpoint_acks)
}

async fn connect_retry(endpoint: &Path) -> Result<haider_client::Connected, Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let config = ClientConfig {
            client_name: "memdaemon-workload".into(),
            client_instance_id: "memdaemon-workload".into(),
            connection_usage: ConnectionUsage::LongLived,
            ..ClientConfig::default()
        };
        match connect(endpoint, config).await {
            Ok(connected) => return Ok(connected),
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn drive_turns(endpoint: &Path, workspace: &Path, turns: u32) -> Result<(), Box<dyn Error>> {
    let connected = connect_retry(endpoint).await?;
    let client = connected.client;
    let mut events = client
        .take_events()
        .ok_or_else(|| protocol_error("event receiver was already taken"))?;

    let create = client
        .request(RequestBody::SessionCreateWithPermissionOverrides {
            command_id: CommandId::new("memdaemon-create"),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: Some(SessionPermissionOverridesV1 {
                allow_exec: true,
                ..SessionPermissionOverridesV1::default()
            }),
            cache_policy: None,
            interaction_mode: SessionInteractionModeV1::Autonomous,
            ssh_scope: None,
            account_alias: None,
            resolve_provider: false,
            resolve_model: false,
            effort: None,
            fast: None,
        })
        .await?;
    let session_id = match create {
        ResponseBody::SessionCreate { session_id, .. } => session_id,
        ResponseBody::Error { code, message, .. } => {
            return Err(protocol_error(format!("session.create failed: {code}: {message}")).into());
        }
        other => {
            return Err(protocol_error(format!(
                "session.create returned an unexpected response: {other:?}"
            ))
            .into());
        }
    };

    let attach = client
        .request(RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        })
        .await?;
    let (attachment_id, mut worker_generation) = match attach {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } => (attachment_id, attach_state.worker_generation),
        ResponseBody::Error { code, message, .. } => {
            return Err(protocol_error(format!("session.attach failed: {code}: {message}")).into());
        }
        other => {
            return Err(protocol_error(format!(
                "session.attach returned an unexpected response: {other:?}"
            ))
            .into());
        }
    };
    loop {
        match events.recv().await {
            Some(WireFrame::AttachCaughtUp {
                attachment_id: caught_up,
                ..
            }) if caught_up == attachment_id => break,
            Some(_) => {}
            None => return Err(protocol_error("disconnected before attach caught up").into()),
        }
    }

    for turn in 1..=turns {
        let response = client
            .request(RequestBody::TurnSubmit {
                command_id: CommandId::new(format!("memdaemon-turn-{turn}")),
                session_id: session_id.clone(),
                worker_generation,
                text: format!("run tiny process turn {turn}"),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            })
            .await?;
        let run_id = match response {
            ResponseBody::TurnSubmit {
                session_id: accepted_session,
                run_id,
                worker_generation: accepted_generation,
                ..
            } if accepted_session == session_id => {
                worker_generation = accepted_generation;
                run_id
            }
            ResponseBody::Error { code, message, .. } => {
                return Err(protocol_error(format!(
                    "turn.submit {turn} failed: {code}: {message}"
                ))
                .into());
            }
            other => {
                return Err(protocol_error(format!(
                    "turn.submit {turn} returned an unexpected response: {other:?}"
                ))
                .into());
            }
        };

        let terminal = tokio::time::timeout(Duration::from_secs(30), async {
            let mut terminal = None;
            let mut idle = false;
            while terminal.is_none() || !idle {
                let Some(frame) = events.recv().await else {
                    return Err(protocol_error(format!(
                        "disconnected while waiting for turn {turn}"
                    )));
                };
                let WireFrame::Event {
                    session_id: event_session,
                    envelope,
                    ..
                } = frame
                else {
                    continue;
                };
                if event_session != session_id {
                    continue;
                }
                let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                    continue;
                };
                match payload {
                    EventPayload::RunState(state)
                        if envelope.run_id.as_ref() == Some(&run_id) && state.is_terminal() =>
                    {
                        terminal = Some(state);
                    }
                    EventPayload::SessionState(SessionState::Idle { .. }) => idle = true,
                    _ => {}
                }
            }
            terminal.ok_or_else(|| protocol_error("terminal state was not observed"))
        })
        .await
        .map_err(|_| protocol_error(format!("turn {turn} timed out")))??;
        if terminal != RunState::Done {
            return Err(protocol_error(format!("turn {turn} ended in {terminal:?}")).into());
        }
        emit(json!({"phase": "turn", "turn": turn}))?;
    }
    let _ = client.close();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    let store = args.root.join("store");
    let runtime = args.root.join("runtime");
    let workspace = args.root.join("workspace");
    let log = args.root.join("haiderd.log");
    fs::create_dir_all(&store)?;
    fs::create_dir_all(&runtime)?;
    fs::create_dir_all(&workspace)?;

    let spec = DaemonSpawn {
        binary: &args.daemon,
        profile_id: PROFILE_ID,
        store_dir: &store,
        runtime_dir: &runtime,
        log_path: &log,
    };
    let (mut daemon, liveness) = spawn_daemon_with_readiness_and_liveness(spec)
        .map_err(|error| io::Error::other(format!("failed to spawn daemon: {error:?}")))?;
    let pid = daemon.child.id();
    let workload = async {
        tokio::time::timeout(Duration::from_secs(10), daemon.readiness.wait())
            .await
            .map_err(|_| protocol_error("daemon readiness timed out"))??;
        emit(json!({"phase": "ready", "pid": pid}))?;
        wait_checkpoint_ack(args.checkpoint_acks)?;
        settle_checkpoint(args.settle_seconds, "idle_settled", args.checkpoint_acks).await?;
        drive_turns(
            &endpoint_path_for(&runtime, PROFILE_ID),
            &workspace,
            args.turns,
        )
        .await?;
        emit(json!({"phase": "turns_complete", "turns": args.turns}))?;
        settle_checkpoint(
            args.settle_seconds,
            "post_turns_settled",
            args.checkpoint_acks,
        )
        .await
    }
    .await;

    let _ = liveness;
    let exited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if daemon.child.try_wait()?.is_some() {
                return Ok::<(), io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if exited.is_err() {
        daemon.child.kill()?;
        daemon.child.wait()?;
    }
    workload
}
