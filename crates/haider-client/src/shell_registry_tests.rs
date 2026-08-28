#![allow(clippy::expect_used)]

use super::shell_registry::{ShellEvent, shell_event_from_frame, shell_registry_available};
use haider_rpc::{
    CapabilitySet, FEATURE_SHELL_REGISTRY_V1, LifecyclePhase, ShellKindWire, ShellOutputStreamWire,
    ShellStatusWire, ShellWire, Welcome, WireFrame,
};

fn welcome() -> Welcome {
    Welcome {
        protocol: 1,
        instance_id: "instance".into(),
        daemon_generation: 1,
        frame_limit: 1_024,
        profile_id: "profile".into(),
        daemon_version: "test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::default(),
        features: Default::default(),
        user_command_withheld: false,
        encoding: None,
    }
}

fn shell() -> ShellWire {
    ShellWire {
        id: "sh-0123456789abcdef0123".into(),
        kind: ShellKindWire::Local,
        status: ShellStatusWire::Running,
        title: "tests".into(),
        cwd_or_host: "/workspace".into(),
        created_at_ms: 1,
        last_activity_ms: 2,
        bytes_out: 3,
    }
}

#[test]
fn shell_registry_surface_obeys_feature_absence_law() {
    let mut welcome = welcome();
    assert!(!shell_registry_available(&welcome));
    welcome.features.insert(FEATURE_SHELL_REGISTRY_V1.into());
    assert!(shell_registry_available(&welcome));
}

#[test]
fn all_shell_event_frames_map_without_parallel_taxonomy() {
    let shell = shell();
    assert_eq!(
        shell_event_from_frame(WireFrame::ShellOpened {
            shell: shell.clone()
        }),
        Some(ShellEvent::Opened(shell.clone()))
    );
    assert_eq!(
        shell_event_from_frame(WireFrame::ShellState {
            shell: shell.clone()
        }),
        Some(ShellEvent::State(shell.clone()))
    );
    assert_eq!(
        shell_event_from_frame(WireFrame::ShellClosed {
            shell: shell.clone()
        }),
        Some(ShellEvent::Closed(shell.clone()))
    );
    assert_eq!(
        shell_event_from_frame(WireFrame::ShellOutput {
            id: shell.id.clone(),
            stream: ShellOutputStreamWire::Stderr,
            chunk_b64: haider_rpc::TerminalOutputWire::new("Zml4dHVyZQ=="),
        }),
        Some(ShellEvent::Output {
            id: shell.id,
            stream: ShellOutputStreamWire::Stderr,
            chunk_b64: haider_rpc::TerminalOutputWire::new("Zml4dHVyZQ=="),
        })
    );
}
