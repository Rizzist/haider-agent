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

fn profile(name: &str) -> haider_rpc::SshProfileWire {
    haider_rpc::SshProfileWire {
        name: name.into(),
        description: Some("fixture".into()),
        host: "127.0.0.1".into(),
        port: 22,
        user: "fixture".into(),
        default_cwd: Some("/srv/fixture".into()),
        host_key: None,
        last_used_ms: None,
        multiplexing: true,
        in_scope: true,
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn status_text(model: &AppModel) -> String {
    crate::render::status_left_segments(model, 240)
        .into_iter()
        .map(|segment| segment.text)
        .collect()
}

// 970 monitorui: the shells/monitors counts LEFT the status strip for the
// band's task line, so this pin inverted — it now asserts the strip stays
// silent about both at every count, and the pluralisation contract it used
// to guard moved to `band_counts` (see `w970_monitorui_tests`).
#[test]
fn status_strip_never_carries_a_shell_or_monitor_count() {
    let mut model = live_session();
    let empty = status_text(&model);
    assert!(!empty.contains("shell"));
    assert!(!empty.contains("monitor"));

    model
        .shells
        .push(shell("sh-one", haider_rpc::ShellKindWire::Local));
    model.monitor_count = 1;
    let one = status_text(&model);
    assert!(
        !one.contains("shell"),
        "status strip regrew a shell count: {one}"
    );
    assert!(
        !one.contains("monitor"),
        "status strip regrew a monitor count: {one}"
    );

    model.shells.push(shell(
        "sh-two",
        haider_rpc::ShellKindWire::Ssh {
            profile: "prod".into(),
        },
    ));
    model.monitor_count = 3;
    let many = status_text(&model);
    assert!(
        !many.contains("shell"),
        "status strip regrew a shell count: {many}"
    );
    assert!(
        !many.contains("monitor"),
        "status strip regrew a monitor count: {many}"
    );
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

#[test]
fn ssh_profile_overlay_opens_add_edit_test_remove_and_shell_flows() {
    let mut model = live_session();
    model.apply_ssh_list(vec![profile("prod")]);
    model.ssh_open = true;

    model.handle_key(key(KeyCode::Char('a')), std::time::Instant::now());
    let add = model.ssh_form.as_ref().expect("add form");
    assert!(add.original.is_none());
    assert_eq!(add.auth, SshFormAuthKind::KeyFile);

    model.ssh_form = None;
    model.handle_key(key(KeyCode::Char('e')), std::time::Instant::now());
    let edit = model.ssh_form.as_ref().expect("edit form");
    assert_eq!(edit.original.as_deref(), Some("prod"));
    assert_eq!(edit.auth, SshFormAuthKind::Keep);

    model.ssh_form = None;
    model.requests.clear();
    model.handle_key(key(KeyCode::Char('t')), std::time::Instant::now());
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::SshTest { profile }] if profile == "prod"
    ));

    model.requests.clear();
    model.handle_key(key(KeyCode::Char('d')), std::time::Instant::now());
    assert_eq!(model.ssh_remove_armed.as_deref(), Some("prod"));
    assert!(model.requests.is_empty());
    model.handle_key(key(KeyCode::Char('d')), std::time::Instant::now());
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::SshRemove { profile }] if profile == "prod"
    ));

    model.requests.clear();
    model.handle_key(key(KeyCode::Enter), std::time::Instant::now());
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::SshShellOpen { profile, .. }] if profile == "prod"
    ));
    assert!(model.ssh_terminal.is_some());
}

#[test]
fn ssh_form_masks_and_redacts_password_before_staging() {
    let sentinel = "tui-secret-sentinel-never-render";
    let mut model = live_session();
    let mut form = SshProfileForm::add();
    form.name = "prod".into();
    form.host = "prod.test".into();
    form.user = "deploy".into();
    form.auth = SshFormAuthKind::Password;
    form.focus = 6;
    model.ssh_form = Some(form);
    model.ssh_form_paste(sentinel);

    let form = model.ssh_form.as_ref().expect("password form");
    assert_eq!(form.credential_display(), "•".repeat(sentinel.len()));
    assert!(!format!("{form:?}").contains(sentinel));

    model.handle_key(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        std::time::Instant::now(),
    );
    assert!(matches!(
        model.requests.last(),
        Some(AppRequest::SshProfileSave {
            mutation: SshProfileMutation {
                auth: SshPendingAuth::Password,
                ..
            },
            secret: Some(_),
        })
    ));
    assert!(!format!("{:?}", model.requests.last()).contains(sentinel));
}

