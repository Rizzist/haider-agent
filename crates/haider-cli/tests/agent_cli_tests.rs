#![allow(clippy::expect_used)]
//! Public agent/workflow doors against the real sibling daemon. Each fixture
//! owns one short profile, one consumptive fake-provider script, and its PID.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// Registry #94: one command may cold-start (30s), issue its RPC (60s),
// observe --timeout 10s, and publish/drain its terminal (2s): 30+60+10+2=102s.
const COMMAND_BOUND: Duration = Duration::from_secs(30 + 60 + 10 + 2);
// Product daemon stop is 20s, followed by 2s process-exit observation.
const STOP_BOUND: Duration = Duration::from_secs(20 + 2);

struct Profile {
    root: tempfile::TempDir,
    profile: PathBuf,
    home: PathBuf,
    runtime: PathBuf,
    workspace: PathBuf,
    script: String,
}

impl Profile {
    fn new(script: Value) -> Self {
        assert_eq!(
            std::env::var("HAIDER_TEST_SIBLINGS_PREBUILT").as_deref(),
            Ok("1"),
            "prebuild haider/haiderd siblings before real-daemon CLI tests"
        );
        let sibling = PathBuf::from(env!("CARGO_BIN_EXE_haider"))
            .with_file_name(format!("haiderd{}", std::env::consts::EXE_SUFFIX));
        assert!(
            std::fs::metadata(sibling).expect("prebuilt daemon").len() > 10 * 1024 * 1024,
            "registry #64: real haiderd must exceed 10 MiB"
        );
        // Unix endpoints must fit sun_path even when the host TMPDIR is long.
        let root = if cfg!(unix) {
            tempfile::Builder::new().prefix("h-ac-").tempdir_in("/tmp")
        } else {
            tempfile::Builder::new().prefix("h-ac-").tempdir()
        }
        .expect("short isolated fixture");
        let profile = root.path().join("p");
        let home = root.path().join("h");
        let runtime = root.path().join("r");
        let workspace = root.path().join("w");
        for path in [&profile, &home, &runtime, &workspace] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        Self {
            root,
            profile,
            home,
            runtime,
            workspace,
            script: script.to_string(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haider"));
        command.env_clear();
        for name in ["PATH", "SYSTEMROOT", "WINDIR", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .args(args)
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("HAIDER_PROFILE_DIR", &self.profile)
            .env("HAIDER_RUNTIME_DIR", &self.runtime)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HAIDER_NO_UPDATE_CHECK", "1")
            .env("HAIDER_TEST_DEVICE_NAME", "test-mac")
            .env("HAIDER_TEST_FAKE_PROVIDER", &self.script)
            .env("RUST_MIN_STACK", "8388608")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        command
    }

    fn invoke(&self, args: &[&str], bound: Duration) -> Result<Output, String> {
        // Files keep output bounded by disk rather than an undrained pipe;
        // no reader thread can survive a killed CLI or inherited descriptor.
        let stdout =
            tempfile::NamedTempFile::new_in(self.root.path()).map_err(|e| e.to_string())?;
        let stderr =
            tempfile::NamedTempFile::new_in(self.root.path()).map_err(|e| e.to_string())?;
        let mut command = self.command(args);
        command
            .stdout(Stdio::from(stdout.reopen().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(stderr.reopen().map_err(|e| e.to_string())?));
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + bound;
        let status = loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) => break status,
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{args:?} exceeded derived {bound:?} bound"));
                }
            }
        };
        Ok(Output {
            status,
            stdout: std::fs::read(stdout.path()).map_err(|e| e.to_string())?,
            stderr: std::fs::read(stderr.path()).map_err(|e| e.to_string())?,
        })
    }

    fn run(&self, args: &[&str]) -> Output {
        self.invoke(args, COMMAND_BOUND)
            .expect("bounded CLI invocation")
    }

