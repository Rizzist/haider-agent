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
