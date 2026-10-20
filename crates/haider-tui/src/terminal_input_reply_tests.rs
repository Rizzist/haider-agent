#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::app::{AppEvent, AppModel, EffortPicker, InputOwner, RuntimeMode, Screen, ThemePicker};
use crate::live::{LiveDriver, LiveReply};
use haider_protocol::{EventPayload, ids::SessionId};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn snapshot(m: &AppModel, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| drop(render(m, f))).unwrap();
    t.backend()
        .buffer()
        .content
        .chunks(usize::from(w))
        .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn queued_action(action: &str) -> Event {
    match action {
        "submit" => Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        "type" => Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        "paste" => Event::Paste("STALE_PASTE".into()),
        "backspace" => Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        "delete" => Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
        "word-erase" => Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
        "tail-erase" => Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        "left" => Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        "select" => Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)),
        "erase" => Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        _ => unreachable!("unknown keyboard input action: {action}"),
    }
}

#[test]
fn free_text_question_owns_keyboard_while_picker_yields() {
    let mut failures = vec![];
    for (w, h) in [(118, 36), (80, 24)] {
        for child in [false, true] {
            for picker in ["none", "theme", "effort"] {
                let mut m = AppModel::new();
                m.composer.clear();
                let session = SessionId::new("keyboard-session");
                m.upsert_live_session(&session);
                m.open_session(&session);
                m.screen = if child {
                    Screen::Subagent
                } else {
                    Screen::Session
                };
                if child {
                    m.chips
                        .push(super::runtime_pointer_tests::question_chip("child", "seed"));
                    m.view_path = vec!["child".into()];
                }
                if picker == "theme" {
                    m.theme_picker = Some(ThemePicker {
                        selection: 0,
                        prior: m.theme_choice,
                    });
                } else if picker == "effort" {
                    m.effort_picker = Some(EffortPicker::default());
                }
                let mut ask = super::input_typeahead_tests::decision("fresh-text-ask");
                ask.options.clear();
                ask.title = "Visible child question".into();
                if child {
                    let payload = EventPayload::MenuOpened(ask);
                    m.route_raw(&super::input_typeahead_tests::child_envelope(&payload, 0));
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(ask))));
                }
                let frame = snapshot(&m, w, h);
                let owner = m.input_owner();
                let expected = InputOwner::Composer(if child {
                    Screen::Subagent
                } else {
                    Screen::Session
                });
                dispatch_input_at(
                    &mut m,
                    &[],
                    Event::Paste("visible answer".into()),
                    Instant::now() + Duration::from_secs(1),
                );
                println!(
                    "ASK child={child} picker={picker} size={w}x{h} owner={owner:?} visible={} answer={:?} flash={:?}\n{frame}",
                    frame.contains("Visible child question"),
                    m.composer.text(),
                    m.flash
                );
                if owner != expected
                    || m.composer.text() != "visible answer"
                    || !frame.contains("Visible child question")
                {
                    failures.push(format!("child={child} {picker} {w}x{h}"));
                }
                snapshot(&m, w, h);
                dispatch_input_at(
                    &mut m,
                    &[],
                    Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
                    Instant::now() + Duration::from_secs(1),
                );
                assert_eq!(
                    m.composer.text(),
                    "visible answerx",
                    "child={child} picker={picker} size={w}x{h}"
                );
            }
        }
    }
    assert!(
        failures.is_empty(),
        "visible free-text answer owner mismatch: {failures:?}"
    );
}

