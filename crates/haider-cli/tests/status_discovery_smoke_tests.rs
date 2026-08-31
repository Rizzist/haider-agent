//! Real-artifact status smoke with device discovery enabled on every platform.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
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
        if name == "haiderd" {
            assert!(
                metadata.len() > 10 * 1024 * 1024,
                "haiderd artifact is smaller than the 10 MiB integrity floor"
            );
        }
    }
    static WARMED: OnceLock<()> = OnceLock::new();
    WARMED.get_or_init(|| {
        for (name, binary) in [("haider", &haider), ("haiderd", &haiderd)] {
            let status = Command::new(binary)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap_or_else(|error| panic!("warm {name} binary: {error}"));
            assert!(
                status.success(),
                "{name} --version warm-up failed: {status}"
            );
        }
    });
    (haider, haiderd)
}

fn isolated_status_command(haider: &Path, profile: &Path, machine_home: &Path) -> Command {
    let mut command = Command::new(haider);
    command
        .args(["status", "--json"])
        .env("HAIDER_PROFILE_DIR", profile)
        .env("HOME", machine_home)
        .env("USERPROFILE", machine_home)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env("HAIDER_DEVICE_DISCOVERY_DISABLED", "1")
        .env("HAIDER_TEST_SIBLINGS_PREBUILT", "1")
        .env_remove("HAIDER_RUNTIME_DIR")
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn status_document(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "status failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("status stdout is one JSON document")
}

/// P0 release gate: run the actual CLI and daemon binaries against a fresh
/// profile. CI globally disables discovery for its other tests, so this child
/// explicitly removes both disable switches. The native Claude store alone is
/// a deterministic typed-unavailable seam; a file-backed Codex fixture must
/// still be discovered and projected into a later status JSON response. The
/// first status must not wait for the native platform calls: it schedules the
/// bounded discovery and returns the last completed snapshot immediately.
#[test]
fn built_status_json_honors_private_xdg_with_enabled_discovery() {
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
    #[cfg(unix)]
    assert_eq!(
        document["runtime_dir_resolution"]["source"],
        "xdg_runtime_dir"
    );
    #[cfg(windows)]
    assert_eq!(document["runtime_dir_resolution"]["source"], "home");
    assert!(
        document["runtime_dir_resolution"]
            .get("rejections")
            .is_none(),
        "an accepted platform-preferred runtime must not report a rejection"
    );
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

/// MUTATION CHECK (explicit runtime reporting): ignore HAIDER_RUNTIME_DIR or
/// omit the additive status field. Expected failure: the real CLI either
/// leaves the configured root or cannot name it as the winning source.
#[test]
fn built_status_honors_a_short_explicit_runtime_root() {
    let (haider, _haiderd) = assert_real_sibling_artifacts();
    #[cfg(unix)]
    let root = tempfile::Builder::new()
        .prefix("hre-")
        .tempdir_in("/tmp")
        .expect("short explicit status root");
    #[cfg(not(unix))]
    let root = tempfile::tempdir().expect("explicit status root");
    let profile = root.path().join("profile");
    let machine_home = root.path().join("home");
    let runtime_root = root.path().join("runtime");
    std::fs::create_dir_all(&machine_home).expect("create isolated machine-user home");
    std::fs::create_dir(&runtime_root).expect("create explicit runtime root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o700))
            .expect("make explicit runtime root owner-private");
    }
    let _daemon = DaemonGuard {
        profile: profile.clone(),
    };

    let mut command = isolated_status_command(&haider, &profile, &machine_home);
    command.env("HAIDER_RUNTIME_DIR", &runtime_root);
    let output = wait_for_output(
        command.spawn().expect("start explicit-root status"),
        STATUS_TIMEOUT,
    );
    let document = status_document(&output);
    assert!(
        output.stderr.is_empty(),
        "accepted explicit root warned unexpectedly"
    );
    assert_eq!(
        document["runtime_dir_resolution"]["source"],
        "haider_runtime_dir"
    );
    assert!(
        document["runtime_dir_resolution"]
            .get("rejections")
            .is_none(),
        "accepted explicit root must not report a rejection"
    );
    let canonical_runtime = runtime_root
        .canonicalize()
        .expect("canonicalize explicit runtime root");
    assert!(
        document["runtime_dir"]
            .as_str()
            .map(Path::new)
            .is_some_and(|path| path.starts_with(&canonical_runtime)),
        "status left the configured explicit root: {document}"
    );
}

/// MUTATION CHECK (XDG rejection visibility): restore the boolean-only XDG
/// selection. Expected failure: status silently uses HOME without the exact
/// 0755 rejection or emits something other than one warning line.
#[test]
#[cfg(unix)]
fn built_status_reports_non_private_xdg_fallback_to_home() {
    use std::os::unix::fs::PermissionsExt as _;

    let (haider, _haiderd) = assert_real_sibling_artifacts();
    let root = tempfile::Builder::new()
        .prefix("hrx-")
        .tempdir_in("/tmp")
        .expect("short rejected-XDG status root");
    let profile = root.path().join("profile");
    let machine_home = root.path().join("home");
    let xdg_runtime = root.path().join("xdg-runtime");
    std::fs::create_dir_all(&machine_home).expect("create isolated machine-user home");
    std::fs::create_dir(&xdg_runtime).expect("create non-private XDG runtime");
    std::fs::set_permissions(&xdg_runtime, std::fs::Permissions::from_mode(0o755))
        .expect("make XDG runtime non-private");
    let _daemon = DaemonGuard {
        profile: profile.clone(),
    };

    let mut command = isolated_status_command(&haider, &profile, &machine_home);
    command.env("XDG_RUNTIME_DIR", &xdg_runtime);
    let output = wait_for_output(
        command.spawn().expect("start rejected-XDG status"),
        STATUS_TIMEOUT,
    );
    let document = status_document(&output);
    assert_eq!(document["runtime_dir_resolution"]["source"], "home");
    assert_eq!(
        document["runtime_dir_resolution"]["rejections"],
        serde_json::json!([{
            "source": "xdg_runtime_dir",
            "reason": {"kind": "not_owner_private", "mode": "0755"}
        }])
    );
    let canonical_home = machine_home
        .canonicalize()
        .expect("canonicalize isolated machine-user home");
    assert!(
        document["runtime_dir"]
            .as_str()
            .map(Path::new)
            .is_some_and(|path| path.starts_with(canonical_home.join(".haider/runtime"))),
        "rejected XDG runtime did not fall back to HOME: {document}"
    );
    let stderr = String::from_utf8(output.stderr).expect("warning stderr is UTF-8");
    assert_eq!(
        stderr.lines().count(),
        1,
        "fallback must emit one warning line"
    );
    assert!(
        stderr.starts_with("haider: warning: runtime directory fallback selected HOME/USERPROFILE")
    );
    assert!(stderr.contains("XDG_RUNTIME_DIR: not owner-private (mode 0755)"));
}

/// MUTATION CHECK (explicit isolation root): restore the Unix `/tmp` escape on
/// AddressTooLong. Expected failure on Unix: status succeeds outside the 147+
/// character configured root. Windows intentionally succeeds because the
/// fixed named-pipe endpoint does not consume filesystem path length.
#[test]
fn built_status_fails_loudly_for_an_overlong_explicit_unix_root() {
    let (haider, _haiderd) = assert_real_sibling_artifacts();
    #[cfg(unix)]
    let root = tempfile::Builder::new()
        .prefix("hrl-")
        .tempdir_in("/tmp")
        .expect("short long-root status fixture");
    #[cfg(not(unix))]
    let root = tempfile::tempdir().expect("long-root status fixture");
    let profile = root.path().join("profile");
    let machine_home = root.path().join("home");
    std::fs::create_dir_all(&machine_home).expect("create isolated machine-user home");
    #[cfg(unix)]
    let long_runtime = tempfile::Builder::new()
        .prefix(&"r".repeat(147))
        .tempdir_in("/tmp")
        .expect("create 147+ character runtime root");
    #[cfg(not(unix))]
    let long_runtime = {
        let path = root.path().join("r".repeat(147));
        std::fs::create_dir(&path).expect("create 147-character runtime root");
        path
    };
    #[cfg(unix)]
    let long_runtime_path = long_runtime.path();
    #[cfg(not(unix))]
    let long_runtime_path = long_runtime.as_path();
    let _daemon = DaemonGuard {
        profile: profile.clone(),
    };

    let mut command = isolated_status_command(&haider, &profile, &machine_home);
    command.env("HAIDER_RUNTIME_DIR", long_runtime_path);
    let output = wait_for_output(
        command.spawn().expect("start long-root status"),
        STATUS_TIMEOUT,
    );

    #[cfg(unix)]
    {
        let expected =
            haider_client::resolve_profile_with_runtime_resolution(&haider_client::ProfileEnv {
                profile_dir: Some(profile),
                home: Some(machine_home.clone()),
                user_profile: Some(machine_home),
                model: None,
                runtime_dir: Some(long_runtime_path.to_path_buf()),
                xdg_runtime_dir: None,
            })
            .expect_err("the library resolver must reject the same explicit root");
        assert_eq!(output.status.code(), Some(76));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).expect("typed failure stderr is UTF-8"),
            format!("haider: {expected}\n")
        );
        let (length, limit) = match &expected {
            haider_client::ProfileError::RuntimeEndpoint {
                source: haider_platform::EndpointError::AddressTooLong { length, limit, .. },
            } => (*length, *limit),
            other => panic!("unexpected explicit runtime failure: {other}"),
        };
        let message = expected.to_string();
        assert!(message.contains(&format!("is {length} bytes")));
        assert!(message.contains(&format!("platform IPC limit is {limit} bytes")));
        assert!(message.contains("set HAIDER_RUNTIME_DIR to a shorter owner-private directory"));
    }

    #[cfg(windows)]
    {
        let document = status_document(&output);
        let canonical_long_runtime = long_runtime_path
            .canonicalize()
            .expect("canonicalize long Windows runtime root");
        assert_eq!(
            document["runtime_dir_resolution"]["source"],
            "haider_runtime_dir"
        );
        assert!(
            document["runtime_dir"]
                .as_str()
                .map(Path::new)
                .is_some_and(|path| path.starts_with(&canonical_long_runtime)),
            "Windows filesystem runtime left the explicit root: {document}"
        );
        let endpoint = document["daemon"]["socket_path"]
            .as_str()
            .expect("Windows status named-pipe endpoint");
        assert!(endpoint.starts_with(r"\\.\pipe\haider-"));
        assert_eq!(endpoint.len(), r"\\.\pipe\haider-".len() + 32);
    }
}
