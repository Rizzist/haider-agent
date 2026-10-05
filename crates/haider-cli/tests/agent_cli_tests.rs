#![allow(clippy::expect_used)]
//! Public agent/workflow doors against the real sibling daemon. Each fixture
//! owns one short profile, one consumptive fake-provider script, and its PID.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// Registry #94: one command may cold-start (30s), issue its RPC (60s),
// observe --timeout 30s, and publish/drain its terminal (2s): 30+60+30+2=122s.
const COMMAND_BOUND: Duration = Duration::from_secs(30 + 60 + 30 + 2);
// Product daemon stop is 20s, followed by 2s process-exit observation.
const STOP_BOUND: Duration = Duration::from_secs(20 + 2);

struct Profile {
    root: tempfile::TempDir,
    profile: PathBuf,
    home: PathBuf,
    runtime: PathBuf,
    workspace: PathBuf,
    script: String,
    tool_exposure: Option<&'static str>,
    extra_env: Vec<(&'static str, String)>,
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
            tool_exposure: Some("spawn_subagent"),
            extra_env: Vec::new(),
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
        if let Some(exposure) = self.tool_exposure {
            command.env("HAIDER_TOOL_EXPOSURE", exposure);
        }
        for (name, value) in &self.extra_env {
            command.env(name, value);
        }
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
        let monitor = haider_platform::ProcessExitMonitor::capture(
            haider_platform::process_id(Some(pid)).expect("valid owned daemon PID"),
        )
        .expect("pin the live status-owned daemon before stopping it");
        let stop_deadline = Instant::now() + STOP_BOUND;
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
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("exit observation runtime")
            .block_on(async {
                // Share the existing 20s stop + 2s exit observation bound;
                // retaining the kernel identity avoids PID reuse and wrapped
                // missing-process errors from recapturing an exited daemon.
                tokio::time::timeout(
                    stop_deadline.saturating_duration_since(Instant::now()),
                    monitor.wait(),
                )
                .await
                .expect("owned daemon exits within the remaining stop bound")
                .expect("authoritative owned daemon exit observation");
            });
        #[cfg(windows)]
        assert!(!haider_platform::process_exists(pid));
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

fn report_script(text: &str) -> Value {
    json!([{"step":"emit_text","text":text},{"step":"finish","reason":"end_turn"}])
}

#[cfg(unix)]
fn ax2_child_script(command: &str, label: &str, slow_ms: u64) -> Value {
    json!([
        {"step":"delay","ms":slow_ms},
        {"step":"emit_tool_call","call_id":format!("write-{label}"),"name":"process_exec","args":{"command":command}},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":format!("write-{label}")},
        {"step":"emit_text","text":format!("{label} implementation and tests written")},
        {"step":"finish","reason":"end_turn"}
    ])
}

#[cfg(unix)]
fn ax2_log(workspace: &std::path::Path, moment: Value) {
    use std::io::Write as _;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workspace.join("experience-log.jsonl"))
        .expect("AX-2 experience log");
    writeln!(log, "{moment}").expect("AX-2 orchestration moment");
}

/// AX-2 T3 replay: three real delegated tool writers and a delayed integration
/// process run through the real CLI/daemon with a private model-routed fake.
/// `HAIDER_AX2_SLOW_SECONDS=90` reproduces the original deliberate build wait;
/// the ordinary suite uses ten seconds so registration wins under CI load.
#[cfg(unix)]
#[test]
fn ax2_t3_fake_parent_finishes_while_three_children_and_build_run() {
    let slow_seconds = std::env::var("HAIDER_AX2_SLOW_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10);
    let csv = r#"mkdir -p csvstat; touch csvstat/__init__.py; cat > csvstat/csvstat.py <<'PY'
import csv, json, statistics, sys
def stats(path, column):
    with open(path, newline='', encoding='utf-8') as stream:
        values = [float(row[column]) for row in csv.DictReader(stream) if row[column].strip()]
    if not values:
        raise ValueError('no numeric values')
    return {'count': len(values), 'mean': statistics.mean(values), 'median': statistics.median(values), 'min': min(values), 'max': max(values)}
if __name__ == '__main__':
    print(json.dumps(stats(sys.argv[1], sys.argv[2]), sort_keys=True))
PY
cat > csvstat/test_csvstat.py <<'PY'
import tempfile, unittest
from pathlib import Path
from csvstat.csvstat import stats
class CsvStatTests(unittest.TestCase):
    def test_column_stats(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'numbers.csv'
            path.write_text('n\n1\n2\n5\n', encoding='utf-8')
            self.assertEqual(stats(path, 'n'), {'count': 3, 'mean': 8/3, 'median': 2, 'min': 1, 'max': 5})
    def test_empty_column(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'numbers.csv'
            path.write_text('n\n\n', encoding='utf-8')
            with self.assertRaises(ValueError): stats(path, 'n')
PY"#;
    let slug = r#"mkdir -p slugify; touch slugify/__init__.py; cat > slugify/slugify.py <<'PY'
import re, sys, unicodedata
def slugify(value):
    value = value.casefold().translate(str.maketrans({'ß':'ss', 'ø':'o', 'ł':'l', 'æ':'ae'}))
    value = unicodedata.normalize('NFKD', value).encode('ascii', 'ignore').decode('ascii')
    return re.sub(r'[^a-z0-9]+', '-', value).strip('-')
if __name__ == '__main__': print(slugify(' '.join(sys.argv[1:])))
PY
cat > slugify/test_slugify.py <<'PY'
import unittest
from slugify.slugify import slugify
class SlugTests(unittest.TestCase):
    def test_unicode(self): self.assertEqual(slugify('Straße Øresund Łódź'), 'strasse-oresund-lodz')
    def test_punctuation(self): self.assertEqual(slugify('  Hello,   world!! '), 'hello-world')
PY"#;
    let path = r#"mkdir -p jsonpath-lite; touch jsonpath-lite/__init__.py; cat > jsonpath-lite/jsonpath_lite.py <<'PY'
import json, re, sys
def extract(data, path):
    tokens = re.findall(r'[^.\[\]]+|\[\d+\]', path)
    if not tokens: raise ValueError('empty path')
    for token in tokens:
        data = data[int(token[1:-1])] if token.startswith('[') else data[token]
    return data
if __name__ == '__main__':
    with open(sys.argv[1], encoding='utf-8') as stream: print(json.dumps(extract(json.load(stream), sys.argv[2])))
PY
cat > jsonpath-lite/test_jsonpath_lite.py <<'PY'
import importlib.util, unittest
from pathlib import Path
spec = importlib.util.spec_from_file_location('jsonpath_lite', Path(__file__).with_name('jsonpath_lite.py'))
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
class PathTests(unittest.TestCase):
    def test_dotted_index(self): self.assertEqual(module.extract({'a': {'b': [{'c': 7}]}}, 'a.b[0].c'), 7)
    def test_missing(self):
        with self.assertRaises(KeyError): module.extract({}, 'missing')
PY"#;
    let setup = r#"cat > run_all_tests.sh <<'SH'
#!/bin/sh
set -eu
python3 -m unittest discover -s csvstat -t . -p 'test_*.py'
python3 -m unittest discover -s slugify -t . -p 'test_*.py'
python3 -m unittest discover -s jsonpath-lite -p 'test_*.py'
SH
chmod +x run_all_tests.sh"#;
    let parent = json!([
        {"step":"emit_tool_call","call_id":"setup","name":"process_exec","args":{"command":setup}},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"setup"},
        {"step":"emit_tool_call","call_id":"csv-child","name":"spawn_subagent","args":{"task":"csvstat","prompt":"Implement CSV statistics and tests","model":"fake-csv","provider":"fake","background":true}},
        {"step":"emit_tool_call","call_id":"slug-child","name":"spawn_subagent","args":{"task":"slugify","prompt":"Implement Unicode slugifier and tests","model":"fake-slug","provider":"fake","background":true}},
        {"step":"emit_tool_call","call_id":"path-child","name":"spawn_subagent","args":{"task":"jsonpath-lite","prompt":"Implement dotted JSON extractor and tests","model":"fake-path","provider":"fake","background":true}},
        {"step":"emit_tool_call","call_id":"integration-build","name":"process_exec","args":{"command":format!("sleep {slow_seconds} && ./run_all_tests.sh > build.log 2>&1"),"background":true,"name":"AX-2 integration"}},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"csv-child"},
        {"step":"expect_tool_result","call_id":"slug-child"},
        {"step":"expect_tool_result","call_id":"path-child"},
        {"step":"expect_tool_result","call_id":"integration-build"},
        {"step":"emit_text","text":"All three children and the integration build are running; collect their handles."},
        {"step":"finish","reason":"end_turn"},
        {"step":"emit_text","text":"The child completion monitor fired; the integration task is still running."},
        {"step":"finish","reason":"end_turn"},
        {"step":"emit_tool_call","call_id":"integration-verify","name":"process_exec","args":{"command":"./run_all_tests.sh"}},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"integration-verify"},
        {"step":"emit_text","text":"Monitor woke the parent; all three tool suites pass."},
        {"step":"finish","reason":"end_turn"}
    ]);
    let script = json!({"by_model": {
        "fake-model": parent,
        "fake-csv": ax2_child_script(csv, "csv", 1200),
        "fake-slug": ax2_child_script(slug, "slug", 0),
        "fake-path": ax2_child_script(path, "path", 0),
    }});
    let mut profile = Profile::new(script);
    profile.tool_exposure = None;
    let models = ["fake-model", "fake-csv", "fake-slug", "fake-path"]
        .into_iter()
        .map(|slug| json!({"slug":slug,"display_name":slug,"supported_efforts":[],"visible":true}))
        .collect::<Vec<_>>();
    let fetched_at = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("time fits");
    haider_store::Store::open(&profile.profile)
        .expect("catalog store")
        .put_provider_models("fake", &Value::Array(models).to_string(), None, fetched_at)
        .expect("publish fake model routes");
    let output = profile.run(&[
        "run",
        "--provider",
        "fake",
        "--model",
        "fake-model",
        "--jsonl",
        "-p",
        "AX-2 T3 orchestration replay",
    ]);
    assert!(
        output.status.success(),
        "parent exit {:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("JSONL event"))
        .collect::<Vec<_>>();
    let result = |call_id: &str| -> Value {
        let event = events
            .iter()
            .find(|event| {
                event["payload"]["type"] == "tool_result" && event["payload"]["call_id"] == call_id
            })
            .expect("tool result");
        serde_json::from_str(field(&event["payload"]["result"], "preview")).expect("typed preview")
    };
    let handles = ["csv-child", "slug-child", "path-child"].map(&result);
    let build = result("integration-build");
    let task_id = field(&build, "task_id").to_owned();
    let parent_session = field(&handles[0], "parent_session_id").to_owned();
    eprintln!("AX2-T3 parent_session={parent_session} integration_task={task_id}");
    for (tool, handle) in ["csvstat", "slugify", "jsonpath-lite"]
        .into_iter()
        .zip(&handles)
    {
        ax2_log(
            &profile.workspace,
            json!({"event":"spawn","tool":tool,"agent":handle["agent"],"status":handle["status"]}),
        );
    }
    ax2_log(
        &profile.workspace,
        json!({"event":"background_build_started","task_id":task_id,"slow_seconds":slow_seconds}),
    );
    assert!(
        handles
            .iter()
            .all(|handle| handle["parent_session_id"].as_str() == Some(parent_session.as_str()))
    );
    let listed = profile.json(
        &["agent", "list", &parent_session, "--json", "--no-spawn"],
        "haider.agent.list.v1",
        0,
    );
    for handle in &handles {
        assert!(
            listed.to_string().contains(field(handle, "agent")),
            "{listed}"
        );
    }
    let status = profile.run(&["status", "--json", "--no-spawn"]);
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).expect("daemon status");
    let endpoint = PathBuf::from(field(&status["daemon"], "socket_path"));
    let generation = profile
        .journal(&parent_session)
        .first()
        .and_then(|event| event["worker_generation"].as_u64())
        .expect("parent worker generation");
    let agent_monitor_id = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("monitor runtime")
        .block_on(async {
            let connection = haider_client::connect(&endpoint, haider_client::ClientConfig::default())
                .await.expect("real monitor connection");
            let session_id = haider_protocol::ids::SessionId::new(parent_session.clone());
            let attached = connection.client.request(haider_rpc::RequestBody::SessionAttach {
                session_id: session_id.clone(), after_seq: 0, mode: haider_rpc::AttachMode::Control,
                sealed_replay: false,
            }).await.expect("control attachment");
            assert!(matches!(&attached, haider_rpc::ResponseBody::SessionAttach { .. }), "{attached:?}");
            let registered = connection.client.request(haider_rpc::RequestBody::MonitorRegister {
                command_id: haider_rpc::CommandId::new("ax2-task-monitor"),
                session_id: session_id.clone(),
                worker_generation: generation,
                source: haider_rpc::MonitorSourceWire::Task { task_id: task_id.clone() },
                filter: None,
                action: haider_rpc::MonitorActionWire { report: true, follow_up: Some("The integration build completed. Run all three suites and report the result.".into()) },
                occurrence: haider_rpc::MonitorOccurrenceWire::Once,
                lifetime: haider_rpc::MonitorLifetimeWire::Session,
            }).await.expect("register task monitor");
            assert!(matches!(&registered, haider_rpc::ResponseBody::MonitorRegister { receipt } if matches!(&receipt.outcome, haider_rpc::MonitorRegisterOutcomeWire::Registered { .. })), "{registered:?}");
            let agent_watch = connection.client.request(haider_rpc::RequestBody::MonitorRegister {
                command_id: haider_rpc::CommandId::new("ax2-agent-monitor"),
                session_id,
                worker_generation: generation,
                source: haider_rpc::MonitorSourceWire::Agent { agent_id: field(&handles[0], "agent").into() },
                filter: None,
                action: haider_rpc::MonitorActionWire { report: true, follow_up: None },
                occurrence: haider_rpc::MonitorOccurrenceWire::Every,
                lifetime: haider_rpc::MonitorLifetimeWire::Session,
            }).await.expect("register agent monitor");
            let agent_monitor_id = match &agent_watch {
                haider_rpc::ResponseBody::MonitorRegister { receipt } => match &receipt.outcome {
                    haider_rpc::MonitorRegisterOutcomeWire::Registered { monitor } => monitor.monitor_id.clone(),
                    other => panic!("agent watch refused: {other:?}"),
                },
                other => panic!("agent watch response: {other:?}"),
            };
            let _ = connection.client.close();
            agent_monitor_id
        });
    ax2_log(
        &profile.workspace,
        json!({"event":"monitor_registered","source":"task","id":task_id}),
    );
    ax2_log(
        &profile.workspace,
        json!({"event":"monitor_registered","source":"agent","id":handles[0]["agent"]}),
    );
    for handle in &handles {
        assert_eq!(handle["status"], "child_spawned", "{handle}");
        ax2_log(
            &profile.workspace,
            json!({"event":"wait","agent":handle["agent"]}),
        );
        let wait_args = [
            "agent",
            "wait",
            field(handle, "parent_session_id"),
            field(handle, "agent"),
            "--json",
            "--timeout",
            "30s",
            "--no-spawn",
        ];
        let waited_output = profile.run(&wait_args);
        if !waited_output.status.success() {
            let child_events = profile.journal(field(handle, "child_session_id"));
            let recent = child_events
                .iter()
                .rev()
                .take(20)
                .map(|event| {
                    json!({
                        "seq": event["seq"],
                        "type": event["payload"]["type"],
                        "state": event["payload"]["state"],
                    })
                })
                .collect::<Vec<_>>();
            panic!(
                "child wait failed: {}\nrecent child events: {recent:#?}",
                String::from_utf8_lossy(&waited_output.stdout)
            );
        }
        let waited: Value = serde_json::from_slice(&waited_output.stdout).expect("agent wait JSON");
        assert_eq!(waited["result"]["state"], "done", "{waited}");
        assert_eq!(
            waited["result"]["report_source"], "child_journal",
            "{waited}"
        );
        eprintln!("AX2-T3 child={} state=done", field(handle, "agent"));
        ax2_log(
            &profile.workspace,
            json!({"event":"result_retrieved","agent":handle["agent"],"state":"done"}),
        );
    }
    assert!(profile.workspace.join("csvstat/csvstat.py").exists());
    assert!(profile.workspace.join("slugify/slugify.py").exists());
    assert!(
        profile
            .workspace
            .join("jsonpath-lite/jsonpath_lite.py")
            .exists()
    );
    let agent_watch_deadline = Instant::now() + Duration::from_secs(5);
    let mut agent_watch_fired = false;
    while Instant::now() < agent_watch_deadline {
        let endpoint = endpoint.clone();
        let session = parent_session.clone();
        let monitor = agent_monitor_id.clone();
        agent_watch_fired = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("list runtime")
            .block_on(async {
                let connection = haider_client::connect(&endpoint, haider_client::ClientConfig::default()).await.expect("monitor list connection");
                let response = connection.client.request(haider_rpc::RequestBody::MonitorList { session_id: haider_protocol::ids::SessionId::new(session) }).await.expect("list monitors");
                let _ = connection.client.close();
                matches!(response, haider_rpc::ResponseBody::MonitorList { receipt } if matches!(&receipt.outcome, haider_rpc::MonitorListOutcomeWire::Listed { monitors } if monitors.iter().any(|entry| entry.monitor_id == monitor && entry.fire_count > 0)))
            });
        if agent_watch_fired {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(agent_watch_fired, "agent-handle monitor did not fire");
    ax2_log(
        &profile.workspace,
        json!({"event":"agent_monitor_fired","monitor_id":agent_monitor_id}),
    );
    let deadline = Instant::now() + Duration::from_secs(slow_seconds + 15);
    let mut saw_monitor_wake = false;
    while Instant::now() < deadline {
        let journal = profile.journal(&parent_session);
        let verifying_run = journal
            .iter()
            .find(|event| {
                event["payload"]["type"] == "tool_result"
                    && event["payload"]["call_id"] == "integration-verify"
                    && event["payload"]["result"]["preview"]
                        .as_str()
                        .is_some_and(|preview| preview.contains("\"exit_code\":0"))
            })
            .and_then(|event| event["run_id"].as_str());
        saw_monitor_wake = verifying_run.is_some_and(|run_id| {
            journal.iter().any(|event| {
                event["run_id"].as_str() == Some(run_id)
                    && event["payload"]["type"] == "run_state"
                    && event["payload"]["state"] == "done"
            })
        });
        if saw_monitor_wake {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !saw_monitor_wake {
        let journal = profile.journal(&parent_session);
        let recent = journal
            .iter()
            .filter(|event| {
                matches!(
                    event["payload"]["type"].as_str(),
                    Some("tool_result" | "user_message")
                )
            })
            .map(|event| {
                json!({
                    "seq": event["seq"],
                    "run": event["run_id"],
                    "type": event["payload"]["type"],
                    "call_id": event["payload"]["call_id"],
                    "text": event["payload"]["text"],
                    "preview": event["payload"]["result"]["preview"],
                })
            })
            .collect::<Vec<_>>();
        let build_log = std::fs::read_to_string(profile.workspace.join("build.log"));
        panic!(
            "task monitor did not wake parent for integration: {recent:#?}\nbuild log: {build_log:?}"
        );
    }
    ax2_log(
        &profile.workspace,
        json!({"event":"task_monitor_wake","task_id":task_id}),
    );
    let build_log =
        std::fs::read_to_string(profile.workspace.join("build.log")).expect("build log");
    assert!(build_log.contains("OK"), "{build_log}");
    ax2_log(
        &profile.workspace,
        json!({"event":"integration_verified","status":"pass"}),
    );
    let experience_log = std::fs::read_to_string(profile.workspace.join("experience-log.jsonl"))
        .expect("recorded orchestration moments");
    assert!(experience_log.lines().count() >= 13, "{experience_log}");
    eprintln!("AX2-T3 task_monitor_wake=done integration_tests=pass slow_seconds={slow_seconds}");
    profile.stop();
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
fn agent_spawn_without_tool_exposure_completes_the_actual_child() {
    let mut profile = Profile::new(report_script("DEFAULT_DELEGATION_REPORT"));
    // env_clear plus None proves the ordinary daemon default, with no
    // exposure override on either the initial CLI or its daemon sibling.
    profile.tool_exposure = None;
    let spawned = profile.spawn();
    let waited = profile.wait(&spawned, 0);
    assert_eq!(waited["result"]["state"], "done");
    assert_eq!(
        waited["result"]["report"]["summary"],
        "DEFAULT_DELEGATION_REPORT"
    );
    assert_eq!(
        provider_requests(&profile.journal(field(&spawned, "session_id"))),
        0,
        "the CLI coordinator does not make a provider request"
    );
    assert_eq!(
        provider_requests(&profile.journal(field(&spawned, "child_session_id"))),
        1,
        "the actual child completes one provider request"
    );
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
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0]["agent_id"], spawned["agent_id"]);
    profile.stop();
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
    // Discover request_input so this pin reaches the typed headless-input policy.
    let profile = Profile::new(json!([
        {"step":"emit_tool_call","call_id":"agentcli-discover-input","name":"list_tools","args":{"filter":"request_input"}},
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"agentcli-discover-input"},
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
    assert_eq!(waited["result"]["report_source"], "child_result");
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
    // Authenticated inventories are account-scoped. Seed a synthetic active
    // descriptor and its matching catalog before startup; xAI has no seeded
    // default. The injected daemon factory routes the pair to the fake script.
    let descriptor = haider_protocol::credential::CredentialDescriptor {
        alias: haider_protocol::ids::CredentialAlias::new("xai-fixture"),
        provider: "xai".into(),
        base_url: None,
        auth_method: haider_protocol::credential::AuthMethod::ApiKey,
        identity: "synthetic xAI account".into(),
        status: haider_protocol::credential::CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    let mut descriptors = serde_json::to_vec_pretty(&vec![descriptor.clone()])
        .expect("synthetic account descriptors");
    descriptors.push(b'\n');
    std::fs::write(profile.profile.join("accounts.json"), descriptors)
        .expect("seed active synthetic account");
    {
        let store = haider_store::Store::open(&profile.profile).expect("catalog store");
        store
            .put_provider_models(
                haider_store::ProviderModelCacheKey::for_account("xai", &descriptor).as_str(),
                r#"[{"slug":"grok-4","display_name":"Grok 4","supported_efforts":[],"visible":true}]"#,
                None,
                u64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("catalog fetch time")
                        .as_millis(),
                )
                .expect("fetch timestamp fits u64"),
            )
            .expect("publish discovered xAI model");
    }
    // Authenticated catalogs do not auto-pick a default. Publish the choice
    // through the daemon's validated management RPC against this inventory.
    assert!(
        profile
            .run(&["provider", "list", "--json"])
            .status
            .success()
    );
    let status = profile.run(&["status", "--json", "--no-spawn"]);
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).expect("daemon endpoint");
    let endpoint = PathBuf::from(field(&status["daemon"], "socket_path"));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("default publication runtime")
        .block_on(async {
            let connection =
                haider_client::connect(&endpoint, haider_client::ClientConfig::default())
                    .await
                    .expect("default publication connection");
            let listed = connection
                .client
                .request(haider_rpc::RequestBody::ProviderList { provider: None })
                .await
                .expect("current catalog revision");
            let haider_rpc::ResponseBody::ProviderList { revision, .. } = listed else {
                panic!("provider inventory: {listed:?}");
            };
            let published = connection
                .client
                .request(haider_rpc::RequestBody::AccountSetDefaultModel {
                    command_id: haider_rpc::CommandId("fixture-publish-xai-default".into()),
                    provider: "xai".into(),
                    model: "grok-4".into(),
                    expected_revision: revision,
                })
                .await
                .expect("publish validated discovered default");
            let haider_rpc::ResponseBody::AccountSetDefaultModel { provider, .. } = published
            else {
                panic!("default publication: {published:?}");
            };
            assert_eq!(provider.default_model.as_deref(), Some("grok-4"));
            let _ = connection.client.close();
        });
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
    assert_eq!(spawned["manifest"]["model_profile"], "grok-4");
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

// ---------------------------------------------------------------------------
// Owner decision 2026-09-25 (973-LANES.md "subagent stop semantics"):
// `run --stop` stops the run AND every subagent in its tree (session walk,
// later child runs included, monitors removed); `--stop-main` stops only the
// run (subagents keep running, a waited-on child's result is delivered
// later); `--stop-subagents` stops only the tree; `agent cancel <id>` stops
// that subagent only; `agent cancel <session> --all` stops the whole tree.

const STOP_OBSERVATION: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct DelegationRow {
    depth: i64,
    agent: String,
    child_session: String,
    parent_session: String,
    terminal: Option<i64>,
    state: Option<String>,
}

impl Profile {
    fn seed_models(&self, models: &[&str]) {
        let models = models
            .iter()
            .map(|slug| json!({"slug":slug,"display_name":slug,"supported_efforts":[],"visible":true}))
            .collect::<Vec<_>>();
        let fetched_at = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("time fits");
        haider_store::Store::open(&self.profile)
            .expect("catalog store")
            .put_provider_models("fake", &Value::Array(models).to_string(), None, fetched_at)
            .expect("publish fake model routes");
    }

    fn db(&self) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_with_flags(
            self.profile.join("store.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read-only store");
        connection
            .busy_timeout(Duration::from_secs(10))
            .expect("busy timeout");
        connection
    }

    fn delegations(&self) -> Vec<DelegationRow> {
        let connection = self.db();
        let mut statement = connection
            .prepare(
                "SELECT d.depth, d.agent_id, d.child_session_id, d.parent_session_id, h.terminal, h.state_json
                 FROM delegations d LEFT JOIN run_heads h
                   ON h.session_id = d.child_session_id AND h.run_id = d.child_run_id
                 ORDER BY d.depth, d.created_at_ms",
            )
            .expect("delegation query");
        statement
            .query_map([], |row| {
                let state: Option<String> = row.get(5)?;
                Ok(DelegationRow {
                    depth: row.get(0)?,
                    agent: row.get(1)?,
                    child_session: row.get(2)?,
                    parent_session: row.get(3)?,
                    terminal: row.get(4)?,
                    state: state.and_then(|state| {
                        serde_json::from_str::<Value>(&state)
                            .ok()
                            .and_then(|value| value["state"].as_str().map(str::to_owned))
                    }),
                })
            })
            .expect("delegation rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode delegation rows")
    }

    /// Nonterminal run heads of a session (original and later runs).
    fn live_runs(&self, session: &str) -> Vec<(String, String)> {
        let connection = self.db();
        let mut statement = connection
            .prepare(
                "SELECT run_id, state_json FROM run_heads WHERE session_id = ?1 AND terminal = 0",
            )
            .expect("run head query");
        statement
            .query_map([session], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("run heads")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode run heads")
    }

    fn row(&self, depth: i64) -> DelegationRow {
        self.delegations()
            .into_iter()
            .find(|row| row.depth == depth)
            .unwrap_or_else(|| panic!("no delegation at depth {depth}"))
    }

    fn start_run(&self, model: &str, prompt: &str) -> (String, String) {
        let output = self.run(&[
            "run",
            "--start",
            "--provider",
            "fake",
            "--model",
            model,
            "--json",
            "-p",
            prompt,
        ]);
        assert!(
            output.status.success(),
            "run --start: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout).expect("run --start JSON");
        (
            field(&body, "run_id").to_owned(),
            field(&body, "session_id").to_owned(),
        )
    }

    fn finish_run(&self, model: &str, prompt: &str) -> (String, String) {
        let output = self.run(&[
            "run",
            "--provider",
            "fake",
            "--model",
            model,
            "--json",
            "-p",
            prompt,
        ]);
        assert!(
            output.status.success(),
            "run: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout).expect("run JSON");
        (
            field(&body, "run_id").to_owned(),
            field(&body, "session_id").to_owned(),
        )
    }

    fn stop_run(&self, flag: &str, run_id: &str) -> Value {
        let output = self.run(&["run", flag, run_id, "--json"]);
        assert!(
            output.status.success(),
            "{flag}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("stop JSON")
    }

    fn run_state(&self, session: &str, run_id: &str) -> Option<String> {
        self.db()
            .query_row(
                "SELECT state_json FROM run_heads WHERE session_id = ?1 AND run_id = ?2",
                [session, run_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|state| {
                serde_json::from_str::<Value>(&state)
                    .ok()
                    .and_then(|value| value["state"].as_str().map(str::to_owned))
            })
    }

    /// A long `agent wait` keeps a client attached (so the ephemeral daemon
    /// never idles out) and doubles as the "wait resolves" observer.
    fn holder(&self, parent_session: &str, agent: &str) -> std::process::Child {
        self.command(&[
            "agent",
            "wait",
            parent_session,
            agent,
            "--json",
            "--timeout",
            "180s",
            "--no-spawn",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agent wait holder")
    }

    fn journal_kinds(&self, session: &str) -> Vec<Value> {
        self.journal(session)
            .into_iter()
            .filter_map(|event| {
                let item = &event["payload"]["item"];
                (item["item"] == "extension").then(|| item.clone())
            })
            .collect()
    }

    fn monitor_count(&self, session: &str) -> usize {
        let output = self.run(&["status", "--json", "--no-spawn"]);
        assert!(output.status.success());
        let status: Value = serde_json::from_slice(&output.stdout).expect("status");
        let endpoint = PathBuf::from(field(&status["daemon"], "socket_path"));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("RPC runtime")
            .block_on(async {
                let connection =
                    haider_client::connect(&endpoint, haider_client::ClientConfig::default())
                        .await
                        .expect("monitor list connection");
                let response = connection
                    .client
                    .request(haider_rpc::RequestBody::MonitorList {
                        session_id: haider_protocol::ids::SessionId::new(session),
                    })
                    .await
                    .expect("monitor.list");
                let _ = connection.client.close();
                match response {
                    haider_rpc::ResponseBody::MonitorList { receipt } => match receipt.outcome {
                        haider_rpc::MonitorListOutcomeWire::Listed { monitors } => monitors.len(),
                        other => panic!("monitor list rejected: {other:?}"),
                    },
                    other => panic!("expected monitor list, got {other:?}"),
                }
            })
    }
}

fn wait_for(what: &str, bound: Duration, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if probe() {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
    panic!("timed out after {bound:?} waiting for {what}");
}

fn holder_exit(mut holder: std::process::Child, bound: Duration) -> (Option<i32>, Value) {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(status) = holder.try_wait().expect("holder status") {
            let mut stdout = String::new();
            use std::io::Read as _;
            holder
                .stdout
                .take()
                .expect("holder stdout")
                .read_to_string(&mut stdout)
                .expect("holder output");
            return (
                status.code(),
                serde_json::from_str(&stdout).unwrap_or(Value::String(stdout)),
            );
        }
        if Instant::now() >= deadline {
            let _ = holder.kill();
            let _ = holder.wait();
            panic!("agent wait did not resolve within {bound:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_step(call_id: &str, task: &str, model: &str, background: bool) -> Value {
    let mut args =
        json!({"task":task,"prompt":format!("do {task}"),"model":model,"provider":"fake"});
    if background {
        args["background"] = Value::Bool(true);
    }
    json!({"step":"emit_tool_call","call_id":call_id,"name":"spawn_subagent","args":args})
}

fn steps_after_call(call_id: &str, text: &str) -> [Value; 4] {
    [
        json!({"step":"finish","reason":"tool_use"}),
        json!({"step":"expect_tool_result","call_id":call_id}),
        json!({"step":"emit_text","text":text}),
        json!({"step":"finish","reason":"end_turn"}),
    ]
}

/// Owner decision: `--stop-main` stops the main run only. A background child
/// and the foreground child the run was waiting on both keep running; the
/// waited-on child's result is delivered to the session when it finishes,
/// the durable "still running" marker is journalled, and `agent wait` on the
/// waited-on child resolves with its real report.
///
/// MUTATION CHECK: restore the worker's cancel cascade
/// (`detach_outstanding_deferred` -> cancel) or drop the delivery watcher.
/// Expected runtime failure: B/C are cancelled, or no delivered result.
#[test]
fn run_stop_main_leaves_children_running_and_delivers_waited_on_result() {
    let root = json!([
        spawn_step("spawn-b", "background b", "fake-b", true),
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"spawn-b"},
        spawn_step("spawn-c", "waited c", "fake-c", false),
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"spawn-c"},
        {"step":"emit_text","text":"root done"},
        {"step":"finish","reason":"end_turn"}
    ]);
    let profile = Profile::new(json!({"by_model": {
        "fake-model": root,
        "fake-b": [{"step":"hang"}],
        "fake-c": [{"step":"delay","ms":8000},{"step":"emit_text","text":"C_REPORT_AFTER_MAIN_STOP"},{"step":"finish","reason":"end_turn"}],
    }}));
    profile.seed_models(&["fake-model", "fake-b", "fake-c"]);
    let (run_id, session) = profile.start_run("fake-model", "spawn b then wait on c");
    wait_for("both children", STOP_OBSERVATION, || {
        profile.delegations().len() == 2
    });
    let rows = profile.delegations();
    let (b, c) = (rows[0].clone(), rows[1].clone());
    let holder = profile.holder(&session, &b.agent);
    let stopped = profile.stop_run("--stop-main", &run_id);
    assert_eq!(stopped["result"]["status"], "accepted", "{stopped}");
    assert_eq!(stopped["result"]["scope"], "main", "{stopped}");
    assert_eq!(
        stopped["result"]["subagents"]["still_running"], 2,
        "{stopped}"
    );
    assert_eq!(
        stopped["result"]["subagents"]["cancelled_runs"], 0,
        "{stopped}"
    );
    wait_for("main run cancelled", STOP_OBSERVATION, || {
        profile.run_state(&session, &run_id).as_deref() == Some("cancelled")
    });
    // The waited-on child finishes on its own and its result is delivered.
    wait_for("delivered result", STOP_OBSERVATION, || {
        profile
            .journal_kinds(&session)
            .iter()
            .any(|item| item["kind"] == "detached_child_result_v1")
    });
    let kinds = profile.journal_kinds(&session);
    let marker = kinds
        .iter()
        .find(|item| item["kind"] == "subagents_still_running_v1")
        .expect("still-running marker");
    assert_eq!(marker["data"]["still_running"], 2, "{marker}");
    let delivered = kinds
        .iter()
        .find(|item| item["kind"] == "detached_child_result_v1")
        .expect("delivered result");
    assert_eq!(delivered["data"]["agent"], c.agent.as_str());
    assert_eq!(delivered["data"]["state"], "done");
    assert_eq!(delivered["data"]["summary"], "C_REPORT_AFTER_MAIN_STOP");
    let waited = profile.json(
        &[
            "agent",
            "wait",
            &session,
            &c.agent,
            "--json",
            "--timeout",
            "10s",
            "--no-spawn",
        ],
        "haider.agent.wait.v1",
        0,
    );
    assert_eq!(waited["result"]["state"], "done", "{waited}");
    assert_eq!(
        waited["result"]["report"]["summary"],
        "C_REPORT_AFTER_MAIN_STOP"
    );
    // The background child is still streaming long after the main stop.
    let b_row = profile
        .delegations()
        .into_iter()
        .find(|row| row.agent == b.agent)
        .expect("b row");
    assert_eq!(b_row.terminal, Some(0), "{b_row:?}");
    assert!(!profile.live_runs(&b.child_session).is_empty());
    // Explicit stop-all ends it and the attached wait resolves.
    let all = profile.json(
        &["agent", "cancel", &session, "--all", "--json", "--no-spawn"],
        "haider.agent.cancel.v1",
        0,
    );
    assert!(
        all["result"]["stopped"]["cancelled_runs"].as_u64() >= Some(1),
        "{all}"
    );
    let (code, _) = holder_exit(holder, STOP_OBSERVATION);
    assert_eq!(code, Some(130));
    profile.stop();
}

/// Owner decision: `agent cancel <id>` stops that subagent only; its own
/// background child keeps running until an explicit `--all`.
///
/// MUTATION CHECK: restore the `manual_ancestor` subtree sweep in
/// `agent_cancel`. Expected runtime failure: G is cancelled with C.
#[test]
fn named_agent_cancel_stops_only_that_subagent() {
    let root = [
        vec![spawn_step("spawn-c", "child c", "fake-c", true)],
        steps_after_call("spawn-c", "root done").to_vec(),
    ]
    .concat();
    let child = [
        vec![spawn_step("spawn-g", "grandchild g", "fake-g", true)],
        vec![
            json!({"step":"finish","reason":"tool_use"}),
            json!({"step":"expect_tool_result","call_id":"spawn-g"}),
            json!({"step":"hang"}),
        ],
    ]
    .concat();
    let profile = Profile::new(json!({"by_model": {
        "fake-model": root, "fake-c": child, "fake-g": [{"step":"hang"}],
    }}));
    profile.seed_models(&["fake-model", "fake-c", "fake-g"]);
    let (_run, session) = profile.finish_run("fake-model", "spawn c in background");
    wait_for("grandchild", STOP_OBSERVATION, || {
        profile.delegations().len() == 2
    });
    let (c, g) = (profile.row(1), profile.row(2));
    wait_for("c second request live", STOP_OBSERVATION, || {
        !profile.live_runs(&c.child_session).is_empty()
    });
    let holder = profile.holder(&g.parent_session, &g.agent);
    let cancelled = profile.json(
        &[
            "agent",
            "cancel",
            &session,
            &c.agent,
            "--json",
            "--no-spawn",
        ],
        "haider.agent.cancel.v1",
        0,
    );
    assert_eq!(cancelled["result"]["status"], "accepted", "{cancelled}");
    wait_for("c cancelled", STOP_OBSERVATION, || {
        profile.row(1).state.as_deref() == Some("cancelled")
    });
    thread::sleep(Duration::from_secs(5));
    let g_now = profile.row(2);
    assert_eq!(
        g_now.terminal,
        Some(0),
        "grandchild must keep running: {g_now:?}"
    );
    let all = profile.json(
        &["agent", "cancel", &session, "--all", "--json", "--no-spawn"],
        "haider.agent.cancel.v1",
        0,
    );
    assert!(
        all["result"]["stopped"]["cancelled_runs"].as_u64() >= Some(1),
        "{all}"
    );
    wait_for("g cancelled", STOP_OBSERVATION, || {
        profile.row(2).state.as_deref() == Some("cancelled")
    });
    let (code, waited) = holder_exit(holder, STOP_OBSERVATION);
    assert_eq!(code, Some(130), "{waited}");
    profile.stop();
}

/// B1 (7-final): a child restarted by `agent message` spawns a background
/// grandchild in its LATER run. `run --stop` on the (already terminal) root
/// run must reach it through the session walk, and an attached `agent wait`
/// on the grandchild must resolve. Fails on 4d04da69 (G kept streaming).
#[test]
fn run_stop_reaches_messaged_childs_grandchild_and_agent_wait_resolves() {
    let root = [
        vec![spawn_step("spawn-c", "child c", "fake-c", false)],
        steps_after_call("spawn-c", "root done").to_vec(),
    ]
    .concat();
    let child = [
        vec![
            json!({"step":"emit_text","text":"c first report"}),
            json!({"step":"finish","reason":"end_turn"}),
            spawn_step("spawn-g", "grandchild g", "fake-g", true),
        ],
        steps_after_call("spawn-g", "c second report").to_vec(),
    ]
    .concat();
    let profile = Profile::new(json!({"by_model": {
        "fake-model": root, "fake-c": child, "fake-g": [{"step":"hang"}],
    }}));
    profile.seed_models(&["fake-model", "fake-c", "fake-g"]);
    let (run_id, session) = profile.finish_run("fake-model", "spawn c");
    let c = profile.row(1);
    profile.json(
        &[
            "agent",
            "message",
            &session,
            &c.agent,
            "spawn g in background",
            "--json",
            "--no-spawn",
        ],
        "haider.agent.message.v1",
        0,
    );
    wait_for("grandchild", STOP_OBSERVATION, || {
        profile.delegations().len() == 2
    });
    let g = profile.row(2);
    let holder = profile.holder(&g.parent_session, &g.agent);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        profile.row(2).terminal,
        Some(0),
        "g streams before the stop"
    );
    let stopped = profile.stop_run("--stop", &run_id);
    assert_eq!(stopped["result"]["status"], "already_terminal", "{stopped}");
    assert!(
        stopped["result"]["subagents"]["cancelled_runs"].as_u64() >= Some(1),
        "{stopped}"
    );
    let started = Instant::now();
    wait_for("g cancelled", Duration::from_secs(15), || {
        profile.row(2).state.as_deref() == Some("cancelled")
    });
    eprintln!(
        "B1 grandchild cancelled {:?} after run --stop",
        started.elapsed()
    );
    let (code, waited) = holder_exit(holder, STOP_OBSERVATION);
    assert_eq!(code, Some(130), "{waited}");
    profile.stop();
}

/// Multi-level relay through LATER runs: C is woken by its own timer monitor
/// and spawns G in that monitor-woken run; G is woken the same way and
/// spawns H. `run --stop` on the terminal root run cancels H and removes the
/// monitors still registered in the tree, so nothing wakes back up.
#[test]
fn run_stop_reaches_monitor_woken_relay_and_removes_descendant_monitors() {
    let timer = |call_id: &str, interval_ms: u64, occurrence: &str| {
        json!({"step":"emit_tool_call","call_id":call_id,"name":"monitor","args":{
            "operation":"register","source":{"kind":"timer","interval_ms":interval_ms},
            "action":{"report":true},"occurrence":occurrence}})
    };
    let root = [
        vec![spawn_step("spawn-c", "child c", "fake-c", false)],
        steps_after_call("spawn-c", "root done").to_vec(),
    ]
    .concat();
    let relay = |me: &str, next_call: &str, next_task: &str, next_model: &str| -> Value {
        Value::Array(
            [
                vec![
                    // A delegated child must discover `monitor` before use.
                    json!({"step":"emit_tool_call","call_id":format!("{me}-discover"),"name":"list_tools","args":{"filter":"monitor"}}),
                    json!({"step":"finish","reason":"tool_use"}),
                    json!({"step":"expect_tool_result","call_id":format!("{me}-discover")}),
                    timer(&format!("{me}-wake"), 1_000, "once"),
                    timer(&format!("{me}-keeper"), 3_600_000, "every"),
                    json!({"step":"finish","reason":"tool_use"}),
                    json!({"step":"expect_tool_result","call_id":format!("{me}-wake")}),
                    json!({"step":"expect_tool_result","call_id":format!("{me}-keeper")}),
                    json!({"step":"emit_text","text":format!("{me} first report")}),
                    json!({"step":"finish","reason":"end_turn"}),
                    spawn_step(next_call, next_task, next_model, true),
                ],
                steps_after_call(next_call, &format!("{me} woke and relayed")).to_vec(),
            ]
            .concat(),
        )
    };
    let profile = Profile::new(json!({"by_model": {
        "fake-model": root,
        "fake-c": relay("c", "spawn-g", "grandchild g", "fake-g"),
        "fake-g": relay("g", "spawn-h", "great-grandchild h", "fake-h"),
        "fake-h": [{"step":"hang"}],
    }}));
    let mut profile = profile;
    profile.tool_exposure = None;
    profile.seed_models(&["fake-model", "fake-c", "fake-g", "fake-h"]);
    let (run_id, _session) = profile.finish_run("fake-model", "start the relay");
    wait_for("three-level relay", Duration::from_secs(90), || {
        profile.delegations().len() == 3
    });
    let (c, g, h) = (profile.row(1), profile.row(2), profile.row(3));
    assert_eq!(g.parent_session, c.child_session);
    assert_eq!(h.parent_session, g.child_session);
    let holder = profile.holder(&h.parent_session, &h.agent);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        profile.row(3).terminal,
        Some(0),
        "h streams before the stop"
    );
    assert!(profile.monitor_count(&c.child_session) >= 1);
    assert!(profile.monitor_count(&g.child_session) >= 1);
    let stopped = profile.stop_run("--stop", &run_id);
    assert!(
        stopped["result"]["subagents"]["cancelled_runs"].as_u64() >= Some(1),
        "{stopped}"
    );
    assert!(
        stopped["result"]["subagents"]["monitors_removed"].as_u64() >= Some(2),
        "{stopped}"
    );
    wait_for("h cancelled", Duration::from_secs(15), || {
        profile.row(3).state.as_deref() == Some("cancelled")
    });
    assert_eq!(profile.monitor_count(&c.child_session), 0);
    assert_eq!(profile.monitor_count(&g.child_session), 0);
    let (code, waited) = holder_exit(holder, STOP_OBSERVATION);
    assert_eq!(code, Some(130), "{waited}");
    assert!(profile.live_runs(&c.child_session).is_empty());
    assert!(profile.live_runs(&g.child_session).is_empty());
    profile.stop();
}

/// Owner decision: `--stop-subagents` stops every subagent of the run and
/// leaves the run itself going; `--stop-main` then ends it.
#[test]
fn run_stop_subagents_leaves_main_running() {
    let root = json!([
        spawn_step("spawn-b", "background b", "fake-b", true),
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"spawn-b"},
        {"step":"hang"}
    ]);
    let profile = Profile::new(json!({"by_model": {
        "fake-model": root, "fake-b": [{"step":"hang"}],
    }}));
    profile.seed_models(&["fake-model", "fake-b"]);
    let (run_id, session) = profile.start_run("fake-model", "spawn b and keep working");
    wait_for("child b", STOP_OBSERVATION, || {
        profile.delegations().len() == 1
    });
    let b = profile.row(1);
    wait_for("b live", STOP_OBSERVATION, || {
        !profile.live_runs(&b.child_session).is_empty()
    });
    let stopped = profile.stop_run("--stop-subagents", &run_id);
    assert_eq!(stopped["result"]["scope"], "subagents", "{stopped}");
    assert_eq!(
        stopped["result"]["subagents"]["cancelled_runs"], 1,
        "{stopped}"
    );
    wait_for("b cancelled", STOP_OBSERVATION, || {
        profile.row(1).state.as_deref() == Some("cancelled")
    });
    thread::sleep(Duration::from_secs(3));
    let main = profile.run_state(&session, &run_id);
    assert!(
        !matches!(main.as_deref(), Some("cancelled" | "done" | "errored")),
        "main must keep running: {main:?}"
    );
    let ended = profile.stop_run("--stop-main", &run_id);
    assert_eq!(ended["result"]["status"], "accepted", "{ended}");
    wait_for("main cancelled", STOP_OBSERVATION, || {
        profile.run_state(&session, &run_id).as_deref() == Some("cancelled")
    });
    profile.stop();
}

fn tree_script(width: usize, fan: usize) -> (Vec<String>, Value) {
    let kids = (0..width).map(|i| format!("fake-k{i}")).collect::<Vec<_>>();
    let mut root = kids
        .iter()
        .enumerate()
        .map(|(i, kid)| spawn_step(&format!("s{i}"), &format!("kid {i}"), kid, true))
        .collect::<Vec<_>>();
    root.push(json!({"step":"finish","reason":"tool_use"}));
    root.extend((0..width).map(|i| json!({"step":"expect_tool_result","call_id":format!("s{i}")})));
    root.push(json!({"step":"hang"}));
    let mut by_model = serde_json::Map::new();
    by_model.insert("fake-model".into(), Value::Array(root));
    by_model.insert("fake-leaf".into(), json!([{"step":"hang"}]));
    for (i, kid) in kids.iter().enumerate() {
        let mut steps = (0..fan)
            .map(|j| {
                spawn_step(
                    &format!("g{j}"),
                    &format!("leaf {i}.{j}"),
                    "fake-leaf",
                    true,
                )
            })
            .collect::<Vec<_>>();
        steps.push(json!({"step":"finish","reason":"tool_use"}));
        steps.extend(
            (0..fan).map(|j| json!({"step":"expect_tool_result","call_id":format!("g{j}")})),
        );
        steps.push(json!({"step":"hang"}));
        by_model.insert(kid.clone(), Value::Array(steps));
    }
    let mut models = vec!["fake-model".to_owned(), "fake-leaf".to_owned()];
    models.extend(kids);
    (models, json!({"by_model": Value::Object(by_model)}))
}

impl Profile {
    /// Non-terminal runs in any delegated child session (cancelling included).
    fn nonterminal_child_runs(&self) -> Vec<(String, String)> {
        let connection = self.db();
        let mut statement = connection
            .prepare(
                "SELECT h.session_id, h.state_json FROM run_heads h
                 JOIN delegations d ON d.child_session_id = h.session_id WHERE h.terminal = 0",
            )
            .expect("nonterminal child query");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("nonterminal rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode nonterminal rows")
    }
}

/// 9-verify3 B-1: `run --stop` landing while a 6x4 tree is still spawning
/// must converge — no child run may stay queued/cancelling — the tree must
/// not keep growing afterwards, and the response must not overclaim.
/// Random stop delays across fresh profiles (the verifier's race harness
/// shape); fails on e66bc195 in most iterations.
///
/// MUTATION CHECK: make spawn dispatch abortable again, or skip the launch
/// finalization under a stopping parent. Expected runtime failure: a child
/// run remains `cancelling`/`queued` after the stop.
#[test]
fn run_stop_while_tree_is_spawning_converges_with_no_stuck_runs() {
    let iterations = std::env::var("HAIDER_STOP_RACE_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6);
    for iteration in 0..iterations {
        let (models, script) = tree_script(6, 4);
        let profile = Profile::new(script);
        profile.seed_models(&models.iter().map(String::as_str).collect::<Vec<_>>());
        let (run_id, session) = profile.start_run("fake-model", "spawn a 6x4 tree");
        let holder = profile
            .command(&["session", &session, "--watch"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("attached watcher");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let delay_ms = 50 + u64::from(nanos % 550);
        thread::sleep(Duration::from_millis(delay_ms));
        let stopped = profile.stop_run("--stop", &run_id);
        let summary = &stopped["result"]["subagents"];
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stuck = profile.nonterminal_child_runs();
        while !stuck.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(250));
            stuck = profile.nonterminal_child_runs();
        }
        let settled = profile.delegations().len();
        thread::sleep(Duration::from_secs(3));
        assert!(
            stuck.is_empty(),
            "iteration {iteration} (stop after {delay_ms} ms): stuck child runs {stuck:?}; response {stopped}"
        );
        assert!(profile.nonterminal_child_runs().is_empty());
        assert_eq!(
            profile.delegations().len(),
            settled,
            "iteration {iteration}: the tree kept growing after the stop"
        );
        assert_eq!(
            summary["still_running"], 0,
            "iteration {iteration}: {stopped}"
        );
        let cancelled = summary["cancelled_runs"].as_u64().unwrap_or(u64::MAX);
        assert!(
            cancelled <= u64::try_from(settled).unwrap_or(u64::MAX),
            "iteration {iteration}: overclaimed {stopped}"
        );
        eprintln!(
            "stop race iteration {iteration}: delay {delay_ms} ms, {settled} delegations, cancelled_runs {cancelled}"
        );
        let mut holder = holder;
        let _ = holder.kill();
        let _ = holder.wait();
        profile.stop();
    }
}

/// 9-verify3 B-2: a waited-on result owed by a main-only stop survives a
/// graceful daemon stop (which cancels the child): after restart the
/// cancelled result is delivered and `agent wait` on the child resolves.
///
/// MUTATION CHECK: filter re-armed children by state again. Expected
/// runtime failure: no delivery after restart and `agent wait` times out.
#[test]
fn owed_detached_result_is_delivered_after_daemon_restart() {
    let root = json!([
        spawn_step("spawn-c", "waited c", "fake-c", false),
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"spawn-c"},
        {"step":"emit_text","text":"root done"},
        {"step":"finish","reason":"end_turn"}
    ]);
    let profile = Profile::new(json!({"by_model": {
        "fake-model": root,
        "fake-c": [{"step":"delay","ms":60000},{"step":"emit_text","text":"never"},{"step":"finish","reason":"end_turn"}],
    }}));
    profile.seed_models(&["fake-model", "fake-c"]);
    let (run_id, session) = profile.start_run("fake-model", "wait on c");
    wait_for("child c", STOP_OBSERVATION, || {
        profile.delegations().len() == 1
    });
    let c = profile.row(1);
    wait_for("c live", STOP_OBSERVATION, || {
        !profile.live_runs(&c.child_session).is_empty()
    });
    let stopped = profile.stop_run("--stop-main", &run_id);
    assert_eq!(
        stopped["result"]["subagents"]["still_running"], 1,
        "{stopped}"
    );
    wait_for("marker", STOP_OBSERVATION, || {
        profile
            .journal_kinds(&session)
            .iter()
            .any(|item| item["kind"] == "subagents_still_running_v1")
    });
    // Explicit graceful daemon stop: cancels everything (O-2).
    profile.stop();
    assert_eq!(profile.row(1).state.as_deref(), Some("cancelled"));
    // Restart through the wait itself; recovery must deliver the cancelled
    // result so the foreground wait resolves.
    let waited = profile.json(
        &[
            "agent",
            "wait",
            &session,
            &c.agent,
            "--json",
            "--timeout",
            "60s",
        ],
        "haider.agent.wait.v1",
        130,
    );
    assert_eq!(waited["result"]["state"], "cancelled", "{waited}");
    let delivered = profile
        .journal_kinds(&session)
        .into_iter()
        .filter(|item| item["kind"] == "detached_child_result_v1")
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0]["data"]["state"], "cancelled");
    profile.stop();
}

/// Orchestrator ruling O-1: an auto-spawned daemon with a finite idle TTL
/// does NOT retire while runs are live. After `run --stop-main` with no
/// attached client, the detached children run to completion (the waited-on
/// result is delivered), and only then does the daemon exit on its own.
///
/// MUTATION CHECK: let the linger deadline shut down with live runs again.
/// Expected runtime failure: the children end `cancelled`, not `done`.
#[test]
fn ephemeral_daemon_outlives_linger_while_detached_children_run() {
    let root = json!([
        spawn_step("spawn-b", "background b", "fake-b", true),
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"spawn-b"},
        spawn_step("spawn-c", "waited c", "fake-c", false),
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"spawn-c"},
        {"step":"emit_text","text":"root done"},
        {"step":"finish","reason":"end_turn"}
    ]);
    let child = |text: &str| json!([{"step":"delay","ms":9000},{"step":"emit_text","text":text},{"step":"finish","reason":"end_turn"}]);
    let mut profile = Profile::new(json!({"by_model": {
        "fake-model": root, "fake-b": child("B_DONE"), "fake-c": child("C_DONE"),
    }}));
    profile
        .extra_env
        .push(("HAIDER_RUN_DAEMON_IDLE_TTL_MS", "1500".into()));
    profile.seed_models(&["fake-model", "fake-b", "fake-c"]);
    let (run_id, session) = profile.start_run("fake-model", "spawn b, wait on c");
    wait_for("both children", STOP_OBSERVATION, || {
        profile.delegations().len() == 2
    });
    let stopped = profile.stop_run("--stop-main", &run_id);
    assert_eq!(
        stopped["result"]["subagents"]["still_running"], 2,
        "{stopped}"
    );
    // No client is attached from here on; the 1.5 s linger passes many
    // times while the children still run.
    wait_for("both children done", Duration::from_secs(60), || {
        profile
            .delegations()
            .iter()
            .all(|row| row.terminal == Some(1))
    });
    for row in profile.delegations() {
        assert_eq!(row.state.as_deref(), Some("done"), "{row:?}");
    }
    // Everything terminal: the daemon now retires on its own, with no
    // client attached. Each status probe is itself a client (it restarts the
    // 1.5 s linger), so probe less often than the linger.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        thread::sleep(Duration::from_secs(4));
        let status = profile.run(&["status", "--json", "--no-spawn"]);
        if status.status.code() == Some(69) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not retire after work ended"
        );
    }
    // The waited-on result was delivered before retirement: a fresh daemon
    // serves it from the parent's durable child_result.
    let c_agent: String = profile
        .db()
        .query_row(
            "SELECT agent_id FROM delegations WHERE task = 'waited c'",
            [],
            |row| row.get(0),
        )
        .expect("waited child c");
    let waited = profile.json(
        &[
            "agent",
            "wait",
            &session,
            &c_agent,
            "--json",
            "--timeout",
            "20s",
        ],
        "haider.agent.wait.v1",
        0,
    );
    assert_eq!(waited["result"]["report"]["summary"], "C_DONE", "{waited}");
    assert_eq!(
        waited["result"]["report_source"], "child_result",
        "{waited}"
    );
    let delivered = profile
        .journal_kinds(&session)
        .into_iter()
        .find(|item| item["kind"] == "detached_child_result_v1")
        .expect("waited-on result delivered");
    assert_eq!(delivered["data"]["summary"], "C_DONE");
    profile.stop();
}

// ---------------------------------------------------------------------------
// Repair 5 (Astra 13-final B1-B3).

impl Profile {
    /// Every run head of a session, terminal or not.
    fn all_runs(&self, session: &str) -> Vec<(String, String)> {
        let connection = self.db();
        let mut statement = connection
            .prepare("SELECT run_id, state_json FROM run_heads WHERE session_id = ?1")
            .expect("run head query");
        statement
            .query_map([session], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("run heads")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode run heads")
    }
}

fn find_kind<'a>(value: &'a Value, kind: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if map.get("kind").and_then(Value::as_str) == Some(kind) {
                return Some(value);
            }
            map.values().find_map(|child| find_kind(child, kind))
        }
        Value::Array(items) => items.iter().find_map(|child| find_kind(child, kind)),
        _ => None,
    }
}

/// B2: a blocking spawn whose `wait_ms` observation ends returns
/// `child_still_running` with an `agent wait` command. After BOTH the parent
/// turn and the child finish, that exact command must collect the child's
/// real report (repeatedly), from the parent's durable child result.
///
/// MUTATION CHECK: drop `detach_observed_child` from the worker's
/// `detach_deferred`. Expected runtime failure: the returned command exits
/// 124 because no parent child result is ever written.
#[test]
fn wait_ms_child_is_collected_by_the_returned_command_after_parent_and_child_finish() {
    let mut spawn = spawn_step("spawn-c", "observed c", "fake-c", false);
    spawn["args"]["wait_ms"] = json!(100);
    let root = json!([
        spawn,
        {"step":"finish","reason":"tool_use"},
        {"step":"expect_tool_result","call_id":"spawn-c"},
        {"step":"emit_text","text":"parent finished observing"},
        {"step":"finish","reason":"end_turn"}
    ]);
    let profile = Profile::new(json!({"by_model": {
        "fake-model": root,
        "fake-c": [
            {"step":"delay","ms":6000},
            {"step":"emit_text","text":"WAIT_MS_CHILD_DONE"},
            {"step":"finish","reason":"end_turn"}
        ],
    }}));
    profile.seed_models(&["fake-model", "fake-c"]);
    let (run_id, session) = profile.finish_run("fake-model", "observe c briefly");
    assert_eq!(
        profile.run_state(&session, &run_id).as_deref(),
        Some("done"),
        "the parent finished normally after the observation window"
    );
    let c = profile.row(1);
    assert_eq!(c.terminal, Some(0), "the child outlives its parent turn");
    let still_running = profile
        .journal(&session)
        .iter()
        .find_map(|event| find_kind(event, "child_still_running").cloned())
        .expect("child_still_running tool result");
    let collect = still_running["collect"]
        .as_str()
        .expect("collection command")
        .to_owned();
    let words = collect.split_whitespace().collect::<Vec<_>>();
    assert_eq!(
        &words[..3],
        &["haider", "agent", "wait"],
        "collection command: {collect}"
    );
    assert!(words.contains(&c.agent.as_str()), "{collect}");
    wait_for("child done", STOP_OBSERVATION, || {
        profile.row(1).state.as_deref() == Some("done")
    });
    for attempt in 0..2 {
        let waited = profile.json(&words[1..], "haider.agent.wait.v1", 0);
        assert_eq!(
            waited["result"]["report"]["summary"], "WAIT_MS_CHILD_DONE",
            "attempt {attempt}: {waited}"
        );
        assert_eq!(
            waited["result"]["report_source"], "child_result",
            "attempt {attempt}: {waited}"
        );
    }
    let delivered = profile
        .journal_kinds(&session)
        .into_iter()
        .filter(|item| item["kind"] == "detached_child_result_v1")
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0]["data"]["reason"], "observation_ended");
    assert_eq!(delivered[0]["data"]["summary"], "WAIT_MS_CHILD_DONE");
    profile.stop();
}

/// B3: the durable delivery obligation is not the 16-entry display list.
/// Sixteen background children fill the marker; the seventeenth, waited-on
/// child is owed its result. After `--stop-main` and a graceful daemon
/// stop (which cancels it), a restart must still deliver its cancelled
/// result, so `agent wait` resolves 130 instead of timing out.
///
/// MUTATION CHECK: make recovery read owed agents from the marker again, or
/// stop journalling `detached_delivery_v1`. Expected runtime failure: the
/// waits exit 124.
#[test]
fn owed_result_beyond_the_display_list_is_delivered_after_restart() {
    const BACKGROUND: usize = 16;
    let mut root = Vec::new();
    let mut models = vec!["fake-model".to_owned()];
    let mut by_model = serde_json::Map::new();
    for index in 0..BACKGROUND {
        let model = format!("fake-b{index}");
        let call = format!("spawn-b{index}");
        root.push(spawn_step(
            &call,
            &format!("background {index}"),
            &model,
            true,
        ));
        root.push(json!({"step":"finish","reason":"tool_use"}));
        root.push(json!({"step":"expect_tool_result","call_id":call}));
        by_model.insert(model.clone(), json!([{"step":"hang"}]));
        models.push(model);
    }
    root.push(spawn_step("spawn-c", "owed seventeenth", "fake-c", false));
    root.extend(steps_after_call("spawn-c", "UNEXPECTED_PARENT_WORK"));
    by_model.insert("fake-c".into(), json!([{"step":"hang"}]));
    models.push("fake-c".into());
    by_model.insert("fake-model".into(), Value::Array(root));
    let profile = Profile::new(json!({ "by_model": by_model }));
    profile.seed_models(&models.iter().map(String::as_str).collect::<Vec<_>>());
    let (run_id, session) = profile.start_run("fake-model", "sixteen then foreground");
    wait_for("seventeen children", Duration::from_secs(90), || {
        profile.delegations().len() == BACKGROUND + 1
    });
    let c_agent: String = profile
        .db()
        .query_row(
            "SELECT agent_id FROM delegations WHERE task = 'owed seventeenth'",
            [],
            |row| row.get(0),
        )
        .expect("owed child");
    let c = profile
        .delegations()
        .into_iter()
        .find(|row| row.agent == c_agent)
        .expect("owed child row");
    wait_for("c live", STOP_OBSERVATION, || {
        !profile.live_runs(&c.child_session).is_empty()
    });
    let stopped = profile.stop_run("--stop-main", &run_id);
    assert_eq!(
        stopped["result"]["subagents"]["still_running"],
        json!(BACKGROUND + 1),
        "{stopped}"
    );
    wait_for("owed fact", STOP_OBSERVATION, || {
        profile.journal_kinds(&session).iter().any(|item| {
            item["kind"] == "detached_delivery_v1"
                && item["data"]["agent"] == c_agent.as_str()
                && item["data"]["phase"] == "owed"
        })
    });
    let marker = profile
        .journal_kinds(&session)
        .into_iter()
        .find(|item| item["kind"] == "subagents_still_running_v1")
        .expect("display marker");
    assert_eq!(marker["data"]["agents_truncated"], true, "{marker}");
    // Graceful stop cancels every live run, including the owed child.
    profile.stop();
    assert_eq!(
        profile
            .delegations()
            .into_iter()
            .find(|row| row.agent == c_agent)
            .and_then(|row| row.state)
            .as_deref(),
        Some("cancelled")
    );
    for attempt in 0..2 {
        let waited = profile.json(
            &[
                "agent",
                "wait",
                &session,
                &c_agent,
                "--json",
                "--timeout",
                "60s",
            ],
            "haider.agent.wait.v1",
            130,
        );
        assert_eq!(
            waited["result"]["state"], "cancelled",
            "attempt {attempt}: {waited}"
        );
    }
    let delivered = profile
        .journal_kinds(&session)
        .into_iter()
        .filter(|item| {
            item["kind"] == "detached_child_result_v1" && item["data"]["agent"] == c_agent.as_str()
        })
        .count();
    assert_eq!(delivered, 1);
    profile.stop();
}

/// A registration attempted during each stop scope receives its own rejected
/// tool result. The named case uses a later messaged run so cancellation of
/// its already terminal primary run cannot make the race vacuous.
#[test]
fn stop_refuses_monitors_registered_during_the_stop_for_every_scope() {
    for scope in ["named", "all", "run", "subagents"] {
        monitor_registration_race(scope);
    }
}

// Independent entry points keep one scope's mutation failure from preventing
// execution of the remaining scope variants in the test binary.
#[test]
fn stop_refuses_monitors_registered_during_named_cancel() {
    monitor_registration_race("named");
}

#[test]
fn stop_refuses_monitors_registered_during_cancel_all() {
    monitor_registration_race("all");
}

#[test]
fn stop_refuses_monitors_registered_during_run_stop() {
    monitor_registration_race("run");
}

#[test]
fn stop_refuses_monitors_registered_during_stop_subagents() {
    monitor_registration_race("subagents");
}

#[test]
fn main_only_stop_leaves_child_monitor_registration_open() {
    monitor_registration_race("main");
}

fn monitor_registration_race(scope: &str) {
    const MONITORS: usize = 25;
    let timer = |call_id: &str, interval_ms: u64| {
        json!({"step":"emit_tool_call","call_id":call_id,"name":"monitor","args":{
            "operation":"register","source":{"kind":"timer","interval_ms":interval_ms},
            "action":{"report":true},"occurrence":"every"}})
    };
    let discover = |tool: &str| json!({"step":"emit_tool_call","call_id":format!("discover-{tool}"),"name":"list_tools","args":{"filter":tool}});
    // The child's gate paths live in the fixture workspace, so the script
    // is installed before the first command starts the daemon.
    let mut profile = Profile::new(json!({}));
    let ready = profile.workspace.join("ready");
    let release = profile.workspace.join("release");
    let mut child = vec![
        discover("monitor"),
        json!({"step":"finish","reason":"tool_use"}),
        json!({"step":"expect_tool_result","call_id":"discover-monitor"}),
    ];
    child.extend([
        discover("process_exec"),
        json!({"step":"finish","reason":"tool_use"}),
        json!({"step":"expect_tool_result","call_id":"discover-process_exec"}),
    ]);
    for index in 0..MONITORS {
        let call = format!("timer-{index}");
        child.push(timer(&call, 3_600_000));
        child.push(json!({"step":"finish","reason":"tool_use"}));
        child.push(json!({"step":"expect_tool_result","call_id":call}));
    }
    if scope == "named" {
        child.push(json!({"step":"emit_text","text":"first run complete"}));
        child.push(json!({"step":"finish","reason":"end_turn"}));
        child.extend([
            json!({"step":"emit_tool_call","call_id":"discover-monitor-later","name":"list_tools","args":{"filter":"monitor"}}),
            json!({"step":"finish","reason":"tool_use"}),
            json!({"step":"expect_tool_result","call_id":"discover-monitor-later"}),
            json!({"step":"emit_tool_call","call_id":"discover-process_exec-later","name":"list_tools","args":{"filter":"process_exec"}}),
            json!({"step":"finish","reason":"tool_use"}),
            json!({"step":"expect_tool_result","call_id":"discover-process_exec-later"}),
            spawn_step("spawn-g", "named descendant", "fake-g", true),
            json!({"step":"finish","reason":"tool_use"}),
            json!({"step":"expect_tool_result","call_id":"spawn-g"}),
        ]);
    }
    let gate = format!(
        "touch '{}'; while [ ! -e '{}' ]; do sleep 0.001; done",
        ready.display(),
        release.display()
    );
    child.extend([
        json!({"step":"emit_tool_call","call_id":"gate","name":"process_exec","args":{"command":gate}}),
        json!({"step":"finish","reason":"tool_use"}),
        json!({"step":"expect_tool_result","call_id":"gate"}),
        timer("late-timer", if scope == "main" { 3_600_000 } else { 2_000 }),
        json!({"step":"finish","reason":"tool_use"}),
        json!({"step":"expect_tool_result","call_id":"late-timer"}),
        json!({"step":"hang"}),
    ]);
    let root_end = if scope == "main" {
        vec![json!({"step":"hang"})]
    } else {
        vec![
            json!({"step":"emit_text","text":"root done"}),
            json!({"step":"finish","reason":"end_turn"}),
        ]
    };
    let mut root = vec![
        spawn_step("spawn-c", "monitor child", "fake-c", true),
        json!({"step":"finish","reason":"tool_use"}),
        json!({"step":"expect_tool_result","call_id":"spawn-c"}),
    ];
    root.extend(root_end);
    profile.script = json!({"by_model": {
        "fake-model": root,
        "fake-c": child,
        "fake-g": [{"step":"hang"}],
    }})
    .to_string();
    profile.seed_models(&["fake-model", "fake-c", "fake-g"]);
    let (run_id, session) = if scope == "main" {
        profile.start_run("fake-model", "monitor stop main-only control")
    } else {
        let output = profile.run(&[
            "run",
            "--auto-allow",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--json",
            "-p",
            "monitor stop race",
        ]);
        assert!(
            output.status.success(),
            "{scope}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout).expect("run JSON");
        (
            field(&body, "run_id").to_owned(),
            field(&body, "session_id").to_owned(),
        )
    };
    wait_for("child delegation", STOP_OBSERVATION, || {
        !profile.delegations().is_empty()
    });
    let c = profile.row(1);
    if scope == "named" {
        wait_for("first child run terminal", STOP_OBSERVATION, || {
            profile.live_runs(&c.child_session).is_empty()
        });
        let messaged = profile.json(
            &[
                "agent",
                "message",
                &session,
                &c.agent,
                "register in later run",
                "--json",
                "--no-spawn",
            ],
            "haider.agent.message.v1",
            0,
        );
        assert_eq!(
            messaged["result"]["receipt"]["agent"].as_str(),
            Some(c.agent.as_str())
        );
    }
    let grandchild = if scope == "named" {
        wait_for("named descendant", STOP_OBSERVATION, || {
            profile.delegations().len() == 2
        });
        let grandchild = profile.row(2);
        assert_eq!(profile.live_runs(&grandchild.child_session).len(), 1);
        Some(grandchild)
    } else {
        None
    };
    wait_for("child at its gate", Duration::from_secs(90), || {
        ready.exists()
    });
    if scope == "named" {
        assert_eq!(
            profile.all_runs(&c.child_session).len(),
            2,
            "named cancellation must race a later child run"
        );
    }
    assert_eq!(profile.monitor_count(&c.child_session), MONITORS, "{scope}");
    let args: Vec<&str> = match scope {
        "named" => vec![
            "agent",
            "cancel",
            &session,
            &c.agent,
            "--json",
            "--no-spawn",
        ],
        "all" => vec!["agent", "cancel", &session, "--all", "--json", "--no-spawn"],
        "run" => vec!["run", "--stop", &run_id, "--json"],
        "subagents" => vec!["run", "--stop-subagents", &run_id, "--json"],
        "main" => vec!["run", "--stop-main", &run_id, "--json"],
        _ => panic!("unknown stop scope {scope}"),
    };
    let stop = profile
        .command(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("stop command");
    // Release the child after the first removal; the remaining removals and
    // cancellation of its live run are still ahead.
    if scope != "main" {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && profile.monitor_count(&c.child_session) >= MONITORS {
            thread::sleep(Duration::from_millis(1));
        }
    } else {
        wait_for("main run stopping", STOP_OBSERVATION, || {
            matches!(
                profile.run_state(&session, &run_id).as_deref(),
                Some("cancelling" | "cancelled")
            )
        });
    }
    std::fs::write(&release, b"go").expect("release the gate");
    let stopped = stop.wait_with_output().expect("stop completes");
    assert!(
        stopped.status.success(),
        "{scope}: {}\n{}",
        String::from_utf8_lossy(&stopped.stdout),
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_for("late monitor tool result", STOP_OBSERVATION, || {
        profile.journal(&c.child_session).iter().any(|event| {
            event["payload"]["type"] == "tool_result" && event["payload"]["call_id"] == "late-timer"
        })
    });
    let late = profile
        .journal(&c.child_session)
        .into_iter()
        .filter(|event| {
            event["payload"]["type"] == "tool_result" && event["payload"]["call_id"] == "late-timer"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        late.len(),
        1,
        "{scope}: late registration must be attempted: {late:?}"
    );
    if scope == "main" {
        let preview: Value = serde_json::from_str(field(&late[0]["payload"]["result"], "preview"))
            .expect("main-only late monitor tool preview");
        assert_eq!(preview["status"], "registered", "{late:?}");
        assert_eq!(profile.monitor_count(&c.child_session), MONITORS + 1);
        assert_eq!(profile.live_runs(&c.child_session).len(), 1);
        let cleanup = profile.run(&["agent", "cancel", &session, "--all", "--json", "--no-spawn"]);
        assert!(cleanup.status.success());
        profile.stop();
        return;
    }
    assert_eq!(
        late[0]["payload"]["result"]["status"], "rejected",
        "{scope}: {late:?}"
    );
    let preview: Value = serde_json::from_str(field(&late[0]["payload"]["result"], "preview"))
        .expect("late monitor tool preview");
    assert_eq!(preview["status"], "subagent_stopping", "{scope}: {late:?}");
    wait_for("child cancelled", STOP_OBSERVATION, || {
        profile.live_runs(&c.child_session).is_empty()
    });
    thread::sleep(Duration::from_secs(1));
    assert_eq!(
        profile.monitor_count(&c.child_session),
        0,
        "{scope}: no monitor survives the stop"
    );
    // A surviving 2 s timer would wake the child into new runs by now.
    thread::sleep(Duration::from_secs(5));
    assert_eq!(profile.monitor_count(&c.child_session), 0, "{scope}");
    let runs = profile.all_runs(&c.child_session);
    assert_eq!(
        runs.len(),
        if scope == "named" { 2 } else { 1 },
        "{scope}: no monitor woke the stopped child: {runs:?}"
    );
    assert!(
        runs.iter().any(|(_, state)| state.contains("cancelled")),
        "{scope}: the child was stopped: {runs:?}"
    );
    if let Some(grandchild) = grandchild {
        assert_eq!(
            profile.live_runs(&grandchild.child_session).len(),
            1,
            "named cancel leaves descendants running"
        );
        let cleanup = profile.run(&["agent", "cancel", &session, "--all", "--json", "--no-spawn"]);
        assert!(cleanup.status.success());
        wait_for("grandchild cleanup", STOP_OBSERVATION, || {
            profile.live_runs(&grandchild.child_session).is_empty()
        });
    }
    profile.stop();
}
