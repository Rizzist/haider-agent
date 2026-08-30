#![cfg(unix)]
//! W-A background task laws at the tools seam: immediate spawn return, the
//! detached-from-broker lifetime, bounded output, the pgid kill ladder, and
//! orphan reaping through the injected liveness seam.
#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::ids::SessionId;
use haider_tools::{
    BackgroundExec, EffectBroker, JournalSink, OrphanReap, PermissionPolicy, PidLiveness,
    ProcessExec, ToolError, ToolResult, default_task_name, probe_group_liveness, reap_orphan_group,
    shared_task_output, supervise_background, task_kill_channel,
};
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct SharedJournal {
    payloads: Arc<Mutex<Vec<EventPayload>>>,
}

impl SharedJournal {
    fn observer(&self) -> Arc<Mutex<Vec<EventPayload>>> {
        Arc::clone(&self.payloads)
    }
}

#[async_trait]
impl JournalSink for SharedJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads
            .lock()
            .map_err(|_| ToolError::journal("recording journal lock poisoned"))?
            .push(payload);
        Ok(())
    }
}

fn broker(root: &std::path::Path) -> (EffectBroker, Arc<Mutex<Vec<EventPayload>>>) {
    let journal = SharedJournal::default();
    let observer = journal.observer();
    let broker = EffectBroker::new_at(
        Box::new(journal),
        root,
        SessionId::new("background-task-session"),
        3,
        1_700_000_000_000,
    )
    .expect("create broker");
    (broker, observer)
}

fn exec_policy() -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::ProcessExec);
    policy
}

fn phases(observer: &Arc<Mutex<Vec<EventPayload>>>) -> Vec<EffectPhase> {
    observer
        .lock()
        .expect("journal observer")
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase.clone()),
            _ => None,
        })
        .collect()
}

fn background(call_id: &str, command: &str) -> BackgroundExec {
    BackgroundExec::new(
        ProcessExec::new(call_id, command),
        default_task_name(command),
    )
    .expect("valid background exec")
}

/// MUTATION CHECK (LT1 seam): make the background spawn wait for the child,
/// or route it through the foreground registry so broker close cancels it.
/// Expected RUNTIME failure: the spawn call no longer returns while a
/// 30-second child runs, the effect outcome is not `Ok` at the spawn
/// boundary, or the child group is dead after broker close.
#[tokio::test]
async fn background_spawn_returns_immediately_and_survives_broker_close() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut broker, observer) = broker(workspace.path());
    let started = Instant::now();
    let spawn = broker
        .process_exec_background(&background("bg-immediate", "sleep 30"), &exec_policy())
        .await
        .expect("background spawn");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "background spawn must return immediately, not wait for the child"
    );
    let recorded = phases(&observer);
    assert_eq!(recorded.len(), 4, "four effect phases journal at spawn");
    assert!(
        matches!(
            recorded.last(),
            Some(EffectPhase::Outcome {
                outcome: EffectOutcome::Ok,
                ..
            })
        ),
        "the background effect terminalizes Ok at the spawn boundary: {recorded:?}"
    );
    let pid = spawn.pid;
    assert_eq!(probe_group_liveness(pid), PidLiveness::Alive);

    // Broker close is the turn-end/esc seam: it must not touch the child.
    let report = broker.close().await.expect("broker close");
    assert!(report.reconciled_effects.is_empty());
    assert!(report.leaked_processes.is_empty());
    assert_eq!(
        probe_group_liveness(pid),
        PidLiveness::Alive,
        "turn end must never kill a background task — outliving the turn is the feature"
    );

    // Reap through the supervised kill ladder so the test leaves no orphan.
    let (kill, kill_signal) = task_kill_channel();
    let output = shared_task_output(1024, 128);
    let supervision = tokio::spawn(supervise_background(
        spawn,
        kill_signal,
        Arc::clone(&output),
        Duration::from_millis(200),
    ));
    kill.kill();
    let status = supervision.await.expect("supervision joins");
    assert!(status.killed, "kill ladder must report killed: {status:?}");
    assert!(
        status
            .workspace_mutation
            .as_ref()
            .is_some_and(|mutation| mutation
                .mutation_digest
                .contains("reason=concurrent_or_interleaved_mutation")),
        "turn close makes the detached lifetime conservatively unknown"
    );
    assert_eq!(probe_group_liveness(pid), PidLiveness::Dead);
}

