#![allow(clippy::expect_used)]

#[cfg(unix)]
use super::hook_command;
use super::{
    CapturedBytes, DecisionState, EngineState, HOOK_DRAIN_PAGE_MAX_REQUESTS,
    HOOK_ENGINE_SNAPSHOT_FILE, HOOK_ENGINE_SNAPSHOT_VERSION, HookDefinition, HookEngine,
    HookEngineSnapshot, HookEngineSnapshotFile, HookKind, HookMatcher, HookService, HookSource,
    HookStartupHydrator, HookTrustPolicy, MatchEvent, SnapshotSchedule, classify, discover,
    encode_hook_snapshot_file, hook_digest, make_output, next_subscriber_backoff,
    prepare_hook_input, prune_terminal_run_trust, reduce_durable_state, run_command,
};
#[cfg(unix)]
use super::{HOOK_LEADER_EXIT_POLL_MAX, poll_hook_leader_exit};
use crate::runtime::finish_hook_hydration_for_test;
use crate::session_hub::{SessionHub, SessionHubConfig};
#[cfg(windows)]
use base64::Engine as _;
use haider_core::{SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand};
use haider_platform::{process_id, process_leader_exited};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::effect::{AuthorizationVerdict, EffectClass, EffectIntent, EffectPhase};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::hook::{
    HookEventPayload, HookInput, HookRuntimeKind, HookSubscription, HookSubscriptionState,
};
use haider_protocol::ids::{ArtifactRef, DeviceId, EffectId, EventId, MenuId, RunId, SessionId};
use haider_protocol::menu::{AnswerVia, DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_protocol::state::RunState;
use haider_protocol::tool::AttachmentBlock;
use haider_rpc::{CommandId, HookTrustStateWire};
use haider_tools::{EffectBroker, JournalSink, PermissionPolicy, ProcessExec, ToolResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn snapshot_schedule_restarts_idle_window_after_a_long_clean_gap() {
    let start = std::time::Instant::now();
    let mut schedule = SnapshotSchedule::new(start);
    let commit = start + Duration::from_secs(10);
    schedule.note_commit(commit);
    assert_eq!(
        schedule.deadline(),
        Some(commit + super::HOOK_SNAPSHOT_IDLE_DELAY)
    );
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn leader_exit_poll_backoff_never_exceeds_detection_cap() {
    let probes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&probes);
    let mut remaining = 9_u8;
    poll_hook_leader_exit(move || {
        observed
            .lock()
            .expect("leader-exit probe log")
            .push(tokio::time::Instant::now());
        if remaining == 0 {
            Ok(true)
        } else {
            remaining = remaining.saturating_sub(1);
            Ok(false)
        }
    })
    .await
    .expect("leader exit observation");

    let probes = probes.lock().expect("leader-exit probe observations");
    let intervals = probes
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]))
        .collect::<Vec<_>>();
    assert!(
        intervals
            .iter()
            .all(|interval| *interval <= HOOK_LEADER_EXIT_POLL_MAX),
        "the next probe always bounds exit detection latency"
    );
    assert!(
        intervals.contains(&HOOK_LEADER_EXIT_POLL_MAX),
        "the fixture reaches the 50 ms capped phase"
    );
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).expect("canonical path")
}

#[cfg(unix)]
fn emit_command(value: &str) -> String {
    format!("printf {value}")
}

#[cfg(windows)]
fn emit_command(value: &str) -> String {
    format!("echo {value}")
}

#[cfg(unix)]
fn write_command(value: &str, path: &Path) -> String {
    format!("printf {value} > '{}'", path.display())
}

#[cfg(windows)]
fn write_command(value: &str, path: &Path) -> String {
    format!(">\"{}\" echo {value}", path.display())
}

#[cfg(unix)]
fn append_marker_command(path: &Path) -> String {
    format!("printf 'x\\n' >> '{}'", path.display())
}

#[cfg(windows)]
fn append_marker_command(path: &Path) -> String {
    format!(">>\"{}\" echo x", path.display())
}

fn marker_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|content| content.lines().count())
        .unwrap_or(0)
}

/// MUTATION CHECK (Android POSIX-shell resolution): restore `/bin/sh` in
/// `hook_command`. Expected failure: both program assertions report `/bin/sh`
/// instead of the executable selected by `$SHELL` or the PATH-resolved `sh`.
#[test]
#[cfg(unix)]
fn hook_shell_resolution_avoids_a_hardcoded_bin_sh() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("shell fixture");
    let configured = root.path().join("sh");
    std::fs::write(&configured, "").expect("write shell fixture");
    std::fs::set_permissions(&configured, std::fs::Permissions::from_mode(0o700))
        .expect("make shell fixture executable");

    let selected = hook_command(":", Some(configured.clone().into_os_string()));
    assert_eq!(selected.as_std().get_program(), configured.as_os_str());

    let platform_default = hook_command(":", None);
    assert_eq!(platform_default.as_std().get_program(), "sh");

    let incompatible = root.path().join("fish");
    std::fs::write(&incompatible, "").expect("write incompatible shell fixture");
    std::fs::set_permissions(&incompatible, std::fs::Permissions::from_mode(0o700))
        .expect("make incompatible shell fixture executable");
    let rejected = hook_command(":", Some(incompatible.into_os_string()));
    assert_eq!(rejected.as_std().get_program(), "sh");
}

#[cfg(unix)]
fn echo_stdin_command() -> String {
    "cat".into()
}

#[cfg(windows)]
fn echo_stdin_command() -> String {
    // `more.com` is the native byte-stream passthrough here. It avoids a
    // cold PowerShell startup and a separate output-file observation while
    // still exercising the real Windows hook subprocess boundary.
    let executable = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .map(|root| root.join("System32").join("more.com"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\more.com"));
    format!("\"{}\"", executable.display())
}

#[cfg(unix)]
fn delayed_emit_command(value: &str) -> String {
    format!("sleep 1; printf {value}")
}

#[cfg(windows)]
fn delayed_emit_command(value: &str) -> String {
    let value = value.replace('\'', "''");
    powershell_command(&format!(
        "Start-Sleep -Seconds 1;[Console]::Out.Write('{value}')"
    ))
}

#[cfg(unix)]
fn bounded_output_command(_fixture: &Path) -> String {
    "yes x | head -c 600000".into()
}

#[cfg(windows)]
fn bounded_output_command(fixture: &Path) -> String {
    format!("type \"{}\"", fixture.display())
}

#[cfg(unix)]
const BOUNDED_OUTPUT_TIMEOUT_MS: u64 = 2_000;

#[cfg(windows)]
const BOUNDED_OUTPUT_TIMEOUT_MS: u64 = 5_000;

#[cfg(unix)]
fn subscriber_tree_command(ready: &Path, survived: &Path) -> String {
    format!(
        "printf ready > '{}'; (sleep 1; printf survived > '{}') & cat >/dev/null",
        ready.display(),
        survived.display()
    )
}

#[cfg(windows)]
fn subscriber_tree_command(ready: &Path, survived: &Path) -> String {
    let directory = ready.parent().expect("ready marker parent");
    let parent = directory.join("subscriber-parent.cmd");
    let child = directory.join("subscriber-child.cmd");
    let system32 = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .map(|root| root.join("System32"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"));
    std::fs::write(
        &child,
        format!(
            "@echo off\r\n>\"{}\" <nul set /p \"=ready\"\r\n\"{}\" -n 2 127.0.0.1 >nul\r\n>\"{}\" <nul set /p \"=survived\"\r\n",
            ready.display(),
            system32.join("ping.exe").display(),
            survived.display(),
        ),
    )
    .expect("write subscriber child fixture");
    std::fs::write(
        &parent,
        format!(
            "@echo off\r\nstart \"\" /b \"{}\" /d /s /c \"\"{}\"\"\r\n\"{}\" >nul\r\n",
            system32.join("cmd.exe").display(),
            child.display(),
            system32.join("more.com").display(),
        ),
    )
    .expect("write subscriber parent fixture");
    format!("\"{}\"", parent.display())
}

#[cfg(windows)]
fn exiting_leader_tree_command(directory: &Path, ready: &Path, survived: &Path) -> String {
    let parent = directory.join("exiting-parent.cmd");
    let child = directory.join("exiting-child.cmd");
    let system32 = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .map(|root| root.join("System32"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"));
    std::fs::write(
        &child,
        format!(
            "@echo off\r\n>\"{}\" <nul set /p \"=ready\"\r\n\"{}\" -n 3 127.0.0.1 >nul\r\n>\"{}\" <nul set /p \"=survived\"\r\n",
            ready.display(),
            system32.join("ping.exe").display(),
            survived.display(),
        ),
    )
    .expect("write exiting child fixture");
    std::fs::write(
        &parent,
        format!(
            "@echo off\r\nstart \"\" /b \"{}\" /d /s /c \"\"{}\"\"\r\n:wait_ready\r\nif exist \"{}\" exit /b 0\r\n\"{}\" -n 2 127.0.0.1 >nul\r\ngoto wait_ready\r\n",
            system32.join("cmd.exe").display(),
            child.display(),
            ready.display(),
            system32.join("ping.exe").display(),
        ),
    )
    .expect("write exiting parent fixture");
    format!("\"{}\"", parent.display())
}

#[cfg(windows)]
fn powershell_executable() -> PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .filter(|path| path.is_file())
        .unwrap_or_else(|| {
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        })
}

#[cfg(windows)]
fn encode_powershell(script: &str) -> String {
    let utf16 = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

#[cfg(windows)]
fn powershell_command(script: &str) -> String {
    format!(
        "\"{}\" -NoProfile -NonInteractive -EncodedCommand {}",
        powershell_executable().display(),
        encode_powershell(script)
    )
}

#[cfg(unix)]
const USER_MESSAGE_CAPTURE_TIMEOUT_MS: u64 = 1_000;

#[cfg(windows)]
const USER_MESSAGE_CAPTURE_TIMEOUT_MS: u64 = 5_000;

#[cfg(unix)]
const USER_MESSAGE_CAPTURE_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(windows)]
const USER_MESSAGE_CAPTURE_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(unix)]
const BOUNDED_OUTPUT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(windows)]
const BOUNDED_OUTPUT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

fn write_profile_policy(profile: &Path, policy: &str) {
    std::fs::write(
        profile.join("hooks.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "haider.hooks.v1",
            "policy": policy,
            "hooks": {},
        }))
        .expect("profile hooks JSON"),
    )
    .expect("write profile hooks");
}

fn write_hook(
    workspace: &Path,
    name: &str,
    event: &str,
    command: &str,
    timeout_ms: u64,
    decision: bool,
    kind: &str,
) {
    let matcher = if decision {
        serde_json::json!({"event": event, "parked_kind": "permission"})
    } else {
        serde_json::json!({"event": event})
    };
    std::fs::write(
        workspace.join("hooks.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "haider.hooks.v1",
            "hooks": {
                name: {
                    "matcher": matcher,
                    "kind": kind,
                    "command": command,
                    "timeout_ms": timeout_ms,
                    "decision": decision,
                }
            }
        }))
        .expect("workspace hooks JSON"),
    )
    .expect("write workspace hooks");
}

fn raw_event(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    id: &str,
    payload: EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("hooks-test-device"),
        authority_epoch: 0,
        worker_generation: generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload"),
    }
}

fn raw_hook_event(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    id: &str,
    payload: HookEventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("hooks-test-device"),
        authority_epoch: 0,
        worker_generation: generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.to_payload_value().expect("hook payload"),
    }
}

fn permission_menu(options: Vec<MenuOption>) -> Menu {
    Menu {
        id: MenuId::new("hook-permission-menu"),
        kind: MenuKind::Permission {
            effect_summary: "run exact hook test command".into(),
        },
        title: "Allow command?".into(),
        body: vec!["exact effect bytes".into()],
        options,
        blocking: true,
        scope: MenuScope::Session,
        origin: "process_exec".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

#[tokio::test]
async fn structurally_inconsistent_hook_snapshot_replays_from_zero_cleanly() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("hook-snapshot-corrupt-session");
    let run_id = RunId::new("hook-snapshot-corrupt-run");
    let mut journal = [raw_hook_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "hook-snapshot-trust",
        HookEventPayload::HookRunTrust { enabled: true },
    )];
    StoreHandle::append(&store, &mut journal)
        .await
        .expect("append authoritative hook fact");

    let mut stale_sessions = std::collections::HashMap::new();
    stale_sessions.insert(session_id.clone(), DecisionState::default());
    let corrupt = HookEngineSnapshot {
        version: HOOK_ENGINE_SNAPSHOT_VERSION,
        sessions: stale_sessions,
        run_trust: std::collections::HashSet::new(),
        terminal_run_trust: std::collections::HashSet::new(),
        terminal_run_trust_complete: true,
        through_seq: std::collections::HashMap::new(),
        through_digest: std::collections::HashMap::new(),
    };
    std::fs::write(
        root.path().join(HOOK_ENGINE_SNAPSHOT_FILE),
        encode_hook_snapshot_file(
            rmp_serde::to_vec_named(&corrupt).expect("encode corrupt snapshot payload"),
        )
        .expect("encode corrupt snapshot fixture"),
    )
    .expect("write corrupt snapshot fixture");

    let mut hydration = HookStartupHydrator::prepare(&store)
        .await
        .expect("corrupt snapshot is a cache miss");
    assert_eq!(hydration.scan_start(&session_id), 0);
    assert!(!hydration.state.sessions.contains_key(&session_id));
    hydration = finish_hook_hydration_for_test(&store, hydration)
        .await
        .expect("full hook fallback");
    assert_eq!(hydration.scan_start(&session_id), journal[0].seq);
    assert!(hydration.state.run_trust.contains(&(session_id, run_id)));
    store.close().await.expect("store close");
}

