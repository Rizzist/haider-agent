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
    #[cfg(unix)]
    let xdg_runtime = {
        use std::os::unix::fs::PermissionsExt as _;

        let path = root.path().join("xdg-runtime");
        std::fs::create_dir(&path).expect("create private XDG runtime");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make XDG runtime owner-private");
        path
    };
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

    let configure = |command: &mut Command, command_profile: &Path| {
        command
            .current_dir(&workspace)
            .env("HAIDER_PROFILE_DIR", command_profile)
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.env("XDG_RUNTIME_DIR", &xdg_runtime);
        #[cfg(not(unix))]
        command.env_remove("XDG_RUNTIME_DIR");
    };
    let start_status = |no_spawn: bool| {
        let mut command = Command::new(&haider);
        command.args(["status", "--json"]);
        if no_spawn {
            command.arg("--no-spawn");
        }
        configure(&mut command, &profile);
        command.spawn().expect("start built haider status")
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
    #[cfg(unix)]
    let xdg_runtime_dir = Some(xdg_runtime.clone());
    #[cfg(not(unix))]
    let xdg_runtime_dir = None;
    let resolved = haider_client::resolve_profile(&haider_client::ProfileEnv {
        profile_dir: Some(profile.clone()),
        home: Some(machine_home.clone()),
        user_profile: Some(machine_home.clone()),
        model: None,
        runtime_dir: None,
        xdg_runtime_dir,
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
    assert_eq!(
        document["profile_path"].as_str().map(Path::new),
        Some(resolved.store_dir.as_path()),
        "status profile path must use the canonical resolver identity"
    );
    assert_eq!(
        document["daemon"]["pipe_dir"].as_str().map(Path::new),
        Some(resolved.store_dir.join("pipe").as_path()),
        "status sidecar directory must share the canonical profile spelling"
    );
    #[cfg(unix)]
    {
        let canonical_xdg = xdg_runtime
            .canonicalize()
            .expect("canonicalize private XDG runtime");
        assert!(
            resolved
                .runtime_dir
                .starts_with(canonical_xdg.join("haider")),
            "a fitting private XDG runtime must outrank HOME"
        );
        assert_eq!(
            Path::new(socket_path)
                .canonicalize()
                .expect("canonicalize live daemon socket"),
            Path::new(socket_path),
            "status socket path must use its canonical filesystem spelling"
        );
        let pid_file = document["daemon"]["pid_file_path"]
            .as_str()
            .map(PathBuf::from)
            .expect("status pid_file_path string");
        assert_eq!(
            pid_file.canonicalize().expect("canonicalize PID file"),
            pid_file,
            "status PID-file path must use its canonical filesystem spelling"
        );
    }
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

    std::fs::write(profile.join("config.json"), "{not json")
        .expect("damage model config after daemon identity is established");
    let mut stop = Command::new(&haider);
    stop.args(["daemon", "stop", "--json", "--timeout", "10s"]);
    configure(&mut stop, &profile);
    let output = wait_for_output(stop.spawn().expect("start daemon stop"), STATUS_TIMEOUT);
    assert!(
        output.status.success(),
        "daemon stop failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stopped: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("daemon stop stdout is JSON");
    assert_eq!(stopped["schema"], "haider.daemon-stop.v1");
    assert_eq!(stopped["outcome"], "stopped_cleanly");
    assert_eq!(stopped["daemon"]["pid"], document["daemon"]["pid"]);
    assert_eq!(stopped["daemon"]["completion"], "graceful");
    assert_eq!(stopped["daemon"]["process_exited"], true);
    assert_eq!(
        stopped["daemon"]["shutdown_acknowledged"], true,
        "a successful stop must expose the RPC acknowledgement"
    );
    assert!(
        stopped["elapsed_ms"]
            .as_u64()
            .is_some_and(|value| value < 10_000),
        "stop lifecycle must be measured inside the caller deadline: {stopped}"
    );

    assert!(
        std::fs::read_dir(&profile)
            .expect("read stopped profile")
            .all(|entry| !entry
                .expect("read stopped profile entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".daemon-stop-")),
        "the caller must consume its generation-bound completion receipt"
    );

    let never_created_profile = root.path().join("never-created-profile");
    let mut absent = Command::new(&haider);
    absent.args(["daemon", "stop", "--json", "--timeout", "1s"]);
    configure(&mut absent, &never_created_profile);
    let output = wait_for_output(absent.spawn().expect("start absent stop"), STATUS_TIMEOUT);
    assert_eq!(output.status.code(), Some(69));
    let absent: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("absent stop stdout is JSON");
    assert_eq!(absent["outcome"], "not_running");
    assert!(
        !never_created_profile.exists(),
        "probing a never-created profile must not create a lock directory"
    );

    let empty_profile = root.path().join("existing-empty-profile");
    std::fs::create_dir(&empty_profile).expect("create empty profile");
    let entries_before = std::fs::read_dir(&empty_profile)
        .expect("read empty profile before stop")
        .count();
    let mut absent = Command::new(&haider);
    absent.args(["daemon", "stop", "--json", "--timeout", "1s"]);
    configure(&mut absent, &empty_profile);
    let output = wait_for_output(
        absent.spawn().expect("start empty-profile stop"),
        STATUS_TIMEOUT,
    );
    assert_eq!(output.status.code(), Some(69));
    let entries_after = std::fs::read_dir(&empty_profile)
        .expect("read empty profile after stop")
        .count();
    assert_eq!(
        entries_after, entries_before,
        "probing an existing empty profile must not create a lock file"
    );

    let wedged_profile = root.path().join("wedged-profile");
    let lease = haider_store::Store::acquire_profile(&wedged_profile)
        .expect("hold a profile lock without serving an endpoint");
    let mut wedged = Command::new(&haider);
    wedged.args(["daemon", "stop", "--json", "--timeout", "100ms"]);
    configure(&mut wedged, &wedged_profile);
    let output = wait_for_output(
        wedged.spawn().expect("start bounded wedged stop"),
        STATUS_TIMEOUT,
    );
    drop(lease);
    assert_eq!(output.status.code(), Some(124));
    let wedged: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("wedged stop stdout is JSON");
    assert_eq!(wedged["outcome"], "did_not_stop");
    assert_eq!(wedged["phase"], "connect");
}
