#![allow(clippy::expect_used)]
//! Exercise the real executable boundary with a native, observable sibling.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn binary(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn bundle() -> tempfile::TempDir {
    // Keep the fixture on the prebuilt executable's filesystem so publishing
    // it can use a hard link instead of opening an executable inode for write.
    tempfile::tempdir_in(
        Path::new(env!("CARGO_BIN_EXE_haider"))
            .parent()
            .expect("binary directory"),
    )
    .expect("bundle")
}

fn publish_cli(directory: &Path) -> PathBuf {
    // The prebuilt binary is immutable throughout this test binary. A copy
    // opens a writer that a parallel fork can retain until exec, even after
    // copy returns. Syncing/renaming that same inode cannot revoke such an fd.
    // Link an already-closed executable, then replace the destination so a
    // writer of an old fixture cannot make this inode ETXTBSY either.
    let staged = directory.join(binary("haider-staged"));
    let cli = directory.join(binary("haider"));
    std::fs::hard_link(env!("CARGO_BIN_EXE_haider"), &staged).expect("immutable thin client");
    std::fs::rename(staged, &cli).expect("publish thin client");
    cli
}

fn command(cli: &Path, home: &Path) -> Command {
    let mut command = Command::new(cli);
    command
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("HAIDER_PROFILE_DIR", home.join("profile"))
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .env_remove("HAIDER_MODEL")
        .env_remove("HAIDER_RUNTIME_DIR")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("HAIDER_TUI_LAUNCH_VERSION")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn probe(directory: &Path) -> PathBuf {
    let source = directory.join("payload_probe.rs");
    // Native on every CI runner; no shell emulation or payload instrumentation.
    // This tiny fixture has no workspace dependencies and invokes no Cargo build.
    std::fs::write(
        &source,
        format!(
            r#"
fn main() {{
    std::hint::black_box({marker:?});
    println!("payload-pid:{{}}", std::process::id());
    println!("payload-argv:{{:?}}", std::env::args_os().collect::<Vec<_>>());
    println!("launch-version:{{}}", std::env::var("HAIDER_TUI_LAUNCH_VERSION").unwrap());
    std::process::exit(37);
}}
"#,
            marker = String::from_utf8(haider_client::payload_identity::marker_for_version(env!(
                "CARGO_PKG_VERSION"
            )))
            .expect("marker UTF-8")
        ),
    )
    .expect("fixture source");
    let executable = directory.join(binary("haider-tui"));
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--edition=2024")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("compile native payload probe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

#[test]
fn every_interactive_verb_executes_payload_with_original_arguments_and_exit_status() {
    let directory = bundle();
    let home = tempfile::tempdir().expect("home");
    let cli = publish_cli(directory.path());
    probe(directory.path());
    for args in [
        vec![],
        vec!["tui"],
        vec!["talk"],
        vec!["tui", "--demo", "--plain"],
        vec!["resume"],
        vec!["resume", "session α"],
        vec!["--resume"],
        vec!["--session", "session α", "--no-update-check"],
        vec!["ssh", "shell", "host"],
    ] {
        let child = command(&cli, home.path())
            .args(&args)
            .spawn()
            .expect("launch thin client");
        let pid = child.id();
        let output = child.wait_with_output().expect("payload output");
        assert_eq!(output.status.code(), Some(37), "{args:?}: {output:?}");
        let text = String::from_utf8(output.stdout).expect("fixture UTF-8");
        assert!(text.contains(&format!("launch-version:{}", env!("CARGO_PKG_VERSION"))));
        // Unix exec preserves the public entrypoint's argv[0]. Windows
        // CreateProcess supplies the payload executable there; every user
        // argument after it still crosses the boundary unchanged.
        let expected_arg0 = if cfg!(unix) {
            cli.clone()
        } else {
            directory.path().join(binary("haider-tui"))
        };
        let expected: Vec<_> = std::iter::once(expected_arg0.into_os_string())
            .chain(args.iter().map(std::ffi::OsString::from))
            .collect();
        assert!(
            text.contains(&format!("payload-argv:{expected:?}")),
            "{text}"
        );
        #[cfg(unix)]
        assert!(
            text.contains(&format!("payload-pid:{pid}")),
            "Unix must exec: {text}"
        );
        #[cfg(not(unix))]
        let _ = pid;
        assert!(
            !home.path().join("profile").exists(),
            "routing must not resolve a profile"
        );
    }
}

#[test]
fn every_headless_verb_never_executes_the_observable_payload() {
    let directory = bundle();
    let cli = publish_cli(directory.path());
    probe(directory.path());
    for args in [
        vec!["--version"],
        vec!["-V"],
        vec!["version"],
        vec!["self-test"],
        vec!["--ready", "invalid"],
        vec!["--install-bundle", "missing", "missing-destination"],
        vec!["run", "--invalid"],
        vec!["agent", "invalid"],
        vec!["workflow", "invalid"],
        vec!["status", "--invalid"],
        vec!["sessions", "--invalid"],
        vec!["sessions", "wait-ready", "--invalid"],
        vec!["daemon", "invalid"],
        vec!["session", "id", "config", "invalid"],
        vec!["session", "id", "recover", "--invalid"],
        vec!["session", "id", "seen", "--invalid"],
        vec!["session", "workspace", "invalid"],
        vec!["session", "id", "workspace", "invalid"],
        vec!["session", "id", "item", "bad"],
        vec!["session", "provider", "rebind", "--invalid"],
        vec!["session", "retract", "--invalid"],
        vec!["session", "id", "--invalid"],
        vec!["account", "invalid"],
        vec!["provider", "invalid"],
        vec!["lockdown", "invalid"],
        vec!["models", "--invalid"],
        vec!["fleet", "--invalid"],
        vec!["events", "--invalid"],
        vec!["graph", "--invalid"],
        vec!["export", "--invalid"],
        vec!["hooks", "invalid"],
        vec!["peer", "list"],
        vec!["peer", "send", "address", "message"],
        vec!["peer", "name", "new-name"],
        vec!["peer", "watch"],
        vec!["peer", "wait-idle", "address"],
        vec!["ssh", "invalid"],
        vec!["ssh", "shell", "host", "--", "echo hi"],
        vec!["shell", "invalid"],
        vec!["update", "--invalid"],
        vec!["resume", "--json", "--invalid"],
        vec!["import", "invalid"],
        vec!["unknown"],
    ] {
        let home = tempfile::tempdir().expect("headless home");
        let output = command(&cli, home.path())
            .args(&args)
            .output()
            .expect("headless output");
        if args[0] == "peer" {
            assert_eq!(output.status.code(), Some(2), "{args:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("unknown or incomplete command `peer`"),
                "{stderr}"
            );
            assert!(!stderr.contains("peer list"), "{stderr}");
        }
        assert_ne!(output.status.code(), Some(37), "{args:?}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("payload-pid:"),
            "{args:?}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr)
                .contains("cannot start matching interactive payload"),
            "{args:?}"
        );
    }
}

#[test]
fn missing_or_mismatched_payload_fails_before_profile_creation_but_version_stays_local() {
    let directory = bundle();
    let home = tempfile::tempdir().expect("home");
    #[cfg(target_os = "linux")]
    let old_writer = {
        let cli = directory.path().join(binary("haider"));
        std::fs::copy(env!("CARGO_BIN_EXE_haider"), &cli).expect("old thin client");
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&cli)
            .expect("hold old executable open for writing");
        let error = command(&cli, home.path())
            .arg("--version")
            .output()
            .expect_err("an executable with a live writer must fail before routing");
        assert_eq!(error.kind(), std::io::ErrorKind::ExecutableFileBusy);
        writer
    };
    let cli = publish_cli(directory.path());
    for present in [false, true] {
        if present {
            std::fs::write(
                directory.path().join(binary("haider-tui")),
                b"wrong version",
            )
            .expect("mismatch");
        }
        let version = command(&cli, home.path())
            .arg("--version")
            .output()
            .expect("version");
        assert!(version.status.success());
        assert_eq!(
            version.stdout,
            format!("haider {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
        );
        let launch = command(&cli, home.path())
            .arg("talk")
            .output()
            .expect("launch");
        assert_eq!(launch.status.code(), Some(69));
        assert!(!home.path().join("profile").exists());
    }
    #[cfg(target_os = "linux")]
    drop(old_writer);
}

#[test]
fn payload_version_probe_ignores_previous_tui_launch_version() {
    let payload = Path::new(env!("CARGO_BIN_EXE_haider")).with_file_name(binary("haider-tui"));
    assert!(payload.is_file(), "prebuild haider-tui alongside haider");
    let output = Command::new(payload)
        .arg("--version")
        .env("HAIDER_TUI_LAUNCH_VERSION", "0.0.969")
        .output()
        .expect("staged version probe");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        format!("haider-tui {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
}

#[test]
fn only_the_actual_payload_contains_its_complete_build_identity() {
    let cli = Path::new(env!("CARGO_BIN_EXE_haider"));
    assert!(
        haider_client::payload_identity::verify_payload(cli).is_err(),
        "thin cannot impersonate payload"
    );
    let daemon = cli.with_file_name(binary("haiderd"));
    assert!(daemon.is_file(), "prebuild haiderd");
    assert!(
        haider_client::payload_identity::verify_payload(&daemon).is_err(),
        "daemon cannot impersonate payload"
    );
    assert!(
        haider_client::payload_identity::verify_payload(&cli.with_file_name(binary("haider-tui")))
            .is_ok()
    );
}

#[test]
fn self_test_rejects_a_payload_from_another_version_before_running_the_daemon() {
    let directory = bundle();
    let home = tempfile::tempdir().expect("home");
    let cli = publish_cli(directory.path());
    let payload = probe(directory.path());
    std::fs::rename(&payload, directory.path().join(binary("haiderd"))).expect("observable daemon");
    std::fs::write(
        payload,
        haider_client::payload_identity::marker_for_version("0.0.999"),
    )
    .expect("future payload");
    let output = command(&cli, home.path())
        .arg("self-test")
        .output()
        .expect("self-test");
    assert_eq!(output.status.code(), Some(69), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "mixed bundle must not run the daemon fixture"
    );
    assert!(!home.path().join("profile").exists());
}