    fn json(&self, args: &[&str], schema: &str, exit: i32) -> Value {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(exit),
            "args={args:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.ends_with(b"\n"), "JSON document ends with LF");
        let document: Value =
            serde_json::from_slice(&output.stdout).expect("one complete JSON document");
        assert_eq!(document["schema"], schema, "{document}");
        assert_eq!(document["ok"], exit == 0, "{document}");
        if exit == 0 {
            assert!(document["error"].is_null(), "{document}");
            assert!(document["result"].is_object(), "{document}");
        } else {
            assert!(document["error"]["code"].as_str().is_some(), "{document}");
            assert!(
                document["error"]["message"].as_str().is_some(),
                "{document}"
            );
            assert!(document["error"]["retryable"].is_boolean(), "{document}");
        }
        document
    }

    fn spawn(&self) -> Value {
        self.json(
            &[
                "agent",
                "spawn",
                "return the fixture report",
                "--task",
                "agentcli-report",
                "--provider",
                "fake",
                "--model",
                "fake-model",
                "--json",
                "--timeout",
                "10s",
            ],
            "haider.agent.spawn.v1",
            0,
        )["result"]
            .clone()
    }

    fn wait(&self, spawned: &Value, exit: i32) -> Value {
        self.json(
            &[
                "agent",
                "wait",
                field(spawned, "session_id"),
                field(spawned, "agent_id"),
                "--json",
                "--timeout",
                "10s",
                "--no-spawn",
            ],
            "haider.agent.wait.v1",
            exit,
        )
    }

    fn journal(&self, session_id: &str) -> Vec<Value> {
        let output = self.run(&["status", "--json", "--no-spawn"]);
        assert!(output.status.success());
        let status: Value = serde_json::from_slice(&output.stdout).expect("status socket identity");
        let endpoint = PathBuf::from(field(&status["daemon"], "socket_path"));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("RPC runtime")
            .block_on(async {
                // Registry #95: the ordinary RpcClient reader owns Ping/Pong
                // while its finite, default 60s request deadline is active.
                let connection =
                    haider_client::connect(&endpoint, haider_client::ClientConfig::default())
                        .await
                        .expect("real-daemon journal connection");
                let response = connection
                    .client
                    .request(haider_rpc::RequestBody::SessionRead {
                        session_id: haider_protocol::ids::SessionId::new(session_id),
                        range: haider_rpc::SeqRange {
                            start_seq: 1,
                            end_seq: 1024,
                        },
                    })
                    .await
                    .expect("real-daemon child journal request");
                let _ = connection.client.close();
                match response {
                    haider_rpc::ResponseBody::SessionRead { result } => result
                        .envelopes
                        .into_iter()
                        .map(|event| serde_json::to_value(event).expect("journal JSON"))
                        .collect(),
                    other => panic!("expected child journal, got {other:?}"),
                }
            })
    }

    fn stop(&self) {
        let status = self.run(&["status", "--json", "--no-spawn"]);
        assert!(
            status.status.success(),
            "owned daemon is reachable before cleanup"
        );
        let status: Value = serde_json::from_slice(&status.stdout).expect("status identity");
        assert_eq!(status["schema"], "haider.observe.v1");
        assert_eq!(
            std::fs::canonicalize(field(&status, "profile_path")).expect("reported profile"),
            std::fs::canonicalize(&self.profile).expect("fixture profile")
        );
        let pid = u32::try_from(
            status["daemon"]["pid"]
                .as_u64()
                .expect("positive owned PID"),
        )
        .expect("OS PID fits u32");
        assert!(pid > 0);
        let output = self
            .invoke(&["daemon", "stop", "--json"], STOP_BOUND)
            .expect("bounded clean stop");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stopped: Value = serde_json::from_slice(&output.stdout).expect("stop receipt");
        assert_eq!(stopped["outcome"], "stopped_cleanly", "{stopped}");
        assert_eq!(stopped["daemon"]["pid"], pid);
        assert_eq!(stopped["daemon"]["process_exited"], true);
        assert!(
            !process_alive(pid),
            "no-orphan proof: owned PID {pid} must disappear"
        );
        let absent = self.run(&["status", "--json", "--no-spawn"]);
        assert_eq!(absent.status.code(), Some(69));
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        // Failure-path cleanup remains profile-scoped. Successful tests call
        // stop() explicitly so cleanup failure changes their verdict.
        let _ = self.invoke(&["daemon", "stop", "--json"], STOP_BOUND);
    }
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .expect(key)
}

