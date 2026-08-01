//! Side-effect-free daemon version smoke used by staged update verification.
#![allow(clippy::expect_used)]

use std::process::Command;

/// MUTATION CHECK: route `--version` through profile parsing or daemon boot.
/// Expected RUNTIME failure: output/status differs or the isolated profile
/// directory is created.
#[test]
fn daemon_version_is_exact_and_has_no_profile_side_effect() {
    let root = tempfile::tempdir().expect("version test root");
    let profile = root.path().join("must-not-exist");
    let output = Command::new(env!("CARGO_BIN_EXE_haiderd"))
        .arg("--version")
        .env("HAIDER_PROFILE_DIR", &profile)
        .output()
        .expect("run haiderd --version");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("haiderd {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(output.stderr.is_empty());
    assert!(!profile.exists());
}