#[tokio::test]
async fn checksum_mismatched_hook_snapshot_replays_from_zero_cleanly() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("hook-snapshot-checksum-session");
    let run_id = RunId::new("hook-snapshot-checksum-run");
    let mut journal = [raw_hook_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "hook-snapshot-checksum-trust",
        HookEventPayload::HookRunTrust { enabled: true },
    )];
    StoreHandle::append(&store, &mut journal)
        .await
        .expect("append authoritative hook fact");
    let mut state = EngineState {
        sessions: std::collections::HashMap::new(),
        run_trust: std::collections::HashSet::new(),
        terminal_run_trust: std::collections::HashSet::new(),
        through_seq: std::collections::HashMap::new(),
        through_digest: std::collections::HashMap::new(),
        notice_dedup: std::collections::HashSet::new(),
        subscribers: std::collections::HashMap::new(),
    };
    reduce_durable_state(&mut state, &journal[0]);
    let payload = rmp_serde::to_vec_named(&HookEngineSnapshot {
        version: HOOK_ENGINE_SNAPSHOT_VERSION,
        sessions: state.sessions,
        run_trust: state.run_trust,
        terminal_run_trust: state.terminal_run_trust,
        terminal_run_trust_complete: true,
        through_seq: state.through_seq,
        through_digest: state.through_digest,
    })
    .expect("encode checksum snapshot payload");
    std::fs::write(
        root.path().join(HOOK_ENGINE_SNAPSHOT_FILE),
        rmp_serde::to_vec_named(&HookEngineSnapshotFile {
            payload,
            digest: "validly-decodable-but-wrong".into(),
        })
        .expect("encode checksum-mismatched snapshot file"),
    )
    .expect("write checksum-mismatched snapshot file");

    let mut hydration = HookStartupHydrator::prepare(&store)
        .await
        .expect("checksum mismatch is a cache miss");
    assert_eq!(hydration.scan_start(&session_id), 0);
    hydration = finish_hook_hydration_for_test(&store, hydration)
        .await
        .expect("full checksum fallback");
    assert!(hydration.state.run_trust.contains(&(session_id, run_id)));
    store.close().await.expect("store close");
}

/// MUTATION CHECK: treat the accelerator snapshot as authoritative and skip
/// the journal suffix committed before a crash. Expected runtime failure: the
/// suffix cannot bind the snapshotted intent to its permission menu.
#[tokio::test]
async fn valid_hook_snapshot_preserves_intent_until_ask_suffix_binds_it() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("hook-snapshot-intent-session");
    let run_id = RunId::new("hook-snapshot-intent-run");
    let effect_id = EffectId::new("hook-snapshot-intent-effect");
    let menu = permission_menu(vec![]);
    let mut prefix = [raw_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "hook-snapshot-intent",
        EventPayload::Effect(EffectPhase::Intent(EffectIntent {
            effect: effect_id.clone(),
            class: EffectClass::ProcessExec,
            summary: "pending permission".into(),
            args_digest: "pending-permission-args".into(),
            workspace_revision: None,
        })),
    )];
    StoreHandle::append(&store, &mut prefix)
        .await
        .expect("append intent prefix");
    let mut state = EngineState {
        sessions: std::collections::HashMap::new(),
        run_trust: std::collections::HashSet::new(),
        terminal_run_trust: std::collections::HashSet::new(),
        through_seq: std::collections::HashMap::new(),
        through_digest: std::collections::HashMap::new(),
        notice_dedup: std::collections::HashSet::new(),
        subscribers: std::collections::HashMap::new(),
    };
    reduce_durable_state(&mut state, &prefix[0]);
    let snapshot = HookEngineSnapshot {
        version: HOOK_ENGINE_SNAPSHOT_VERSION,
        sessions: state.sessions,
        run_trust: state.run_trust,
        terminal_run_trust: state.terminal_run_trust,
        terminal_run_trust_complete: true,
        through_seq: state.through_seq,
        through_digest: state.through_digest,
    };
    std::fs::write(
        root.path().join(HOOK_ENGINE_SNAPSHOT_FILE),
        encode_hook_snapshot_file(
            rmp_serde::to_vec_named(&snapshot).expect("encode valid intent snapshot payload"),
        )
        .expect("encode valid intent snapshot"),
    )
    .expect("write valid intent snapshot");

    let mut suffix = [raw_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "hook-snapshot-ask",
        EventPayload::Effect(EffectPhase::Authorized {
            effect: effect_id.clone(),
            verdict: AuthorizationVerdict::Ask {
                menu: menu.id.clone(),
            },
        }),
    )];
    StoreHandle::append(&store, &mut suffix)
        .await
        .expect("append ask suffix");

    let mut hydration = HookStartupHydrator::prepare(&store)
        .await
        .expect("load valid intent snapshot");
    assert_eq!(hydration.scan_start(&session_id), prefix[0].seq);
    hydration = finish_hook_hydration_for_test(&store, hydration)
        .await
        .expect("fold ask suffix");
    let decision = hydration
        .state
        .sessions
        .get(&session_id)
        .expect("session decision state");
    assert_eq!(
        decision.bindings.get(&menu.id).map(|intent| &intent.effect),
        Some(&effect_id)
    );
    store.close().await.expect("store close");
}

#[test]
fn completed_permission_decisions_release_effect_reducer_state() {
    let session_id = SessionId::new("hook-effect-reducer-session");
    let run_id = RunId::new("hook-effect-reducer-run");
    let effect_id = EffectId::new("hook-effect-reducer-effect");
    let menu = permission_menu(vec![]);
    let mut state = EngineState {
        sessions: std::collections::HashMap::new(),
        run_trust: std::collections::HashSet::new(),
        terminal_run_trust: std::collections::HashSet::new(),
        through_seq: std::collections::HashMap::new(),
        through_digest: std::collections::HashMap::new(),
        notice_dedup: std::collections::HashSet::new(),
        subscribers: std::collections::HashMap::new(),
    };
    for event in [
        raw_event(
            &session_id,
            &run_id,
            1,
            "hook-effect-intent",
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect_id.clone(),
                class: EffectClass::ProcessExec,
                summary: "bounded reducer".into(),
                args_digest: "bounded-reducer-args".into(),
                workspace_revision: None,
            })),
        ),
        raw_event(
            &session_id,
            &run_id,
            1,
            "hook-effect-ask",
            EventPayload::Effect(EffectPhase::Authorized {
                effect: effect_id,
                verdict: AuthorizationVerdict::Ask {
                    menu: menu.id.clone(),
                },
            }),
        ),
        raw_event(
            &session_id,
            &run_id,
            1,
            "hook-effect-menu",
            EventPayload::MenuOpened(menu.clone()),
        ),
        raw_event(
            &session_id,
            &run_id,
            1,
            "hook-effect-answer",
            EventPayload::MenuAnswered(haider_protocol::menu::MenuAnswer {
                menu: menu.id,
                option_key: None,
                option_index: 0,
                value: None,
                via: AnswerVia::Hook,
            }),
        ),
    ] {
        reduce_durable_state(&mut state, &event);
    }
    let session = state.sessions.get(&session_id).expect("session reducer");
    assert!(session.intents.is_empty());
    assert!(session.bindings.is_empty());
    assert!(session.menus.is_empty());

    reduce_durable_state(
        &mut state,
        &raw_hook_event(
            &session_id,
            &run_id,
            1,
            "hook-run-trust-enable",
            HookEventPayload::HookRunTrust { enabled: true },
        ),
    );
    reduce_durable_state(
        &mut state,
        &raw_event(
            &session_id,
            &run_id,
            1,
            "hook-run-terminal",
            EventPayload::RunState(RunState::Done),
        ),
    );
    assert_eq!(state.terminal_run_trust.len(), 1);
    prune_terminal_run_trust(&mut state);
    assert!(state.run_trust.is_empty());
    assert!(state.terminal_run_trust.is_empty());
}

struct BrokerJournal;

#[async_trait::async_trait]
impl JournalSink for BrokerJournal {
    async fn append(&mut self, _payload: EventPayload) -> ToolResult<()> {
        Ok(())
    }
}

async fn broker_permission_menu(workspace: &Path) -> Menu {
    let mut broker = EffectBroker::new_at(
        Box::new(BrokerJournal),
        workspace,
        SessionId::new("hooks-test-session"),
        41,
        1_700_000_000_000,
    )
    .expect("effect broker");
    let operation = ProcessExec::new("hook-ask-fixture", "printf exact");
    let intent = broker
        .normalize(&operation)
        .await
        .expect("normalize effect");
    let mut policy = PermissionPolicy::default();
    policy.ask(EffectClass::ProcessExec);
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&intent, &policy)
        .await
        .expect("broker ask")
    else {
        panic!("fixture policy must ask");
    };
    broker
        .permission_menu(&menu)
        .expect("broker permission menu")
        .clone()
}

struct EngineFixture {
    _workspace_guard: tempfile::TempDir,
    _profile_guard: tempfile::TempDir,
    workspace: PathBuf,
    store: SqliteStoreHandle,
    hub: SessionHub,
    service: HookService,
    engine: HookEngine,
    session_id: SessionId,
    run_id: RunId,
}

impl EngineFixture {
    async fn start(command: &str, timeout_ms: u64, decision: bool, kind: &str) -> Self {
        Self::start_with_trust(command, timeout_ms, decision, kind, true).await
    }

    async fn start_user_message(command: &str) -> Self {
        Self::start_with_event_and_trust(
            command,
            USER_MESSAGE_CAPTURE_TIMEOUT_MS,
            false,
            "exec",
            true,
            "user_message",
        )
        .await
    }

    async fn start_untrusted(command: &str, timeout_ms: u64) -> Self {
        Self::start_with_trust(command, timeout_ms, false, "exec", false).await
    }

    async fn start_with_trust(
        command: &str,
        timeout_ms: u64,
        decision: bool,
        kind: &str,
        trust: bool,
    ) -> Self {
        let event = if decision {
            "run_parked"
        } else {
            "run_started"
        };
        Self::start_with_event_and_trust(command, timeout_ms, decision, kind, trust, event).await
    }

    async fn start_with_event_and_trust(
        command: &str,
        timeout_ms: u64,
        decision: bool,
        kind: &str,
        trust: bool,
        event: &str,
    ) -> Self {
        let workspace_guard = tempfile::tempdir().expect("workspace");
        let profile_guard = tempfile::tempdir().expect("profile");
        let workspace = canonical(workspace_guard.path());
        let profile = canonical(profile_guard.path());
        write_profile_policy(&profile, "per_digest");
        write_hook(
            &workspace,
            "test_hook",
            event,
            command,
            timeout_ms,
            decision,
            kind,
        );
        let store = SqliteStoreHandle::open(&profile).await.expect("store");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let (service, engine) = HookEngine::start(profile.clone(), store.clone(), hub.clone())
            .await
            .expect("hook engine");
        hub.install_hooks(service.clone()).expect("install hooks");
        let session_id = SessionId::new("hooks-test-session");
        let run_id = RunId::new("hooks-test-run");
        hub.create_internal_session(SessionCreateCommand {
            command_id: "create-hooks-test".into(),
            request_digest: "create-hooks-test-digest".into(),
            request_json: r#"{"session":"hooks-test"}"#.into(),
            session_id: session_id.clone(),
            cwd: workspace.to_str().expect("UTF-8 workspace").to_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "hooks-test-v1".into(),
            event_id: EventId::new("hooks-test-created"),
            device_id: DeviceId::new("hooks-test-device"),
        })
        .await
        .expect("create session");
        if trust {
            let (_, _, hooks) = service.list(workspace.clone()).await.expect("list hooks");
            let digest = hooks.first().expect("discovered hook").digest.clone();
            service
                .apply_trust(CommandId::new("trust-hooks-test"), digest, true)
                .await
                .expect("trust hook");
        }
        Self {
            _workspace_guard: workspace_guard,
            _profile_guard: profile_guard,
            workspace,
            store,
            hub,
            service,
            engine,
            session_id,
            run_id,
        }
    }

    async fn accept_user_message(
        &self,
        id: &str,
        text: &str,
        mode: DeliveryMode,
        attachments: Vec<AttachmentBlock>,
    ) {
        let request_json = serde_json::json!({
            "session_id": &self.session_id,
            "worker_generation": self.store.worker_generation(),
            "branch_id": serde_json::Value::Null,
            "text": text,
            "attachments": &attachments,
            "mode": mode,
        })
        .to_string();
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        self.hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("{id}-command"),
                request_digest,
                request_json,
                session_id: self.session_id.clone(),
                worker_generation: self.store.worker_generation(),
                run_id: self.run_id.clone(),
                agent_id: None,
                branch_id: None,
                text: text.to_owned(),
                attachments,
                mode,
                queued_event_id: EventId::new(format!("{id}-queued")),
                user_event_id: EventId::new(format!("{id}-user")),
                active_event_id: EventId::new(format!("{id}-active")),
                device_id: DeviceId::new("hooks-test-device"),
            })
            .await
            .expect("accept user message");
    }

    async fn append_permission(&self, menu: Menu) {
        let generation = self.store.worker_generation();
        let effect = EffectId::new("hooks-test-effect");
        let mut events = [
            raw_event(
                &self.session_id,
                &self.run_id,
                generation,
                "hooks-test-intent",
                EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                    effect: effect.clone(),
                    class: EffectClass::ProcessExec,
                    summary: "run exact hook test command".into(),
                    args_digest: "exact-hook-test-args".into(),
                    workspace_revision: None,
                })),
            ),
            raw_event(
                &self.session_id,
                &self.run_id,
                generation,
                "hooks-test-authorized",
                EventPayload::Effect(EffectPhase::Authorized {
                    effect,
                    verdict: AuthorizationVerdict::Ask {
                        menu: menu.id.clone(),
                    },
                }),
            ),
            raw_event(
                &self.session_id,
                &self.run_id,
                generation,
                "hooks-test-menu-opened",
                EventPayload::MenuOpened(menu.clone()),
            ),
            raw_event(
                &self.session_id,
                &self.run_id,
                generation,
                "hooks-test-permission-required",
                EventPayload::RunState(RunState::PermissionRequired { menu: menu.id }),
            ),
        ];
        self.hub
            .append(&mut events)
            .await
            .expect("append permission");
    }

    async fn events(&self) -> Vec<RawEnvelope> {
        self.store
            .read(&self.session_id, 0, 256)
            .await
            .expect("read hook events")
    }

    async fn close(self) {
        self.engine.shutdown().await;
        self.hub.shutdown().await.expect("hub shutdown");
        self.store.close().await.expect("store close");
    }
}