fn process_alive(pid: u32) -> bool {
    haider_platform::process_id(Some(pid))
        .and_then(|id| haider_platform::ProcessExitMonitor::capture(id).ok())
        .is_some()
}

fn report_script(text: &str) -> Value {
    json!([{"step":"emit_text","text":text},{"step":"finish","reason":"end_turn"}])
}

fn provider_requests(events: &[Value]) -> usize {
    events
        .iter()
        .filter(|event| {
            let payload = &event["payload"];
            payload["type"] == "item"
                && payload["event"] == "completed"
                && payload["item"]["item"] == "extension"
                && payload["item"]["kind"] == "cache_request_attempt_v1"
        })
        .count()
}

#[test]
fn agent_spawn_list_wait_publish_durable_child_result_and_exact_identities() {
    let profile = Profile::new(report_script("AGENTCLI_CHILD_RESULT_NONCE"));
    let spawned = profile.spawn();
    for key in [
        "session_id",
        "run_id",
        "agent_id",
        "child_session_id",
        "child_run_id",
    ] {
        let _ = field(&spawned, key);
    }
    assert_ne!(spawned["session_id"], spawned["child_session_id"]);
    assert_ne!(spawned["run_id"], spawned["child_run_id"]);
    let waited = profile.wait(&spawned, 0);
    let result = &waited["result"];
    assert_eq!(result["state"], "done");
    assert_eq!(result["child_run_id"], spawned["child_run_id"]);
    assert_eq!(result["report"]["agent"], spawned["agent_id"]);
    assert_eq!(result["report"]["summary"], "AGENTCLI_CHILD_RESULT_NONCE");
    assert!(result["terminal_seq"].as_u64().is_some_and(|seq| seq > 0));
    assert!(
        result["child_result_seq"]
            .as_u64()
            .is_some_and(|seq| seq > 0)
    );
    assert_eq!(result["report_source"], "child_result");
    let parent_journal = profile.journal(field(&spawned, "session_id"));
    let durable_report = parent_journal
        .iter()
        .find(|event| event["seq"] == result["child_result_seq"])
        .expect("public result sequence names a real parent journal item");
    assert_eq!(durable_report["payload"]["item"]["item"], "child_result");
    assert_eq!(
        durable_report["payload"]["item"]["report"],
        result["report"]
    );
    assert_eq!(
        provider_requests(&parent_journal),
        0,
        "coordinator never calls a provider"
    );
    let child_journal = profile.journal(field(&spawned, "child_session_id"));
    assert_eq!(
        provider_requests(&child_journal),
        1,
        "one sole child provider request"
    );
    let durable_terminal = child_journal
        .iter()
        .find(|event| event["seq"] == result["terminal_seq"])
        .expect("terminal sequence names a real child journal fact");
    assert_eq!(durable_terminal["payload"]["type"], "run_state");
    assert_eq!(durable_terminal["payload"]["state"], "done");
    let listed = profile.json(
        &[
            "agent",
            "list",
            field(&spawned, "session_id"),
            "--json",
            "--no-spawn",
        ],
        "haider.agent.list.v1",
        0,
    );
    let roots = listed["result"]["roots"]
        .as_array()
        .expect("native fleet roots");
    assert_eq!(
        roots.len(),
        1,
        "the coordinator spawns exactly one actual child"
    );
    assert_eq!(roots[0]["agent_id"], spawned["agent_id"]);
    assert_eq!(roots[0]["task"], "agentcli-report");
    // A second observer must read the same durable result without another
    // fake-provider segment or a fabricated completion.
    assert_eq!(profile.wait(&spawned, 0)["result"], waited["result"]);
    profile.stop();
}