#[test]
fn ssh_form_allows_plain_s_in_cwd_without_submitting() {
    let mut model = live_session();
    let mut form = SshProfileForm::add();
    form.name = "prod".into();
    form.host = "prod.test".into();
    form.user = "deploy".into();
    form.credential = "/keys/prod".into();
    form.focus = 7;
    model.ssh_form = Some(form);

    model.handle_key(key(KeyCode::Char('s')), std::time::Instant::now());

    assert_eq!(model.ssh_form.as_ref().expect("cwd form").cwd, "s");
    assert!(model.requests.is_empty());
}

#[test]
fn ssh_form_refuses_port_zero_before_rpc_submission() {
    let mut form = SshProfileForm::add();
    form.name = "prod".into();
    form.host = "prod.test".into();
    form.user = "deploy".into();
    form.port = "0".into();
    form.credential = "/keys/prod".into();

    assert_eq!(
        form.take_request().expect_err("port zero must be refused"),
        "port must be 1..=65535"
    );
}

#[test]
fn ssh_terminal_input_is_redacted_and_ctrl_d_is_typed_eof() {
    let mut model = live_session();
    let mut terminal = SshTerminalPane::opening("prod".into(), model.ssh_terminal_size);
    terminal.shell_id = Some("shell-pty".into());
    model.ssh_terminal = Some(terminal);

    model.handle_key(key(KeyCode::Char('x')), std::time::Instant::now());
    assert!(matches!(
        model.requests.last(),
        Some(AppRequest::SshShellInput { id, input })
            if id == "shell-pty" && input.as_slice() == b"x"
    ));
    assert!(!format!("{:?}", model.requests.last()).contains('x'));

    model.handle_key(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        std::time::Instant::now(),
    );
    assert!(matches!(
        model.requests.last(),
        Some(AppRequest::SshShellEof { id }) if id == "shell-pty"
    ));
}

#[test]
fn opening_ssh_terminal_swallows_paste_until_the_channel_id_arrives() {
    let mut model = live_session();
    model.ssh_terminal = Some(SshTerminalPane::opening(
        "prod".into(),
        model.ssh_terminal_size,
    ));
    let draft_before = model.composer.text().to_owned();

    model.handle(AppEvent::Paste(Pasted::new("not-composer-input".into())));

    assert_eq!(model.composer.text(), draft_before);
    assert!(model.requests.is_empty());
}

#[test]
fn ssh_terminal_splits_large_paste_at_the_wire_input_bound() {
    let mut model = live_session();
    let mut terminal = SshTerminalPane::opening("prod".into(), model.ssh_terminal_size);
    terminal.shell_id = Some("shell-pty".into());
    model.ssh_terminal = Some(terminal);
    let paste = "x".repeat(haider_rpc::SSH_PTY_INPUT_MAX_BYTES + 1);

    model.handle(AppEvent::Paste(Pasted::new(paste)));

    assert_eq!(model.requests.len(), 2);
    let lengths = model
        .requests
        .iter()
        .map(|request| match request {
            AppRequest::SshShellInput { id, input } if id == "shell-pty" => input.as_slice().len(),
            _ => 0,
        })
        .collect::<Vec<_>>();
    assert_eq!(lengths, vec![haider_rpc::SSH_PTY_INPUT_MAX_BYTES, 1]);
    assert!(!format!("{:?}", model.requests).contains('x'));
}

#[test]
fn ssh_test_host_key_mismatch_keeps_its_typed_code() {
    let context = crate::link::CommandContext::of(&crate::live::LiveCommand::SshTest {
        profile: "prod".into(),
    });
    let replies = crate::link::map_response(
        &context,
        haider_rpc::ResponseBody::Error {
            code: "ssh_host_key_changed".into(),
            message: "expected SHA256:old; received SHA256:new".into(),
            retryable: false,
            data: None,
        },
    );
    assert!(matches!(
        replies.as_slice(),
        [crate::live::LiveReply::SshTestFailed { code, .. }]
            if code == "ssh_host_key_changed"
    ));
}
