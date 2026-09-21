use super::*;

#[test]
fn test_function_parser_ignores_helpers_and_finds_configured_tokio_tests() {
    let source = concat!(
        "fn concurrent_helper() {}\n",
        "#[tokio::test(flavor = \"multi_thread\", worker_threads = 2)]\n",
        "async fn simultaneous_clients_have_one_winner() {}\n",
        "#[cfg(unix)]\n",
        "#[test]\n",
        "fn interleaved_writes_are_atomic() {}\n",
    );
    assert_eq!(
        test_functions(source),
        [
            "simultaneous_clients_have_one_winner",
            "interleaved_writes_are_atomic"
        ]
    );
}

#[test]
fn catalog_parser_rejects_duplicates_and_unknown_platforms() {
    let content = concat!(
        "same\tplan9\tcrate\tlib\trace\tcrates/crate/src/lib.rs\n",
        "same\tlinux\tcrate\tlib\trace\tcrates/crate/src/lib.rs\n",
    );
    let violations = parse_catalog(content).expect_err("invalid catalog must fail");
    assert!(
        violations
            .iter()
            .any(|item| item.contains("invalid platform"))
    );
    assert!(violations.iter().any(|item| item.contains("duplicates id")));
    assert!(
        violations
            .iter()
            .any(|item| item.contains("duplicates crates/crate/src/lib.rs::race"))
    );
}

#[test]
fn explicit_race_tokens_do_not_match_grace_or_traced() {
    assert!(is_explicit_race_name("writers_racing_shutdown"));
    assert!(is_explicit_race_name("concurrent_writers"));
    assert!(is_explicit_race_name("interleaved_frames"));
    assert!(!is_explicit_race_name("waits_for_grace"));
    assert!(!is_explicit_race_name("traced_output"));
    assert!(!is_explicit_race_name("concurrent_process_writer_child"));
}

#[test]
fn target_constraint_parser_prevents_zero_match_platform_runs() {
    let source = concat!(
        "#[cfg(target_os = \"macos\")]\n",
        "#[test]\n",
        "fn mac_only_race() {}\n",
        "#[cfg(unix)]\n",
        "#[tokio::test]\n",
        "async fn portable_race() {}\n",
    );
    let constraints = test_target_constraints(source);
    assert_eq!(
        constraints.get("mac_only_race").map(String::as_str),
        Some("macos")
    );
    assert!(!constraints.contains_key("portable_race"));
}

#[test]
fn audit_requires_every_named_race_test_and_accepts_manual_timing_rows() {
    let root = std::env::temp_dir().join(format!(
        "haider-race-catalog-{}-{}",
        std::process::id(),
        line!()
    ));
    let source = root.join("crates/example/src/lib.rs");
    fs::create_dir_all(source.parent().expect("source parent")).expect("create source tree");
    fs::create_dir_all(root.join("scripts")).expect("create scripts tree");
    fs::write(
        &source,
        "#[test]\nfn concurrent_writers_commit_once() {}\n#[test]\nfn timeout_window() {}\n",
    )
    .expect("write source");
    fs::write(
        root.join(CATALOG),
        "manual\tlinux\texample\tlib\ttimeout_window\tcrates/example/src/lib.rs\n",
    )
    .expect("write initial catalog");

    let missing = audit(&root);
    assert!(
        missing
            .violations
            .iter()
            .any(|item| item.contains("uncataloged named race test"))
    );

    fs::write(
        root.join(CATALOG),
        concat!(
            "manual\tlinux\texample\tlib\ttimeout_window\tcrates/example/src/lib.rs\n",
            "writers\tlinux\texample\tlib\tconcurrent_writers_commit_once\tcrates/example/src/lib.rs\n",
        ),
    )
    .expect("write complete catalog");
    let complete = audit(&root);
    let _ = fs::remove_dir_all(&root);
    assert!(complete.violations.is_empty(), "{:?}", complete.violations);
    assert_eq!(complete.entries, 2);
    assert_eq!(complete.named_race_tests, 1);
}