#[test]
fn agent_message_to_idle_child_returns_new_run_delivery_receipt() {
    let profile = Profile::new(json!([
        {"step":"emit_text","text":"FIRST_CHILD_REPORT"},{"step":"finish","reason":"end_turn"},
        {"step":"emit_text","text":"SECOND_CHILD_REPORT"},{"step":"finish","reason":"end_turn"}
    ]));
    let spawned = profile.spawn();
    profile.wait(&spawned, 0);
    let messaged = profile.json(
        &[
            "agent",
            "message",
            field(&spawned, "session_id"),
            field(&spawned, "agent_id"),
            "perform the follow-up",
            "--json",
            "--no-spawn",
        ],
        "haider.agent.message.v1",
        0,
    );
    let receipt = &messaged["result"]["receipt"];
    assert_eq!(receipt["agent"], spawned["agent_id"]);
    assert_eq!(receipt["delivery"], "delivered_queued");
    assert_ne!(receipt["child_run_id"], spawned["child_run_id"]);
    let waited = profile.wait(&spawned, 0);
    assert_eq!(waited["result"]["child_run_id"], receipt["child_run_id"]);
    assert_eq!(waited["result"]["report"]["summary"], "SECOND_CHILD_REPORT");
    assert_eq!(waited["result"]["report_source"], "child_journal");
    assert!(waited["result"]["child_result_seq"].is_null());
    profile.stop();
}

#[test]
fn agent_wait_timeout_observes_without_cancelling_then_cancel_is_terminal() {
    let profile = Profile::new(json!([{"step":"hang"}]));
    let spawned = profile.spawn();
    let timed_out = profile.json(
        &[
            "agent",
            "wait",
            field(&spawned, "session_id"),
            field(&spawned, "agent_id"),
            "--json",
            "--timeout",
            "20ms",
            "--no-spawn",
        ],
        "haider.agent.wait.v1",
        124,
    );
    assert_eq!(timed_out["error"]["code"], "timeout");
    assert_eq!(timed_out["result"]["session_id"], spawned["session_id"]);
    assert_eq!(timed_out["result"]["agent_id"], spawned["agent_id"]);
    let cancelled = profile.json(
        &[
            "agent",
            "cancel",
            field(&spawned, "session_id"),
            field(&spawned, "agent_id"),
            "--json",
            "--no-spawn",
        ],
        "haider.agent.cancel.v1",
        0,
    );
    assert_eq!(cancelled["result"]["child_run_id"], spawned["child_run_id"]);
    assert!(
        cancelled["result"]["terminal_seq"].is_null(),
        "wait timeout must not cancel the child"
    );
    let waited = profile.wait(&spawned, 130);
    assert_eq!(waited["error"]["code"], "child_cancelled");
    assert_eq!(waited["result"]["state"], "cancelled");
    profile.stop();
}

#[test]
fn agent_headless_input_is_rejected_and_provider_continues_to_child_result() {
    let profile = Profile::new(json!([
        {"step":"emit_request_input","call_id":"agentcli-input","kind":"question","title":"Need operator input"},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"agentcli-input"},
        {"step":"emit_text","text":"AGENTCLI_CONTINUED_WITHOUT_HUMAN"},
        {"step":"finish","reason":"end_turn"}
    ]));
    let spawned = profile.spawn();
    let waited = profile.wait(&spawned, 0);
    assert_eq!(
        waited["result"]["report"]["summary"],
        "AGENTCLI_CONTINUED_WITHOUT_HUMAN"
    );
    let events = profile.journal(field(&spawned, "child_session_id"));
    let rejection = events
        .iter()
        .map(|event| &event["payload"])
        .find(|payload| payload["type"] == "tool_result" && payload["call_id"] == "agentcli-input")
        .expect("request_input has a durable result");
    assert_eq!(rejection["result"]["status"], "rejected");
    let preview: Value =
        serde_json::from_str(field(&rejection["result"], "preview")).expect("typed rejection");
    assert_eq!(preview["code"], "no_human_available");
    assert!(
        !events
            .iter()
            .any(|event| event["payload"]["state"] == "input_required")
    );
    profile.stop();
}

