#![cfg(windows)]
#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::SessionId;
use haider_tools::{
    BackgroundExec, EffectBroker, JournalSink, PermissionPolicy, ProcessExec, ToolResult,
    default_task_name, shared_task_output, supervise_background, task_kill_channel,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{path::PathBuf, time::Instant};

#[derive(Default)]
struct Journal(Arc<Mutex<Vec<EventPayload>>>);

#[async_trait]
impl JournalSink for Journal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.0.lock().expect("journal lock").push(payload);
        Ok(())
    }
}

/// Background execution passes through the same reduced-environment and Job
/// Object spawn seam as foreground execution.
#[tokio::test]
async fn background_exec_restores_windows_bootstrap_environment() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut broker = EffectBroker::new_at(
        Box::new(Journal::default()),
        workspace.path(),
        SessionId::new("windows-background-session"),
        1,
        1_700_000_000_000,
    )
    .expect("broker");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ProcessExec);
    let command = "if defined SystemRoot (>background-bootstrap.txt echo ok) else exit /b 9";
    let operation = BackgroundExec::new(
        ProcessExec::new("windows-background-bootstrap", command),
        default_task_name(command),
    )
    .expect("background operation");
    let spawn = broker
        .process_exec_background(&operation, &policy)
        .await
        .expect("background spawn");
    let (_kill, receiver) = task_kill_channel();
    let output = shared_task_output(4096, 512);
    let status = supervise_background(spawn, receiver, output, Duration::from_millis(200)).await;
    assert_eq!(status.exit_code, Some(0));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("background-bootstrap.txt"))
            .expect("bootstrap marker")
            .trim(),
        "ok"
    );
    broker.close().await.expect("broker close");
}

fn system32() -> PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .map(|root| root.join("System32"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"))
}

/// The background supervisor must own the exact Job token after the leader
/// launches descendants; cancellation cannot be a leader-only process kill.
#[tokio::test]
async fn background_kill_sweeps_windows_descendants() {
    let workspace = tempfile::tempdir().expect("workspace");
    let ready = workspace.path().join("background-child-ready.txt");
    let survived = workspace.path().join("background-child-survived.txt");
    let child = workspace.path().join("background-child.cmd");
    let parent = workspace.path().join("background-parent.cmd");
    let system32 = system32();
    std::fs::write(
        &child,
        format!(
            "@echo off\r\n>\"{}\" <nul set /p \"=ready\"\r\n\"{}\" -n 5 127.0.0.1 >nul\r\n>\"{}\" <nul set /p \"=survived\"\r\n",
            ready.display(),
            system32.join("ping.exe").display(),
            survived.display(),
        ),
    )
    .expect("write background child");
    std::fs::write(
        &parent,
        format!(
            "@echo off\r\nstart \"\" /b \"{}\" /d /s /c \"\"{}\"\"\r\n:wait_forever\r\n\"{}\" -n 2 127.0.0.1 >nul\r\ngoto wait_forever\r\n",
            system32.join("cmd.exe").display(),
            child.display(),
            system32.join("ping.exe").display(),
        ),
    )
    .expect("write background parent");

    let mut broker = EffectBroker::new_at(
        Box::new(Journal::default()),
        workspace.path(),
        SessionId::new("windows-background-kill-session"),
        1,
        1_700_000_000_000,
    )
    .expect("broker");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ProcessExec);
    let command = format!("\"{}\"", parent.display());
    let operation = BackgroundExec::new(
        ProcessExec::new("windows-background-kill", &command),
        default_task_name(&command),
    )
    .expect("background operation");
    let spawn = broker
        .process_exec_background(&operation, &policy)
        .await
        .expect("background spawn");
    let pid = spawn.pid;
    let (kill, receiver) = task_kill_channel();
    let output = shared_task_output(4096, 512);
    let supervision = tokio::spawn(supervise_background(
        spawn,
        receiver,
        output,
        Duration::from_millis(200),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready.exists(), "fixture descendant never launched");
    kill.kill();
    let status = supervision.await.expect("join supervision");
    assert!(status.killed);
    assert!(
        status.fault.is_none(),
        "supervisor fault: {:?}",
        status.fault
    );
    assert_eq!(
        haider_tools::probe_group_liveness(pid),
        haider_tools::PidLiveness::Dead
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!survived.exists(), "background descendant escaped its Job");
    broker.close().await.expect("broker close");
}