async fn wait_for_hook_outbox_drain(store: &SqliteStoreHandle) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store
                .pending_hook_dispatches(64)
                .await
                .expect("read hook outbox")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("hook outbox drain deadline");
}

/// MUTATION CHECK: persist on every committed batch, omit the terminal or
/// shutdown boundary. Expected runtime failure: one of the exact persist-count
/// deltas changes.
#[tokio::test]
async fn snapshot_cadence_coalesces_batches_and_forces_terminal_delete_and_shutdown() {
    let command = emit_command("terminal");
    let fixture = EngineFixture::start_with_event_and_trust(
        &command,
        1_000,
        false,
        "exec",
        true,
        "run_finished",
    )
    .await;
    wait_for_hook_outbox_drain(&fixture.store).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let baseline = fixture.service.snapshot_persist_count();

    for index in 0..4 {
        let mut batch = [raw_event(
            &fixture.session_id,
            &fixture.run_id,
            fixture.store.worker_generation(),
            &format!("snapshot-burst-{index}"),
            EventPayload::RunState(RunState::Thinking),
        )];
        fixture.hub.append(&mut batch).await.expect("commit burst");
    }
    // One durable append larger than the hook page-count ceiling must wake
    // successive immediate pages without retaining or losing the tail.
    let mut paged_batch = (0..=HOOK_DRAIN_PAGE_MAX_REQUESTS)
        .map(|index| {
            raw_event(
                &fixture.session_id,
                &fixture.run_id,
                fixture.store.worker_generation(),
                &format!("snapshot-paged-{index}"),
                EventPayload::RunState(RunState::Thinking),
            )
        })
        .collect::<Vec<_>>();
    fixture
        .hub
        .append(&mut paged_batch)
        .await
        .expect("commit paged burst");
    wait_for_hook_outbox_drain(&fixture.store).await;
    assert!(
        fixture.service.snapshot_persist_count() - baseline <= 1,
        "batches inside one second produce at most one snapshot persist"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.service.snapshot_persist_count() == baseline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle snapshot deadline");
    assert_eq!(
        fixture.service.snapshot_persist_count() - baseline,
        1,
        "the burst coalesces into one idle persist"
    );

    let before_terminal = fixture.service.snapshot_persist_count();
    let mut terminal = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "snapshot-terminal",
        EventPayload::RunState(RunState::Done),
    )];
    fixture
        .hub
        .append(&mut terminal)
        .await
        .expect("commit terminal run");
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.service.snapshot_persist_count() == before_terminal {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("terminal snapshot deadline");

    let before_delete = fixture.service.snapshot_persist_count();
    fixture
        .hub
        .delete_session(fixture.session_id.clone())
        .await
        .expect("durably delete terminal hook session");
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.service.snapshot_persist_count() == before_delete {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session-delete snapshot deadline");

    let before_shutdown = fixture.service.snapshot_persist_count();
    fixture.engine.shutdown().await;
    assert!(
        fixture.service.snapshot_persist_count() > before_shutdown,
        "shutdown forces a final snapshot persist"
    );
    fixture.hub.shutdown().await.expect("hub shutdown");
    fixture.store.close().await.expect("store close");
}

/// MUTATION CHECK: restore the post-replay boot persist. Expected runtime
/// failure: the count is two before any committed message exists.
#[tokio::test]
async fn hook_start_performs_exactly_one_immediate_snapshot_persist() {
    let profile = tempfile::tempdir().expect("hook profile");
    let profile_root = canonical(profile.path());
    let store = SqliteStoreHandle::open(&profile_root).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let (service, engine) = HookEngine::start(profile_root, store.clone(), hub.clone())
        .await
        .expect("hook engine");
    tokio::task::yield_now().await;
    assert_eq!(service.snapshot_persist_count(), 1);
    engine.shutdown().await;
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: allocate a discovery cache per envelope instead of per
/// committed batch. Expected runtime failure: three classifiable facts cause
/// three stamp computations instead of one.
#[tokio::test]
async fn committed_batch_computes_discovery_stamp_once() {
    let command = emit_command("stamped");
    let fixture = EngineFixture::start(&command, 1_000, false, "exec").await;
    wait_for_hook_outbox_drain(&fixture.store).await;
    let baseline = fixture.service.discovery_stamp_count();
    let mut batch = (0..3)
        .map(|index| {
            raw_event(
                &fixture.session_id,
                &fixture.run_id,
                fixture.store.worker_generation(),
                &format!("stamp-once-{index}"),
                EventPayload::RunState(RunState::Thinking),
            )
        })
        .collect::<Vec<_>>();
    fixture
        .hub
        .append(&mut batch)
        .await
        .expect("commit classifiable batch");
    wait_for_hook_outbox_drain(&fixture.store).await;
    assert_eq!(fixture.service.discovery_stamp_count() - baseline, 1);
    fixture.close().await;
}

/// MUTATION CHECK: execute an unpinned hook or suppress the refusal notice.
/// Expected RUNTIME failure: the marker appears or no durable honest notice
/// names the untrusted reason.
#[tokio::test]
async fn untrusted_hook_never_executes_and_notices_honestly() {
    let marker_dir = tempfile::tempdir().expect("marker dir");
    let marker = marker_dir.path().join("untrusted-fired");
    let command = write_command("forbidden", &marker);
    let fixture = EngineFixture::start_untrusted(&command, 1_000).await;
    let mut event = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "untrusted-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture.hub.append(&mut event).await.expect("commit fact");
    let events = wait_for(&fixture, |events| {
        events.iter().any(|event| {
            matches!(
                HookEventPayload::from_payload_value(event.payload.clone()),
                Ok(HookEventPayload::HookNotice(ref notice))
                    if notice.reason == "hook is untrusted and was not executed"
            )
        })
    })
    .await;
    assert!(!marker.exists());
    assert!(events.iter().any(|event| {
        matches!(
            HookEventPayload::from_payload_value(event.payload.clone()),
            Ok(HookEventPayload::HookNotice(ref notice))
                if notice.hook.as_deref() == Some("test_hook")
        )
    }));
    fixture.close().await;
}

/// MUTATION CHECK: deduplicate refusal notices without the hook digest.
/// Expected RUNTIME failure: editing one still-untrusted hook suppresses the
/// second honest refusal and leaves only the stale digest in the journal.
#[tokio::test]
async fn untrusted_notice_dedup_is_digest_sensitive() {
    let fixture = EngineFixture::start_untrusted("printf first", 1_000).await;
    let mut first = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "untrusted-first-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture.hub.append(&mut first).await.expect("first fact");
    let first_events = wait_for(&fixture, |events| {
        events.iter().any(|event| {
            matches!(
                HookEventPayload::from_payload_value(event.payload.clone()),
                Ok(HookEventPayload::HookNotice(_))
            )
        })
    })
    .await;
    let first_digest = first_events
        .iter()
        .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
        .find_map(|payload| match payload {
            HookEventPayload::HookNotice(notice) => notice.digest,
            _ => None,
        })
        .expect("first digest");
    write_hook(
        &fixture.workspace,
        "test_hook",
        "run_started",
        "printf second",
        1_000,
        false,
        "exec",
    );
    let mut second = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "untrusted-second-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture.hub.append(&mut second).await.expect("second fact");
    let events = wait_for(&fixture, |events| {
        events
            .iter()
            .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
            .filter(|payload| matches!(payload, HookEventPayload::HookNotice(_)))
            .count()
            >= 2
    })
    .await;
    let digests = events
        .iter()
        .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
        .filter_map(|payload| match payload {
            HookEventPayload::HookNotice(notice) => notice.digest,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(digests.iter().any(|digest| digest == &first_digest));
    assert!(digests.iter().any(|digest| digest != &first_digest));
    fixture.close().await;
}

/// WIRE-GAPS H4: revoked-by-edit is classified by the daemon and sent as
/// wire truth. A client needs no remembered digest baseline to distinguish
/// it from a hook that was never trusted.
#[tokio::test]
async fn hooks_list_reports_revoked_by_edit_as_wire_truth() {
    let fixture = EngineFixture::start("printf first", 1_000, false, "exec").await;
    let (_, revision, trusted) = fixture
        .service
        .list(fixture.workspace.clone())
        .await
        .expect("trusted list");
    assert_eq!(revision, 1);
    assert!(trusted[0].trusted);
    assert_eq!(trusted[0].trust_state, Some(HookTrustStateWire::Trusted));

    write_hook(
        &fixture.workspace,
        "test_hook",
        "run_started",
        "printf edited",
        1_000,
        false,
        "exec",
    );
    let (_, edited_revision, edited) = fixture
        .service
        .list(fixture.workspace.clone())
        .await
        .expect("edited list");
    assert_eq!(edited_revision, revision, "an edit is not a trust mutation");
    assert!(!edited[0].trusted);
    assert_eq!(
        edited[0].trust_state,
        Some(HookTrustStateWire::RevokedByEdit)
    );
    fixture.close().await;
}

/// WIRE-GAPS H4: every new trust receipt advances the list revision exactly
/// once and mirrors one durable, non-UI fact into open session journals.
/// Same-command replay returns the original revision and cannot duplicate
/// the fact.
#[tokio::test]
async fn hook_trust_revision_and_journal_fact_are_receipted_once() {
    let fixture = EngineFixture::start_untrusted("printf first", 1_000).await;
    let (_, revision, hooks) = fixture
        .service
        .list(fixture.workspace.clone())
        .await
        .expect("initial list");
    assert_eq!(revision, 0);
    let digest = hooks[0].digest.clone();
    let command = CommandId::new("trust-revision-once");
    let trusted = fixture
        .service
        .apply_trust(command.clone(), digest.clone(), true)
        .await
        .expect("trust");
    assert_eq!(trusted.revision, 1);

    let facts = fixture
        .events()
        .await
        .into_iter()
        .filter_map(|event| {
            HookEventPayload::from_payload_value(event.payload.clone())
                .ok()
                .map(|payload| (event, payload))
        })
        .filter_map(|(event, payload)| match payload {
            HookEventPayload::HookTrustChanged {
                digest,
                trusted,
                revision,
            } => Some((event, digest, trusted, revision)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].1, digest);
    assert!(facts[0].2);
    assert_eq!(facts[0].3, 1);
    assert!(!facts[0].0.render.ui);
    assert!(facts[0].0.render.durable);

    let replay = fixture
        .service
        .apply_trust(command, digest, true)
        .await
        .expect("same-command replay");
    assert_eq!(replay.revision, 1);
    let replay_fact_count = fixture
        .events()
        .await
        .iter()
        .filter(|event| {
            matches!(
                HookEventPayload::from_payload_value(event.payload.clone()),
                Ok(HookEventPayload::HookTrustChanged { revision: 1, .. })
            )
        })
        .count();
    assert_eq!(replay_fact_count, 1);
    assert_eq!(
        fixture
            .service
            .list(fixture.workspace.clone())
            .await
            .expect("revised list")
            .1,
        1
    );
    fixture.close().await;
}

async fn wait_for(
    fixture: &EngineFixture,
    predicate: impl Fn(&[RawEnvelope]) -> bool,
) -> Vec<RawEnvelope> {
    wait_for_with_timeout(fixture, Duration::from_secs(5), predicate).await
}

async fn wait_for_with_timeout(
    fixture: &EngineFixture,
    timeout: Duration,
    predicate: impl Fn(&[RawEnvelope]) -> bool,
) -> Vec<RawEnvelope> {
    tokio::time::timeout(timeout, async {
        loop {
            let events = fixture.events().await;
            if predicate(&events) {
                break events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("hook result deadline")
}

/// MUTATION CHECK: hash parsed JSON or omit the command bytes. Expected
/// RUNTIME failure: the literal digest no longer matches raw-file+command.
#[test]
fn digest_is_raw_hooks_bytes_followed_by_command_bytes() {
    let bytes = br#"{"schema":"haider.hooks.v1","hooks":{}}"#;
    let command = "printf exact";
    let mut expected = blake3::Hasher::new();
    expected.update(bytes);
    expected.update(command.as_bytes());
    assert_eq!(
        hook_digest(bytes, command),
        expected.finalize().to_hex().to_string()
    );
}

/// MUTATION CHECK: reserve a name only after successful decoding. Expected
/// RUNTIME failure: the valid parent hook unexpectedly replaces the malformed
/// nearest entry and becomes executable.
#[test]
fn nearest_malformed_named_entry_reserves_the_parent_name() {
    let root = tempfile::tempdir().expect("root");
    let profile = tempfile::tempdir().expect("profile");
    let child = root.path().join("child");
    std::fs::create_dir(&child).expect("child");
    write_profile_policy(profile.path(), "per_digest");
    std::fs::write(
        root.path().join("hooks.json"),
        r#"{"schema":"haider.hooks.v1","hooks":{"same":{"matcher":{"event":"run_started"},"kind":"exec","command":"printf parent"}}}"#,
    )
    .expect("parent hooks");
    std::fs::write(
        child.join("hooks.json"),
        r#"{"schema":"haider.hooks.v1","hooks":{"same":{"matcher":{"event":"run_started"},"kind":"exec","command":""},"child":{"matcher":{"event":"run_started"},"kind":"exec","command":"printf child"}}}"#,
    )
    .expect("child hooks");
    let discovery = discover(&canonical(&child), &canonical(profile.path())).expect("discover");
    assert!(!discovery.hooks.contains_key("same"));
    assert_eq!(
        discovery.hooks.get("child").expect("child hook").command,
        "printf child"
    );
    assert!(
        discovery
            .notices
            .iter()
            .any(|notice| notice.hook.as_deref() == Some("same"))
    );
}

/// MUTATION CHECK: treat a missing/non-object hooks field as an empty valid
/// document. Expected RUNTIME failure: discovery returns no honest notice for
/// the malformed schema document.
#[test]
fn malformed_hooks_container_produces_an_honest_notice() {
    let workspace = tempfile::tempdir().expect("workspace");
    let profile = tempfile::tempdir().expect("profile");
    write_profile_policy(profile.path(), "per_digest");
    std::fs::write(
        workspace.path().join("hooks.json"),
        r#"{"schema":"haider.hooks.v1","hooks":[]}"#,
    )
    .expect("malformed hooks");
    let discovery =
        discover(&canonical(workspace.path()), &canonical(profile.path())).expect("discover");
    assert!(
        discovery
            .notices
            .iter()
            .any(|notice| { notice.reason.contains("field `hooks` must be an object") })
    );
}

/// MUTATION CHECK: omit canonical workspace cwd from subscriber identity.
/// Expected RUNTIME failure: one profile/ancestor subscriber key is reused by
/// two workspaces even though each process must run in its own cwd.
#[test]
fn subscriber_identity_is_scoped_to_workspace_cwd() {
    let first = tempfile::tempdir().expect("first workspace");
    let second = tempfile::tempdir().expect("second workspace");
    let mut first_definition = standalone_definition(&canonical(first.path()), "cat".into());
    let mut second_definition = standalone_definition(&canonical(second.path()), "cat".into());
    let shared_source = PathBuf::from("/profile/hooks.json");
    first_definition.source_path = shared_source.clone();
    second_definition.source_path = shared_source;
    assert_ne!(
        first_definition.subscriber_key(),
        second_definition.subscriber_key()
    );
}

/// MUTATION CHECK: apply provider filters only to session metadata. Expected
/// RUNTIME failure: account_expired(openai) fails to match in a session whose
/// ordinary provider metadata is different.
#[test]
fn matcher_filters_use_fact_specific_provider_outcome_and_parked_kind() {
    let session_id = SessionId::new("matcher-session");
    let run_id = RunId::new("matcher-run");
    let account = raw_hook_event(
        &session_id,
        &run_id,
        1,
        "matcher-account-expired",
        HookEventPayload::AccountExpired {
            provider: "openai".into(),
            alias: "primary".into(),
        },
    );
    let account_facts = classify(&account).expect("account facts");
    assert!(
        HookMatcher {
            event: MatchEvent::AccountExpired,
            session: Some("matcher-session".into()),
            provider: Some("openai".into()),
            outcome: None,
            parked_kind: None,
            mode: None,
            has_attachments: None,
        }
        .matches(&account, "different-session-provider", &account_facts)
    );

    let finished = raw_event(
        &session_id,
        &run_id,
        1,
        "matcher-finished",
        EventPayload::RunState(RunState::Errored),
    );
    let finished_facts = classify(&finished).expect("finished facts");
    assert!(
        HookMatcher {
            event: MatchEvent::RunFinished,
            session: None,
            provider: None,
            outcome: Some("errored".into()),
            parked_kind: None,
            mode: None,
            has_attachments: None,
        }
        .matches(&finished, "fake", &finished_facts)
    );

    let parked = raw_event(
        &session_id,
        &run_id,
        1,
        "matcher-parked",
        EventPayload::RunState(RunState::InputRequired {
            menu: MenuId::new("input-menu"),
        }),
    );
    let parked_facts = classify(&parked).expect("parked facts");
    assert!(
        HookMatcher {
            event: MatchEvent::RunParked,
            session: None,
            provider: None,
            outcome: None,
            parked_kind: Some("input".into()),
            mode: None,
            has_attachments: None,
        }
        .matches(&parked, "fake", &parked_facts)
    );
}

/// MUTATION CHECK: dispatch from a surface callback, add surface identity to
/// hook JSON, or bypass the committed UserMessage classifier. Expected
/// RUNTIME failure: one marker is absent/duplicated or the two captured JSON
/// byte strings differ.
#[tokio::test]
async fn committed_user_message_hook_projection_is_surface_neutral() {
    let command = echo_stdin_command();

    // These fixtures begin at the shared daemon acceptance seam. Surface
    // clients have already converged before the canonical fact is committed.
    let headless = EngineFixture::start_user_message(&command).await;
    headless
        .accept_user_message(
            "headless-user-message",
            "surface-neutral text",
            DeliveryMode::Queue,
            vec![],
        )
        .await;
    let headless_events = wait_for_with_timeout(
        &headless,
        USER_MESSAGE_CAPTURE_OBSERVATION_TIMEOUT,
        |events| {
            events.iter().any(|event| {
                HookEventPayload::from_payload_value(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, HookEventPayload::HookFired(_)))
            })
        },
    )
    .await;
    assert_eq!(
        headless_events
            .iter()
            .filter(|event| {
                HookEventPayload::from_payload_value(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, HookEventPayload::HookFired(_)))
            })
            .count(),
        1
    );
    headless.close().await;

    let rpc = EngineFixture::start_user_message(&command).await;
    rpc.accept_user_message(
        "rpc-user-message",
        "surface-neutral text",
        DeliveryMode::Queue,
        vec![],
    )
    .await;
    let rpc_events =
        wait_for_with_timeout(&rpc, USER_MESSAGE_CAPTURE_OBSERVATION_TIMEOUT, |events| {
            events.iter().any(|event| {
                HookEventPayload::from_payload_value(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, HookEventPayload::HookFired(_)))
            })
        })
        .await;
    assert_eq!(
        rpc_events
            .iter()
            .filter(|event| {
                HookEventPayload::from_payload_value(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, HookEventPayload::HookFired(_)))
            })
            .count(),
        1
    );
    rpc.close().await;

    let captured_json = |events: &[RawEnvelope], surface: &str| {
        let fired = events
            .iter()
            .find_map(|event| {
                match HookEventPayload::from_payload_value(event.payload.clone()).ok()? {
                    HookEventPayload::HookFired(fired) => Some(fired),
                    _ => None,
                }
            })
            .unwrap_or_else(|| panic!("{surface} hook fired"));
        assert_eq!(fired.exit_code, Some(0), "{surface} hook exit");
        assert!(!fired.timed_out, "{surface} hook timeout");
        assert!(!fired.stdout.truncated, "{surface} hook output truncation");
        fired.stdout.preview.into_bytes()
    };
    let headless_json = captured_json(&headless_events, "headless");
    let rpc_json = captured_json(&rpc_events, "RPC");
    assert_eq!(headless_json, rpc_json);
    let value: serde_json::Value = serde_json::from_slice(&headless_json).expect("hook input");
    assert_eq!(value["event"], "user_message");
    assert!(value.get("surface").is_none());
    assert!(value.get("client_kind").is_none());
}

/// MUTATION CHECK: raise/remove the text cap, truncate at an invalid UTF-8
/// boundary, or derive the flag from the bounded output. Expected RUNTIME
/// failure: the literal prefix/32768-byte boundary or either flag differs.
#[tokio::test]
async fn text_bounded_with_truncated_flag() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let session_id = SessionId::new("bounded-session");
    let run_id = RunId::new("bounded-run");
    let text = format!("{}é", "x".repeat(32_767));
    let oversized = raw_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "bounded-user",
        EventPayload::UserMessage {
            text,
            attachments: vec![],
            mode: DeliveryMode::Steer,
        },
    );
    let input: HookInput = serde_json::from_slice(
        &prepare_hook_input(&store, &oversized)
            .await
            .expect("bounded input"),
    )
    .expect("typed bounded input");
    let HookInput::UserMessage {
        text, truncated, ..
    } = input;
    assert_eq!(text, "x".repeat(32_767));
    assert!(truncated);
    assert!(text.len() <= 32_768);

    let exact = raw_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "exact-user",
        EventPayload::UserMessage {
            text: "y".repeat(32_768),
            attachments: vec![],
            mode: DeliveryMode::Steer,
        },
    );
    let input: HookInput = serde_json::from_slice(
        &prepare_hook_input(&store, &exact)
            .await
            .expect("exact input"),
    )
    .expect("typed exact input");
    let HookInput::UserMessage {
        text, truncated, ..
    } = input;
    assert_eq!(text.len(), 32_768);
    assert!(!truncated);
    store.close().await.expect("store close");
}

/// MUTATION CHECK: serialize AttachmentBlock/resolved CAS contents, omit byte
/// lengths, or label pasted text with image MIME. Expected RUNTIME failure:
/// the exact metadata/count drifts or the planted content appears in JSON.
#[tokio::test]
async fn attachment_metadata_never_carries_bytes() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let image_bytes = b"ATTACHMENT_BYTES_MUST_NEVER_APPEAR";
    let text_bytes = b"pasted text body must stay in CAS";
    let image_artifact = store
        .put(image_bytes.to_vec())
        .await
        .expect("image artifact");
    let text_artifact = store.put(text_bytes.to_vec()).await.expect("text artifact");
    let event = raw_event(
        &SessionId::new("attachment-session"),
        &RunId::new("attachment-run"),
        store.worker_generation(),
        "attachment-user",
        EventPayload::UserMessage {
            text: "inspect metadata only".into(),
            attachments: vec![
                AttachmentBlock::Image {
                    artifact: image_artifact.clone(),
                    mime: "image/png".into(),
                    width: Some(640),
                    height: Some(480),
                },
                AttachmentBlock::PastedText {
                    artifact: text_artifact.clone(),
                    lines: 3,
                },
            ],
            mode: DeliveryMode::Queue,
        },
    );
    let bytes = prepare_hook_input(&store, &event)
        .await
        .expect("metadata-only input");
    let input: HookInput = serde_json::from_slice(&bytes).expect("typed hook input");
    let HookInput::UserMessage { attachments, .. } = &input;
    assert_eq!(attachments.count, 2);
    assert_eq!(attachments.items[0].mime, "image/png");
    assert_eq!(attachments.items[0].bytes, image_bytes.len() as u64);
    assert_eq!(attachments.items[0].artifact, image_artifact);
    assert_eq!(attachments.items[1].mime, "text/plain");
    assert_eq!(attachments.items[1].bytes, text_bytes.len() as u64);
    assert_eq!(attachments.items[1].artifact, text_artifact);

    let value = serde_json::to_value(input).expect("hook input value");
    for item in value["attachments"]["items"]
        .as_array()
        .expect("attachment items")
    {
        let object = item.as_object().expect("metadata object");
        assert_eq!(object.len(), 3);
        assert!(object.contains_key("mime"));
        assert!(object.contains_key("bytes"));
        assert!(object.contains_key("artifact"));
    }
    let json = String::from_utf8(bytes).expect("UTF-8 hook JSON");
    assert!(!json.contains("ATTACHMENT_BYTES_MUST_NEVER_APPEAR"));
    assert!(!json.contains("pasted text body must stay in CAS"));
    assert!(!json.contains("data_base64"));
    assert!(!json.contains("content"));
    store.close().await.expect("store close");
}

/// MUTATION CHECK: ignore mode/attachment filters, invert attachment presence,
/// or apply them to non-user facts. Expected RUNTIME failure: one literal
/// queue/steer or attached/unattached match changes truth value.
#[test]
fn matcher_filters_respected() {
    let session_id = SessionId::new("filter-session");
    let run_id = RunId::new("filter-run");
    let attached_queue = raw_event(
        &session_id,
        &run_id,
        1,
        "attached-queue",
        EventPayload::UserMessage {
            text: "queued image".into(),
            attachments: vec![AttachmentBlock::Image {
                artifact: ArtifactRef::new("blake3:filter-image"),
                mime: "image/png".into(),
                width: None,
                height: None,
            }],
            mode: DeliveryMode::Queue,
        },
    );
    let attached_facts = classify(&attached_queue).expect("attached facts");
    let matcher = |mode, has_attachments| HookMatcher {
        event: MatchEvent::UserMessage,
        session: None,
        provider: None,
        outcome: None,
        parked_kind: None,
        mode,
        has_attachments,
    };
    assert!(matcher(Some(DeliveryMode::Queue), Some(true)).matches(
        &attached_queue,
        "fake",
        &attached_facts
    ));
    assert!(!matcher(Some(DeliveryMode::Steer), Some(true)).matches(
        &attached_queue,
        "fake",
        &attached_facts
    ));
    assert!(!matcher(Some(DeliveryMode::Queue), Some(false)).matches(
        &attached_queue,
        "fake",
        &attached_facts
    ));

    let empty_steer = raw_event(
        &session_id,
        &run_id,
        1,
        "empty-steer",
        EventPayload::UserMessage {
            text: "steer text".into(),
            attachments: vec![],
            mode: DeliveryMode::Steer,
        },
    );
    let empty_facts = classify(&empty_steer).expect("empty facts");
    assert!(matcher(Some(DeliveryMode::Steer), Some(false)).matches(
        &empty_steer,
        "fake",
        &empty_facts
    ));
    let thinking = raw_event(
        &session_id,
        &run_id,
        1,
        "thinking",
        EventPayload::RunState(RunState::Thinking),
    );
    let thinking_facts = classify(&thinking).expect("thinking facts");
    let wrong_event = HookMatcher {
        event: MatchEvent::RunStarted,
        session: None,
        provider: None,
        outcome: None,
        parked_kind: None,
        mode: Some(DeliveryMode::Steer),
        has_attachments: Some(false),
    };
    assert!(!wrong_event.matches(&thinking, "fake", &thinking_facts));
}

/// MUTATION CHECK: accept malformed mode/attachment filters permissively or
/// fail discovery as a whole. Expected RUNTIME failure: either invalid hook is
/// executable or its name lacks an honest malformed-entry notice.
#[test]
fn malformed_user_message_filters_are_skipped_honestly() {
    let workspace = tempfile::tempdir().expect("workspace");
    let profile = tempfile::tempdir().expect("profile");
    write_profile_policy(profile.path(), "per_digest");
    std::fs::write(
        workspace.path().join("hooks.json"),
        r#"{"schema":"haider.hooks.v1","hooks":{"bad_mode":{"matcher":{"event":"user_message","mode":"interrupt"},"kind":"exec","command":"printf forbidden"},"bad_attachments":{"matcher":{"event":"user_message","has_attachments":"yes"},"kind":"exec","command":"printf forbidden"}}}"#,
    )
    .expect("malformed filters");
    let discovery =
        discover(&canonical(workspace.path()), &canonical(profile.path())).expect("discovery");
    assert!(discovery.hooks.is_empty());
    for name in ["bad_mode", "bad_attachments"] {
        assert!(discovery.notices.iter().any(|notice| {
            notice.hook.as_deref() == Some(name)
                && notice.reason.contains("hook entry is malformed")
        }));
    }
}

/// MUTATION CHECK: resolve `allow` to an always-grant or bypass the menu CAS.
/// Expected RUNTIME failure: the answer is absent, has the wrong provenance,
/// or does not select the committed AllowOnce option.
#[tokio::test]
async fn decision_hook_allow_uses_existing_menu_cas_and_allow_once_only() {
    let fixture = EngineFixture::start(&emit_command("allow"), 1_000, true, "exec").await;
    fixture
        .append_permission(broker_permission_menu(&fixture.workspace).await)
        .await;
    let events = wait_for(&fixture, |events| {
        events.iter().any(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::MenuAnswered(_))
            )
        })
    })
    .await;
    let answer = events
        .iter()
        .find_map(|event| {
            match serde_json::from_value::<EventPayload>(event.payload.clone()).ok()? {
                EventPayload::MenuAnswered(answer) => Some(answer),
                _ => None,
            }
        })
        .expect("hook menu answer");
    assert_eq!(answer.option_key.as_deref(), Some("approve_once"));
    assert_eq!(answer.option_index, 0);
    assert_eq!(answer.via, AnswerVia::Hook);
    fixture.close().await;
}