#[test]
fn agent_wait_failed_child_returns_durable_red_report_and_exit_one() {
    let profile = Profile::new(json!([{"step":"malformed_frame"}]));
    let spawned = profile.spawn();
    let waited = profile.wait(&spawned, 1);
    assert_eq!(waited["error"]["code"], "child_failed");
    assert_eq!(waited["result"]["state"], "errored");
    assert_eq!(waited["result"]["report"]["verified"], "red");
    assert!(
        waited["result"]["child_result_seq"]
            .as_u64()
            .is_some_and(|seq| seq > 0)
    );
    profile.stop();
}

#[test]
fn workflow_list_run_status_expose_actual_child_graph_activation() {
    let profile = Profile::new(json!([{"step":"hang"}]));
    let listed = profile.json(
        &["workflow", "list", "--json"],
        "haider.workflow.list.v1",
        0,
    );
    let workflows = listed["result"]["workflows"]
        .as_array()
        .expect("authoritative workflow catalog");
    assert!(
        workflows
            .iter()
            .any(|workflow| workflow["id"] == "child-implement-verify")
    );
    let spawned = profile.json(
        &[
            "workflow",
            "run",
            "child-implement-verify",
            "implement and independently verify",
            "--trigger",
            "mutation_with_independent_verification",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--json",
            "--timeout",
            "10s",
        ],
        "haider.workflow.run.v1",
        0,
    )["result"]
        .clone();
    let status = profile.json(
        &[
            "workflow",
            "status",
            field(&spawned, "child_session_id"),
            "--json",
            "--no-spawn",
        ],
        "haider.workflow.status.v1",
        0,
    );
    assert_eq!(status["result"]["session_id"], spawned["child_session_id"]);
    assert!(
        status["result"]["graph"].is_object(),
        "execution has a real graph: {status}"
    );
    assert!(
        status["result"].get("activation").is_some(),
        "typed activation absence is explicit: {status}"
    );
    profile.json(
        &[
            "agent",
            "cancel",
            field(&spawned, "session_id"),
            field(&spawned, "agent_id"),
            "--json",
        ],
        "haider.agent.cancel.v1",
        0,
    );
    profile.wait(&spawned, 130);
    profile.stop();
}

#[test]
fn agent_and_workflow_no_spawn_leave_fresh_profile_without_daemon() {
    let profile = Profile::new(report_script("must not consume"));
    for (args, schema) in [
        (
            vec![
                "agent",
                "spawn",
                "no spawn",
                "--provider",
                "fake",
                "--json",
                "--no-spawn",
            ],
            "haider.agent.spawn.v1",
        ),
        (
            vec!["agent", "list", "missing-parent", "--json", "--no-spawn"],
            "haider.agent.list.v1",
        ),
        (
            vec!["workflow", "list", "--json", "--no-spawn"],
            "haider.workflow.list.v1",
        ),
        (
            vec![
                "workflow",
                "status",
                "missing-child",
                "--json",
                "--no-spawn",
            ],
            "haider.workflow.status.v1",
        ),
    ] {
        profile.json(&args, schema, 69);
    }
    assert!(!profile.profile.join("lock.owner").exists());
    assert_eq!(
        profile
            .run(&["status", "--json", "--no-spawn"])
            .status
            .code(),
        Some(69)
    );
}

