#![allow(clippy::expect_used)]

use super::*;

#[test]
fn local_and_ssh_shells_share_one_idempotent_lifecycle_registry() {
    let registry = ShellRegistry::default();
    let local = registry
        .open(ShellKindWire::Local, "tests", "/workspace")
        .expect("open local");
    let ssh = registry
        .open(
            ShellKindWire::Ssh {
                profile: "prod".into(),
            },
            "remote tests",
            "deploy@example.test",
        )
        .expect("open ssh");

    local.running().expect("local running");
    local.add_output(7).expect("local output");
    local.exited(Some(0)).expect("local exit");
    ssh.running().expect("ssh running");
    assert_eq!(registry.active_count(), 1);

    let first = registry.close(ssh.id()).expect("close ssh");
    let second = registry.close(ssh.id()).expect("idempotent close");
    assert_eq!(first, second);
    assert_eq!(first.status, ShellStatusWire::Closed);
    assert_eq!(registry.active_count(), 0);
    assert_eq!(registry.list().expect("list").len(), 2);
}

#[tokio::test]
async fn interactive_controls_and_output_keep_input_out_of_registry_rows_and_debug() {
    let registry = ShellRegistry::default();
    let mut events = registry.subscribe();
    let (shell, mut controls) = registry
        .open_interactive(
            ShellKindWire::Ssh {
                profile: "prod".into(),
            },
            "prod",
            "prod.example.test",
            Some("connection-1".into()),
        )
        .expect("open interactive shell");
    let shell_id = shell.id().to_owned();
    let sentinel = b"remote-password-sentinel-never-retained";
    let denied = registry
        .control(
            &shell_id,
            Some("connection-2"),
            ShellControl::Input(Zeroizing::new(sentinel.to_vec())),
        )
        .expect_err("another connection cannot inject PTY input");
    assert!(matches!(denied, ShellRegistryError::ControlDenied(_)));
    assert!(controls.try_recv().is_err());
    let denied_close = registry
        .close_control(&shell_id, Some("connection-2"))
        .expect_err("another connection cannot close the PTY");
    assert!(matches!(denied_close, ShellRegistryError::ControlDenied(_)));
    registry
        .control(
            &shell_id,
            Some("connection-1"),
            ShellControl::Input(Zeroizing::new(sentinel.to_vec())),
        )
        .expect("send terminal input");
    let control = controls.recv().await.expect("receive control");
    assert!(!format!("{control:?}").contains("remote-password-sentinel"));
    assert!(matches!(control, ShellControl::Input(bytes) if bytes.as_slice() == sentinel));

    shell
        .publish_output(ShellOutputStreamWire::Stdout, b"fixture-output")
        .expect("publish output");
    let output = loop {
        let event = events.recv().await.expect("registry event");
        assert!(!format!("{event:?}").contains("fixture-output"));
        if let ShellRegistryEvent::Output {
            owner, id, bytes, ..
        } = event
            && id == shell_id
        {
            assert_eq!(owner.as_deref(), Some("connection-1"));
            break bytes;
        }
    };
    assert_eq!(output.as_slice(), b"fixture-output");
    let rows = serde_json::to_vec(&registry.list().expect("registry rows"))
        .expect("serialize registry rows");
    assert!(
        !rows
            .windows(sentinel.len())
            .any(|window| window == sentinel)
    );
    assert!(!String::from_utf8_lossy(&rows).contains("fixture-output"));

    shell.exited(Some(0)).expect("record exit");
    registry
        .close_owner("connection-1")
        .expect("disconnect owner");
    assert_eq!(
        registry.get(&shell_id).expect("exited shell").status,
        ShellStatusWire::Exited { code: Some(0) }
    );
    let after_exit = registry
        .control(
            &shell_id,
            Some("connection-1"),
            ShellControl::Input(Zeroizing::new(b"too-late".to_vec())),
        )
        .expect_err("terminal shell refuses further input");
    assert!(matches!(after_exit, ShellRegistryError::ControlClosed(_)));
}

#[test]
fn close_wins_a_race_with_the_running_transition() {
    let registry = ShellRegistry::default();
    let (shell, _controls) = registry
        .open_interactive(
            ShellKindWire::Ssh {
                profile: "prod".into(),
            },
            "prod",
            "prod.example.test",
            Some("connection-1".into()),
        )
        .expect("open interactive shell");
    registry.close(shell.id()).expect("close before setup ends");

    let error = shell
        .running()
        .expect_err("closed row cannot transition back to running");
    assert!(matches!(error, ShellRegistryError::ControlClosed(_)));
}
