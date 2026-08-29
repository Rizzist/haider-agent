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
/// still be discovered and projected into status JSON.
#[test]
fn built_status_json_completes_with_enabled_discovery() {
    let (haider, _haiderd) = assert_real_sibling_artifacts();
    let root = tempfile::tempdir().expect("fresh status smoke root");
    let profile = root.path().join("profile");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("smoke workspace");
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

    let child = Command::new(haider)
        .args(["status", "--json"])
        .current_dir(&workspace)
        .env("HAIDER_PROFILE_DIR", &profile)
        // The daemon's lockdown ledger is machine-user global. Keep this
        // real-artifact smoke hermetic while discovery paths remain the
        // explicit fixtures below.
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
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
        .expect("start built haider status");
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
    let adoption = document["account_adoption_available"]
        .as_array()
        .expect("enabled discovery projects the seeded candidate");
    assert!(
        adoption.iter().any(|notice| notice["source"] == "codex"),
        "discovery was disabled or skipped: {document}"
    );
}