/// MUTATION CHECK: map deny to an allow/persistent option or accept arbitrary
/// stdout as a decision. Expected RUNTIME failure: deny does not select the
/// committed RejectOnce option, or malformed output resolves the second menu.
#[tokio::test]
async fn decision_deny_is_reject_once_and_malformed_output_falls_through() {
    let deny = EngineFixture::start(&emit_command("deny"), 1_000, true, "exec").await;
    deny.append_permission(broker_permission_menu(&deny.workspace).await)
        .await;
    let events = wait_for(&deny, |events| {
        events.iter().any(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::MenuAnswered(_))
            )
        })
    })
    .await;
    let answer = events
        .iter()
        .find_map(
            |event| match serde_json::from_value(event.payload.clone()).ok()? {
                EventPayload::MenuAnswered(answer) => Some(answer),
                _ => None,
            },
        )
        .expect("deny answer");
    assert_eq!(answer.option_key.as_deref(), Some("deny"));
    assert_eq!(answer.option_index, 2);
    assert_eq!(answer.via, AnswerVia::Hook);
    deny.close().await;

    let malformed = EngineFixture::start(&emit_command("maybe"), 1_000, true, "exec").await;
    malformed
        .append_permission(broker_permission_menu(&malformed.workspace).await)
        .await;
    let events = wait_for(&malformed, |events| {
        events.iter().any(|event| {
            matches!(
                HookEventPayload::from_payload_value(event.payload.clone()),
                Ok(HookEventPayload::HookFired(_))
            )
        })
    })
    .await;
    assert!(!events.iter().any(|event| {
        matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::MenuAnswered(_))
        )
    }));
    malformed.close().await;
}

