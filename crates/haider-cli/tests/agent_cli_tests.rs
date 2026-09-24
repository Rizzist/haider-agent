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
    // Publish a discovered catalog before startup; xAI has no seeded default.
    // The injected daemon factory routes the published pair to the fake script.
    {
        let store = haider_store::Store::open(&profile.profile).expect("catalog store");
        store
            .put_provider_models(
                "xai",
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
