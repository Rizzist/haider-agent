//! Real-process fixture timing belongs to its signals, not to test admission.

use super::support::{self, UdsClient};
use haider_protocol::{EventPayload, ids::RunId, state::RunState};
use haider_rpc::WireFrame;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::Semaphore, time::Instant};

pub const HEARTBEAT_PERIOD: Duration = Duration::from_millis(10);
// N = 1000 nominal heartbeats: one existing RPC keepalive interval for the
// external writer and observer to be scheduled. Readiness needs observed
// growth, not 1000 bytes, and progress never restarts this finite window.
const STARTUP_HEARTBEATS: u32 =
    (support::KEEPALIVE_INTERVAL.as_millis() / HEARTBEAT_PERIOD.as_millis()) as u32;
const HEARTBEAT_WINDOW: Duration = HEARTBEAT_PERIOD.saturating_mul(STARTUP_HEARTBEATS);
// cmd.exe's loop uses ping -n 2 (one one-second interval). Three polls allow
// entry into the wait, release observation, and the forbidden survivor write.
pub const DESCENDANT_POLL: Duration = Duration::from_secs(1);
pub const STOP_OBSERVATION: Duration = DESCENDANT_POLL.saturating_mul(3);

pub struct DescendantProbe(PathBuf);

impl DescendantProbe {
    pub fn new(workspace: &Path) -> Self {
        Self(workspace.join("descendant-probe"))
    }

    pub fn release(&self) {
        fs::write(&self.0, b"probe").expect("probe cancelled descendant");
    }

    pub fn assert_released(&self) {
        assert_eq!(
            fs::read(&self.0).expect("descendant probe must be released before checking survival"),
            b"probe",
            "descendant probe release must remain observable",
        );
    }
}

impl Drop for DescendantProbe {
    fn drop(&mut self) {
        // Release on unwind too. The descendant also exits if TempDir removes
        // its start marker before it gets scheduled to observe this probe.
        let _ = fs::write(&self.0, b"probe");
    }
}

fn spawn_budget() -> Duration {
    // Established shell cold-start policies (support/mod.rs and hooks_tests),
    // followed by one heartbeat observation window. No foreground execution
    // or kill budget belongs in a readiness wait.
    Duration::from_secs(if cfg!(windows) { 30 } else { 5 }) + HEARTBEAT_WINDOW
}

// Five tests, eight real shell invocations: four tool round trips, one natural
// exit, one non-repository exec, one shell RPC, and one cancellation. Bound
// admission by that many existing complete process-operation budgets, with
// progress reports each keepalive interval. This is a starvation watchdog,
// not a timing assertion about successful queue latency.
#[cfg(windows)]
static WINDOWS_REAL_PROCESS_TEST_GATE: Semaphore = Semaphore::const_new(1);

#[cfg(windows)]
pub async fn windows_real_process_test_guard(
    test_name: &'static str,
) -> tokio::sync::SemaphorePermit<'static> {
    acquire_process_test_gate(
        &WINDOWS_REAL_PROCESS_TEST_GATE,
        test_name,
        support::DEADLINE * 8,
    )
    .await
    .unwrap_or_else(|reason| panic!("{reason}"))
}

async fn acquire_process_test_gate<'a>(
    gate: &'a Semaphore,
    test_name: &str,
    budget: Duration,
) -> Result<tokio::sync::SemaphorePermit<'a>, String> {
    let started = Instant::now();
    let acquire = gate.acquire();
    tokio::pin!(acquire);
    let mut report = tokio::time::interval(support::KEEPALIVE_INTERVAL);
    report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            permit = &mut acquire => {
                let permit = permit.map_err(|error| format!("process-test gate closed: {error}"))?;
                eprintln!("haider-daemond windows-process test={test_name} phase=running gate_wait={:?}", started.elapsed());
                return Ok(permit);
            }
            _ = tokio::time::sleep_until(started + budget) => {
                return Err(format!("haider-daemond windows-process test={test_name} phase=waiting-for-gate timed out; elapsed={:?}; budget={budget:?}", started.elapsed()));
            }
            _ = report.tick() => {
                eprintln!("haider-daemond windows-process test={test_name} phase=waiting-for-gate elapsed={:?} budget={budget:?}", started.elapsed());
            }
        }
    }
}

