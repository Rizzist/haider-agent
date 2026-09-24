#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn strict_register_list_remove_parsing() {
    let register = MonitorRequest::from_tool_args(serde_json::json!({
        "operation": "register",
        "source": {"kind": "sms"},
        "filter": {"field": "body", "operator": "contains", "value": "deploy"},
        "action": {"report": true, "follow_up": "summarize it"},
        "occurrence": "once",
        "lifetime": {"kind": "timeout", "timeout_ms": 1_000}
    }))
    .unwrap();
    assert!(matches!(register, MonitorRequest::Register { .. }));
    assert_eq!(
        MonitorRequest::from_tool_args(serde_json::json!({"operation": "list"})).unwrap(),
        MonitorRequest::List
    );
    assert!(matches!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "remove", "monitor_id": "monitor-1"
        }))
        .unwrap(),
        MonitorRequest::Remove { .. }
    ));
}

#[test]
fn nested_and_top_level_unknown_fields_are_rejected() {
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "list", "surprise": true
        }))
        .is_err()
    );
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "process", "command": "printf ok", "surprise": true},
            "action": {"report": true}
        }))
        .is_err()
    );
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "sms"},
            "action": {"report": true},
            "lifetime": {"kind": "session", "surprise": true}
        }))
        .is_err()
    );
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "remove", "monitor_id": "monitor-1", "surprise": true
        }))
        .is_err()
    );
}

#[test]
fn every_tagged_monitor_variant_still_parses() {
    for source in [
        serde_json::json!({"kind": "sms"}),
        serde_json::json!({"kind": "task", "task_id": "task-123"}),
        serde_json::json!({"kind": "agent", "agent_id": "agent-123"}),
        serde_json::json!({"kind": "process", "command": "printf ok"}),
        serde_json::json!({"kind": "file", "path": "status.txt"}),
        serde_json::json!({"kind": "poll", "command": "printf ok", "interval_ms": 5_000}),
        serde_json::json!({"kind": "timer", "interval_ms": 1_000}),
        serde_json::json!({"kind": "cli", "preset": "custom", "argv": ["printf", "ok"]}),
    ] {
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": source,
                "action": {"report": true},
                "lifetime": {"kind": "session"}
            }))
            .is_ok()
        );
    }
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "sms"},
            "action": {"report": true},
            "lifetime": {"kind": "timeout", "timeout_ms": 100}
        }))
        .is_ok()
    );
}

#[test]
fn source_filter_action_and_duration_cross_checks_are_enforced() {
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "sms"},
            "filter": {"field": "payload", "operator": "contains", "value": "x"},
            "action": {"report": true}
        }))
        .is_err()
    );
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "timer", "interval_ms": 999},
            "action": {"report": true}
        }))
        .is_err()
    );
    assert!(
        MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "sms"},
            "action": {"report": false}
        }))
        .is_err()
    );
}

#[test]
fn cli_presets_expand_to_exact_argv_and_missing_binary_is_typed() {
    assert_eq!(
        cli_preset_argv(MonitorCliPreset::Codex, &["say hi".into()]),
        ["codex", "exec", "--json", "say hi"]
    );
    assert_eq!(
        cli_preset_argv(MonitorCliPreset::ClaudeCode, &["say hi".into()]),
        ["claude", "-p", "--output-format", "stream-json", "say hi"]
    );
    assert_eq!(
        cli_preset_argv(MonitorCliPreset::Opencode, &["say hi".into()]),
        ["opencode", "run", "say hi"]
    );
    assert_eq!(
        cli_preset_argv(MonitorCliPreset::Antigravity, &["say hi".into()]),
        ["antigravity", "run", "say hi"]
    );
    assert_eq!(
        cli_preset_argv(MonitorCliPreset::GhCi, &["123".into()]),
        ["gh", "run", "view", "123", "--json", "status,conclusion"]
    );
    assert_eq!(
        cli_preset_argv(
            MonitorCliPreset::Custom,
            &["custom-bin".into(), "--flag=value".into()]
        ),
        ["custom-bin", "--flag=value"]
    );

    let missing = MonitorSource::Cli {
        preset: MonitorCliPreset::Custom,
        argv: vec!["__not_a_real_cmd__".into()],
        env_passthrough: Vec::new(),
        cwd: None,
        interval_ms: None,
    };
    let error = MonitorCommandApproval::new(&missing, Path::new("."))
        .expect_err("missing monitor binary must be rejected before approval");
    assert!(
        error.to_string().contains("binary is missing"),
        "unexpected missing-binary error: {error}"
    );
}

#[test]
fn external_file_monitor_uses_fs_read_approval_while_workspace_file_does_not() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("watched.txt");
    std::fs::write(&external, "ready").unwrap();
    let approval = MonitorApproval::new(
        &MonitorSource::File {
            path: external.to_string_lossy().into_owned(),
        },
        workspace.path(),
    )
    .unwrap()
    .expect("external file requires approval");
    assert_eq!(approval.effect_class(), EffectClass::FsRead);
    let canonical_external = std::fs::canonicalize(&external).unwrap();
    assert_eq!(approval.external_file(), Some(canonical_external.as_path()));

    assert!(
        MonitorApproval::new(
            &MonitorSource::File {
                path: "watched.txt".into(),
            },
            workspace.path(),
        )
        .unwrap()
        .is_none()
    );

    let relative_external = PathBuf::from("..").join(
        outside
            .path()
            .file_name()
            .expect("outside temporary directory name"),
    );
    let relative_approval = MonitorApproval::new(
        &MonitorSource::File {
            path: relative_external
                .join("watched.txt")
                .to_string_lossy()
                .into_owned(),
        },
        workspace.path(),
    )
    .unwrap()
    .expect("relative traversal outside the workspace requires approval");
    assert_eq!(
        relative_approval.external_file(),
        Some(canonical_external.as_path())
    );
}

#[cfg(windows)]
#[test]
fn windows_program_resolution_honors_pathext_for_exe_and_command_wrappers() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("codex.EXE");
    let wrapper = root.path().join("helper.CMD");
    std::fs::write(&executable, b"test").unwrap();
    std::fs::write(&wrapper, b"test").unwrap();

    assert_eq!(
        resolve_program_candidate_with_extensions(
            &root.path().join("codex"),
            ".COM;.EXE;.BAT;.CMD"
        ),
        Some(std::fs::canonicalize(executable).unwrap())
    );
    assert_eq!(
        resolve_program_candidate_with_extensions(
            &root.path().join("helper"),
            ".COM;.EXE;.BAT;.CMD"
        ),
        Some(std::fs::canonicalize(wrapper).unwrap())
    );
}