#[test]
fn agent_missing_target_and_malformed_cli_are_typed_errors() {
    let profile = Profile::new(report_script("valid child"));
    let spawned = profile.spawn();
    profile.wait(&spawned, 0);
    for verb in ["wait", "cancel"] {
        profile.json(
            &[
                "agent",
                verb,
                field(&spawned, "session_id"),
                "missing-agent",
                "--json",
                "--timeout",
                "1s",
            ],
            &format!("haider.agent.{verb}.v1"),
            70,
        );
    }
    profile.json(
        &[
            "agent",
            "message",
            field(&spawned, "session_id"),
            "missing-agent",
            "hello",
            "--json",
        ],
        "haider.agent.message.v1",
        70,
    );
    profile.json(
        &[
            "agent",
            "wait",
            field(&spawned, "session_id"),
            field(&spawned, "agent_id"),
            "--json",
            "--timeout",
            "invalid",
        ],
        "haider.agent.wait.v1",
        2,
    );
    profile.json(
        &[
            "workflow",
            "run",
            "child-implement-verify",
            "prompt",
            "--json",
            "--trigger",
            "invalid",
        ],
        "haider.workflow.run.v1",
        2,
    );
    profile.stop();
}

#[test]
fn agent_spawn_prompt_flag_and_cwd_are_public_noninteractive_inputs() {
    let profile = Profile::new(report_script("FLAG_PROMPT_RESULT"));
    let spawned = profile.json(
        &[
            "agent",
            "spawn",
            "-p",
            "flag prompt",
            "--task",
            "flag-task",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--cwd",
            profile.workspace.to_str().expect("fixture path"),
            "--json",
        ],
        "haider.agent.spawn.v1",
        0,
    )["result"]
        .clone();
    assert_eq!(
        profile.wait(&spawned, 0)["result"]["report"]["summary"],
        "FLAG_PROMPT_RESULT"
    );
    profile.stop();
    // Each daemon owns one finite provider script; each independent prompt
    // case gets its own script and process cleanup proof.
    let profile = Profile::new(report_script("FLAG_PROMPT_RESULT"));
    let literal_help = profile.json(
        &[
            "agent",
            "spawn",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--json",
            "--",
            "--help",
        ],
        "haider.agent.spawn.v1",
        0,
    )["result"]
        .clone();
    assert_eq!(
        profile.wait(&literal_help, 0)["result"]["report"]["summary"],
        "FLAG_PROMPT_RESULT"
    );
    profile.stop();
    let profile = Profile::new(report_script("FLAG_PROMPT_RESULT"));
    let literal_json = profile.run(&[
        "agent",
        "spawn",
        "--provider",
        "fake",
        "--model",
        "fake-model",
        "--",
        "--json",
    ]);
    assert!(literal_json.status.success());
    let plain_result: Value =
        serde_json::from_slice(&literal_json.stdout).expect("human-readable result object");
    assert!(plain_result.get("schema").is_none());
    assert_eq!(
        profile.wait(&plain_result, 0)["result"]["report"]["summary"],
        "FLAG_PROMPT_RESULT"
    );
    profile.stop();
}

#[test]
fn agent_spawn_provider_only_resolves_model_through_daemon_session_authority() {
    let profile = Profile::new(report_script("DAEMON_RESOLVED_MODEL_REPORT"));
    // The injected daemon factory routes every built-in provider to the fake
    // script. xai supplies a real published default in the native catalog;
    // the ad hoc `fake` provider deliberately has no default model.
    let spawned = profile.json(
        &[
            "agent",
            "spawn",
            "resolve the provider default model",
            "--provider",
            "xai",
            "--json",
        ],
        "haider.agent.spawn.v1",
        0,
    )["result"]
        .clone();
    let waited = profile.wait(&spawned, 0);
    assert_eq!(
        waited["result"]["report"]["summary"],
        "DAEMON_RESOLVED_MODEL_REPORT"
    );
    profile.stop();
}

#[test]
fn agent_spawn_provider_without_published_default_retains_native_rejection() {
    let profile = Profile::new(report_script("must not consume a provider request"));
    let rejected = profile.json(
        &[
            "agent",
            "spawn",
            "no invented default",
            "--provider",
            "fake",
            "--json",
        ],
        "haider.agent.spawn.v1",
        70,
    );
    assert_eq!(rejected["error"]["code"], "no_default_model");
    assert!(rejected["result"].is_null());
    profile.stop();
}