#[test]
fn external_editor_replies_drop_old_keyboard_input() {
    let mut failures = vec![];
    for (w, h) in [(118, 36), (80, 24)] {
        for operation in ["set", "set-same", "insert", "clear"] {
            for action in [
                "submit",
                "type",
                "paste",
                "erase",
                "backspace",
                "delete",
                "word-erase",
                "tail-erase",
                "left",
                "select",
            ] {
                let mut m = AppModel::new();
                m.mode = RuntimeMode::Live;
                let session = SessionId::new("terminal-input-edit");
                m.upsert_live_session(&session);
                m.open_session(&session);
                m.composer.set_text("OLD_DRAFT");
                m.requests.clear();
                m.outbox.clear();
                snapshot(&m, w, h);
                let old = m.painted_stamp();
                let received_at = Instant::now();
                let op = match operation {
                    "set" => haider_rpc::SurfaceInjectOp::Set {
                        text: "NEW_REMOTE".into(),
                    },
                    "set-same" => haider_rpc::SurfaceInjectOp::Set {
                        text: "OLD_DRAFT".into(),
                    },
                    "insert" => haider_rpc::SurfaceInjectOp::Insert {
                        text: "_REMOTE".into(),
                    },
                    _ => haider_rpc::SurfaceInjectOp::Clear,
                };
                LiveDriver::new("terminal-input")
                    .apply(&mut m, LiveReply::InputInjected { session, op });
                snapshot(&m, w, h);
                let before = m.composer.text().to_owned();
                let event = queued_action(action);
                let accepted = dispatch_queued_input(
                    &mut m,
                    &[],
                    QueuedInput {
                        event,
                        presented: old,
                        received_at,
                    },
                );
                println!(
                    "EDIT {operation}/{action} before={before:?} accepted={accepted} after={:?} requests={:?}",
                    m.composer.text(),
                    m.requests
                );
                assert_eq!(
                    m.composer.text(),
                    before,
                    "{operation}/{action} changed replacement text"
                );
                assert!(
                    !m.requests
                        .iter()
                        .any(|r| matches!(r, crate::app::AppRequest::SubmitText { .. }))
                );
                if accepted {
                    failures.push(format!("{operation}/{action}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "old keyboard input reached the replacement draft: {failures:?}"
    );
}

fn open_profile(
    m: &mut AppModel,
    live: &mut LiveDriver,
    name: &str,
) -> crate::link::CommandContext {
    m.ssh_open = true;
    m.ssh_profiles = vec![haider_rpc::SshProfileWire {
        name: name.into(),
        description: None,
        host: "127.0.0.1".into(),
        port: 22,
        user: "synthetic".into(),
        default_cwd: None,
        host_key: None,
        last_used_ms: None,
        multiplexing: true,
        in_scope: true,
    }];
    m.ssh_cursor = 0;
    m.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert_eq!(m.ssh_terminal.as_ref().unwrap().profile, name);
    let request = m.requests.pop().unwrap();
    let commands = live.handle_request(m, request);
    assert!(
        matches!(&commands[..], [crate::live::LiveCommand::SshShellOpen { profile, .. }] if profile == name)
    );
    crate::link::CommandContext::of(&commands[0])
}
fn apply_response(
    m: &mut AppModel,
    live: &mut LiveDriver,
    context: &crate::link::CommandContext,
    response: haider_rpc::ResponseBody,
) {
    let replies = crate::link::map_response(context, response);
    assert_eq!(replies.len(), 1);
    for reply in replies {
        live.apply(m, reply);
    }
}
fn admitted(id: &str, profile: &str) -> haider_rpc::ShellWire {
    haider_rpc::ShellWire {
        id: id.into(),
        kind: haider_rpc::ShellKindWire::Ssh {
            profile: profile.into(),
        },
        status: haider_rpc::ShellStatusWire::Starting,
        setup_error: None,
        title: "synthetic".into(),
        cwd_or_host: "127.0.0.1".into(),
        created_at_ms: 1,
        last_activity_ms: 1,
        bytes_out: 0,
    }
}
fn admitted_with_status(
    id: &str,
    profile: &str,
    status: haider_rpc::ShellStatusWire,
) -> haider_rpc::ShellWire {
    let mut shell = admitted(id, profile);
    shell.status = status;
    shell
}
#[test]
fn cancelled_remote_connection_replies_preserve_new_attempt() {
    let mut failures = vec![];
    for (w, h) in [(118, 36), (80, 24)] {
        for status in [
            haider_rpc::ShellStatusWire::Starting,
            haider_rpc::ShellStatusWire::Running,
        ] {
            for successor in ["same", "different"] {
                for order in ["old-first", "new-first"] {
                    for old_reply in ["opened", "failed"] {
                        let mut m = AppModel::new();
                        m.mode = RuntimeMode::Live;
                        m.screen = Screen::Session;
                        let mut live = LiveDriver::new("terminal-input");
                        let old_context = open_profile(&mut m, &mut live, "profile-a");
                        m.requests.clear();
                        m.handle(AppEvent::Key(KeyEvent::new(
                            KeyCode::Char('c'),
                            KeyModifiers::CONTROL,
                        )));
                        assert!(m.ssh_terminal.is_none());
                        let next = if successor == "same" {
                            "profile-a"
                        } else {
                            "profile-b"
                        };
                        let new_context = open_profile(&mut m, &mut live, next);
                        m.requests.clear();
                        if order == "new-first" {
                            apply_response(
                                &mut m,
                                &mut live,
                                &new_context,
                                haider_rpc::ResponseBody::SshShellOpen {
                                    shell: admitted_with_status("shell-b", next, status.clone()),
                                },
                            );
                        }
                        let before_old_reply = m.ssh_terminal.clone();
                        if old_reply == "opened" {
                            apply_response(
                                &mut m,
                                &mut live,
                                &old_context,
                                haider_rpc::ResponseBody::SshShellOpen {
                                    shell: admitted_with_status(
                                        "shell-a",
                                        "profile-a",
                                        status.clone(),
                                    ),
                                },
                            );
                        } else {
                            apply_response(
                                &mut m,
                                &mut live,
                                &old_context,
                                haider_rpc::ResponseBody::Error {
                                    code: "ssh_timeout".into(),
                                    message: "stale attempt A".into(),
                                    retryable: false,
                                    data: None,
                                },
                            );
                        }
                        assert_eq!(
                            m.ssh_terminal, before_old_reply,
                            "old reply changed the successor pane"
                        );
                        snapshot(&m, w, h);
                        let owner = m
                            .ssh_terminal
                            .as_ref()
                            .map(|t| (t.profile.clone(), t.shell_id.clone()));
                        let expected = Some((
                            next.into(),
                            if order == "new-first" {
                                Some("shell-b".into())
                            } else {
                                None
                            },
                        ));
                        let stale_closed=old_reply!="opened" || m.requests.iter().any(|r| matches!(r,crate::app::AppRequest::ShellClose{id} if id=="shell-a"));
                        println!(
                            "SSH successor={successor} order={order} old={old_reply} expected={expected:?} actual={owner:?} stale_closed={stale_closed} requests={:?}",
                            m.requests
                        );
                        if owner != expected || !stale_closed {
                            failures.push(format!("{successor}/{order}/{old_reply}"));
                        }
                        if order == "old-first" {
                            apply_response(
                                &mut m,
                                &mut live,
                                &new_context,
                                haider_rpc::ResponseBody::SshShellOpen {
                                    shell: admitted_with_status("shell-b", next, status.clone()),
                                },
                            );
                            if m.ssh_terminal.as_ref().and_then(|t| t.shell_id.as_deref())
                                != Some("shell-b")
                            {
                                failures
                                    .push(format!("successor destroyed: {successor}/{old_reply}"));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "old remote connection reply changed the new attempt: {failures:?}"
    );
}

#[test]
fn external_editor_replies_accept_fresh_input_and_ignore_other_sessions() {
    let mut m = AppModel::new();
    m.mode = RuntimeMode::Live;
    let session = SessionId::new("terminal-input-control");
    m.upsert_live_session(&session);
    m.open_session(&session);
    m.composer.set_text("OLD_DRAFT");
    m.requests.clear();
    m.outbox.clear();
    snapshot(&m, 118, 36);
    let old = m.painted_stamp();
    let mut live = LiveDriver::new("terminal-input");
    live.apply(
        &mut m,
        LiveReply::InputInjected {
            session: SessionId::new("not-active"),
            op: haider_rpc::SurfaceInjectOp::Set {
                text: "IGNORED".into(),
            },
        },
    );
    assert_eq!(m.composer.text(), "OLD_DRAFT");
    assert!(dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            event: Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            presented: old,
            received_at: Instant::now()
        }
    ));
    live.apply(
        &mut m,
        LiveReply::InputInjected {
            session,
            op: haider_rpc::SurfaceInjectOp::Set {
                text: "FRESH_REMOTE".into(),
            },
        },
    );
    snapshot(&m, 118, 36);
    let fresh = m.painted_stamp();
    assert!(dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            event: Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            presented: fresh,
            received_at: Instant::now()
        }
    ));
    assert!(
        m.requests.iter().any(
            |r| matches!(r,crate::app::AppRequest::SubmitText{text,..} if text=="FRESH_REMOTE")
        )
    );
}

#[test]
fn authoring_replies_drop_keyboard_input_for_old_draft() {
    use haider_protocol::loom::{
        LoomAuthorConfirmed, LoomAuthorDraft, LoomAuthorKind, LoomRegistration,
    };
    let mut failures = vec![];
    for (w, h) in [(118, 36), (80, 24)] {
        for kind in [LoomAuthorKind::AgentType, LoomAuthorKind::Workflow] {
            for reply in ["draft", "confirmed"] {
                for action in [
                    "type",
                    "paste",
                    "erase",
                    "backspace",
                    "delete",
                    "word-erase",
                    "tail-erase",
                    "left",
                    "select",
                ] {
                    let mut m = AppModel::new();
                    m.mode = RuntimeMode::Live;
                    m.screen = Screen::Loom;
                    m.daemon_features = [
                        haider_rpc::FEATURE_LOOM_V1.to_owned(),
                        haider_rpc::FEATURE_LOOM_AUTHORING_V1.to_owned(),
                    ]
                    .into_iter()
                    .collect();
                    m.loom_authoring = Some(crate::app::LoomAuthoringState {
                        generation: 7,
                        kind,
                        authoring_id: Some("author-seven".into()),
                        revision: Some(1),
                        errors: vec![],
                        confirmed: None,
                        pending: true,
                        validated: false,
                        preview_digest: None,
                        install_job: None,
                    });
                    m.composer.set_text("pending editor bytes");
                    snapshot(&m, w, h);
                    let old = m.painted_stamp();
                    let received_at = Instant::now();
                    let outcome = if reply == "draft" {
                        LiveReply::LoomAuthorDrafted {
                            generation: 7,
                            epoch: 0,
                            draft: LoomAuthorDraft {
                                authoring_id: "author-seven".into(),
                                revision: 2,
                                kind,
                                text: "returned editor bytes".into(),
                                errors: vec![],
                            },
                        }
                    } else {
                        LiveReply::LoomAuthorConfirmed {
                            generation: 7,
                            epoch: 0,
                            errors: vec![],
                            confirmed: Some(LoomAuthorConfirmed {
                                authoring_id: "author-seven".into(),
                                kind,
                                canonical_text: "returned editor bytes".into(),
                                registration: LoomRegistration {
                                    id: "fixture-seven".into(),
                                    rev: 2,
                                    digest: "fixture-digest".into(),
                                    updated: true,
                                },
                                execution_digest: "fixture-execution".into(),
                                install_job_id: None,
                            }),
                        }
                    };
                    LiveDriver::new("terminal-input").apply(&mut m, outcome);
                    assert_eq!(m.composer.text(), "returned editor bytes");
                    snapshot(&m, w, h);
                    let event = queued_action(action);
                    let accepted = dispatch_queued_input(
                        &mut m,
                        &[],
                        QueuedInput {
                            event,
                            presented: old,
                            received_at,
                        },
                    );
                    println!(
                        "AUTHORING kind={kind:?} reply={reply} action={action} accepted={accepted} after={:?}",
                        m.composer.text()
                    );
                    if accepted {
                        failures.push(format!("{kind:?}/{reply}/{action}"));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "pending editor input reached returned text: {failures:?}"
    );
}

#[test]
fn current_remote_connection_failure_closes_its_own_prompt() {
    let mut m = AppModel::new();
    m.mode = RuntimeMode::Live;
    let mut live = LiveDriver::new("terminal-input");
    let context = open_profile(&mut m, &mut live, "profile-a");
    apply_response(
        &mut m,
        &mut live,
        &context,
        haider_rpc::ResponseBody::Error {
            code: "ssh_timeout".into(),
            message: "current attempt failed".into(),
            retryable: false,
            data: None,
        },
    );
    assert!(m.ssh_terminal.is_none());
    assert!(
        m.flash
            .as_deref()
            .unwrap()
            .contains("current attempt failed")
    );
}

#[test]
fn remote_connection_success_without_a_request_closes_the_orphan() {
    let mut m = AppModel::new();
    m.mode = RuntimeMode::Live;
    let mut live = LiveDriver::new("terminal-input");
    let current = open_profile(&mut m, &mut live, "profile-a");
    let before = m.ssh_terminal.clone();
    let context = crate::link::CommandContext::of(&crate::live::LiveCommand::ShellList);
    apply_response(
        &mut m,
        &mut live,
        &context,
        haider_rpc::ResponseBody::SshShellOpen {
            shell: admitted("unowned-shell", "profile-a"),
        },
    );
    assert_eq!(m.ssh_terminal, before);
    assert!(
        matches!(m.requests.as_slice(), [crate::app::AppRequest::ShellClose { id }] if id == "unowned-shell")
    );
    apply_response(
        &mut m,
        &mut live,
        &current,
        haider_rpc::ResponseBody::SshShellOpen {
            shell: admitted("owned-shell", "profile-a"),
        },
    );
    assert_eq!(
        m.ssh_terminal.as_ref().unwrap().shell_id.as_deref(),
        Some("owned-shell")
    );
}

#[test]
fn old_remote_connection_replies_preserve_output_from_the_new_shell() {
    for (w, h) in [(118, 36), (80, 24)] {
        for success in [false, true] {
            let mut m = AppModel::new();
            m.mode = RuntimeMode::Live;
            let mut live = LiveDriver::new("terminal-input");
            let old = open_profile(&mut m, &mut live, "profile-a");
            m.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )));
            let current = open_profile(&mut m, &mut live, "profile-a");
            apply_response(
                &mut m,
                &mut live,
                &current,
                haider_rpc::ResponseBody::SshShellOpen {
                    shell: admitted("new-shell", "profile-a"),
                },
            );
            m.apply_ssh_shell_output("new-shell", b"new shell output");
            let before = m.ssh_terminal.clone();
            let response = if success {
                haider_rpc::ResponseBody::SshShellOpen {
                    shell: admitted("old-shell", "profile-a"),
                }
            } else {
                haider_rpc::ResponseBody::Error {
                    code: "ssh_timeout".into(),
                    message: "old attempt".into(),
                    retryable: false,
                    data: None,
                }
            };
            apply_response(&mut m, &mut live, &old, response);
            assert_eq!(m.ssh_terminal, before);
            assert!(!m.ssh_terminal.as_ref().unwrap().connecting);
            assert!(snapshot(&m, w, h).contains("new shell output"));
        }
    }
}

#[test]
fn empty_external_editor_writes_drop_old_keyboard_input() {
    for (w, h) in [(118, 36), (80, 24)] {
        for op in [
            haider_rpc::SurfaceInjectOp::Set {
                text: String::new(),
            },
            haider_rpc::SurfaceInjectOp::Insert {
                text: String::new(),
            },
            haider_rpc::SurfaceInjectOp::Clear,
        ] {
            let mut m = AppModel::new();
            m.mode = RuntimeMode::Live;
            let session = SessionId::new("empty-external-draft");
            m.upsert_live_session(&session);
            m.open_session(&session);
            m.composer.clear();
            snapshot(&m, w, h);
            let old = m.painted_stamp();
            let received_at = Instant::now();
            LiveDriver::new("terminal-input")
                .apply(&mut m, LiveReply::InputInjected { session, op });
            snapshot(&m, w, h);
            assert!(!dispatch_queued_input(
                &mut m,
                &[],
                QueuedInput {
                    event: queued_action("paste"),
                    presented: old,
                    received_at
                }
            ));
            assert!(m.composer.is_empty());
        }
    }
}

#[test]
fn keyboard_input_for_old_connection_cannot_cancel_a_new_attempt() {
    for (w, h) in [(118, 36), (80, 24)] {
        let mut m = AppModel::new();
        m.mode = RuntimeMode::Live;
        let mut live = LiveDriver::new("terminal-input");
        open_profile(&mut m, &mut live, "profile-a");
        snapshot(&m, w, h);
        let old = m.painted_stamp();
        let received_at = Instant::now();
        let cancel = || Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        m.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        open_profile(&mut m, &mut live, "profile-a");
        snapshot(&m, w, h);
        assert!(!dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                event: cancel(),
                presented: old,
                received_at
            }
        ));
        assert!(m.ssh_terminal.is_some());
        snapshot(&m, w, h);
        let fresh = m.painted_stamp();
        assert!(dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                event: cancel(),
                presented: fresh,
                received_at: Instant::now()
            }
        ));
        assert!(m.ssh_terminal.is_none());
    }
}

fn result_order_model() -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Session;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SSH_TYPED_SETUP_ERRORS_V1.into());
    model
}

fn result_order_list(m: &mut AppModel, live: &mut LiveDriver, shells: Vec<haider_rpc::ShellWire>) {
    live.apply(m, LiveReply::ShellListed { shells });
}

#[test]
fn remote_result_order_reviewer_corpus() {
    let mut failures = Vec::new();
    for (w, h) in [(118, 36), (80, 24)] {
        for state in [
            "starting",
            "running",
            "closed",
            "exited",
            "ssh_timeout",
            "ssh_connection_failed",
            "ssh_authentication_failed",
            "ssh_host_key_changed",
        ] {
            for order in ["state-first", "reply-first"] {
                let mut m = result_order_model();
                m.mode = RuntimeMode::Live;
                m.screen = Screen::Session;
                let mut live = LiveDriver::new("terminal-result-order");
                let context = open_profile(&mut m, &mut live, "profile-a");
                m.requests.clear();
                let starting = admitted("current-shell", "profile-a");
                let mut latest = starting.clone();
                latest.last_activity_ms = 2;
                latest.status = match state {
                    "starting" => haider_rpc::ShellStatusWire::Starting,
                    "running" => haider_rpc::ShellStatusWire::Running,
                    "closed" => haider_rpc::ShellStatusWire::Closed,
                    "exited" => haider_rpc::ShellStatusWire::Exited { code: Some(17) },
                    _ => haider_rpc::ShellStatusWire::Exited { code: None },
                };
                if state.starts_with("ssh_") {
                    latest.setup_error = Some(haider_rpc::ShellSetupErrorWire {
                        code: state.into(),
                        message: "synthetic terminal setup result".into(),
                        retryable: false,
                    });
                }
                if order == "state-first" {
                    live.apply(
                        &mut m,
                        LiveReply::ShellChanged {
                            shell: latest.clone(),
                        },
                    );
                }
                apply_response(
                    &mut m,
                    &mut live,
                    &context,
                    haider_rpc::ResponseBody::SshShellOpen { shell: starting },
                );
                if order == "reply-first" {
                    live.apply(&mut m, LiveReply::ShellChanged { shell: latest });
                }
                let before_key = m.ssh_terminal.clone();
                let flash = m.flash.clone();
                let frame = snapshot(&m, w, h);
                let pass = if state == "starting" {
                    m.requests.clear();
                    m.handle(AppEvent::Key(KeyEvent::new(
                        KeyCode::Char('c'),
                        KeyModifiers::CONTROL,
                    )));
                    before_key.as_ref().is_some_and(|pane| pane.connecting && pane.shell_id.as_deref()==Some("current-shell"))
                        && m.requests.iter().any(|r| matches!(r, crate::app::AppRequest::ShellClose { id } if id == "current-shell"))
                } else if state == "running" {
                    m.requests.clear();
                    m.handle(AppEvent::Key(KeyEvent::new(
                        KeyCode::Char('c'),
                        KeyModifiers::CONTROL,
                    )));
                    before_key.as_ref().is_some_and(|pane| !pane.connecting)
                        && m.requests.iter().any(|r| matches!(r, crate::app::AppRequest::SshShellInput { id, .. } if id == "current-shell"))
                        && !m.requests.iter().any(|r| matches!(r, crate::app::AppRequest::ShellClose { .. }))
                } else {
                    before_key.is_none()
                        && (!state.starts_with("ssh_")
                            || flash.as_ref().is_some_and(|v| v.contains(state)))
                };
                println!(
                    "ORDER {}",
                    serde_json::json!({"width":w,"height":h,"state":state,"order":order,"passed":pass,"pane_present":before_key.is_some(),"connecting":before_key.as_ref().map(|p|p.connecting),"flash":flash,"cached_status":m.shells.first().map(|s|format!("{:?}",s.status))})
                );
                if !pass {
                    failures.push(format!("{w}x{h}/{state}/{order}"));
                    println!("{frame}");
                }
            }
        }
        for order in ["output-first", "reply-first"] {
            let mut m = result_order_model();
            m.mode = RuntimeMode::Live;
            m.screen = Screen::Session;
            let mut live = LiveDriver::new("terminal-result-order");
            let context = open_profile(&mut m, &mut live, "profile-a");
            if order == "output-first" {
                m.apply_ssh_shell_output("current-shell", b"EARLY_TERMINAL_OUTPUT\r\n");
            }
            apply_response(
                &mut m,
                &mut live,
                &context,
                haider_rpc::ResponseBody::SshShellOpen {
                    shell: admitted("current-shell", "profile-a"),
                },
            );
            if order == "reply-first" {
                m.apply_ssh_shell_output("current-shell", b"EARLY_TERMINAL_OUTPUT\r\n");
            }
            let frame = snapshot(&m, w, h);
            let pass = frame.contains("EARLY_TERMINAL_OUTPUT")
                && m.ssh_terminal.as_ref().is_some_and(|p| !p.connecting);
            println!(
                "ORDER {}",
                serde_json::json!({"width":w,"height":h,"state":"terminal-output","order":order,"passed":pass,"visible_output":frame.contains("EARLY_TERMINAL_OUTPUT"),"connecting":m.ssh_terminal.as_ref().map(|p|p.connecting)})
            );
            if !pass {
                failures.push(format!("{w}x{h}/output/{order}"));
                println!("{frame}");
            }
        }
    }
    assert!(
        failures.is_empty(),
        "terminal state/output was lost before its connection reply: {failures:?}"
    );
}

fn setup_result(code: &str) -> haider_rpc::ShellWire {
    let mut shell = admitted_with_status(
        "current-shell",
        "profile-a",
        haider_rpc::ShellStatusWire::Exited { code: None },
    );
    shell.setup_error = Some(haider_rpc::ShellSetupErrorWire {
        code: code.into(),
        message: "synthetic setup result".into(),
        retryable: false,
    });
    shell
}

#[test]
fn remote_result_order_all_setup_results_survive_lists_and_equal_timestamps() {
    for (w, h) in [(118, 36), (80, 24)] {
        for code in [
            "ssh_timeout",
            "ssh_connection_failed",
            "ssh_authentication_failed",
            "ssh_host_key_changed",
            "ssh_channel_quota",
            "ssh_agent_unavailable",
            "ssh_key_invalid",
            "ssh_vault_error",
            "ssh_profile_store_corrupt",
            "ssh_profile_not_found",
            "ssh_profile_invalid",
            "ssh_channel_closed",
            "legacy_setup_text",
        ] {
            for list in ["none", "empty", "older", "same-ms"] {
                let mut m = result_order_model();
                if code == "legacy_setup_text" {
                    m.daemon_features
                        .remove(haider_rpc::FEATURE_SSH_TYPED_SETUP_ERRORS_V1);
                }
                let mut live = LiveDriver::new("result-order");
                let context = open_profile(&mut m, &mut live, "profile-a");
                let mut result = setup_result(code);
                if code == "legacy_setup_text" {
                    result.setup_error = None;
                    m.apply_ssh_shell_output(
                        "current-shell",
                        b"legacy_setup_text: setup failed\r\n",
                    );
                }
                live.apply(&mut m, LiveReply::ShellChanged { shell: result });
                if list != "none" {
                    let mut stale = admitted("current-shell", "profile-a");
                    stale.last_activity_ms = u64::from(list == "same-ms");
                    result_order_list(
                        &mut m,
                        &mut live,
                        if list == "empty" { vec![] } else { vec![stale] },
                    );
                }
                apply_response(
                    &mut m,
                    &mut live,
                    &context,
                    haider_rpc::ResponseBody::SshShellOpen {
                        shell: admitted("current-shell", "profile-a"),
                    },
                );
                assert!(m.ssh_terminal.is_none(), "{code}/{list}/{w}x{h}");
                assert!(m.flash.as_deref().unwrap().contains(code), "{code}/{list}");
                // Narrow frames may clip the message, but must leave the opening pane.
                assert!(!snapshot(&m, w, h).contains("opening remote terminal"));
            }
        }
    }
}

#[test]
fn remote_result_order_running_survives_lists_and_keeps_command_input() {
    for (w, h) in [(118, 36), (80, 24)] {
        for list in ["empty", "older", "same-ms"] {
            for with_output in [false, true] {
                let mut m = result_order_model();
                let mut live = LiveDriver::new("result-order");
                let context = open_profile(&mut m, &mut live, "profile-a");
                m.apply_shell_event(admitted_with_status(
                    "current-shell",
                    "profile-a",
                    haider_rpc::ShellStatusWire::Running,
                ));
                if with_output {
                    m.apply_ssh_shell_output("current-shell", b"EARLY_OUTPUT\r\n");
                }
                let mut stale = admitted("current-shell", "profile-a");
                stale.last_activity_ms = u64::from(list == "same-ms");
                result_order_list(
                    &mut m,
                    &mut live,
                    if list == "empty" { vec![] } else { vec![stale] },
                );
                apply_response(
                    &mut m,
                    &mut live,
                    &context,
                    haider_rpc::ResponseBody::SshShellOpen {
                        shell: admitted("current-shell", "profile-a"),
                    },
                );
                assert!(!m.ssh_terminal.as_ref().unwrap().connecting, "{list}");
                assert_eq!(snapshot(&m, w, h).contains("EARLY_OUTPUT"), with_output);
                m.requests.clear();
                m.handle(AppEvent::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )));
                assert!(
                    matches!(m.requests.as_slice(), [crate::app::AppRequest::SshShellInput { id, input }] if id == "current-shell" && input.as_slice() == b"\x03")
                );
            }
        }
    }
}

