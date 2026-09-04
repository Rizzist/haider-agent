#![allow(clippy::expect_used)]

use super::{HookEngine, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use crate::session_hub::{SessionHub, SessionHubConfig};
use haider_core::{SessionCreateCommand, SqliteStoreHandle};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(unix)]
const PROCESS_SPAWN_ALLOWANCE: Duration = Duration::from_secs(5);
#[cfg(windows)]
const PROCESS_SPAWN_ALLOWANCE: Duration = Duration::from_secs(30);
const HOOK_WALL: Duration = Duration::from_secs(1);
// One process start + the configured one-second hook wall + the product's
// child-reap/stdout/stderr one-second settlement bounds + one poll. This is
// 5s + 1s + 3s + 10ms = 9.010s on Unix and 34.010s on Windows.
const OBSERVATION_TIMEOUT: Duration = PROCESS_SPAWN_ALLOWANCE
    .saturating_add(HOOK_WALL)
    .saturating_add(super::HOOK_CHILD_REAP_TIMEOUT.saturating_mul(3))
    .saturating_add(POLL_INTERVAL);

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).expect("canonical fixture path")
}

#[cfg(unix)]
fn marker_command(path: &Path) -> String {
    format!("printf fired >> '{}'", path.display())
}

#[cfg(windows)]
fn marker_command(path: &Path) -> String {
    format!(">>\"{}\" <nul set /p \"=fired\"", path.display())
}

fn write_trusted_workspace_hook(profile: &Path, workspace: &Path, marker: &Path) {
    std::fs::write(
        profile.join("hooks.json"),
        serde_json::to_vec(&json!({
            "schema": "haider.hooks.v1",
            "policy": "trust_workspace",
            "hooks": {},
        }))
        .expect("profile hook policy JSON"),
    )
    .expect("write profile hook policy");
    std::fs::write(
        workspace.join("hooks.json"),
        serde_json::to_vec(&json!({
            "schema": "haider.hooks.v1",
            "hooks": {
                "post_install": {
                    "matcher": {"event": "run_started"},
                    "kind": "exec",
                    "command": marker_command(marker),
                    "timeout_ms": 1_000,
                    "decision": false,
                }
            }
        }))
        .expect("workspace hook JSON"),
    )
    .expect("write workspace hook");
}

#[cfg(unix)]
fn subscriber_command(ready: &Path, survived: &Path) -> String {
    format!(
        "printf ready > '{}'; sleep 2; printf survived > '{}'; cat >/dev/null",
        ready.display(),
        survived.display()
    )
}

#[cfg(windows)]
fn subscriber_command(ready: &Path, survived: &Path) -> String {
    format!(
        ">\"{}\" echo ready & ping -n 3 127.0.0.1 >nul & >\"{}\" echo survived & more >nul",
        ready.display(),
        survived.display()
    )
}

fn write_trusted_subscriber(workspace: &Path, ready: &Path, survived: &Path) {
    std::fs::write(
        workspace.join("hooks.json"),
        serde_json::to_vec(&json!({
            "schema": "haider.hooks.v1",
            "hooks": {
                "removal_probe": {
                    "matcher": {"event": "run_started"},
                    "kind": "subscribe",
                    "command": subscriber_command(ready, survived),
                    "timeout_ms": 1_000,
                    "decision": false,
                }
            }
        }))
        .expect("subscriber hook JSON"),
    )
    .expect("write subscriber hook");
}

fn remove_workspace_hooks(workspace: &Path) {
    std::fs::write(
        workspace.join("hooks.json"),
        br#"{"schema":"haider.hooks.v1","hooks":{}}"#,
    )
    .expect("remove workspace hooks");
}

fn envelope(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    event_id: &str,
    payload: serde_json::Value,
) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("hook-peak-rss-test-device"),
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
        payload: payload.into(),
    }
}

