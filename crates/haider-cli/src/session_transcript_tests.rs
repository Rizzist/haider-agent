#![allow(clippy::expect_used)]
use super::*;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

#[test]
fn transcript_cli_parses_defaults_and_explicit_paging() {
    let defaults = parse_options(&args(&["session-1"]))
        .expect("parse")
        .expect("options");
    assert_eq!(defaults.request.after_seq, 0);
    assert_eq!(defaults.request.limit, 100);
    assert!(!defaults.json);
    let explicit = parse_options(&args(&[
        "session-1",
        "--after",
        "52",
        "--limit",
        "12",
        "--output",
        "json",
    ]))
    .expect("parse")
    .expect("options");
    assert_eq!(explicit.request.after_seq, 52);
    assert_eq!(explicit.request.limit, 12);
    assert!(explicit.json);
    assert!(matches!(
        super::super::routing::parse_command(&args(&["session", "transcript", "session-1"])),
        Ok(super::super::routing::Command::SessionTranscript(_))
    ));
}

#[test]
fn transcript_cli_rejects_invalid_or_ambiguous_arguments() {
    for input in [
        vec![],
        vec![""],
        vec!["session-1", "--after", "-1"],
        vec!["session-1", "--after", "18446744073709551615"],
        vec!["session-1", "--limit", "0"],
        vec!["session-1", "--limit", "1025"],
        vec!["session-1", "--limit"],
        vec!["session-1", "--output", "xml"],
        vec!["session-1", "--after", "0", "--after", "2"],
        vec!["session-1", "--profile", "elsewhere"],
    ] {
        assert!(parse_options(&args(&input)).is_err(), "{input:?}");
    }
    assert!(parse_options(&args(&["--help"])).expect("help").is_none());
}