#[test]
fn remote_result_order_fresh_list_recovers_dropped_status() {
    for (w, h) in [(118, 36), (80, 24)] {
        for state in ["running", "failed", "exited", "closed"] {
            for registry_open in [false, true] {
                let mut m = result_order_model();
                let mut live = LiveDriver::new("result-order");
                let context = open_profile(&mut m, &mut live, "profile-a");
                apply_response(
                    &mut m,
                    &mut live,
                    &context,
                    haider_rpc::ResponseBody::SshShellOpen {
                        shell: admitted("current-shell", "profile-a"),
                    },
                );
                m.shells_open = registry_open;
                if registry_open {
                    m.flash = Some("· reading terminal registry…".into());
                    result_order_list(&mut m, &mut live, vec![]);
                    assert!(m.flash.is_none(), "registry loading notice was not settled");
                }
                let result = match state {
                    "failed" => setup_result("ssh_timeout"),
                    "running" => admitted_with_status(
                        "current-shell",
                        "profile-a",
                        haider_rpc::ShellStatusWire::Running,
                    ),
                    "exited" => admitted_with_status(
                        "current-shell",
                        "profile-a",
                        haider_rpc::ShellStatusWire::Exited { code: Some(0) },
                    ),
                    _ => admitted_with_status(
                        "current-shell",
                        "profile-a",
                        haider_rpc::ShellStatusWire::Closed,
                    ),
                };
                result_order_list(&mut m, &mut live, vec![result]);
                if state == "running" {
                    assert!(!m.ssh_terminal.as_ref().unwrap().connecting);
                    result_order_list(
                        &mut m,
                        &mut live,
                        vec![admitted("current-shell", "profile-a")],
                    );
                    assert!(!m.ssh_terminal.as_ref().unwrap().connecting);
                } else {
                    assert!(m.ssh_terminal.is_none(), "{state}/{w}x{h}");
                    assert!(m.flash.as_deref().unwrap().contains(match state {
                        "failed" => "ssh_timeout",
                        "exited" => "exited 0",
                        _ => "closed",
                    }));
                }
                assert!(!snapshot(&m, w, h).contains("opening remote terminal"));
                let notice = m.flash.clone();
                result_order_list(&mut m, &mut live, vec![]);
                assert_eq!(m.flash, notice, "background list erased {state} result");
            }
        }
    }
}

