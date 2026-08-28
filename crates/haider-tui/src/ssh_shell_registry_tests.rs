#![allow(clippy::expect_used)]

use super::*;

fn live_session() -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Session;
    model.active_session = Some(SessionId::new("session-ssh-ui"));
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SSH_PROFILES_V1.into());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SHELL_REGISTRY_V1.into());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_MONITOR_CONTROL_V1.into());
    model.requests.clear();
    model
}

fn shell(id: &str, kind: haider_rpc::ShellKindWire) -> haider_rpc::ShellWire {
    haider_rpc::ShellWire {
        id: id.into(),
        kind,
        status: haider_rpc::ShellStatusWire::Running,
        title: "tests".into(),
        cwd_or_host: "/workspace".into(),
        created_at_ms: 1,
        last_activity_ms: 2,
        bytes_out: 3,
    }
}

fn status_text(model: &AppModel) -> String {
    crate::render::status_left_segments(model, 240)
        .into_iter()
        .map(|segment| segment.text)
        .collect()
}

#[test]
fn status_strip_omits_zero_and_pluralizes_one_and_many_separately() {
    let mut model = live_session();
    let empty = status_text(&model);
    assert!(!empty.contains("shell"));
    assert!(!empty.contains("monitor"));

    model
        .shells
        .push(shell("sh-one", haider_rpc::ShellKindWire::Local));
    model.monitor_count = 1;
    let one = status_text(&model);
    assert!(one.contains("1 shell"));
    assert!(!one.contains("1 shells"));
    assert!(one.contains("1 monitor"));
    assert!(!one.contains("1 monitors"));

    model.shells.push(shell(
        "sh-two",
        haider_rpc::ShellKindWire::Ssh {
            profile: "prod".into(),
        },
    ));
    model.monitor_count = 3;
    let many = status_text(&model);
    assert!(many.contains("2 shells"));
    assert!(many.contains("3 monitors"));
}

#[test]
fn ssh_list_overlay_and_scope_commands_emit_typed_requests() {
    let mut model = live_session();
    model.ssh_command("");
    assert!(model.ssh_open);
    assert!(matches!(model.requests.as_slice(), [AppRequest::SshList]));

    model.requests.clear();
    model.ssh_command("scope prod,stage");
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::SshSetScope {
            scope: haider_rpc::SshScopeWire::Allow { names }
        }] if names == &["prod".to_owned(), "stage".to_owned()]
    ));
}

#[test]
fn shell_and_monitor_segments_open_distinct_overlays() {
    let mut model = live_session();
    model
        .shells
        .push(shell("sh-one", haider_rpc::ShellKindWire::Local));
    model.monitor_count = 1;

    model.handle_hit(Hit::ShellStatus);
    assert!(model.shells_open);
    assert!(!model.monitors_open);

    model.shells_open = false;
    model.handle_hit(Hit::MonitorStatus);
    assert!(model.monitors_open);
    assert!(!model.shells_open);
    assert!(matches!(
        model.requests.last(),
        Some(AppRequest::MonitorList)
    ));
}
