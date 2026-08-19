//! Manual `haider update` parser laws.
#![allow(clippy::expect_used)]
#![allow(dead_code)]

#[path = "../src/main.rs"]
mod cli_main;

use cli_main::update::check_policy::{
    BackgroundCheckOutcome, CheckReservation, UPDATE_CHECK_INTERVAL_SECS, UPDATE_CHECK_STAMP_FILE,
    automatic_checks_disabled_with, background_outcome_from_discovery, check_due, reserve_check,
};
use cli_main::update::tui_restart::{RestartMode, restart_plan_for};
use cli_main::update::{
    EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE, UpdateAvailability, UpdateError,
    UpdateOptions, parse_update_options,
};
use cli_main::{BareTuiOptions, parse_bare_tui_options};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
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

/// MUTATION CHECK: forget to publish the first stamp, shorten the interval,
/// or rewrite a skipped reservation. Expected RUNTIME failure: the persisted
/// timestamp or the second decision/content assertion changes.
#[test]
fn on_open_check_reservation_writes_fresh_stamp_then_skips_within_six_hours() {
    let profile = tempfile::tempdir().expect("profile tempdir");
    let first = 1_800_000_000;
    assert_eq!(
        reserve_check(profile.path(), first, false).expect("reserve fresh check"),
        CheckReservation::Due
    );
    let stamp = profile.path().join(UPDATE_CHECK_STAMP_FILE);
    assert_eq!(
        std::fs::read_to_string(&stamp).expect("read fresh stamp"),
        format!("{first}\n")
    );

    assert_eq!(
        reserve_check(
            profile.path(),
            first + UPDATE_CHECK_INTERVAL_SECS - 1,
            false,
        )
        .expect("evaluate recent check"),
        CheckReservation::Skipped
    );
    assert_eq!(
        std::fs::read_to_string(stamp).expect("read unchanged stamp"),
        format!("{first}\n")
    );
}

/// MUTATION CHECK: compare elapsed time in the wrong direction, make the
/// boundary exclusive, or let clock rollback force a request. Expected
/// RUNTIME failure: this pure clock table changes without relying on sleeps.
#[test]
fn on_open_check_due_is_a_pure_six_hour_clock_policy() {
    let now = 50_000;
    assert!(check_due(None, now));
    assert!(!check_due(Some(now), now));
    assert!(!check_due(Some(now), now + UPDATE_CHECK_INTERVAL_SECS - 1));
    assert!(check_due(Some(now), now + UPDATE_CHECK_INTERVAL_SECS));
    assert!(!check_due(Some(now + 1), now));
}

/// MUTATION CHECK: treat every non-empty value as true or ignore the additive
/// flag twin. Expected RUNTIME failure: one row in this explicit table flips.
#[test]
fn automatic_update_check_opt_out_honors_exact_env_and_flag_twins() {
    assert!(!automatic_checks_disabled_with(false, None));
    assert!(!automatic_checks_disabled_with(
        false,
        Some(OsStr::new("0"))
    ));
    assert!(automatic_checks_disabled_with(false, Some(OsStr::new("1"))));
    assert!(automatic_checks_disabled_with(true, None));
}

/// MUTATION CHECK: leak an offline discovery failure into the UI or announce
/// an equal release. Expected RUNTIME failure: either case stops mapping to
/// the deliberately non-error `Silent` background outcome.
#[test]
fn background_check_silences_network_failure_and_non_new_release() {
    assert_eq!(
        background_outcome_from_discovery(Err(UpdateError::Network("offline".into()))),
        BackgroundCheckOutcome::Silent
    );
    assert_eq!(
        background_outcome_from_discovery(Ok(UpdateAvailability::Current {
            version: "0.0.932".into(),
        })),
        BackgroundCheckOutcome::Silent
    );
    assert_eq!(
        background_outcome_from_discovery(Ok(UpdateAvailability::Available {
            current: "0.0.932".into(),
            latest: "0.0.933".into(),
        })),
        BackgroundCheckOutcome::Available {
            version: "0.0.933".into(),
        }
    );
}

/// MUTATION CHECK: make the explicit-check bypass a no-op or forget to
/// refresh the automatic-check stamp. Expected RUNTIME failure: the decision
/// or persisted second timestamp remains at `first`.
#[test]
fn explicit_check_bypasses_rate_limit_and_refreshes_stamp() {
    let profile = tempfile::tempdir().expect("profile tempdir");
    let first = 1_800_000_000;
    reserve_check(profile.path(), first, false).expect("reserve automatic check");
    assert_eq!(
        reserve_check(profile.path(), first + 1, true).expect("reserve explicit check"),
        CheckReservation::Due
    );
    assert_eq!(
        std::fs::read_to_string(profile.path().join(UPDATE_CHECK_STAMP_FILE))
            .expect("read refreshed stamp"),
        format!("{}\n", first + 1)
    );
}

/// MUTATION CHECK: accept the environment twin but drop the additive bare or
/// `haider tui` flag before it reaches the front door. Expected failure: the
/// parsed policy bit or session id changes.
#[test]
fn bare_tui_update_opt_out_flag_preserves_session_arguments() {
    let expected = BareTuiOptions {
        session: Some("session-42".to_owned()),
        no_update_check: true,
    };
    for args in [
        vec!["--no-update-check", "--session", "session-42"],
        vec!["--session", "session-42", "--no-update-check"],
    ] {
        let args = args.into_iter().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            parse_bare_tui_options(&args).expect("parse bare TUI flags"),
            Some(expected.clone())
        );
    }
}

/// MUTATION CHECK: reconstruct arguments from strings, omit a session flag,
/// or choose the wrong hand-off per OS. Expected failure: either pure plan no
/// longer retains exact `OsString`s or the strategy table changes.
#[test]
fn tui_restart_plans_preserve_original_argv_for_both_process_strategies() {
    let executable = PathBuf::from("/opt/haider/bin/haider");
    let original = vec![
        OsString::from("haider"),
        OsString::from("tui"),
        OsString::from("--session"),
        OsString::from("session-42"),
        OsString::from("--no-update-check"),
    ];
    let expected_args = original[1..].to_vec();

    let unix = restart_plan_for(executable.clone(), &original, false);
    assert_eq!(unix.executable, executable);
    assert_eq!(unix.args, expected_args);
    assert_eq!(unix.mode, RestartMode::Exec);

    let windows = restart_plan_for(executable.clone(), &original, true);
    assert_eq!(windows.executable, executable);
    assert_eq!(windows.args, expected_args);
    assert_eq!(windows.mode, RestartMode::DetachedSpawn);
}
