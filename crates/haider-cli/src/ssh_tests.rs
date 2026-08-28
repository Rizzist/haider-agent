#![allow(clippy::expect_used)]

use super::*;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn cli_json_shapes_are_stable_and_secret_free() {
    let profile = haider_rpc::SshProfileWire {
        name: "prod".into(),
        description: Some("Production".into()),
        host: "prod.example.invalid".into(),
        port: 22,
        user: "deploy".into(),
        default_cwd: None,
        host_key: None,
        last_used_ms: None,
        multiplexing: true,
        in_scope: true,
    };
    let list = serde_json::to_value(SshListDocument {
        schema: SSH_LIST_SCHEMA,
        profiles: std::slice::from_ref(&profile),
    })
    .expect("list JSON");
    assert_eq!(
        list,
        serde_json::json!({
            "schema": "haider.ssh.list.v1",
            "profiles": [{
                "name": "prod",
                "description": "Production",
                "host": "prod.example.invalid",
                "port": 22,
                "user": "deploy",
                "multiplexing": true,
                "in_scope": true
            }]
        })
    );
    let bytes = serde_json::to_vec(&list).expect("JSON bytes");
    assert!(!String::from_utf8_lossy(&bytes).contains("vault"));
}

#[test]
fn cli_parser_requires_one_explicit_auth_method_and_has_no_jump_host() {
    assert!(matches!(
        parse(&args(&[
            "add",
            "prod",
            "--host",
            "prod.test",
            "--user",
            "deploy",
            "--agent"
        ])),
        Ok(SshCommand::Add(_))
    ));
    assert!(
        parse(&args(&[
            "add",
            "prod",
            "--host",
            "prod.test",
            "--user",
            "deploy"
        ]))
        .is_err()
    );
    assert!(
        parse(&args(&[
            "add",
            "prod",
            "--host",
            "prod.test",
            "--user",
            "deploy",
            "--agent",
            "--jump",
            "bastion"
        ]))
        .is_err()
    );
}

#[test]
fn cli_parser_distinguishes_interactive_pty_from_one_shot_exec() {
    assert!(matches!(
        parse(&args(&["shell", "prod"])),
        Ok(SshCommand::Shell { command: None, .. })
    ));
    assert!(matches!(
        parse(&args(&["shell", "prod", "--", "uname", "-a"])),
        Ok(SshCommand::Shell { command: Some(command), .. }) if command == "uname -a"
    ));
}

#[test]
fn typed_refusals_map_to_scriptable_exit_codes() {
    assert_eq!(exit_code_for_refusal("ssh_timeout"), EX_TIMEOUT);
    assert_eq!(exit_code_for_refusal("ssh_host_key_changed"), EX_BLOCKED);
    assert_eq!(
        exit_code_for_refusal("ssh_agent_unavailable"),
        EX_UNAVAILABLE
    );
    assert_eq!(exit_code_for_refusal("ssh_profile_invalid_name"), EX_USAGE);
    assert_eq!(exit_code_for_refusal("ssh_command_failed"), EX_SOFTWARE);
    assert_eq!(exit_code_for_refusal("ssh_output_limit"), EX_SOFTWARE);
    assert_eq!(exit_code_for_refusal("ssh_channel_quota"), EX_BLOCKED);
}