/// MUTATION CHECK: restore the eager `pending_hook_dispatches_bounded` read
/// before the discovery/interest gate. Expected failure: the decode counter
/// advances while no hook exists. Removing the metadata interest classifier
/// also makes the large unrelated item/node assertion fail.
#[tokio::test]
async fn zero_hooks_retain_without_decode_then_post_install_replay_fires_old_event() {
    let profile_guard = tempfile::tempdir().expect("profile");
    let workspace_guard = tempfile::tempdir().expect("workspace");
    let marker_guard = tempfile::tempdir().expect("marker");
    let profile = canonical(profile_guard.path());
    let workspace = canonical(workspace_guard.path());
    let marker = marker_guard.path().join("post-install-fired");
    let store = SqliteStoreHandle::open(&profile).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let (service, engine) = HookEngine::start(profile.clone(), store.clone(), hub.clone())
        .await
        .expect("hook engine");
    hub.install_hooks(service.clone())
        .expect("install hook service");
    let session_id = SessionId::new("hook-peak-rss-session");
    let run_id = RunId::new("hook-peak-rss-run");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-hook-peak-rss".into(),
        request_digest: "create-hook-peak-rss-digest".into(),
        request_json: r#"{"session":"hook-peak-rss"}"#.into(),
        session_id: session_id.clone(),
        cwd: workspace.to_str().expect("UTF-8 workspace").to_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "hook-peak-rss-test-v1".into(),
        event_id: EventId::new("hook-peak-rss-created"),
        device_id: DeviceId::new("hook-peak-rss-test-device"),
    })
    .await
    .expect("create session");
    hub.bind_lockdown_turn(
        &session_id,
        &run_id,
        "fake",
        crate::auto_hermetic::ProviderLockdownPolicy::Full,
    )
    .expect("bind provider boundary");

    let scans_before = service.dispatch_metadata_scan_count();
    let mut historical = [envelope(
        &session_id,
        &run_id,
        store.worker_generation(),
        "historical-run-started",
        json!({"type": "run_state", "state": "thinking"}),
    )];
    hub.append(&mut historical)
        .await
        .expect("commit historical hook input");
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        while service.dispatch_metadata_scan_count() <= scans_before {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("metadata-only no-hook scan");
    assert_eq!(
        service.dispatch_decode_count(),
        0,
        "zero configured hooks must not decode committed envelopes"
    );
    assert!(
        store
            .has_pending_hook_dispatches(&session_id)
            .await
            .expect("pending outbox query"),
        "potential hook input stays durable for a later installation"
    );

    write_trusted_workspace_hook(&profile, &workspace, &marker);
    let (_, _, hooks) = service
        .list(workspace.clone())
        .await
        .expect("list new hook");
    assert!(hooks.iter().any(|hook| hook.trusted));
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        while !marker.exists()
            || store
                .has_pending_hook_dispatches(&session_id)
                .await
                .expect("pending outbox query")
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("post-install replay of historical event");
    assert!(service.dispatch_decode_count() > 0);

    // Keep one lower outbox row intentionally retained in a clean workspace.
    // Later rows in the configured workspace are ACKed, recreating the exact
    // mixed-delete shape in which SQLite can reuse a ROWID below a global
    // cursor. The production cursor is per-session sequence instead.
    let clean_workspace_guard = tempfile::tempdir().expect("clean workspace");
    let clean_workspace = canonical(clean_workspace_guard.path());
    let clean_session_id = SessionId::new("hook-peak-rss-clean-session");
    let clean_run_id = RunId::new("hook-peak-rss-clean-run");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-hook-peak-rss-clean".into(),
        request_digest: "create-hook-peak-rss-clean-digest".into(),
        request_json: r#"{"session":"hook-peak-rss-clean"}"#.into(),
        session_id: clean_session_id.clone(),
        cwd: clean_workspace
            .to_str()
            .expect("UTF-8 clean workspace")
            .to_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "hook-peak-rss-test-v1".into(),
        event_id: EventId::new("hook-peak-rss-clean-created"),
        device_id: DeviceId::new("hook-peak-rss-test-device"),
    })
    .await
    .expect("create clean session");
    hub.bind_lockdown_turn(
        &clean_session_id,
        &clean_run_id,
        "fake",
        crate::auto_hermetic::ProviderLockdownPolicy::Full,
    )
    .expect("bind clean provider boundary");
    let clean_scans_before = service.dispatch_metadata_scan_count();
    let mut retained = [envelope(
        &clean_session_id,
        &clean_run_id,
        store.worker_generation(),
        "clean-retained-run-started",
        json!({"type": "run_state", "state": "thinking"}),
    )];
    hub.append(&mut retained)
        .await
        .expect("commit retained row");
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        // One page observes the retained rows and a second proves the
        // per-session cursor reached its current tail.
        while service.dispatch_metadata_scan_count() < clean_scans_before + 2 {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("clean-session metadata scan");

    let decode_count = service.dispatch_decode_count();
    let large_text = "x".repeat(1_114_112);
    let mut unrelated = [
        envelope(
            &session_id,
            &run_id,
            store.worker_generation(),
            "large-agent-item",
            json!({
                "type": "item",
                "event": "completed",
                "item_id": "large-agent-item-id",
                "item": {"item": "agent_message", "text": large_text},
            }),
        ),
        envelope(
            &session_id,
            &run_id,
            store.worker_generation(),
            "large-agent-node",
            json!({
                "type": "node_committed",
                "node": "large-agent-node-id",
                "kind": {"kind": "assistant_commit", "text": "x".repeat(1_114_112)},
            }),
        ),
    ];
    hub.append(&mut unrelated)
        .await
        .expect("commit unrelated large envelopes");
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        while store
            .has_pending_hook_dispatches(&session_id)
            .await
            .expect("pending outbox query")
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("metadata-only acknowledgement of unrelated envelopes");
    assert_eq!(
        service.dispatch_decode_count(),
        decode_count,
        "a run-started hook cannot match large item/node payloads"
    );

    let first_marker_len = std::fs::metadata(&marker).expect("first marker").len();
    let mut after_mixed_deletes = [envelope(
        &session_id,
        &run_id,
        store.worker_generation(),
        "run-started-after-mixed-ack",
        json!({"type": "run_state", "state": "thinking"}),
    )];
    hub.append(&mut after_mixed_deletes)
        .await
        .expect("commit event after mixed retain/ACK");
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        while std::fs::metadata(&marker)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
            <= first_marker_len
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("new match after mixed retain/ACK was dispatched");
    assert!(
        store
            .has_pending_hook_dispatches(&clean_session_id)
            .await
            .expect("clean pending outbox query"),
        "the unrelated clean-session row remains durable"
    );

    // A cached nonempty→empty config transition must decode one event to
    // reconcile the already-live subscriber. Otherwise the old process can
    // outlive policy removal and emit its delayed marker.
    let ready = marker_guard.path().join("subscriber-ready");
    let survived = marker_guard.path().join("subscriber-survived");
    write_trusted_subscriber(&workspace, &ready, &survived);
    service
        .list(workspace.clone())
        .await
        .expect("discover subscriber");
    let mut start_subscriber = [envelope(
        &session_id,
        &run_id,
        store.worker_generation(),
        "subscriber-started-before-removal",
        json!({"type": "run_state", "state": "thinking"}),
    )];
    hub.append(&mut start_subscriber)
        .await
        .expect("commit subscriber start");
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        while !ready.exists() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("subscriber readiness");
    remove_workspace_hooks(&workspace);
    let mut reconcile_removal = [envelope(
        &session_id,
        &run_id,
        store.worker_generation(),
        "subscriber-removal-reconciliation",
        json!({"type": "run_state", "state": "thinking"}),
    )];
    hub.append(&mut reconcile_removal)
        .await
        .expect("commit config-removal reconciliation fact");
    tokio::time::timeout(OBSERVATION_TIMEOUT, async {
        while store
            .has_pending_hook_dispatches(&session_id)
            .await
            .expect("configured-session pending query")
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("config removal was reconciled");
    // Two seconds is the fixture's delayed write; the product's one-second
    // child-reap bound plus that literal delay and one poll bounds observation.
    tokio::time::sleep(
        super::HOOK_CHILD_REAP_TIMEOUT
            .saturating_add(Duration::from_secs(2))
            .saturating_add(POLL_INTERVAL),
    )
    .await;
    assert!(
        !survived.exists(),
        "subscriber survived removal of its hook definition"
    );

    engine.shutdown().await;
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