/// MUTATION CHECK: treat allow as permission to choose AllowAlways when the
/// committed Ask did not offer AllowOnce. Expected RUNTIME failure: a menu
/// answer appears even though the hook cannot grant that scope.
#[tokio::test]
async fn decision_hook_cannot_exceed_committed_ask_scope() {
    let fixture = EngineFixture::start(&emit_command("allow"), 1_000, true, "exec").await;
    fixture
        .append_permission(permission_menu(vec![MenuOption {
            key: "allow_always".into(),
            label: "Always allow".into(),
            detail: None,
            decision: Some(DecisionKind::AllowAlways),
        }]))
        .await;
    let events = wait_for(&fixture, |events| {
        events.iter().any(|event| {
            HookEventPayload::from_payload_value(event.payload.clone())
                .is_ok_and(|payload| matches!(payload, HookEventPayload::HookFired(_)))
        })
    })
    .await;
    assert!(!events.iter().any(|event| {
        matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::MenuAnswered(_))
        )
    }));
    let fired = events
        .iter()
        .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
        .find_map(|payload| match payload {
            HookEventPayload::HookFired(fired) => Some(fired),
            _ => None,
        })
        .expect("hook fired fact");
    assert_eq!(fired.menu_id, Some(MenuId::new("hook-permission-menu")));
    assert!(!fired.decision_applied);
    fixture.close().await;
}

async fn durable_ask_bytes_without_hooks(workspace: &Path, menu: Menu) -> Vec<u8> {
    let profile = tempfile::tempdir().expect("baseline profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("hooks-test-session");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-hooks-test".into(),
        request_digest: "create-hooks-test-digest".into(),
        request_json: r#"{"session":"hooks-test"}"#.into(),
        session_id: session_id.clone(),
        cwd: workspace.to_str().expect("UTF-8").to_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "hooks-test-v1".into(),
        event_id: EventId::new("hooks-test-created"),
        device_id: DeviceId::new("hooks-test-device"),
    })
    .await
    .expect("create");
    let run_id = RunId::new("hooks-test-run");
    let mut opening = [raw_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "hooks-test-menu-opened",
        EventPayload::MenuOpened(menu),
    )];
    hub.append(&mut opening)
        .await
        .expect("append baseline menu");
    let bytes = serde_json::to_vec(&opening[0].payload).expect("baseline bytes");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
    bytes
}

/// MUTATION CHECK: answer after timeout, pre-transform the Ask for hooks, or
/// use a degenerate hook that never could allow. Expected RUNTIME failure:
/// an answer appears, the exact MenuOpened payload bytes differ from the
/// no-hook journal, or the paired allow fixture above ceases to resolve.
#[tokio::test]
async fn decision_timeout_falls_through_to_byte_identical_ask() {
    let fixture = EngineFixture::start(&delayed_emit_command("allow"), 20, true, "exec").await;
    let baseline = durable_ask_bytes_without_hooks(
        &fixture.workspace,
        broker_permission_menu(&fixture.workspace).await,
    )
    .await;
    fixture
        .append_permission(broker_permission_menu(&fixture.workspace).await)
        .await;
    let events = wait_for(&fixture, |events| {
        events.iter().any(|event| {
            HookEventPayload::from_payload_value(event.payload.clone()).is_ok_and(|payload| {
                matches!(payload, HookEventPayload::HookFired(ref fired) if fired.timed_out)
            })
        })
    })
    .await;
    assert!(!events.iter().any(|event| {
        matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::MenuAnswered(_))
        )
    }));
    let ask = events
        .iter()
        .find(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::MenuOpened(_))
            )
        })
        .expect("durable Ask");
    assert_eq!(
        serde_json::to_vec(&ask.payload).expect("ask bytes"),
        baseline
    );
    fixture.close().await;
}

/// MUTATION CHECK: dispatch before commit or call the hook observer on an
/// append error. Expected RUNTIME failure: the marker is created by the
/// deliberately rejected envelope.
#[tokio::test]
async fn matcher_fires_only_after_commit() {
    let marker_dir = tempfile::tempdir().expect("marker dir");
    let marker = marker_dir.path().join("fired");
    let command = write_command("fired", &marker);
    let fixture = EngineFixture::start(&command, 1_000, false, "exec").await;
    let mut seed = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "duplicate-hook-event",
        EventPayload::IdleDecayed,
    )];
    fixture
        .hub
        .append(&mut seed)
        .await
        .expect("seed duplicate id");
    let mut rejected = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "duplicate-hook-event",
        EventPayload::RunState(RunState::Thinking),
    )];
    assert!(fixture.hub.append(&mut rejected).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!marker.exists());
    let mut committed = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "committed-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture
        .hub
        .append(&mut committed)
        .await
        .expect("commit fact");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("hook marker");
    fixture.close().await;
}

/// MUTATION CHECK: consult mutable provider trust again while handling the
/// committed event, or let trusted hooks bypass the provider ceiling.
/// Expected failure: the marker is created, the typed refusal disappears, or
/// changing the proposal widens the already-bound run.
#[tokio::test]
async fn lockdown_run_binding_suppresses_hooks_until_the_next_run() {
    let marker_dir = tempfile::tempdir().expect("marker dir");
    let marker = marker_dir.path().join("lockdown-hook-fired");
    let command = write_command("forbidden", &marker);
    let fixture = EngineFixture::start_user_message(&command).await;
    assert!(
        fixture
            .hub
            .bind_lockdown_turn(&fixture.session_id, &fixture.run_id, "fake", true)
            .expect("bind lockdown turn")
    );
    assert!(
        fixture
            .hub
            .bind_lockdown_turn(&fixture.session_id, &fixture.run_id, "fake", false)
            .expect("reuse lockdown turn"),
        "a trust toggle must not widen the in-flight run"
    );
    fixture
        .accept_user_message(
            "lockdown-hook",
            "research only",
            DeliveryMode::Steer,
            Vec::new(),
        )
        .await;
    wait_for_hook_outbox_drain(&fixture.store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!marker.exists());
    assert!(fixture.events().await.iter().any(|event| {
        matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::LockdownRefused(refusal))
                if refusal.provider == "fake" && refusal.tool == "hooks"
        )
    }));
    assert!(
        !fixture
            .hub
            .bind_lockdown_turn(
                &fixture.session_id,
                &RunId::new("hooks-test-next-run"),
                "fake",
                false,
            )
            .expect("bind next Full turn"),
        "the changed policy takes effect at the next run boundary"
    );
    fixture.close().await;
}