struct Startup {
    phase: &'static str,
    deadline: Instant,
    streaming: bool,
    first_heartbeat: Option<u64>,
    last_state: Option<RunState>,
}

impl Startup {
    fn new() -> Self {
        Self {
            phase: "waiting-for-streaming",
            deadline: Instant::now() + support::DEADLINE,
            streaming: false,
            first_heartbeat: None,
            last_state: None,
        }
    }

    fn event(&mut self, payload: EventPayload) -> Result<(), String> {
        match payload {
            EventPayload::RunState(state) => {
                self.last_state = Some(state.clone());
                if state.is_terminal() {
                    return Err(format!("run terminalized before cancellation: {state:?}"));
                }
                if state == RunState::Streaming && !self.streaming {
                    self.streaming = true;
                    self.phase = "waiting-for-first-heartbeat";
                    self.deadline = Instant::now() + spawn_budget();
                }
            }
            EventPayload::RunFailed { code, message, .. } => {
                return Err(format!("run failed: {code:?}: {message}"));
            }
            EventPayload::ToolResult { call_id, result } if call_id == "cancel-process" => {
                // FakeStep::Hang can keep the run Streaming after a tool has
                // already timed out. RunState alone cannot diagnose startup.
                return Err(format!("process ended before cancellation: {result:?}"));
            }
            _ => {}
        }
        Ok(())
    }

    fn observe(&mut self, heartbeat_bytes: Option<u64>, descendant_started: bool) -> bool {
        if !self.streaming {
            return false;
        }
        let Some(bytes) = heartbeat_bytes.filter(|bytes| *bytes > 0) else {
            return false;
        };
        match self.first_heartbeat {
            None => {
                self.first_heartbeat = Some(bytes);
                self.phase = "waiting-for-heartbeat-growth-and-descendant";
                self.deadline = Instant::now() + HEARTBEAT_WINDOW;
                false
            }
            Some(first) => bytes > first && descendant_started,
        }
    }
}