#[test]
fn remote_result_order_ordinary_output_keeps_the_command_exit_code() {
    for (w, h) in [(118, 36), (80, 24)] {
        for running in ["early", "dropped", "reply-first"] {
            for typed in [false, true] {
                for code in [Some(0), Some(17), None] {
                    let mut m = result_order_model();
                    if !typed && code.is_none() && running == "dropped" {
                        // A text-only legacy peer cannot distinguish this completion
                        // from its uncoded setup failures without observing Running.
                        continue;
                    }
                    if !typed {
                        m.daemon_features
                            .remove(haider_rpc::FEATURE_SSH_TYPED_SETUP_ERRORS_V1);
                    }
                    let mut live = LiveDriver::new("result-order");
                    let context = open_profile(&mut m, &mut live, "profile-a");
                    let result = admitted_with_status(
                        "current-shell",
                        "profile-a",
                        haider_rpc::ShellStatusWire::Running,
                    );
                    if running == "early" {
                        m.apply_shell_event(result.clone());
                    }
                    apply_response(
                        &mut m,
                        &mut live,
                        &context,
                        haider_rpc::ResponseBody::SshShellOpen {
                            shell: admitted("current-shell", "profile-a"),
                        },
                    );
                    if running == "reply-first" {
                        m.apply_shell_event(result);
                    }
                    m.apply_ssh_shell_output("current-shell", b"hello\r\n");
                    assert!(snapshot(&m, w, h).contains("hello"));
                    assert!(!m.ssh_terminal.as_ref().unwrap().connecting);
                    m.apply_shell_event(admitted_with_status(
                        "current-shell",
                        "profile-a",
                        haider_rpc::ShellStatusWire::Exited { code },
                    ));
                    assert!(m.ssh_terminal.is_none());
                    assert_eq!(
                        m.flash,
                        Some(code.map_or_else(
                            || "· remote terminal exited".into(),
                            |code| format!("· remote terminal exited {code}"),
                        )),
                        "typed={typed}/{running}/{w}x{h}"
                    );
                }
            }
        }
    }
}

