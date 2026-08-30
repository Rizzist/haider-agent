//! Real-artifact status smoke with device discovery enabled on every platform.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

fn daemon_pid(profile: &Path) -> Option<u32> {
    std::fs::read_to_string(profile.join("lock.owner"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("pid="))?
        .trim()
        .parse()
        .ok()
}

struct DaemonGuard {
    profile: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = daemon_pid(&self.profile) {
            let _ = haider_platform::kill_process_tree(pid, true);
        }
    }
}

fn wait_for_output(mut child: Child, deadline: Duration) -> Output {
    let end = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().expect("collect status output"),
            Ok(None) if Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("haider status --json exceeded its hard {deadline:?} deadline");
            }
            Err(error) => panic!("observe status child: {error}"),
        }
    }
}

fn assert_real_sibling_artifacts() -> (PathBuf, PathBuf) {
    let haider = PathBuf::from(env!("CARGO_BIN_EXE_haider"));
    let haiderd = haider
        .parent()
        .expect("haider binary parent")
        .join(format!("haiderd{}", std::env::consts::EXE_SUFFIX));
    for (name, binary) in [("haider", &haider), ("haiderd", &haiderd)] {
        let metadata = std::fs::metadata(binary)
            .unwrap_or_else(|error| panic!("{name} artifact {}: {error}", binary.display()));
        assert!(metadata.is_file(), "{name} artifact is not a file");
        assert!(metadata.len() > 64 * 1024, "{name} artifact is truncated");
    }
    (haider, haiderd)
}

/// P0 release gate: run the actual CLI and daemon binaries against a fresh
/// profile. CI globally disables discovery for its other tests, so this child
/// explicitly removes both disable switches. The native Claude store alone is
/// a deterministic typed-unavailable seam; a file-backed Codex fixture must
/// still be discovered and projected into a later status JSON response. The
/// first status must not wait for the native platform calls: it schedules the
/// bounded discovery and returns the last completed snapshot immediately.
#[test]
fn built_status_json_completes_with_enabled_discovery() {
    let (haider, _haiderd) = assert_real_sibling_artifacts();
    #[cfg(unix)]
    let root = tempfile::Builder::new()
        .prefix("hs-")
        .tempdir_in("/tmp")
        .expect("fresh short status smoke root");
    #[cfg(not(unix))]
    let root = tempfile::tempdir().expect("fresh status smoke root");
    let profile = root.path().join("profile");
    let workspace = root.path().join("workspace");
    #[cfg(unix)]
    let machine_home = root.path().join("machine-home-").join("h".repeat(100));
    #[cfg(not(unix))]
    let machine_home = root.path().join("machine-home");
    std::fs::create_dir_all(&workspace).expect("smoke workspace");
    std::fs::create_dir_all(&machine_home).expect("smoke machine-user home");
    let codex = root.path().join("codex-auth.json");
    std::fs::write(
        &codex,
        br#"{
  "tokens": {
    "access_token": "fixture.header.signature",
    "refresh_token": "fixture-refresh-token",
    "account_id": "status-smoke-account"
  }
}"#,
    )
    .expect("write deterministic Codex discovery fixture");
    let missing = root.path().join("missing");
    let _daemon = DaemonGuard {
        profile: profile.clone(),
    };

    let start_status = |no_spawn: bool| {
        let mut command = Command::new(&haider);
        command.args(["status", "--json"]);
        if no_spawn {
            command.arg("--no-spawn");
        }
        command
            .current_dir(&workspace)
            .env("HAIDER_PROFILE_DIR", &profile)
            // The daemon's lockdown ledger is machine-user global. Keep this
            // real-artifact smoke hermetic while discovery paths remain the
            // explicit fixtures below.
            .env("HOME", &machine_home)
            .env("USERPROFILE", &machine_home)
            .env_remove("HAIDER_DISCOVERY_DISABLED")
            .env_remove("HAIDER_DEVICE_DISCOVERY_DISABLED")
            .env("HAIDER_TEST_CLAUDE_CREDENTIAL_STORE", "unavailable")
            .env("HAIDER_CODEX_AUTH_PATH", &codex)
            .env("HAIDER_CLAUDE_CREDS_PATH", &missing)
            .env("HAIDER_CLAUDE_OAUTH_PATH", &missing)
            .env("HAIDER_KIMI_CREDS_PATH", &missing)
            .env("HAIDER_KIMI_DEVICE_ID_PATH", &missing)
            .env("HAIDER_GROK_AUTH_PATH", &missing)
            .env("HAIDER_GEMINI_CREDS_PATH", &missing)
            .env("HAIDER_GCLOUD_CONFIG_DIR", &missing)
            .env_remove("HAIDER_RUNTIME_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start built haider status")
    };

    let child = start_status(false);
    let output = wait_for_output(child, STATUS_TIMEOUT);
    assert!(
        output.status.success(),
        "status failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status stdout is one JSON document");
    assert_eq!(document["kind"], "status");
    assert_eq!(document["daemon"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(document["daemon"]["ready"], true);
    assert!(
        document["daemon"]["pid"]
            .as_u64()
            .is_some_and(|pid| pid > 0),
        "status did not expose the serving daemon PID: {document}"
    );
    let socket_path = document["daemon"]["socket_path"]
        .as_str()
        .expect("status socket_path string");
    assert!(Path::new(socket_path).is_absolute());
    let resolved = haider_client::resolve_profile(&haider_client::ProfileEnv {
        profile_dir: Some(profile.clone()),
        home: Some(machine_home.clone()),
        user_profile: Some(machine_home.clone()),
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve the status fixture profile");
    assert_eq!(
        Path::new(socket_path),
        resolved.endpoint_path,
        "status must report the shared platform rendezvous address"
    );
    assert_eq!(
        document["daemon"]["pid_file_path"]
            .as_str()
            .map(Path::new)
            .and_then(Path::parent),
        Some(resolved.runtime_dir.as_path()),
        "the PID file must stay under the resolved filesystem runtime"
    );
    assert_eq!(
        document["runtime_dir"].as_str().map(Path::new),
        Some(resolved.runtime_dir.as_path()),
        "status must report the resolved filesystem runtime"
    );
    #[cfg(unix)]
    assert!(
        !Path::new(socket_path).starts_with(&machine_home),
        "an overlong HOME must report the live short fallback, not the rejected preferred path"
    );
    let discovery_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = wait_for_output(start_status(true), STATUS_TIMEOUT);
        assert!(
            output.status.success(),
            "follow-up status failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)
            .expect("follow-up status stdout is one JSON document");
        let discovered = document["account_adoption_available"]
            .as_array()
            .is_some_and(|adoption| adoption.iter().any(|notice| notice["source"] == "codex"));
        if discovered {
            break;
        }
        assert!(
            Instant::now() < discovery_deadline,
            "enabled lazy discovery never projected the seeded candidate: {document}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