pub async fn wait_for_process_tree(
    client: &mut UdsClient,
    frame_limit: usize,
    run: &RunId,
    workspace: &Path,
    initial_events: Vec<EventPayload>,
) -> Result<(), String> {
    let heartbeat = workspace.join("heartbeat.log");
    let descendant = workspace.join("descendant-started.log");
    let mut startup = Startup::new();
    let mut poll = tokio::time::interval(HEARTBEAT_PERIOD);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: Result<(), String> = async {
        // Streaming may precede the TurnSubmit reply. Preserve those events
        // too: reply/push ordering must not decide whether the clock starts.
        for event in initial_events {
            startup.event(event)?;
        }
        let mut next_keepalive = Instant::now() + support::KEEPALIVE_INTERVAL;
        loop {
            // try_receive keeps its decoder in UdsClient and only awaits a
            // cancellation-safe stream read. Do not race try_next's internal
            // write_all against a readiness poll: that can split a Ping frame.
            tokio::select! {
                frame = client.try_receive() => {
                    let frame = frame.ok_or("connection closed during process startup")?;
                    if let WireFrame::Event { envelope, .. } = frame
                        && envelope.run_id.as_ref() == Some(run)
                        && let Ok(payload) = serde_json::from_value(envelope.payload.into())
                    {
                        let previous_phase = startup.phase;
                        startup.event(payload)?;
                        if startup.phase != previous_phase {
                            eprintln!("process tree run={run} phase={} budget={:?}", startup.phase, spawn_budget());
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_keepalive) => {
                    // Finish the entire write before another readiness poll.
                    // On a write deadline the test fails and drops this peer;
                    // no subsequent TurnCancel can follow a partial frame.
                    tokio::time::timeout_at(startup.deadline, client.send(
                        &WireFrame::Ping { nonce: u64::MAX - 2 }, frame_limit,
                    )).await.map_err(|_| "keepalive write deadline elapsed")?;
                    next_keepalive = Instant::now() + support::KEEPALIVE_INTERVAL;
                }
                _ = tokio::time::sleep_until(startup.deadline) => {
                    return Err("phase deadline elapsed".into());
                }
                _ = poll.tick() => {
                    let bytes = fs::metadata(&heartbeat).map(|metadata| metadata.len()).ok();
                    let previous_phase = startup.phase;
                    if startup.observe(bytes, descendant.exists()) {
                        eprintln!("process tree run={run} phase=started heartbeat_bytes={bytes:?} descendant_started=true");
                        return Ok(());
                    }
                    if startup.phase != previous_phase {
                        eprintln!("process tree run={run} phase={} heartbeat_bytes={bytes:?} budget={HEARTBEAT_WINDOW:?}", startup.phase);
                    }
                }
            }
        }
    }.await;
    if let Err(failure) = result {
        let bytes = fs::metadata(&heartbeat).map(|metadata| metadata.len()).ok();
        let reason = format!(
            "process tree run={run} phase={} failed: {failure}; heartbeat_bytes={bytes:?}; descendant_started={}; last_state={:?}; heartbeat_period={HEARTBEAT_PERIOD:?}; startup_heartbeats={STARTUP_HEARTBEATS}",
            startup.phase,
            descendant.exists(),
            startup.last_state,
        );
        client.report_connection_failure(&reason);
        return Err(reason);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queued_process_test_gets_fresh_streaming_and_heartbeat_deadlines() {
    let gate = Semaphore::new(1);
    let held = gate.acquire().await.expect("hold gate");
    let waiting = acquire_process_test_gate(&gate, "queued-regression", support::DEADLINE * 8);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(spawn_budget() * 2, &mut waiting)
            .await
            .is_err()
    );
    drop(held);
    let _permit = waiting.await.expect("acquire after queue delay");
    let mut startup = Startup::new();
    tokio::time::advance(spawn_budget()).await;
    assert!(
        !startup.observe(Some(100), true),
        "files alone do not prove Streaming"
    );
    startup
        .event(EventPayload::RunState(RunState::Streaming))
        .expect("Streaming");
    assert_eq!(startup.deadline, Instant::now() + spawn_budget());
    assert!(
        !startup.observe(Some(100), true),
        "old bytes do not prove a live leader"
    );
    let deadline = startup.deadline;
    tokio::time::advance(HEARTBEAT_PERIOD).await;
    startup
        .event(EventPayload::RunState(RunState::Streaming))
        .expect("replayed Streaming");
    assert!(
        !startup.observe(Some(100), true),
        "a stalled heartbeat is not ready"
    );
    assert!(
        !startup.observe(Some(101), false),
        "the descendant must start too"
    );
    assert_eq!(
        startup.deadline, deadline,
        "polling does not extend the bound"
    );
    assert!(startup.observe(Some(102), true));
}

#[tokio::test(start_paused = true)]
async fn process_gate_reports_a_stalled_holder() {
    let gate = Semaphore::new(0);
    let failure = acquire_process_test_gate(&gate, "blocked-regression", HEARTBEAT_WINDOW)
        .await
        .expect_err("a gate that never opens is bounded");
    assert!(failure.contains("test=blocked-regression phase=waiting-for-gate timed out"));
}

#[tokio::test(start_paused = true)]
async fn streaming_does_not_hide_a_failed_process_tool() {
    let mut startup = Startup::new();
    startup
        .event(EventPayload::RunState(RunState::Streaming))
        .expect("Streaming");
    let failed_tool = serde_json::from_value(serde_json::json!({
        "type": "tool_result",
        "call_id": "cancel-process",
        "result": {
            "preview": "process limit reached: WallTimeout",
            "truncated": false,
            "status": "failed",
            "reason": "process exceeded WallTimeout"
        }
    }))
    .expect("failed process result");
    let failure = startup
        .event(failed_tool)
        .expect_err("tool failure ends startup immediately");
    assert!(failure.contains("WallTimeout"));
    assert_eq!(startup.last_state, Some(RunState::Streaming));
}
