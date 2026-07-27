//! Bare-`haider` auto-spawn acceptance over REAL subprocess binaries
//! (report §6.2 tests): the concurrent-launch race, stale-socket recovery by
//! the winner only, and parent-exit-leaves-daemon.
//!
//! Each test isolates its profile in a fresh store directory (`profile_id`
//! is store-path-derived, so endpoints never collide) and kills its daemon
//! through a drop guard, so no test leaks a process past its assertions.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use haider_client::{ProfileEnv, ResolvedProfile, resolve_profile};

fn haider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_haider"))
}

/// The sibling `haiderd` next to the `haider` under test — built on demand
/// when this test crate is run in isolation (a full workspace test build
/// already produced it).
fn ensure_haiderd_built() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let sibling = haider_binary()
        .parent()
        .expect("haider binary has a parent directory")
        .join("haiderd");
    if !sibling.exists() {
        BUILD.call_once(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "haider-daemond", "--bin", "haiderd"])
                .status()
                .expect("build haiderd for auto-spawn tests");
            assert!(status.success(), "haiderd build failed");
        });
    }
    assert!(
        sibling.exists(),
        "haiderd sibling missing at {}",
        sibling.display()
    );
    sibling
}

fn resolved_for(store: &Path) -> ResolvedProfile {
    resolve_profile(&ProfileEnv {
        profile_dir: Some(store.to_path_buf()),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve test profile")
}

fn haider_command(store: &Path) -> Command {
    let mut command = Command::new(haider_binary());
    command
        .env("HAIDER_PROFILE_DIR", store)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

/// Kills the profile's daemon (via the advisory pid in the lock file) when
/// the test ends, so failed assertions cannot leak daemons.
struct DaemonGuard {
    store: PathBuf,
}

impl DaemonGuard {
    fn pid(&self) -> Option<u32> {
        let contents = std::fs::read_to_string(self.store.join("lock")).ok()?;
        contents
            .lines()
            .find_map(|line| line.strip_prefix("pid="))
            .and_then(|pid| pid.trim().parse().ok())
    }

    fn terminate_and_wait(&self, endpoint: &Path) {
        if let Some(pid) = self.pid() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while endpoint.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

fn assert_daemon_serves(profile: &ResolvedProfile) {
    let endpoint = profile.endpoint_path.clone();
    let expected_profile = profile.profile_id.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async move {
        let connected = haider_client::connect(&endpoint, haider_client::ClientConfig::default())
            .await
            .expect("daemon endpoint must be serving");
        assert_eq!(connected.welcome.profile_id, expected_profile);
        assert!(
            connected
                .welcome
                .features
                .is_superset(&haider_client::required_live_features()),
            "daemon must advertise the live feature families"
        );
        connected.client.close();
    });
}

// MUTATION CHECK: R8 concurrent-launch arbitration — the store lock elects
// exactly one daemon, a losing candidate exits 75, and BOTH parents complete
// a Ready handshake. Mutating ensure_daemon to treat exit 75 as fatal (or to
// stop polling once its own candidate dies) makes one launcher exit nonzero
// and fails this test.
#[test]
fn two_simultaneous_launchers_elect_one_daemon_and_both_reach_ready() {
    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };

    let first = haider_command(store.path())
        .spawn()
        .expect("spawn first haider");
    let second = haider_command(store.path())
        .spawn()
        .expect("spawn second haider");
    let first = first.wait_with_output().expect("first haider output");
    let second = second.wait_with_output().expect("second haider output");
    for (name, output) in [("first", &first), ("second", &second)] {
        assert!(
            output.status.success(),
            "{name} launcher failed: status {:?}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("daemon ready"),
            "{name} launcher must report a ready daemon"
        );
    }

    // Exactly one daemon serves the shared endpoint afterward, and a third
    // launcher attaches without spawning.
    assert_daemon_serves(&profile);
    let third = haider_command(store.path())
        .output()
        .expect("third haider output");
    assert!(third.status.success());
    assert!(
        String::from_utf8_lossy(&third.stdout).contains("already running"),
        "third launcher must attach to the incumbent: {}",
        String::from_utf8_lossy(&third.stdout)
    );

    guard.terminate_and_wait(&profile.endpoint_path);
}

// MUTATION CHECK: stale-endpoint recovery is exclusively the lock-winning
// daemon's job. The launcher observes ConnectionRefused on the dead socket
// node, spawns a candidate, and the daemon's claim/probe/unlink law recovers
// the node. Mutating the client to unlink the socket itself has no test to
// hide behind — the client crate contains no unlink call — and mutating
// ensure_daemon to treat Refused as fatal fails this test.
#[test]
fn stale_owner_socket_is_recovered_by_the_winning_daemon() {
    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };

    // Plant a dead same-owner socket node at the endpoint: bind, then drop
    // the listener. Connecting to the node now yields ConnectionRefused.
    std::fs::create_dir_all(&profile.runtime_dir).expect("create runtime dir");
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(&profile.runtime_dir, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::remove_file(&profile.endpoint_path);
    let stale =
        std::os::unix::net::UnixListener::bind(&profile.endpoint_path).expect("bind stale socket");
    drop(stale);
    assert!(
        profile.endpoint_path.exists(),
        "stale socket node must exist"
    );

    let output = haider_command(store.path())
        .output()
        .expect("haider output over stale socket");
    assert!(
        output.status.success(),
        "launcher must recover through the daemon: stdout {} stderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("daemon ready"));
    assert_daemon_serves(&profile);

    guard.terminate_and_wait(&profile.endpoint_path);
}

// MUTATION CHECK: R8 shutdown policy — parent exit leaves the daemon
// running; a client exit never implies daemon shutdown. Mutating the front
// door to kill or signal its spawned child on exit fails the post-exit
// serving assertion.
#[test]
fn parent_exit_leaves_the_daemon_running() {
    ensure_haiderd_built();
    let store = tempfile::tempdir().expect("store dir");
    let profile = resolved_for(store.path());
    let guard = DaemonGuard {
        store: store.path().to_path_buf(),
    };

    let output = haider_command(store.path())
        .output()
        .expect("haider output");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("daemon ready"));
    assert!(
        stdout.contains("spawned"),
        "first launch must spawn: {stdout}"
    );

    // The launcher has fully exited; the daemon must still serve, and the
    // daemon log must exist owner-only in the profile store.
    assert_daemon_serves(&profile);
    let log = store.path().join(haider_client::DAEMON_LOG_FILE);
    assert!(log.exists(), "owner-only daemon log must exist");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log)
            .expect("log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "daemon log must be owner-only");
    }

    guard.terminate_and_wait(&profile.endpoint_path);
}