/// MUTATION CHECK: drop the drain-cycle acknowledgement flush, or flush a
/// cycle's rows before handling them. Expected RUNTIME failure: live-handled
/// rows stay pending below forever — or the markers appear while their rows
/// were already gone from the outbox (unhandled work a crash would lose).
#[tokio::test]
async fn live_drain_cycle_acknowledges_handled_rows_in_one_batch() {
    let marker_dir = tempfile::tempdir().expect("marker dir");
    let marker = marker_dir.path().join("live-batch-lines");
    let command = append_marker_command(&marker);
    let fixture = EngineFixture::start(&command, 1_000, false, "exec").await;
    for id in ["live-batch-1", "live-batch-2", "live-batch-3"] {
        let mut batch = [raw_event(
            &fixture.session_id,
            &fixture.run_id,
            fixture.store.worker_generation(),
            id,
            EventPayload::RunState(RunState::Thinking),
        )];
        fixture.hub.append(&mut batch).await.expect("commit fact");
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while marker_lines(&marker) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        while !fixture
            .store
            .pending_hook_dispatches(16)
            .await
            .expect("pending")
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("live drain acknowledges every handled row");
    assert_eq!(marker_lines(&marker), 3, "each committed fact fired once");
    fixture.close().await;
}

/// MUTATION CHECK: omit the transaction-coupled hook outbox or acknowledge a
/// fact before its hook work completes. Expected RUNTIME failure: the
/// committed pre-crash RunStarted fact survives but never creates the marker
/// when the hook engine starts after the simulated crash boundary.
#[tokio::test]
async fn committed_fact_survives_crash_before_publish_and_fires_on_recovery() {
    let profile_guard = tempfile::tempdir().expect("profile");
    let workspace_guard = tempfile::tempdir().expect("workspace");
    let marker_guard = tempfile::tempdir().expect("marker");
    let profile = canonical(profile_guard.path());
    let workspace = canonical(workspace_guard.path());
    let marker = marker_guard.path().join("recovered-fired");
    write_profile_policy(&profile, "per_digest");
    write_hook(
        &workspace,
        "recovery_hook",
        "run_started",
        &write_command("recovered", &marker),
        1_000,
        false,
        "exec",
    );
    let session_id = SessionId::new("hooks-recovery-session");
    let run_id = RunId::new("hooks-recovery-run");

    let store = SqliteStoreHandle::open(&profile).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let (service, engine) = HookEngine::start(profile.clone(), store.clone(), hub.clone())
        .await
        .expect("engine");
    hub.install_hooks(service.clone()).expect("install");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-hooks-recovery".into(),
        request_digest: "create-hooks-recovery-digest".into(),
        request_json: r#"{"session":"hooks-recovery"}"#.into(),
        session_id: session_id.clone(),
        cwd: workspace.to_str().expect("UTF-8").to_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "hooks-test-v1".into(),
        event_id: EventId::new("hooks-recovery-created"),
        device_id: DeviceId::new("hooks-test-device"),
    })
    .await
    .expect("create");
    let digest = service.list(workspace.clone()).await.expect("list").2[0]
        .digest
        .clone();
    service
        .apply_trust(CommandId::new("trust-hooks-recovery"), digest, true)
        .await
        .expect("trust");
    engine.shutdown().await;
    hub.shutdown().await.expect("hub shutdown");
    drop(service);

    let mut survived = [raw_event(
        &session_id,
        &run_id,
        store.worker_generation(),
        "hooks-recovery-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    store
        .append(&mut survived)
        .await
        .expect("commit without live observer");
    assert!(!marker.exists());
    store.close().await.expect("close crashed generation");

    let store = SqliteStoreHandle::open(&profile)
        .await
        .expect("reopen store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("reopen hub");
    let (service, engine) = HookEngine::start(profile, store.clone(), hub.clone())
        .await
        .expect("recovery engine");
    hub.install_hooks(service).expect("reinstall");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recovered hook marker");
    engine.shutdown().await;
    hub.shutdown().await.expect("reopen hub shutdown");
    store.close().await.expect("reopen store close");
}

/// MUTATION CHECK: acknowledge a replay page before handling it, skip the
/// per-page flush, or replay acknowledged rows too. Expected RUNTIME failure:
/// recovery fires a marker for the pre-acknowledged fact, misses one of the
/// two unacknowledged facts, or leaves outbox rows pending after the drain.
#[tokio::test]
async fn recovery_replays_exactly_the_unacknowledged_rows() {
    let profile_guard = tempfile::tempdir().expect("profile");
    let workspace_guard = tempfile::tempdir().expect("workspace");
    let marker_guard = tempfile::tempdir().expect("marker");
    let profile = canonical(profile_guard.path());
    let workspace = canonical(workspace_guard.path());
    let marker = marker_guard.path().join("replayed-lines");
    write_profile_policy(&profile, "per_digest");
    write_hook(
        &workspace,
        "exact_replay_hook",
        "run_started",
        &append_marker_command(&marker),
        1_000,
        false,
        "exec",
    );
    let session_id = SessionId::new("hooks-exact-replay-session");
    let run_id = RunId::new("hooks-exact-replay-run");

    let store = SqliteStoreHandle::open(&profile).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let (service, engine) = HookEngine::start(profile.clone(), store.clone(), hub.clone())
        .await
        .expect("engine");
    hub.install_hooks(service.clone()).expect("install");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-hooks-exact-replay".into(),
        request_digest: "create-hooks-exact-replay-digest".into(),
        request_json: r#"{"session":"hooks-exact-replay"}"#.into(),
        session_id: session_id.clone(),
        cwd: workspace.to_str().expect("UTF-8").to_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "hooks-test-v1".into(),
        event_id: EventId::new("hooks-exact-replay-created"),
        device_id: DeviceId::new("hooks-test-device"),
    })
    .await
    .expect("create");
    let digest = service.list(workspace.clone()).await.expect("list").2[0]
        .digest
        .clone();
    service
        .apply_trust(CommandId::new("trust-hooks-exact-replay"), digest, true)
        .await
        .expect("trust");
    engine.shutdown().await;
    hub.shutdown().await.expect("hub shutdown");
    drop(service);

    // Three facts committed with no live engine: three undrained outbox rows.
    let mut seqs = Vec::new();
    for id in [
        "hooks-exact-replay-1",
        "hooks-exact-replay-2",
        "hooks-exact-replay-3",
    ] {
        let mut batch = [raw_event(
            &session_id,
            &run_id,
            store.worker_generation(),
            id,
            EventPayload::RunState(RunState::Thinking),
        )];
        store
            .append(&mut batch)
            .await
            .expect("commit without live observer");
        seqs.push(batch[0].seq);
    }
    assert!(!marker.exists());
    store.close().await.expect("close crashed generation");

    // The middle row was handled and batch-acknowledged before the crash.
    let store = SqliteStoreHandle::open(&profile)
        .await
        .expect("reopen store");
    store
        .complete_hook_dispatches(vec![(session_id.clone(), seqs[1])])
        .await
        .expect("pre-ack middle row");
    let one_oversized = store
        .pending_hook_dispatches_bounded(16, 1)
        .await
        .expect("byte-bounded recovery page");
    assert_eq!(
        one_oversized
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        [seqs[0]],
        "a page smaller than one envelope still makes FIFO progress"
    );

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("reopen hub");
    let (service, engine) = HookEngine::start(profile, store.clone(), hub.clone())
        .await
        .expect("recovery engine");
    hub.install_hooks(service).expect("reinstall");
    tokio::time::timeout(Duration::from_secs(5), async {
        while marker_lines(&marker) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        while !store
            .pending_hook_dispatches(16)
            .await
            .expect("pending")
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recovery drains the two unacknowledged rows");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        marker_lines(&marker),
        2,
        "the pre-acknowledged fact must not refire"
    );
    engine.shutdown().await;
    hub.shutdown().await.expect("reopen hub shutdown");
    store.close().await.expect("reopen store close");
}

/// MUTATION CHECK: hydrate only profile trust receipts and ignore the durable
/// HookRunTrust fact. Expected RUNTIME failure: the unpinned hook cannot run
/// after restart even though the run-scoped authority and RunStarted fact
/// committed in the same pre-crash batch.
#[tokio::test]
async fn run_scoped_hook_trust_is_reduced_before_recovery_dispatch() {
    let profile_guard = tempfile::tempdir().expect("profile");
    let workspace_guard = tempfile::tempdir().expect("workspace");
    let marker_guard = tempfile::tempdir().expect("marker");
    let profile = canonical(profile_guard.path());
    let workspace = canonical(workspace_guard.path());
    let marker = marker_guard.path().join("run-trust-fired");
    write_profile_policy(&profile, "per_digest");
    write_hook(
        &workspace,
        "run_trust_hook",
        "run_started",
        &write_command("scoped", &marker),
        1_000,
        false,
        "exec",
    );
    let session_id = SessionId::new("hooks-run-trust-session");
    let run_id = RunId::new("hooks-run-trust-run");
    let store = SqliteStoreHandle::open(&profile).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-hooks-run-trust".into(),
        request_digest: "create-hooks-run-trust-digest".into(),
        request_json: r#"{"session":"hooks-run-trust"}"#.into(),
        session_id: session_id.clone(),
        cwd: workspace.to_str().expect("UTF-8").to_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "hooks-test-v1".into(),
        event_id: EventId::new("hooks-run-trust-created"),
        device_id: DeviceId::new("hooks-test-device"),
    })
    .await
    .expect("create");
    hub.shutdown().await.expect("hub shutdown");
    let generation = store.worker_generation();
    let mut committed = [
        raw_hook_event(
            &session_id,
            &run_id,
            generation,
            "hooks-run-trust-authority",
            HookEventPayload::HookRunTrust { enabled: true },
        ),
        raw_event(
            &session_id,
            &run_id,
            generation,
            "hooks-run-trust-thinking",
            EventPayload::RunState(RunState::Thinking),
        ),
    ];
    store
        .append(&mut committed)
        .await
        .expect("atomic run trust and fact");
    store
        .complete_hook_dispatch(&session_id, committed[0].seq)
        .await
        .expect("authority was handled before crash");
    let mut terminal = [raw_event(
        &session_id,
        &run_id,
        generation,
        "hooks-run-trust-done",
        EventPayload::RunState(RunState::Done),
    )];
    store.append(&mut terminal).await.expect("commit terminal");
    store
        .complete_hook_dispatch(&session_id, terminal[0].seq)
        .await
        .expect("terminal was handled before crash");
    store.close().await.expect("close crashed generation");

    let store = SqliteStoreHandle::open(&profile)
        .await
        .expect("reopen store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("reopen hub");
    let (service, engine) = HookEngine::start(profile, store.clone(), hub.clone())
        .await
        .expect("engine");
    hub.install_hooks(service).expect("install");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run trust recovery marker");
    engine.shutdown().await;
    hub.shutdown().await.expect("reopen hub shutdown");
    store.close().await.expect("reopen store close");
}

/// MUTATION CHECK: trust by hook name/path or skip the pre-spawn digest
/// re-check. Expected RUNTIME failure: an edited command executes under the
/// old pin instead of producing an honest untrusted notice.
#[tokio::test]
async fn digest_change_revokes_trust_before_fire() {
    let marker_dir = tempfile::tempdir().expect("marker dir");
    let marker = marker_dir.path().join("changed-fired");
    let fixture = EngineFixture::start("printf original", 1_000, false, "exec").await;
    write_hook(
        &fixture.workspace,
        "test_hook",
        "run_started",
        &write_command("changed", &marker),
        1_000,
        false,
        "exec",
    );
    let mut event = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "digest-changed-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture.hub.append(&mut event).await.expect("commit fact");
    let events = wait_for(&fixture, |events| {
        events.iter().any(|event| {
            matches!(
                HookEventPayload::from_payload_value(event.payload.clone()),
                Ok(HookEventPayload::HookNotice(ref notice))
                    if notice.reason.contains("untrusted")
            )
        })
    })
    .await;
    assert!(!marker.exists());
    assert!(events.iter().any(|event| {
        matches!(
            HookEventPayload::from_payload_value(event.payload.clone()),
            Ok(HookEventPayload::HookNotice(_))
        )
    }));
    fixture.close().await;
}

/// MUTATION CHECK: implement trust_workspace as a mutable path/name allowlist
/// or keep its first digest only in memory. Expected RUNTIME failure: editing
/// the command remains trusted now or becomes trusted after daemon reopen.
#[tokio::test]
async fn trust_workspace_pins_its_first_digest_across_restart() {
    let profile_guard = tempfile::tempdir().expect("profile");
    let workspace_guard = tempfile::tempdir().expect("workspace");
    let profile = canonical(profile_guard.path());
    let workspace = canonical(workspace_guard.path());
    write_profile_policy(&profile, "trust_workspace");
    write_hook(
        &workspace,
        "workspace_hook",
        "run_started",
        "printf original",
        1_000,
        false,
        "exec",
    );
    let store = SqliteStoreHandle::open(&profile).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let (service, engine) = HookEngine::start(profile.clone(), store.clone(), hub.clone())
        .await
        .expect("engine");
    hub.install_hooks(service.clone()).expect("install");
    assert!(
        service.list(workspace.clone()).await.expect("list").2[0].trusted,
        "first workspace digest is policy-pinned"
    );
    write_hook(
        &workspace,
        "workspace_hook",
        "run_started",
        "printf changed",
        1_000,
        false,
        "exec",
    );
    assert!(
        !service
            .list(workspace.clone())
            .await
            .expect("changed list")
            .2[0]
            .trusted,
        "changed digest is revoked"
    );
    engine.shutdown().await;
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");

    let store = SqliteStoreHandle::open(&profile)
        .await
        .expect("reopen store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("reopen hub");
    let (service, engine) = HookEngine::start(profile.clone(), store.clone(), hub.clone())
        .await
        .expect("reopen engine");
    hub.install_hooks(service.clone()).expect("reinstall");
    assert!(
        !service.list(workspace).await.expect("reopen list").2[0].trusted,
        "restart must not bless the edited digest"
    );
    engine.shutdown().await;
    hub.shutdown().await.expect("reopen hub shutdown");
    store.close().await.expect("reopen store close");
}

fn standalone_definition(workspace: &Path, command: String) -> HookDefinition {
    HookDefinition {
        name: "standalone".into(),
        matcher: HookMatcher {
            event: MatchEvent::RunStarted,
            session: None,
            provider: None,
            outcome: None,
            parked_kind: None,
            mode: None,
            has_attachments: None,
        },
        kind: HookKind::Exec,
        command,
        timeout: Duration::from_secs(5),
        decision: false,
        digest: "0".repeat(64),
        source_path: workspace.join("hooks.json"),
        source: HookSource::Workspace,
        workspace_cwd: workspace.to_path_buf(),
    }
}

/// Windows Job Objects must remain authoritative after the command
/// interpreter exits. The descendant inherits the output handles, so waiting
/// for pipe EOF before sweeping would also turn this into a multi-second hang.
#[cfg(windows)]
#[tokio::test]
async fn hook_natural_leader_exit_sweeps_live_descendant_before_output_drain() {
    let profile = tempfile::tempdir().expect("profile");
    let workspace_directory = tempfile::tempdir().expect("workspace");
    let fixture_directory = workspace_directory.path().to_path_buf();
    let workspace = canonical(&fixture_directory);
    let ready = fixture_directory.join("descendant-ready.txt");
    let survived = fixture_directory.join("escaped-descendant.txt");
    let command = exiting_leader_tree_command(&fixture_directory, &ready, &survived);
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let result = run_command(
        &standalone_definition(&workspace, command),
        b"{}",
        &store,
        tokio::sync::watch::channel(false).1,
    )
    .await;
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    assert!(ready.exists(), "fixture never launched its descendant");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !survived.exists(),
        "leader descendant escaped its Job Object"
    );
    store.close().await.expect("store close");
}