/// MUTATION CHECK (LT2 seam): drop the retained-head cap, stop counting
/// total bytes, or let the tail grow unbounded. Expected RUNTIME failure:
/// retained/total/tail assertions below change while the task still exits 0.
#[tokio::test]
async fn background_output_is_bounded_with_honest_truncation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut broker, _observer) = broker(workspace.path());
    // 200 lines x 41 bytes = 8_200 bytes, well past the 1 KiB retained cap.
    let spawn = broker
        .process_exec_background(
            &background(
                "bg-bounded",
                "i=0; while [ $i -lt 200 ]; do printf '%040d\\n' $i; i=$((i+1)); done",
            ),
            &exec_policy(),
        )
        .await
        .expect("background spawn");
    let (_kill, kill_signal) = task_kill_channel();
    let output = shared_task_output(1024, 64);
    let status = supervise_background(
        spawn,
        kill_signal,
        Arc::clone(&output),
        Duration::from_millis(200),
    )
    .await;
    assert_eq!(status.exit_code, Some(0), "fixture exits clean: {status:?}");
    assert!(!status.killed);
    let buffer = haider_tools::lock_task_output(&output);
    assert_eq!(buffer.total_bytes(), 8_200);
    assert_eq!(buffer.retained().len(), 1024, "head retained to the cap");
    assert!(buffer.truncated(), "dropped bytes are marked honestly");
    let tail = buffer.tail_lossy();
    assert_eq!(tail.len(), 64, "tail preview is the rolling last bytes");
    assert!(
        tail.ends_with("0000199\n"),
        "tail must end with the LAST line's bytes: {tail:?}"
    );
    let (chunk, next) = buffer.read_from(0, 40);
    assert_eq!(chunk.len(), 40);
    assert_eq!(next, 40);
    let (past_end, cursor) = buffer.read_from(9_999, 40);
    assert!(
        past_end.is_empty(),
        "cursor past retention yields empty, never rewinds"
    );
    assert_eq!(cursor, 1024);
}

/// Workspace revision producer guard: detached `process_exec` reports the
/// same mutation provenance shape as a foreground execution when it changes
/// the workspace, while the spawn-boundary effect remains immediately `Ok`.
#[tokio::test]
async fn background_process_reports_post_completion_workspace_mutation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut broker, _observer) = broker(workspace.path());
    let spawn = broker
        .process_exec_background(
            &background(
                "bg-workspace-mutation",
                "printf 'changed by background task' > changed.txt",
            ),
            &exec_policy(),
        )
        .await
        .expect("background spawn");
    let effect = spawn.effect.clone();
    let (_kill, kill_signal) = task_kill_channel();
    let status = supervise_background(
        spawn,
        kill_signal,
        shared_task_output(1024, 64),
        Duration::from_millis(200),
    )
    .await;
    assert_eq!(status.exit_code, Some(0));
    let mutation = status
        .workspace_mutation
        .expect("background write reports mutation");
    assert_eq!(mutation.effect_id, effect);
    assert!(mutation.mutation_digest.starts_with("blake3:"));
    assert!(mutation.workspace_revision.is_none());
    assert!(mutation.subject_digest.is_none());
}

