//! Black-box tests for the `haider` binary surface (v0.0.1 scope).
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use std::process::Command;

fn haider() -> Command {
    Command::new(env!("CARGO_BIN_EXE_haider"))
}

#[test]
fn version_prints_workspace_version() {
    let out = haider().arg("--version").output().expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(text.trim(), format!("haider {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn self_test_reports_ok_json() {
    let out = haider().arg("self-test").output().expect("binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert!(text.contains(r#""schema":"haider.selftest.v0""#));
    assert!(text.contains(r#""ok":true"#));
    assert!(text.contains("link:haider-protocol"));
    assert!(text.contains("link:haider-tui"));
}

#[test]
fn unknown_command_exits_2() {
    let out = haider().arg("frobnicate").output().expect("binary runs");
    assert_eq!(out.status.code(), Some(2));
}
