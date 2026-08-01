//! Manual `haider update` parser laws.
#![allow(clippy::expect_used)]
#![allow(dead_code)]

#[path = "../src/main.rs"]
mod cli_main;

use cli_main::update::{
    EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE, UpdateError, UpdateOptions,
    parse_update_options,
};
use std::process::Command;

/// MUTATION CHECK: accept extra flags or make `--check` mutating. Expected
/// RUNTIME failure: exact parser results or usage exit authority change.
#[test]
fn update_parser_accepts_only_bare_or_check() {
    assert_eq!(
        parse_update_options(&[]).expect("bare update"),
        UpdateOptions { check: false }
    );
    assert_eq!(
        parse_update_options(&["--check".into()]).expect("check update"),
        UpdateOptions { check: true }
    );
    let error = parse_update_options(&["--check".into(), "extra".into()])
        .expect_err("extra argument refused");
    assert_eq!(error.exit_code(), EX_USAGE);
}

/// MUTATION CHECK: dispatch invalid update arguments into discovery before
/// usage validation. Expected RUNTIME failure: the black-box process no
/// longer exits 2 immediately with empty stdout.
#[test]
fn invalid_update_cli_arm_exits_usage_without_discovery() {
    let output = Command::new(env!("CARGO_BIN_EXE_haider"))
        .args(["update", "--check", "extra"])
        .output()
        .expect("run invalid update arm");
    assert_eq!(output.status.code(), Some(i32::from(EX_USAGE)));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: haider update [--check]"));
}

/// MUTATION CHECK: collapse network, local I/O, or post-rollback health into
/// one generic status. Expected RUNTIME failure: this complete stable mapping
/// table changes.
#[test]
fn update_error_classes_have_stable_exit_codes() {
    let cases = [
        (UpdateError::Usage("fixture".into()), EX_USAGE),
        (UpdateError::Network("fixture".into()), EX_UNAVAILABLE),
        (UpdateError::Io("fixture".into()), EX_IOERR),
        (UpdateError::Health("fixture".into()), EX_PROTOCOL),
        (UpdateError::Refused("fixture".into()), EX_SOFTWARE),
        (UpdateError::RestartTimeout("fixture".into()), EX_SOFTWARE),
        (UpdateError::Internal("fixture".into()), EX_SOFTWARE),
    ];
    for (error, expected) in cases {
        assert_eq!(error.exit_code(), expected);
    }
}