#[test]
fn remote_result_order_only_the_admitted_shell_can_settle_the_attempt() {
    for same_profile in [false, true] {
        let mut m = result_order_model();
        let mut live = LiveDriver::new("result-order");
        let context = open_profile(&mut m, &mut live, "profile-a");
        let mut other = setup_result("ssh_timeout");
        if same_profile {
            other.id = "old-shell".into();
        } else {
            other.kind = haider_rpc::ShellKindWire::Ssh {
                profile: "profile-b".into(),
            };
        }
        m.apply_shell_event(other.clone());
        result_order_list(&mut m, &mut live, vec![other]);
        m.apply_ssh_shell_output("old-shell", b"unrelated output");
        apply_response(
            &mut m,
            &mut live,
            &context,
            haider_rpc::ResponseBody::SshShellOpen {
                shell: admitted("current-shell", "profile-a"),
            },
        );
        assert!(m.ssh_terminal.as_ref().unwrap().connecting);
        m.apply_ssh_shell_output("old-shell", b"unrelated output");
        assert!(!snapshot(&m, 118, 36).contains("unrelated output"));
    }
}

#[test]
fn remote_result_order_typed_result_enriches_a_plain_completion_snapshot() {
    let mut m = result_order_model();
    let mut live = LiveDriver::new("result-order");
    let context = open_profile(&mut m, &mut live, "profile-a");
    result_order_list(
        &mut m,
        &mut live,
        vec![admitted_with_status(
            "current-shell",
            "profile-a",
            haider_rpc::ShellStatusWire::Exited { code: None },
        )],
    );
    m.apply_shell_event(setup_result("ssh_timeout"));
    apply_response(
        &mut m,
        &mut live,
        &context,
        haider_rpc::ResponseBody::SshShellOpen {
            shell: admitted("current-shell", "profile-a"),
        },
    );
    assert!(m.ssh_terminal.is_none());
    assert!(m.flash.as_deref().unwrap().contains("ssh_timeout"));
}