/// MUTATION CHECK (kill-fence seam): signal only the leader pid instead of
/// the group, or skip the KILL escalation. Expected RUNTIME failure: the
/// TERM-immune group member survives the ladder and the group probe stays
/// `Alive`.
#[tokio::test]
async fn task_kill_ladder_kills_the_whole_process_group() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut broker, _observer) = broker(workspace.path());
    // The leader waits on a TERM-trapping member: only a group KILL ends both.
    let spawn = broker
        .process_exec_background(
            &background("bg-group", "trap '' TERM; sleep 30 & wait"),
            &exec_policy(),
        )
        .await
        .expect("background spawn");
    let pid = spawn.pid;
    let (kill, kill_signal) = task_kill_channel();
    let output = shared_task_output(1024, 64);
    let supervision = tokio::spawn(supervise_background(
        spawn,
        kill_signal,
        output,
        Duration::from_millis(200),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    kill.kill();
    let status = supervision.await.expect("supervision joins");
    assert!(
        status.killed,
        "ladder reports the deliberate kill: {status:?}"
    );
    assert_eq!(
        probe_group_liveness(pid),
        PidLiveness::Dead,
        "the WHOLE group must die (pgid kill), not just the leader"
    );
}

/// Lane 967-P1 owner decision: normal completion ends ownership in background
/// mode too. `task_kill` still sweeps while the task leader is live, but a
/// naturally completed leader leaves its descendants alone.
#[tokio::test]
async fn natural_exit_leaves_lingering_group_members_alone() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut broker, _observer) = broker(workspace.path());
    let spawn = broker
        .process_exec_background(
            &background(
                "bg-linger",
                concat!(
                    "/usr/bin/perl -e '$pid = fork; ",
                    "if ($pid) { while (!-e q(started)) { select undef, undef, undef, 0.01 } ",
                    "print q(leader); exit 0 } ",
                    "open $started, q(>), q(started); print $started q(started); close $started; ",
                    "select undef, undef, undef, 0.5; ",
                    "open $survived, q(>), q(survived); print $survived q(survived); ",
                    "close $survived; while (!-e q(cleanup)) { ",
                    "select undef, undef, undef, 0.01 }'",
                ),
            ),
            &exec_policy(),
        )
        .await
        .expect("background spawn");
    let pid = spawn.pid;
    let (_kill, kill_signal) = task_kill_channel();
    let output = shared_task_output(1024, 64);
    let output_observer = Arc::clone(&output);
    let status = supervise_background(spawn, kill_signal, output, Duration::from_millis(200)).await;
    assert_eq!(status.exit_code, Some(0));
    assert!(!status.killed, "a natural exit is not a kill: {status:?}");
    assert_eq!(
        output_observer
            .lock()
            .expect("background output observer")
            .retained(),
        b"leader"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        probe_group_liveness(pid),
        PidLiveness::Alive,
        "lingering same-group members become unmanaged when the task completes"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("survived"))
            .expect("unmanaged background descendant survives"),
        "survived"
    );
    fs::write(workspace.path().join("cleanup"), b"stop").expect("release descendant fixture");
    for _ in 0..200 {
        if probe_group_liveness(pid) == PidLiveness::Dead {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("unmanaged background descendant did not exit after fixture cleanup");
}

/// MUTATION CHECK (LT6 seam): ignore the injected liveness probe, or skip
/// the pgid kill for a live orphan. Expected RUNTIME failure: the fake-Dead
/// probe still signals (observed via the probe call ledger + surviving
/// child), or the real orphan group survives the reap.
#[tokio::test]
async fn orphan_reap_honors_the_liveness_seam_and_kills_stale_groups() {
    use std::os::unix::process::CommandExt as _;
    // A fake-Dead probe must short-circuit: the live child stays untouched.
    let mut untouched = std::process::Command::new("/bin/sh");
    untouched.arg("-c").arg("sleep 30");
    untouched.process_group(0);
    let mut untouched = untouched.spawn().expect("spawn orphan fixture");
    let untouched_pid = i32::try_from(untouched.id()).expect("pid fits");
    let probed = Arc::new(Mutex::new(0_u32));
    let ledger = Arc::clone(&probed);
    let reap = reap_orphan_group(untouched_pid, Duration::from_millis(50), move |_| {
        *ledger.lock().expect("probe ledger") += 1;
        PidLiveness::Dead
    })
    .await;
    assert_eq!(reap, OrphanReap::AlreadyDead);
    assert_eq!(
        *probed.lock().expect("probe ledger"),
        1,
        "the seam was consulted"
    );
    assert_eq!(
        probe_group_liveness(untouched_pid),
        PidLiveness::Alive,
        "a Dead verdict must short-circuit before any signal"
    );

    // The same group through the REAL probe is killed by the ladder.
    let reap = reap_orphan_group(
        untouched_pid,
        Duration::from_millis(200),
        probe_group_liveness,
    )
    .await;
    assert_eq!(reap, OrphanReap::Killed, "a live stale group is reaped");
    let status = untouched.wait().expect("reap fixture zombie");
    assert!(!status.success());
    assert_eq!(probe_group_liveness(untouched_pid), PidLiveness::Dead);
}

/// MUTATION CHECK: default the task name to something other than the
/// command's first-token basename, or accept an empty/oversized name.
/// Expected RUNTIME failure: the literals below change or the typed
/// refusals disappear.
#[tokio::test]
async fn task_names_default_from_the_command_and_validate() {
    assert_eq!(default_task_name("cargo watch -x test"), "cargo");
    assert_eq!(
        default_task_name("/usr/bin/python3 -m http.server"),
        "python3"
    );
    assert_eq!(default_task_name("   "), "task");
    assert!(BackgroundExec::new(ProcessExec::new("c", "sleep 1"), "  ").is_err());
    assert!(BackgroundExec::new(ProcessExec::new("c", "sleep 1"), "x".repeat(81)).is_err());
    let named =
        BackgroundExec::new(ProcessExec::new("c", "sleep 1"), "dev server").expect("valid name");
    assert_eq!(named.name(), "dev server");
}

/// MUTATION CHECK: exempt the background shape from permission policy.
/// Expected RUNTIME failure: an Ask policy no longer yields the typed
/// authorization-required error before any spawn.
#[tokio::test]
async fn background_spawn_respects_ask_policy() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut broker, observer) = broker(workspace.path());
    let error = broker
        .process_exec_background(
            &background("bg-ask", "sleep 30"),
            &PermissionPolicy::default(),
        )
        .await
        .expect_err("ask policy blocks the spawn");
    assert!(
        matches!(error, ToolError::AuthorizationRequired { .. }),
        "typed ask surface: {error:?}"
    );
    let recorded = phases(&observer);
    assert!(
        !recorded
            .iter()
            .any(|phase| matches!(phase, EffectPhase::Dispatched { .. })),
        "nothing dispatches before authorization: {recorded:?}"
    );
}