/// MUTATION CHECK: remove the fd sweep or inherit the parent environment.
/// Expected RUNTIME failure: the live hook observes descriptor 333, HOME, or
/// another non-allowlisted variable.
#[tokio::test]
#[cfg(unix)]
async fn hook_spawn_is_live_but_inherits_no_descriptors_or_secret_environment() {
    let sentinel = "H2_VAULT_SENTINEL_7d3930b1";
    let child = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .args([
            "--exact",
            "hooks::tests::hook_secret_child_probe",
            "--nocapture",
        ])
        .env("HAIDER_HOOK_VAULT_SENTINEL", sentinel)
        .status()
        .expect("run isolated secret probe");
    assert!(child.success(), "isolated secret probe failed: {child}");

    let profile = tempfile::tempdir().expect("profile");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace = canonical(workspace.path());
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let (mine, theirs) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    let planted = rustix::io::fcntl_dupfd_cloexec(&theirs, 333).expect("plant high fd");
    rustix::io::fcntl_setfd(&planted, rustix::io::FdFlags::empty()).expect("clear CLOEXEC");
    drop(theirs);
    let raw_fd = std::os::fd::AsRawFd::as_raw_fd(&planted);
    let command = format!(
        "if kill -0 $$ && [ ! -e /dev/fd/{raw_fd} ]; then printf alive-clean; else printf leaked; fi; printf '\\n'; env"
    );
    let result = run_command(
        &standalone_definition(&workspace, command),
        b"{}",
        &store,
        tokio::sync::watch::channel(false).1,
    )
    .await;
    assert_eq!(result.exit_code, Some(0));
    let mut lines = result.stdout.preview.lines();
    assert_eq!(lines.next(), Some("alive-clean"));
    for line in lines {
        let name = line.split_once('=').map_or(line, |(name, _)| name);
        assert!(
            [
                "PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "PWD", "SHLVL", "_"
            ]
            .contains(&name),
            "unexpected inherited environment variable {name}"
        );
    }
    assert!(
        !result
            .stdout
            .preview
            .lines()
            .any(|line| line.starts_with("HOME="))
    );
    drop(planted);
    drop(mine);
    store.close().await.expect("store close");
}

/// Isolated child half of the secret-byte fixture above. Running this test in
/// the ordinary suite is a no-op; the parent re-executes it with a planted
/// vault sentinel so no process-global environment mutation is required.
#[test]
fn hook_secret_child_probe() {
    let Ok(sentinel) = std::env::var("HAIDER_HOOK_VAULT_SENTINEL") else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().expect("probe runtime");
    runtime.block_on(async {
        let profile = tempfile::tempdir().expect("profile");
        let workspace = tempfile::tempdir().expect("workspace");
        let workspace = canonical(workspace.path());
        let store = SqliteStoreHandle::open(profile.path())
            .await
            .expect("store");
        let result = run_command(
            &standalone_definition(&workspace, "env".into()),
            b"{}",
            &store,
            tokio::sync::watch::channel(false).1,
        )
        .await;
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.stdout.preview.contains(&sentinel));
        assert!(
            !result
                .stdout
                .preview
                .contains("HAIDER_HOOK_VAULT_SENTINEL=")
        );
        store.close().await.expect("store close");
    });
}

/// MUTATION CHECK: retain unbounded output, omit the CAS spill, replace the
/// retained bytes, or fail to journal the bounded result. Expected RUNTIME
/// failure: the durable HookFired fields, exact artifact, or preview cap
/// differs from the asserted values.
#[tokio::test]
async fn exec_output_is_bounded_and_overflow_is_in_cas() {
    let output_fixture = tempfile::tempdir().expect("bounded output fixture");
    let output_path = output_fixture.path().join("output.bin");
    std::fs::write(&output_path, b"x\n".repeat(300_000)).expect("write bounded output fixture");
    let fixture = EngineFixture::start(
        &bounded_output_command(&output_path),
        BOUNDED_OUTPUT_TIMEOUT_MS,
        false,
        "exec",
    )
    .await;
    let mut event = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "bounded-output-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture.hub.append(&mut event).await.expect("commit fact");
    let events = wait_for_with_timeout(&fixture, BOUNDED_OUTPUT_OBSERVATION_TIMEOUT, |events| {
        events.iter().any(|event| {
            matches!(
                HookEventPayload::from_payload_value(event.payload.clone()),
                Ok(HookEventPayload::HookFired(ref fired))
                    if fired.observed_seq == event.seq.saturating_sub(1)
            )
        })
    })
    .await;
    let fired = events
        .iter()
        .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
        .find_map(|payload| match payload {
            HookEventPayload::HookFired(fired) if fired.hook == "test_hook" => Some(fired),
            _ => None,
        })
        .expect("durable HookFired");
    assert!(fired.stdout.truncated);
    assert_eq!(fired.stdout.bytes, 512 * 1024);
    assert!(fired.stdout.preview.len() <= 8 * 1024);
    let artifact = fired.stdout.artifact.expect("CAS overflow");
    let retained = fixture.store.get(&artifact).await.expect("CAS bytes");
    assert_eq!(retained, b"x\n".repeat(256 * 1024));
    fixture.close().await;
}

/// MUTATION CHECK: bound raw preview bytes before lossy UTF-8 conversion but
/// forget that replacement characters expand. Expected RUNTIME failure: the
/// invalid-byte preview exceeds 8192 UTF-8 bytes or its exact raw bytes are
/// not retained in CAS.
#[tokio::test]
async fn invalid_utf8_preview_stays_within_the_byte_cap() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let raw = vec![0xff; 8 * 1024];
    let output = make_output(
        &store,
        CapturedBytes {
            bytes: raw.clone(),
            truncated: false,
        },
    )
    .await;
    assert!(output.preview.len() <= 8 * 1024);
    assert!(output.truncated);
    let artifact = output.artifact.expect("expanded preview CAS spill");
    assert_eq!(store.get(&artifact).await.expect("raw CAS bytes"), raw);
    store.close().await.expect("store close");
}

/// MUTATION CHECK: remove exponential growth or exceed the 5s cap. Expected
/// RUNTIME failure: the first two restart-scheduled facts are not 200/400ms
/// or any durable schedule exceeds the literal upper bound.
#[tokio::test]
async fn subscribe_restart_backoff_is_exponential_and_bounded() {
    let fixture = EngineFixture::start("exit 7", 1_000, false, "subscribe").await;
    let mut event = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "subscriber-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture.hub.append(&mut event).await.expect("commit fact");
    let events = wait_for(&fixture, |events| {
        events
            .iter()
            .filter_map(|event| {
                let HookEventPayload::HookSubscription(subscription) =
                    HookEventPayload::from_payload_value(event.payload.clone()).ok()?
                else {
                    return None;
                };
                (subscription.state == HookSubscriptionState::RestartScheduled)
                    .then_some(subscription)
            })
            .count()
            >= 2
    })
    .await;
    let schedules = events
        .iter()
        .filter_map(|event| {
            let HookEventPayload::HookSubscription(subscription) =
                HookEventPayload::from_payload_value(event.payload.clone()).ok()?
            else {
                return None;
            };
            (subscription.state == HookSubscriptionState::RestartScheduled).then_some(subscription)
        })
        .collect::<Vec<HookSubscription>>();
    assert_eq!(schedules[0].backoff_ms, Some(200));
    assert_eq!(schedules[1].backoff_ms, Some(400));
    assert!(
        schedules
            .iter()
            .all(|schedule| schedule.backoff_ms.is_some_and(|delay| delay <= 5_000))
    );
    fixture.close().await;
}

/// MUTATION CHECK: remove or raise the literal 5000-ms cap. Expected RUNTIME
/// failure: the extracted schedule no longer reaches 5000 and stays there on
/// the next restart.
#[test]
fn subscribe_restart_schedule_reaches_and_holds_the_five_second_cap() {
    let mut delay = Duration::from_millis(200);
    let mut schedule = vec![delay.as_millis()];
    for _ in 0..6 {
        delay = next_subscriber_backoff(delay);
        schedule.push(delay.as_millis());
    }
    assert_eq!(schedule, vec![200, 400, 800, 1_600, 3_200, 5_000, 5_000]);
}

/// MUTATION CHECK: kill only the subscriber shell or rely on Child drop.
/// Expected RUNTIME failure: the background process-group member writes its
/// delayed marker after the digest is revoked.
#[tokio::test]
async fn subscribe_revoke_kills_the_entire_process_group() {
    let marker_guard = tempfile::tempdir().expect("marker");
    let ready = marker_guard.path().join("ready");
    let survived = marker_guard.path().join("survived");
    let command = subscriber_tree_command(&ready, &survived);
    let fixture = EngineFixture::start(&command, 2_000, false, "subscribe").await;
    let mut event = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        fixture.store.worker_generation(),
        "subscriber-revoke-thinking",
        EventPayload::RunState(RunState::Thinking),
    )];
    fixture.hub.append(&mut event).await.expect("commit fact");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("subscriber readiness");
    let digest = fixture
        .service
        .list(fixture.workspace.clone())
        .await
        .expect("list")
        .2[0]
        .digest
        .clone();
    fixture
        .service
        .apply_trust(CommandId::new("revoke-live-subscriber"), digest, false)
        .await
        .expect("revoke");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!survived.exists(), "subscriber descendant survived revoke");
    fixture.close().await;
}

/// MUTATION CHECK: silently ignore subscribe hooks under --trust-hooks or
/// let their process live beyond the authorized run. Expected RUNTIME
/// failure: no subscriber starts, or its delayed child writes after Done.
#[tokio::test]
async fn run_scoped_trust_authorizes_subscribe_only_for_that_run() {
    let marker_guard = tempfile::tempdir().expect("marker");
    let ready = marker_guard.path().join("ready");
    let survived = marker_guard.path().join("survived");
    let command = subscriber_tree_command(&ready, &survived);
    let fixture = EngineFixture::start_with_trust(&command, 2_000, false, "subscribe", false).await;
    let generation = fixture.store.worker_generation();
    let mut started = [
        raw_hook_event(
            &fixture.session_id,
            &fixture.run_id,
            generation,
            "subscriber-run-trust",
            HookEventPayload::HookRunTrust { enabled: true },
        ),
        raw_event(
            &fixture.session_id,
            &fixture.run_id,
            generation,
            "subscriber-run-thinking",
            EventPayload::RunState(RunState::Thinking),
        ),
    ];
    fixture.hub.append(&mut started).await.expect("start run");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run subscriber readiness");
    let mut done = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        generation,
        "subscriber-run-done",
        EventPayload::RunState(RunState::Done),
    )];
    fixture.hub.append(&mut done).await.expect("finish run");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!survived.exists(), "run-scoped subscriber outlived run");
    fixture.close().await;
}

/// Keep these private variants visibly exercised by the separate test module.
#[test]
fn hook_runtime_and_policy_literals_are_stable() {
    assert_eq!(HookRuntimeKind::Decision, HookRuntimeKind::Decision);
    assert_eq!(HookTrustPolicy::TrustNone.as_str(), "trust_none");
    assert_eq!(HookTrustPolicy::PerDigest.as_str(), "per_digest");
    assert_eq!(HookTrustPolicy::TrustWorkspace.as_str(), "trust_workspace");
}

/// MUTATION CHECK: drop the digest-match half of `definition_current` (keep
/// only `is_trusted`). Expected RUNTIME failure: with TWO pinned digests, a
/// hook file swapped between match and fire re-verifies as current even
/// though the matched definition and the fire-time definition differ — the
/// wrong (albeit trusted) command would run for the matched event.
#[tokio::test]
async fn fire_time_reverification_refuses_a_swapped_pinned_definition() {
    let fixture = EngineFixture::start("printf first", 1_000, false, "exec").await;
    let profile_root = fixture._profile_guard.path().to_path_buf();
    let matched = crate::hooks::discover_async(fixture.workspace.clone(), profile_root.clone())
        .await
        .expect("discover matched")
        .hooks
        .get("test_hook")
        .cloned()
        .expect("matched definition");

    // Swap the file to a DIFFERENT command and pin the new digest too —
    // both definitions are individually trusted.
    write_hook(
        &fixture.workspace,
        "test_hook",
        "run_started",
        "printf second",
        1_000,
        false,
        "exec",
    );
    let swapped = crate::hooks::discover_async(fixture.workspace.clone(), profile_root)
        .await
        .expect("discover swapped")
        .hooks
        .get("test_hook")
        .cloned()
        .expect("swapped definition");
    fixture
        .service
        .apply_trust(
            CommandId::new("trust-swap-pin"),
            swapped.digest.clone(),
            true,
        )
        .await
        .expect("pin swapped digest");
    assert_ne!(matched.digest, swapped.digest);

    // The MATCHED (pre-swap) definition must fail fire-time
    // re-verification even though both digests are pinned.
    assert!(
        !crate::hooks::definition_current(&fixture.service, &matched, false).await,
        "a swapped definition must not re-verify as current"
    );
    // The swapped definition itself is honestly current.
    assert!(crate::hooks::definition_current(&fixture.service, &swapped, false).await);
    fixture.close().await;
}

// ── hooks_server_v1 lifecycle (v0.0.934) ────────────────────────────────

fn write_server_hook(workspace: &Path, command: &str, idle_timeout_ms: u64) {
    // A cold PowerShell process can take more than two seconds to reach its
    // first JSONL response under the full Windows per-crate test load. This is
    // fixture setup latency, not the server lifecycle behavior under test.
    let timeout_ms = if cfg!(windows) { 10_000 } else { 2_000 };
    std::fs::write(
        workspace.join("hooks.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "haider.hooks.v1",
            "hooks": {
                "test_hook": {
                    "matcher": {"event": "user_message"},
                    "kind": "exec",
                    "command": command,
                    "timeout_ms": timeout_ms,
                    "decision": false,
                    "mode": "server",
                    "idle_timeout_ms": idle_timeout_ms,
                }
            }
        }))
        .expect("server hooks JSON"),
    )
    .expect("write server hooks");
}

async fn server_fixture(
    idle_timeout_ms: u64,
    kind: TestServerKind,
) -> (EngineFixture, std::path::PathBuf) {
    let fixture = EngineFixture::start_with_event_and_trust(
        "printf placeholder",
        2_000,
        false,
        "exec",
        false,
        "run_started",
    )
    .await;
    // The hook engine keeps a canonical workspace for its cwd identity, but a
    // fixture path embedded in shell command text needs the spelling accepted
    // by that shell. In particular, Windows canonicalization produces a
    // `\\?\` verbatim path, which `cmd.exe` does not reliably accept on its
    // command line or in batch-file redirections. The retained tempfile path
    // names the same directory without crossing that API/shell boundary.
    let fixture_workspace = fixture._workspace_guard.path();
    let spawn_log = fixture_workspace.join("spawns.log");
    let command = server_command(fixture_workspace, &spawn_log, kind);
    write_server_hook(&fixture.workspace, &command, idle_timeout_ms);
    let (_, _, hooks) = fixture
        .service
        .list(fixture.workspace.clone())
        .await
        .expect("list server hook");
    let digest = hooks
        .first()
        .expect("discovered server hook")
        .digest
        .clone();
    fixture
        .service
        .apply_trust(CommandId::new("trust-server-hook"), digest, true)
        .await
        .expect("trust server hook");
    (fixture, spawn_log)
}

#[derive(Clone, Copy)]
enum TestServerKind {
    Resident,
    OneShot,
}

#[cfg(unix)]
fn server_command(_workspace: &Path, spawn_log: &Path, kind: TestServerKind) -> String {
    let prefix = format!("printf 'spawn\\n' >> '{}'", spawn_log.display());
    match kind {
        TestServerKind::Resident => {
            format!("{prefix}; while IFS= read -r line; do printf '\"ok\"\\n'; done")
        }
        TestServerKind::OneShot => {
            format!("{prefix}; IFS= read -r line; printf '\"ok\"\\n'")
        }
    }
}

#[cfg(windows)]
fn server_command(_workspace: &Path, spawn_log: &Path, kind: TestServerKind) -> String {
    let spawn_log = spawn_log.display().to_string().replace('\'', "''");
    // Lane 953j: cmd.exe's interactive `set /p` did not remain a reusable
    // redirected JSONL reader across events, so the correct engine eventually
    // saw an exited fixture and respawned it. Use an actual stream reader whose
    // ReadLine blocks for every event and returns null only when stdin closes.
    let prefix = format!(
        "$log='{spawn_log}';[IO.File]::AppendAllText($log,('spawn'+[char]10),[Text.Encoding]::ASCII);\
         $stdin=[IO.StreamReader]::new([Console]::OpenStandardInput(),[Text.Encoding]::UTF8,$false,4096,$true);"
    );
    let respond = "[Console]::Out.WriteLine('\"ok\"');[Console]::Out.Flush()";
    let script = match kind {
        TestServerKind::Resident => {
            format!("{prefix}while($null -ne $stdin.ReadLine()){{{respond}}}")
        }
        TestServerKind::OneShot => {
            format!("{prefix}if($null -ne $stdin.ReadLine()){{{respond}}}")
        }
    };
    powershell_command(&script)
}

/// Counts spawns recorded in the fixture's log.
///
/// This used to end in `.unwrap_or(0)`, which conflated TWO different facts: the
/// hook server never spawned, and the log could not be read. Those are opposite
/// diagnoses and the assertion reported both as `left: 0`.
///
/// That matters on Windows specifically, where the log is appended by a child
/// process and can be locked or unflushed when the parent reads it. Three
/// `server_mode_*` tests fail there with `left: 0, right: 2`, and with the old
/// helper there was no way to tell a product failure from an unreadable file.
///
/// A read failure now panics with the OS error instead of masquerading as zero.
/// A test that cannot observe the thing it asserts on must say so, not guess.
fn spawn_count(log: &Path) -> usize {
    match std::fs::read_to_string(log) {
        Ok(content) => content.lines().count(),
        // Not-found is a REAL zero: the fixture creates the log lazily on first
        // spawn, so its absence means nothing was spawned.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!(
            "spawn log at {} could not be read: {error} (kind {:?}). \
             This is NOT the same as zero spawns — the assertion that follows \
             would have reported it as `left: 0` and blamed the product.",
            log.display(),
            error.kind()
        ),
    }
}

async fn fired_count(fixture: &EngineFixture) -> usize {
    fixture
        .events()
        .await
        .iter()
        .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
        .filter(|payload| matches!(payload, HookEventPayload::HookFired(_)))
        .count()
}

async fn successful_server_fire_count(fixture: &EngineFixture) -> usize {
    fixture
        .events()
        .await
        .iter()
        .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
        .filter(|payload| {
            matches!(
                payload,
                HookEventPayload::HookFired(fired)
                    if fired.exit_code == Some(0) && !fired.timed_out
            )
        })
        .count()
}

// Outer TEST observation budget only. The fixture exchange timeout above and
// the product-owned idle timeout remain independently bounded; full-suite
// scheduling and SQLite/process I/O may legitimately delay observation.
const SERVER_MODE_OBSERVATION_TIMEOUT: Duration =
    Duration::from_secs(if cfg!(windows) { 30 } else { 15 });

fn server_test_state(fixture: &EngineFixture) -> Arc<super::hooks_server::ServerTestState> {
    fixture
        .service
        .inner
        .servers
        .only_test_state()
        .expect("fixture owns exactly one server actor")
}

async fn wait_for_server_fires(fixture: &EngineFixture, expected: usize) {
    wait_for_with_timeout(fixture, SERVER_MODE_OBSERVATION_TIMEOUT, |events| {
        events
            .iter()
            .filter_map(|event| HookEventPayload::from_payload_value(event.payload.clone()).ok())
            .filter(|payload| matches!(payload, HookEventPayload::HookFired(_)))
            .count()
            >= expected
    })
    .await;
}

async fn wait_for_server_leader_exit(raw_pid: u32) {
    let pid = process_id(Some(raw_pid)).expect("valid server pid");
    tokio::time::timeout(SERVER_MODE_OBSERVATION_TIMEOUT, async {
        loop {
            match process_leader_exited(pid) {
                Ok(true) => break,
                Ok(false) => tokio::time::sleep(Duration::from_millis(10)).await,
                // Tokio may reap between observations. Not-found everywhere,
                // and ECHILD on Unix, both establish the required premise.
                Err(error) if process_observation_means_exited(&error) => break,
                Err(error) => panic!("observe server leader {raw_pid}: {error}"),
            }
        }
    })
    .await
    .expect("server leader exit observation deadline");
}

fn process_observation_means_exited(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::NotFound {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) {
        return true;
    }
    false
}

async fn wait_for_server_stopped(state: &super::hooks_server::ServerTestState) {
    tokio::time::timeout(SERVER_MODE_OBSERVATION_TIMEOUT, async {
        while state.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server actor stop observation deadline");
}

async fn fire_user_message(fixture: &EngineFixture, index: usize) {
    let generation = fixture.store.worker_generation();
    let mut events = [raw_event(
        &fixture.session_id,
        &fixture.run_id,
        generation,
        &format!("server-user-{index}"),
        EventPayload::UserMessage {
            text: "fire".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
    )];
    fixture
        .hub
        .append(&mut events)
        .await
        .expect("append user message fact");
}

/// Sends distinctly identified events until one more successful server
/// exchange and its exact spawn count are observable. Spawn/setup failures
/// are facts too, so waiting for an arbitrary `HookFired` can mistake a failed
/// cold Windows launch for lifecycle progress.
async fn fire_fresh_events_until_spawn_count(
    fixture: &EngineFixture,
    spawn_log: &Path,
    next_index: &mut usize,
    expected: usize,
) {
    let successful_target = successful_server_fire_count(fixture).await + 1;
    let observed = tokio::time::timeout(SERVER_MODE_OBSERVATION_TIMEOUT, async {
        loop {
            let spawns = spawn_count(spawn_log);
            assert!(
                spawns <= expected,
                "server spawned {spawns} times before exact target {expected}"
            );
            if spawns == expected
                && successful_server_fire_count(fixture).await >= successful_target
            {
                return;
            }

            let fires_before = fired_count(fixture).await;
            fire_user_message(fixture, *next_index).await;
            *next_index += 1;
            while fired_count(fixture).await == fires_before {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    })
    .await;
    if observed.is_err() {
        let successful = successful_server_fire_count(fixture).await;
        panic!(
            "server spawn observation deadline: expected {expected}, spawns={}, \
             successful fires={successful}",
            spawn_count(spawn_log)
        );
    }
}

/// Sends exactly one event after an observed idle reap or child exit. Unlike
/// cold-launch setup retries, this must not let a later event hide a failure
/// to respawn on the very next event.
async fn fire_one_event_and_expect_spawn_count(
    fixture: &EngineFixture,
    spawn_log: &Path,
    index: usize,
    expected: usize,
) {
    let fires_before = fired_count(fixture).await;
    let successful_before = successful_server_fire_count(fixture).await;
    fire_user_message(fixture, index).await;
    let observed = tokio::time::timeout(SERVER_MODE_OBSERVATION_TIMEOUT, async {
        loop {
            let spawns = spawn_count(spawn_log);
            assert!(
                spawns <= expected,
                "server spawned {spawns} times before exact target {expected}"
            );
            if spawns == expected
                && fired_count(fixture).await > fires_before
                && successful_server_fire_count(fixture).await > successful_before
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if observed.is_err() {
        let successful = successful_server_fire_count(fixture).await;
        panic!(
            "single-event spawn observation deadline: expected {expected}, spawns={}, \
             successful fires={successful}",
            spawn_count(spawn_log)
        );
    }
}

/// MUTATION CHECK (hooks_server_v1): make server-mode dispatch spawn per
/// event like spawn mode. Expected runtime failure: the spawn log below
/// records three pids instead of one.
#[tokio::test]
async fn server_mode_spawns_once_serializes_and_dies_on_drain() {
    let (fixture, spawn_log) = server_fixture(0, TestServerKind::Resident).await;
    for index in 0..3 {
        fire_user_message(&fixture, index).await;
        wait_for_server_fires(&fixture, index + 1).await;
    }
    assert_eq!(fired_count(&fixture).await, 3, "every event got a response");
    assert_eq!(
        spawn_count(&spawn_log),
        1,
        "idle_timeout_ms=0 keeps ONE resident server across events"
    );
    let state = server_test_state(&fixture);
    assert!(
        state.is_running(),
        "the resident server is live before drain"
    );
    fixture.close().await;
    assert!(
        !state.is_running(),
        "engine drain waits until the resident server is dropped"
    );
}

/// MUTATION CHECK (hooks_server_v1): drop the actor-start shutdown check in
/// `run_server_actor` and rely on `changed()` alone — `subscribe()` marks
/// the already-flipped flag as seen. Expected runtime failure: the dispatch
/// processed after the flag flip spawns a fresh server process and the spawn
/// log below gains a line.
#[tokio::test]
async fn post_shutdown_dispatch_never_spawns_a_server_actor() {
    let (fixture, spawn_log) = server_fixture(0, TestServerKind::Resident).await;
    // Reproduce the shutdown window deterministically: the flag has flipped
    // and the registry has drained, but the engine is still draining queued
    // commits (`HookEngine::shutdown` only ENQUEUES its Shutdown message, so
    // an already-queued Committed batch is processed after both steps).
    fixture.service.inner.shutdown.send_replace(true);
    fixture.service.inner.servers.shutdown().await;
    fire_user_message(&fixture, 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        spawn_count(&spawn_log),
        0,
        "a post-shutdown dispatch must exit before its first spawn"
    );
    // The pending fire resolved through the shutdown-aware path: the engine
    // is not wedged and the ordinary drain still completes.
    fixture.close().await;
}

/// MUTATION CHECK (hooks_server_v1): drop the idle reaper (treat every
/// idle_timeout_ms as 0). Expected runtime failure: the product-synchronized
/// stop observation times out while the first server remains running; if that
/// premise were bypassed, the next event would reuse it and leave one spawn.
#[tokio::test]
async fn server_mode_reaps_idle_and_respawns_for_the_next_event() {
    let (fixture, spawn_log) = server_fixture(150, TestServerKind::Resident).await;
    let mut next_index = 0;
    fire_fresh_events_until_spawn_count(&fixture, &spawn_log, &mut next_index, 1).await;
    let state = server_test_state(&fixture);
    // The actor flips this only when its idle branch has killed and dropped
    // the process. This is the product synchronization point, not wall time.
    wait_for_server_stopped(&state).await;
    fire_one_event_and_expect_spawn_count(&fixture, &spawn_log, next_index, 2).await;
    assert_eq!(
        spawn_count(&spawn_log),
        2,
        "a clean idle reap respawns lazily on the next event"
    );
    fixture.close().await;
}

/// A server that exits after one response is a crash from the registry's
/// view: the NEXT event respawns and still succeeds — no wedged hook.
///
/// MUTATION CHECK: change the exited-child `try_wait` branch to `Ok(None)`.
/// The one allowed next event fails on the dead pipe and cannot produce the
/// exact second successful spawn before the outer deadline.
#[tokio::test]
async fn server_mode_respawns_after_the_process_exits() {
    let (fixture, spawn_log) = server_fixture(0, TestServerKind::OneShot).await;
    let mut next_index = 0;
    fire_fresh_events_until_spawn_count(&fixture, &spawn_log, &mut next_index, 1).await;
    // Read the daemon-owned PID, not a nested script's `$$`: shell tail-exec
    // is an optimization and differs across Unix variants.
    let pid = server_test_state(&fixture)
        .leader_pid()
        .expect("server leader pid");
    wait_for_server_leader_exit(pid).await;
    fire_one_event_and_expect_spawn_count(&fixture, &spawn_log, next_index, 2).await;
    assert_eq!(
        spawn_count(&spawn_log),
        2,
        "each exit is followed by a lazy respawn for the next event"
    );
    fixture.close().await;
}
