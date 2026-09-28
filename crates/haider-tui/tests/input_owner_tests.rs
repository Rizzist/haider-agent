//! Production-dispatch regressions for the single visible input owner.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::{
    EventPayload,
    ids::{AgentId, EffectId, MenuId, SessionId},
    menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope},
    state::HarnessStatus,
};
use haider_tui::{
    app::{
        AccountAddKind, AppEvent, AppModel, ChipModel, ChipQuestion, EffortPicker, Hit, InputOwner,
        LoginCard, PromptBacktrack, RuntimeMode, Screen, ThemePicker, ToolDetailView,
        TranscriptSearch,
    },
    live::{LiveDriver, LiveReply},
    runtime::dispatch_input,
    script::{ChipDisplayState, ChipSeed},
    talk::{CommitIntent, TalkEvent, TalkPhase},
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::Rect,
};

const DRAFT: &str = "KEEP_THIS_DRAFT words";

fn session() -> SessionId {
    SessionId::new("input-owner-test")
}

fn model() -> AppModel {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&session());
    model.open_session(&session());
    model.composer.set_text(DRAFT);
    model.requests.clear();
    model.outbox.clear();
    model
}

fn permission() -> Menu {
    Menu {
        id: MenuId::new("input-owner-permission"),
        kind: MenuKind::Permission {
            effect_summary: "synthetic".into(),
            file_review: None,
        },
        title: "Allow?".into(),
        body: vec![],
        options: vec![
            MenuOption {
                key: "allow".into(),
                label: "Allow once".into(),
                detail: None,
                decision: Some(DecisionKind::AllowOnce),
            },
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: None,
                decision: Some(DecisionKind::RejectOnce),
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "test".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn review_menu() -> Menu {
    use haider_protocol::file_review::{
        FileDiffHunk, FileDiffLine, FileDiffLineKind, FileReview, FileReviewOperation,
    };
    let mut menu = permission();
    if let MenuKind::Permission { file_review, .. } = &mut menu.kind {
        *file_review = Some(FileReview {
            effect: EffectId::new("synthetic-review"),
            path: "f".into(),
            operation: FileReviewOperation::Edit,
            old_digest: None,
            new_digest: "blake3:synthetic".into(),
            added: 1,
            removed: 0,
            hunks: vec![FileDiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![FileDiffLine {
                    kind: FileDiffLineKind::Context,
                    old_line: Some(1),
                    new_line: Some(1),
                    text: "line".into(),
                }],
            }],
            truncated: false,
        });
    }
    menu
}

fn arm(model: &mut AppModel, mode: &str) {
    match mode {
        "detail" => model.tool_detail = Some(ToolDetailView::default()),
        "help" => model.help_open = true,
        "search" => model.transcript_search = Some(TranscriptSearch::default()),
        "inbox" => model.inbox_open = true,
        "shells" => model.shells_open = true,
        "ssh" => model.ssh_open = true,
        "ssh-form" => {
            model.ssh_open = true;
            key(model, KeyCode::Char('a'), KeyModifiers::NONE);
        }
        "ssh-terminal" => {
            model.ssh_open = true;
            model.ssh_profiles.push(haider_rpc::SshProfileWire {
                name: "fixture".into(),
                description: None,
                host: "127.0.0.1".into(),
                port: 22,
                user: "fixture".into(),
                default_cwd: None,
                host_key: None,
                last_used_ms: None,
                multiplexing: true,
                in_scope: true,
            });
            key(model, KeyCode::Enter, KeyModifiers::NONE);
        }
        "monitors" => model.monitors_open = true,
        "lockdown" => model.lockdown_overlay = true,
        "model" => model.open_model_picker(String::new()),
        "theme" => {
            model.theme_picker = Some(ThemePicker {
                selection: 0,
                prior: model.theme_choice,
            });
        }
        "effort" => model.effort_picker = Some(EffortPicker::default()),
        "history" => model.backtrack = Some(PromptBacktrack { selection: 0 }),
        "permission" => model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            permission(),
        )))),
        "plan-doc" => {
            let mut menu = permission();
            menu.origin = "plan".into();
            model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
        }
        "file-review-doc" => model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            review_menu(),
        )))),
        "login" => model.login = Some(LoginCard::new("fake".into(), "alias".into(), 1)),
        "accounts-replace" => {
            model.screen = Screen::Accounts;
            model.login = Some(LoginCard::new("fake".into(), "alias".into(), 1));
            model.accounts.pending_replace = Some(AccountAddKind::Custom);
            model.accounts.message = Some("Replace this form?".into());
        }
        "talk-setup" => {
            model.talk_setup = Some(haider_tui::talk::TalkSetupCard::new(
                model.talk_config.clone(),
                false,
            ));
        }
        "custom-form" => {
            model.screen = Screen::Providers;
            model
                .daemon_features
                .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.into());
            key(model, KeyCode::Char('o'), KeyModifiers::NONE);
        }
        "fleet-detail" => {
            for index in 0..4 {
                model.chips.push(ChipModel::from_seed(ChipSeed {
                    agent: format!("child-{index}"),
                    parent: None,
                    ros: None,
                    callsign: format!("Child {index}"),
                    hon: "(c)",
                    full: format!("Child agent {index}"),
                    name: "test".into(),
                    model: "fake".into(),
                    device: "local".into(),
                    state: ChipDisplayState::Running,
                    tokens: 0,
                    prefill: Vec::new(),
                }));
            }
            model.fleet.snapshot = Some(haider_tui::fleet::snapshot_from_chips(
                &model.chips,
                &session(),
                0,
            ));
            model.screen = Screen::Fleet;
            model.fleet.detail = Some(AgentId::new("child-3"));
        }
        "fleet" => model.screen = Screen::Fleet,
        "sessions" => model.screen = Screen::Sessions,
        "accounts" => model.screen = Screen::Accounts,
        "providers" => model.screen = Screen::Providers,
        "usage" => model.screen = Screen::Usage,
        "graph" => model.screen = Screen::Graph,
        "tools" => model.screen = Screen::Tools,
        "tree" => model.screen = Screen::Tree,
        "hooks" => model.screen = Screen::Hooks,
        "boot" => model.screen = Screen::Boot,
        "session" => {}
        "launcher" => model.screen = Screen::Launcher,
        "subagent" => model.screen = Screen::Subagent,
        "subagent-question" => {
            let mut chip = ChipModel::from_seed(ChipSeed {
                agent: "child".into(),
                parent: None,
                ros: None,
                callsign: "Child".into(),
                hon: "(c)",
                full: "Child agent".into(),
                name: "test".into(),
                model: "fake".into(),
                device: "local".into(),
                state: ChipDisplayState::InputRequired,
                tokens: 0,
                prefill: Vec::new(),
            });
            chip.question = Some(ChipQuestion {
                recovery: false,
                text: "Choose?".into(),
                options: vec!["Allow once".into(), "Deny".into()],
                resolved: false,
            });
            chip.transcript
                .apply(&EventPayload::MenuOpened(permission()));
            model.chips.push(chip);
            model.screen = Screen::Subagent;
            model.view_path = vec!["child".into()];
        }
        "aura" => model.screen = Screen::Aura,
        "loom" => model.screen = Screen::Loom,
        _ => panic!("unknown owner: {mode}"),
    }
    model.requests.clear();
    model.outbox.clear();
}

fn key(model: &mut AppModel, code: KeyCode, modifiers: KeyModifiers) {
    model.handle(AppEvent::Key(KeyEvent::new(code, modifiers)));
}

fn paste(model: &mut AppModel, text: &str) {
    dispatch_input(model, &[], Event::Paste(text.into()));
}

fn stale_pointer(model: &mut AppModel, hit: Hit, kind: MouseEventKind) {
    dispatch_input(
        model,
        &[(Rect::new(0, 0, 10, 1), hit)],
        Event::Mouse(MouseEvent {
            kind,
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
    );
}

#[test]
fn a1_a2_a3_readline_chords_never_edit_a_parked_draft() {
    for mode in [
        "search",
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "model",
        "theme",
        "effort",
        "history",
        "help",
        "lockdown",
        "fleet",
        "fleet-detail",
        "sessions",
        "accounts",
        "providers",
        "usage",
        "graph",
        "tools",
        "tree",
        "hooks",
        "boot",
    ] {
        for chord in ['u', 'w', 'k', 'a', 'e'] {
            let mut model = model();
            model.composer.set_cursor(5);
            arm(&mut model, mode);
            let before = model.composer.cursor();
            key(&mut model, KeyCode::Char(chord), KeyModifiers::CONTROL);
            assert_eq!(model.composer.text(), DRAFT, "{mode} Ctrl+{chord}");
            assert_eq!(model.composer.cursor(), before, "{mode} Ctrl+{chord}");
        }
    }
}

#[test]
fn a5_covered_shortcuts_and_a6_fleet_detail_leave_their_background_alone() {
    for mode in [
        "help",
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "model",
        "theme",
        "effort",
        "history",
        "permission",
    ] {
        for chord in ['f', 'o', 'g'] {
            let mut model = model();
            arm(&mut model, mode);
            let token_panel = model.token_panel;
            key(&mut model, KeyCode::Char(chord), KeyModifiers::CONTROL);
            assert_eq!(model.token_panel, token_panel, "{mode} Ctrl+{chord}");
            assert!(model.transcript_search.is_none() || mode == "search");
            assert!(model.tool_detail.is_none());
        }
    }
    for code in [
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Char('j'),
        KeyCode::Char('k'),
    ] {
        let mut model = model();
        arm(&mut model, "fleet-detail");
        model.fleet.sel = 3;
        key(&mut model, code, KeyModifiers::NONE);
        assert_eq!(model.fleet.sel, 3, "fleet member detail {code:?}");
    }
}

#[test]
fn a4_custom_provider_form_owns_keys_over_a_retained_draft() {
    let mut model = model();
    model.screen = Screen::Providers;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.into());
    key(&mut model, KeyCode::Char('o'), KeyModifiers::NONE);
    assert!(model.custom_add.is_some());
    model.composer.move_left(true);
    let selected = model.composer.selected_text().unwrap().to_owned();
    for chord in ['u', 'w', 'k'] {
        key(&mut model, KeyCode::Char(chord), KeyModifiers::CONTROL);
    }
    assert_eq!(model.composer.text(), DRAFT);
    key(&mut model, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(model.composer.text(), DRAFT);
    assert_eq!(model.composer.selected_text(), Some(selected.as_str()));
}

#[test]
fn a_modal_over_accounts_cannot_page_the_covered_roster() {
    let mut model = model();
    model.screen = Screen::Accounts;
    model.accounts.scroll_max.set(80);
    model.accounts.scroll.set(8);
    arm(&mut model, "detail");
    assert_eq!(model.input_owner(), InputOwner::ToolDetail);
    key(&mut model, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(model.accounts.scroll.get(), 8);
}

#[test]
fn b1_b2_search_fields_receive_paste_without_touching_the_draft() {
    for mode in ["search", "model"] {
        let wide = "界".repeat(160);
        for text in [
            "needle",
            "first\nsecond",
            "one\ntwo\nthree\nfour",
            wide.as_str(),
        ] {
            let mut model = model();
            arm(&mut model, mode);
            paste(&mut model, text);
            assert_eq!(model.composer.text(), DRAFT, "{mode}");
            let query = if mode == "search" {
                &model.transcript_search.as_ref().unwrap().query
            } else {
                &model.model_picker.as_ref().unwrap().query
            };
            assert!(query.contains(text), "{mode}");
        }
    }
}

#[test]
fn b8_clipboard_response_uses_the_owner_at_delivery_time() {
    let mut model = model();
    key(&mut model, KeyCode::Char('v'), KeyModifiers::CONTROL);
    arm(&mut model, "search");
    paste(&mut model, "IN_FLIGHT");
    assert_eq!(model.composer.text(), DRAFT);
    assert_eq!(model.transcript_search.as_ref().unwrap().query, "IN_FLIGHT");

    let mut model = self::model();
    arm(&mut model, "detail");
    key(&mut model, KeyCode::Char('v'), KeyModifiers::CONTROL);
    model.close_tool_detail();
    paste(&mut model, "AFTER_CLOSE");
    assert!(model.composer.text().contains("AFTER_CLOSE"));
}

#[test]
fn b3_b4_b5_paste_is_discarded_by_read_only_owners() {
    for mode in [
        "detail",
        "help",
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "lockdown",
        "theme",
        "effort",
        "history",
        "permission",
        "fleet",
        "fleet-detail",
        "sessions",
        "accounts",
        "providers",
        "usage",
        "graph",
        "tools",
        "tree",
        "hooks",
        "boot",
    ] {
        let large = "P".repeat(400);
        for text in [
            "PASTE",
            "line 1\nline 2",
            "one\ntwo\nthree\nfour",
            large.as_str(),
        ] {
            let mut model = model();
            arm(&mut model, mode);
            paste(&mut model, text);
            assert_eq!(model.composer.text(), DRAFT, "{mode} paste");
        }
    }
}

#[test]
fn b7_paste_under_a_card_does_not_realize_a_talk_ghost() {
    let mut model = model();
    model
        .talk
        .begin(haider_stt::config::TranscriptionEngine::Local);
    model.talk.phase = TalkPhase::Listening;
    model.talk.ghost = "VOICE_GHOST".into();
    arm(&mut model, "permission");
    paste(&mut model, "PASTE");
    assert_eq!(model.composer.text(), DRAFT);
    assert!(model.talk.engaged());
}

#[test]
fn b6_subagent_question_discards_paste_before_the_parked_draft() {
    let mut model = model();
    let mut chip = ChipModel::from_seed(ChipSeed {
        agent: "child".into(),
        parent: None,
        ros: None,
        callsign: "Child".into(),
        hon: "(c)",
        full: "Child agent".into(),
        name: "test".into(),
        model: "fake".into(),
        device: "local".into(),
        state: ChipDisplayState::InputRequired,
        tokens: 0,
        prefill: Vec::new(),
    });
    chip.question = Some(ChipQuestion {
        recovery: false,
        text: "Choose?".into(),
        options: vec!["Allow once".into(), "Deny".into()],
        resolved: false,
    });
    chip.transcript
        .apply(&EventPayload::MenuOpened(permission()));
    model.chips.push(chip);
    model.screen = Screen::Subagent;
    model.view_path = vec!["child".into()];
    assert_eq!(model.input_owner(), InputOwner::SubagentQuestion);
    paste(&mut model, "HIDDEN_PASTE");
    assert_eq!(model.composer.text(), DRAFT);
    assert!(model.composer.text().find("HIDDEN_PASTE").is_none());
}

#[test]
fn d_clipboard_and_hidden_selection_copy_do_not_reach_a_covered_draft() {
    for mode in [
        "search", "inbox", "shells", "ssh", "monitors", "model", "theme", "effort", "history",
        "help", "lockdown", "detail",
    ] {
        let mut model = model();
        model.composer.move_left(true);
        model.composer.move_left(true);
        arm(&mut model, mode);
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        ] {
            key(&mut model, KeyCode::Char('v'), modifiers);
        }
        assert_eq!(model.composer.text(), DRAFT, "{mode}");
        assert!(!model.accepts_clipboard_image(), "{mode}");
        model.requests.clear();
        key(
            &mut model,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(model.requests.is_empty(), "{mode} copied a hidden draft");
    }
}

#[test]
fn d_captured_right_click_cannot_read_into_a_read_only_owner() {
    for mode in [
        "detail", "inbox", "shells", "ssh", "monitors", "help", "lockdown",
    ] {
        let mut model = model();
        arm(&mut model, mode);
        dispatch_input(
            &mut model,
            &[],
            Event::Mouse(ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::Down(
                    ratatui::crossterm::event::MouseButton::Right,
                ),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(
            model.requests.is_empty(),
            "{mode} requested a clipboard read"
        );
        assert_eq!(model.composer.text(), DRAFT);
    }
}

#[test]
fn c_selected_draft_survives_a_modal_escape() {
    for mode in [
        "detail",
        "help",
        "search",
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "model",
        "theme",
        "effort",
        "history",
        "fleet-detail",
        "fleet",
        "sessions",
        "accounts",
        "providers",
        "usage",
        "graph",
        "tools",
        "tree",
        "hooks",
    ] {
        let mut model = model();
        model.composer.move_left(true);
        model.composer.move_left(true);
        let selected = model.composer.selected_text().unwrap().to_owned();
        arm(&mut model, mode);
        key(&mut model, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(model.composer.text(), DRAFT, "{mode}");
        assert_eq!(
            model.composer.selected_text(),
            Some(selected.as_str()),
            "{mode}"
        );
    }
}

#[test]
fn c_ctrl_c_navigates_without_copying_or_clearing_a_hidden_draft_selection() {
    for mode in [
        "detail",
        "help",
        "inbox",
        "model",
        "fleet-detail",
        "accounts",
    ] {
        let mut model = model();
        model.composer.move_left(true);
        model.composer.move_left(true);
        let selected = model.composer.selected_text().unwrap().to_owned();
        arm(&mut model, mode);
        key(&mut model, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(model.screen, Screen::Launcher, "{mode}");
        assert!(
            model
                .requests
                .iter()
                .all(|request| !matches!(request, haider_tui::app::AppRequest::CopyText(_))),
            "{mode}"
        );
        model.open_session(&session());
        assert_eq!(model.composer.text(), DRAFT, "{mode}");
        assert_eq!(
            model.composer.selected_text(),
            Some(selected.as_str()),
            "{mode}"
        );
    }
}

#[test]
fn e1_stale_allow_click_cannot_answer_a_covered_permission() {
    for cover in ["inbox", "model"] {
        let mut model = model();
        arm(&mut model, "permission");
        arm(&mut model, cover);
        let hit = Hit::MenuOption {
            menu: MenuId::new("input-owner-permission"),
            index: 0,
        };
        stale_pointer(
            &mut model,
            hit.clone(),
            MouseEventKind::Down(MouseButton::Left),
        );
        stale_pointer(&mut model, hit, MouseEventKind::Up(MouseButton::Left));
        assert!(model.outbox.is_empty(), "{cover}");
        assert!(model.projection.open_menu().is_some(), "{cover}");
    }
}

#[test]
fn e2_stale_hover_never_moves_a_covered_choice() {
    for cover in [
        "detail", "inbox", "model", "shells", "ssh", "monitors", "lockdown",
    ] {
        let mut model = model();
        arm(&mut model, "permission");
        model.menu_selection = 0;
        arm(&mut model, cover);
        stale_pointer(
            &mut model,
            Hit::MenuOption {
                menu: MenuId::new("input-owner-permission"),
                index: 1,
            },
            MouseEventKind::Moved,
        );
        assert_eq!(model.menu_selection, 0, "{cover}");
    }
    for choice in ["theme", "effort"] {
        let mut model = model();
        arm(&mut model, choice);
        arm(&mut model, "permission");
        if choice == "theme" {
            let before = model.theme_picker.unwrap().selection;
            stale_pointer(&mut model, Hit::ThemeOption(2), MouseEventKind::Moved);
            assert_eq!(model.theme_picker.unwrap().selection, before);
        } else {
            let before = model.effort_picker.as_ref().unwrap().selection;
            stale_pointer(&mut model, Hit::EffortOption(1), MouseEventKind::Moved);
            assert_eq!(model.effort_picker.as_ref().unwrap().selection, before);
        }
    }
}

#[test]
fn e3_e4_e5_covered_composer_pointer_cannot_move_or_copy_a_selection() {
    for mode in [
        "detail", "help", "inbox", "shells", "ssh", "monitors", "model", "theme", "effort",
        "lockdown", "fleet",
    ] {
        let mut model = model();
        model.composer.set_cursor(5);
        model.composer.move_left(true);
        let before = model.composer.cursor();
        let surface = model.surface_key();
        let revision = model.composer.revision();
        let epoch = model.geometry_epoch.get();
        model.composer_drag = true;
        arm(&mut model, mode);
        model.composer_press(0, DRAFT, 0, surface, revision, epoch);
        model.composer_drag_to(0);
        model.composer_release();
        assert_eq!(model.composer.cursor(), before, "{mode}");
        assert!(model.requests.is_empty(), "{mode}");
    }
}

#[test]
fn e3_runtime_rechecks_a_stale_composer_hit_before_the_next_draw() {
    for cover in ["detail", "inbox", "help", "model", "fleet"] {
        let mut model = model();
        model.composer.set_cursor(5);
        let mut terminal = Terminal::new(TestBackend::new(118, 36)).unwrap();
        let mut old_hits = Vec::new();
        terminal
            .draw(|frame| old_hits = haider_tui::render::render(&model, frame))
            .unwrap();
        let rect = old_hits
            .iter()
            .find_map(|(rect, hit)| matches!(hit, Hit::ComposerText { .. }).then_some(*rect))
            .expect("visible composer hit");
        arm(&mut model, cover);
        model.requests.clear();
        let before = model.composer.cursor();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dispatch_input(
                &mut model,
                &old_hits,
                Event::Mouse(MouseEvent {
                    kind,
                    column: rect.x + 1,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        assert_eq!(model.composer.cursor(), before, "{cover}");
        assert!(
            model.requests.is_empty(),
            "{cover} copied a hidden selection"
        );
    }
}

#[test]
fn e4_any_app_event_owner_change_retires_a_composer_drag() {
    let mut model = model();
    model.composer.set_text("/help");
    model.composer_drag = true;
    key(&mut model, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(model.input_owner(), InputOwner::Help);
    assert!(!model.composer_drag);
    model.composer_release();
    assert!(model.requests.is_empty());
}

#[test]
fn e6_e7_e9_wheel_scrolls_visible_transcript_only() {
    for mode in [
        "detail",
        "help",
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "lockdown",
        "model",
        "theme",
        "effort",
        "history",
        "permission",
        "search",
        "fleet-detail",
    ] {
        let mut model = model();
        model.scroll_max.set(100);
        model.scroll_back.set(10);
        model.fleet.sel = 3;
        arm(&mut model, mode);
        dispatch_input(
            &mut model,
            &[],
            Event::Mouse(ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::ScrollUp,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        );
        if matches!(
            mode,
            "theme" | "effort" | "history" | "permission" | "search"
        ) {
            assert_eq!(model.scroll_back.get(), 13, "{mode}");
        } else {
            assert_eq!(model.scroll_back.get(), 10, "{mode}");
        }
        if mode == "fleet-detail" {
            assert_eq!(model.fleet.sel, 3);
        }
    }
}

#[test]
fn e8_document_wheel_scrolls_its_document_instead_of_the_transcript() {
    for kind in ["plan", "file-review"] {
        let mut model = model();
        let mut menu = permission();
        if kind == "plan" {
            menu.origin = "plan".into();
        } else {
            menu = review_menu();
        }
        model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
        model.scroll_max.set(100);
        model.scroll_back.set(20);
        model.plan_scroll_max.set(100);
        model.file_review_scroll_max.set(100);
        model.plan_scroll.set(10);
        model.file_review_scroll.set(10);
        dispatch_input(
            &mut model,
            &[],
            Event::Mouse(ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::ScrollUp,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert_eq!(model.scroll_back.get(), 20, "{kind}");
        let document_scroll = if kind == "plan" {
            model.plan_scroll.get()
        } else {
            model.file_review_scroll.get()
        };
        assert_eq!(document_scroll, 7, "{kind}");
    }
}

#[test]
fn e6_e8_edge_drag_scrolls_only_the_visible_owner() {
    for mode in ["detail", "help", "inbox", "plan-doc", "file-review-doc"] {
        let mut model = model();
        model.scroll_max.set(100);
        model.scroll_back.set(20);
        model.plan_scroll_max.set(100);
        model.file_review_scroll_max.set(100);
        model.plan_scroll.set(10);
        model.file_review_scroll.set(10);
        arm(&mut model, mode);
        if let Some(detail) = model.tool_detail.as_mut() {
            detail.scroll = 10;
            detail.scroll_max.set(100);
        }
        model.transcript_view.set(Rect::new(0, 3, 80, 10));
        model.mouse_down = Some((10, 8));
        dispatch_input(
            &mut model,
            &[],
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 10,
                row: 3,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert_eq!(model.scroll_back.get(), 20, "{mode}");
        match mode {
            "detail" => assert_eq!(model.tool_detail.as_ref().unwrap().scroll, 9),
            "plan-doc" => assert_eq!(model.plan_scroll.get(), 9),
            "file-review-doc" => assert_eq!(model.file_review_scroll.get(), 9),
            _ => {}
        }
    }
}

#[test]
fn ordinary_cards_allow_visible_transcript_hits_but_covering_views_do_not() {
    for mode in ["permission", "search", "theme", "effort", "history"] {
        let mut model = model();
        model.scroll_back.set(10);
        arm(&mut model, mode);
        stale_pointer(
            &mut model,
            Hit::JumpToBottom,
            MouseEventKind::Down(MouseButton::Left),
        );
        stale_pointer(
            &mut model,
            Hit::JumpToBottom,
            MouseEventKind::Up(MouseButton::Left),
        );
        assert_eq!(model.scroll_back.get(), 0, "{mode}");
    }
    for mode in ["detail", "inbox", "model", "plan-doc", "file-review-doc"] {
        let mut model = model();
        model.scroll_back.set(10);
        arm(&mut model, mode);
        stale_pointer(
            &mut model,
            Hit::JumpToBottom,
            MouseEventKind::Down(MouseButton::Left),
        );
        stale_pointer(
            &mut model,
            Hit::JumpToBottom,
            MouseEventKind::Up(MouseButton::Left),
        );
        assert_eq!(model.scroll_back.get(), 10, "{mode}");
    }
}

#[test]
fn ordinary_cards_allow_edge_drag_of_the_visible_transcript() {
    for mode in ["permission", "search", "theme", "effort", "history"] {
        let mut model = model();
        model.scroll_max.set(100);
        model.scroll_back.set(20);
        arm(&mut model, mode);
        model.drag_autoscroll(true);
        assert_eq!(model.scroll_back.get(), 21, "{mode}");
    }
}

#[test]
fn f1_f2_injected_operations_cannot_edit_or_activate_a_modal() {
    for mode in [
        "detail",
        "help",
        "search",
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "model",
        "theme",
        "effort",
        "history",
        "lockdown",
        "permission",
    ] {
        for op in [
            haider_rpc::SurfaceInjectOp::Set {
                text: "REPLACE".into(),
            },
            haider_rpc::SurfaceInjectOp::Insert {
                text: "INSERT".into(),
            },
            haider_rpc::SurfaceInjectOp::Clear,
            haider_rpc::SurfaceInjectOp::Submit,
        ] {
            let mut model = model();
            arm(&mut model, mode);
            LiveDriver::new("owner-test").apply(
                &mut model,
                LiveReply::InputInjected {
                    session: session(),
                    op,
                },
            );
            assert_eq!(model.composer.text(), DRAFT, "{mode}");
            assert!(model.outbox.is_empty(), "{mode}");
        }
    }
}

#[test]
fn g1_g2_refused_foreign_edit_waits_without_republishing_old_draft() {
    for mode in [
        "detail",
        "help",
        "search",
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "model",
        "theme",
        "effort",
        "history",
        "lockdown",
        "permission",
        "login",
        "talk-setup",
        "talk-engaged",
        "subagent",
    ] {
        let mut model = model();
        model
            .daemon_features
            .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.into());
        model.composer.set_text("OLD");
        let mut driver = LiveDriver::new("owner-test");
        assert_eq!(driver.sync_input_mirror(&model).len(), 1);
        driver.apply(
            &mut model,
            LiveReply::SurfaceWatching {
                caller_owner: Some("local".into()),
                epoch: 0,
                session: session(),
                input: None,
            },
        );
        assert_eq!(driver.sync_input_mirror(&model).len(), 1);
        if mode == "talk-engaged" {
            model
                .talk
                .begin(haider_stt::config::TranscriptionEngine::Local);
            model.talk.phase = TalkPhase::Listening;
        } else {
            arm(&mut model, mode);
        }
        driver.apply(
            &mut model,
            LiveReply::SurfaceInput {
                session: session(),
                input: haider_rpc::SurfaceInputWire {
                    text: "NEW".into(),
                    attachments: vec![],
                    revision: 1,
                    owner: "foreign".into(),
                },
            },
        );
        assert_eq!(model.composer.text(), "OLD", "{mode}");
        assert!(
            driver.sync_input_mirror(&model).is_empty(),
            "{mode} republished OLD"
        );
        match mode {
            "detail" => model.tool_detail = None,
            "help" => model.help_open = false,
            "search" => model.transcript_search = None,
            "inbox" => model.inbox_open = false,
            "shells" => model.shells_open = false,
            "ssh" => model.ssh_open = false,
            "monitors" => model.monitors_open = false,
            "model" => model.model_picker = None,
            "theme" => model.theme_picker = None,
            "effort" => model.effort_picker = None,
            "history" => model.backtrack = None,
            "lockdown" => model.lockdown_overlay = false,
            "permission" => {
                model.projection.apply(&EventPayload::MenuClosed {
                    menu: MenuId::new("input-owner-permission"),
                    reason: haider_protocol::menu::MenuCloseReason::Dismissed,
                });
            }
            "login" => model.login = None,
            "talk-setup" => model.talk_setup = None,
            "talk-engaged" => model.talk.phase = TalkPhase::Idle,
            "subagent" => model.screen = Screen::Session,
            _ => unreachable!(),
        }
        driver.apply_pending_input(&mut model);
        assert_eq!(model.composer.text(), "NEW", "{mode}");
        assert!(
            driver.sync_input_mirror(&model).is_empty(),
            "{mode} published after apply"
        );
    }
}

#[test]
fn g3_older_refused_revision_cannot_replace_the_newest_pending_edit() {
    let mut model = model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.into());
    model.composer.set_text("OLD");
    let mut driver = LiveDriver::new("owner-test");
    let _ = driver.sync_input_mirror(&model);
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            caller_owner: Some("local".into()),
            epoch: 0,
            session: session(),
            input: None,
        },
    );
    let _ = driver.sync_input_mirror(&model);
    arm(&mut model, "detail");
    for (revision, text) in [(2, "NEWEST"), (1, "STALE")] {
        driver.apply(
            &mut model,
            LiveReply::SurfaceInput {
                session: session(),
                input: haider_rpc::SurfaceInputWire {
                    text: text.into(),
                    attachments: vec![],
                    revision,
                    owner: "foreign".into(),
                },
            },
        );
    }
    model.close_tool_detail();
    driver.apply_pending_input(&mut model);
    assert_eq!(model.composer.text(), "NEWEST");
}

#[test]
fn g3_pending_foreign_edit_from_an_old_connection_is_discarded() {
    let mut model = model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.into());
    model.composer.set_text("OLD");
    let mut driver = LiveDriver::new("owner-test");
    let _ = driver.sync_input_mirror(&model);
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            caller_owner: Some("local".into()),
            epoch: 0,
            session: session(),
            input: None,
        },
    );
    let _ = driver.sync_input_mirror(&model);
    arm(&mut model, "detail");
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: session(),
            input: haider_rpc::SurfaceInputWire {
                text: "OLD_EPOCH".into(),
                attachments: vec![],
                revision: 1,
                owner: "foreign".into(),
            },
        },
    );
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "test redial".into(),
        },
    );
    model.close_tool_detail();
    driver.apply_pending_input(&mut model);
    assert_eq!(model.composer.text(), "OLD");
}

#[test]
fn foreign_edit_applies_before_retained_dictation_on_reentry() {
    let mut model = model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.into());
    model.composer.set_text("OLD");
    let mut driver = LiveDriver::new("owner-test");
    let _ = driver.sync_input_mirror(&model);
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            caller_owner: Some("local".into()),
            epoch: 0,
            session: session(),
            input: None,
        },
    );
    let _ = driver.sync_input_mirror(&model);
    model
        .talk
        .begin(haider_stt::config::TranscriptionEngine::Local);
    model.talk.phase = TalkPhase::Finishing;
    let generation = model.talk.generation;
    arm(&mut model, "detail");
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: session(),
            input: haider_rpc::SurfaceInputWire {
                text: "NEW".into(),
                attachments: vec![],
                revision: 1,
                owner: "foreign".into(),
            },
        },
    );
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Ok(haider_stt::TranscriptionResult {
            text: "VOICE_FINAL".into(),
            segments: 1,
            duration_ms: 10,
        }),
    });
    model.close_tool_detail();
    assert_eq!(model.composer.text(), "OLD");
    driver.apply_pending_input(&mut model);
    assert!(model.composer.text().starts_with("NEW"));
    assert!(model.composer.text().contains("VOICE_FINAL"));
}

#[test]
fn foreign_edit_waiting_during_visible_talk_precedes_auto_send() {
    let mut model = model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.into());
    model.composer.set_text("OLD");
    let mut driver = LiveDriver::new("owner-test");
    let _ = driver.sync_input_mirror(&model);
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            caller_owner: Some("local".into()),
            epoch: 0,
            session: session(),
            input: None,
        },
    );
    let _ = driver.sync_input_mirror(&model);
    model
        .talk
        .begin(haider_stt::config::TranscriptionEngine::Local);
    model.talk.phase = TalkPhase::Finishing;
    model.talk.intent = CommitIntent::Submit;
    let generation = model.talk.generation;
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: session(),
            input: haider_rpc::SurfaceInputWire {
                text: "NEW".into(),
                attachments: vec![],
                revision: 1,
                owner: "foreign".into(),
            },
        },
    );
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Ok(haider_stt::TranscriptionResult {
            text: "VOICE_FINAL".into(),
            segments: 1,
            duration_ms: 10,
        }),
    });
    assert_eq!(model.composer.text(), "OLD");
    assert!(
        model
            .requests
            .iter()
            .all(|request| !matches!(request, haider_tui::app::AppRequest::SubmitText { .. }))
    );
    driver.apply_pending_input(&mut model);
    assert!(model.composer.text().starts_with("NEW"));
    assert!(model.composer.text().contains("VOICE_FINAL"));
    assert!(
        model
            .requests
            .iter()
            .all(|request| !matches!(request, haider_tui::app::AppRequest::SubmitText { .. }))
    );
}

#[test]
fn h1_h2_finished_dictation_waits_for_visible_composer_and_downgrades_submit() {
    for intent in [CommitIntent::Insert, CommitIntent::Submit] {
        let mut model = model();
        model
            .talk
            .begin(haider_stt::config::TranscriptionEngine::Local);
        model.talk.phase = TalkPhase::Finishing;
        model.talk.intent = intent;
        let generation = model.talk.generation;
        arm(&mut model, "detail");
        model.handle_talk(TalkEvent::Finished {
            generation,
            result: Ok(haider_stt::TranscriptionResult {
                text: "VOICE_FINAL".into(),
                segments: 1,
                duration_ms: 10,
            }),
        });
        assert_eq!(model.composer.text(), DRAFT);
        assert!(model.outbox.is_empty());
        assert!(
            model
                .requests
                .iter()
                .all(|request| !matches!(request, haider_tui::app::AppRequest::SubmitText { .. }))
        );
        model.close_tool_detail();
        assert!(model.composer.text().contains("VOICE_FINAL"));
        assert!(model.outbox.is_empty());
        assert!(
            model
                .requests
                .iter()
                .all(|request| !matches!(request, haider_tui::app::AppRequest::SubmitText { .. }))
        );
        assert!(
            model
                .flash
                .as_deref()
                .is_some_and(|text| text.contains("dictation added"))
        );
    }
}

#[test]
fn visible_composer_allows_explicit_talk_auto_submit() {
    let mut model = model();
    model
        .talk
        .begin(haider_stt::config::TranscriptionEngine::Local);
    model.talk.phase = TalkPhase::Finishing;
    model.talk.intent = CommitIntent::Submit;
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Ok(haider_stt::TranscriptionResult {
            text: "VOICE_FINAL".into(),
            segments: 1,
            duration_ms: 10,
        }),
    });
    assert!(model.requests.iter().any(|request| matches!(request, haider_tui::app::AppRequest::SubmitText { text, .. } if text.contains("VOICE_FINAL"))), "visible auto-send must submit");
}

#[test]
fn h3_h4_cap_and_error_ghosts_wait_for_the_visible_composer() {
    for path in [
        "cap",
        "finished-error",
        "health-failed",
        "supervisor-failed",
    ] {
        let mut model = model();
        model
            .talk
            .begin(haider_stt::config::TranscriptionEngine::Local);
        model.talk.phase = TalkPhase::Listening;
        model.talk.ghost = "VOICE_GHOST".into();
        let generation = model.talk.generation;
        arm(&mut model, "detail");
        match path {
            "cap" => {
                model.handle_talk(TalkEvent::CapReached { generation });
                assert_eq!(model.talk.phase, TalkPhase::Finishing);
                model.handle_talk(TalkEvent::Finished {
                    generation,
                    result: Ok(haider_stt::TranscriptionResult {
                        text: "VOICE_FINAL".into(),
                        segments: 1,
                        duration_ms: 10,
                    }),
                });
            }
            "finished-error" => {
                model.talk.phase = TalkPhase::Finishing;
                model.handle_talk(TalkEvent::Finished {
                    generation,
                    result: Err(haider_stt::SttError::Io("synthetic".into())),
                });
            }
            "health-failed" => model.handle_talk(TalkEvent::Health {
                generation,
                health: haider_stt::capture::CaptureHealth::Failed {
                    error: "synthetic".into(),
                },
            }),
            "supervisor-failed" => model.handle_talk(TalkEvent::SupervisorFailed {
                reason: "synthetic".into(),
            }),
            _ => unreachable!(),
        }
        assert_eq!(model.composer.text(), DRAFT, "{path}");
        model.close_tool_detail();
        assert!(
            model.composer.text().contains(if path == "cap" {
                "VOICE_FINAL"
            } else {
                "VOICE_GHOST"
            }),
            "{path}"
        );
    }
}

#[test]
fn h5_menu_arriving_during_talk_owns_digits_and_enter() {
    for code in [KeyCode::Char('1'), KeyCode::Enter] {
        let mut model = model();
        model
            .talk
            .begin(haider_stt::config::TranscriptionEngine::Local);
        model.talk.phase = TalkPhase::Listening;
        model.talk.ghost = "VOICE_GHOST".into();
        arm(&mut model, "permission");
        key(&mut model, code, KeyModifiers::NONE);
        assert_eq!(model.composer.text(), DRAFT, "{code:?}");
        assert!(!model.outbox.is_empty(), "{code:?} did not answer card");
    }
}

#[test]
fn h6_setup_opened_under_detail_waits_until_detail_closes() {
    let mut model = model();
    model
        .talk
        .begin(haider_stt::config::TranscriptionEngine::Local);
    model.talk.phase = TalkPhase::Starting;
    let generation = model.talk.generation;
    arm(&mut model, "detail");
    model.handle_talk(TalkEvent::StartFailed {
        generation,
        error: haider_stt::SttError::ModelMissing {
            model_id: "synthetic".into(),
        },
    });
    assert!(model.talk_setup.is_some());
    assert_eq!(model.input_owner(), haider_tui::app::InputOwner::ToolDetail);
    key(&mut model, KeyCode::Tab, KeyModifiers::NONE);
    key(&mut model, KeyCode::Char('1'), KeyModifiers::NONE);
    assert_eq!(model.input_owner(), haider_tui::app::InputOwner::ToolDetail);
    key(&mut model, KeyCode::Esc, KeyModifiers::NONE);
    assert!(model.tool_detail.is_none());
    assert_eq!(model.input_owner(), haider_tui::app::InputOwner::TalkSetup);
}

#[test]
fn h7_talk_setup_and_new_approval_agree_that_setup_is_visible_owner() {
    let mut model = model();
    model
        .talk
        .begin(haider_stt::config::TranscriptionEngine::Local);
    model.talk.phase = TalkPhase::Starting;
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::StartFailed {
        generation,
        error: haider_stt::SttError::ModelMissing {
            model_id: "synthetic".into(),
        },
    });
    arm(&mut model, "permission");
    assert_eq!(model.input_owner(), InputOwner::TalkSetup);
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).unwrap();
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = haider_tui::render::render(&model, frame))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let painted = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
        .collect::<Vec<_>>()
        .join("");
    assert!(painted.contains("talk setup"));
    assert!(
        !painted.contains("Allow once"),
        "approval painted over talk setup"
    );
    assert!(
        hits.iter()
            .all(|(_, hit)| !matches!(hit, Hit::MenuOption { .. }))
    );
    key(&mut model, KeyCode::Char('1'), KeyModifiers::NONE);
    assert!(model.outbox.is_empty(), "a hidden approval was answered");
}

#[test]
fn h9_stale_generation_and_closed_session_dictation_are_discarded() {
    for close_session in [false, true] {
        let mut model = model();
        model
            .talk
            .begin(haider_stt::config::TranscriptionEngine::Local);
        model.talk.phase = TalkPhase::Finishing;
        let generation = model.talk.generation;
        if close_session {
            model.back_to_launcher();
        }
        model.handle_talk(TalkEvent::Finished {
            generation: if close_session {
                generation
            } else {
                generation + 1
            },
            result: Ok(haider_stt::TranscriptionResult {
                text: "STALE_VOICE".into(),
                segments: 1,
                duration_ms: 10,
            }),
        });
        if close_session {
            model.open_session(&session());
        }
        assert!(!model.composer.text().contains("STALE_VOICE"));
    }
}

#[test]
fn h8_same_session_fleet_and_inbox_clicks_do_not_realize_talk_into_hidden_draft() {
    for destination in ["fleet", "inbox"] {
        let mut model = model();
        model
            .talk
            .begin(haider_stt::config::TranscriptionEngine::Local);
        model.talk.phase = TalkPhase::Listening;
        model.talk.ghost = "VOICE_GHOST".into();
        let hit = if destination == "fleet" {
            model.fleet.snapshot = Some(haider_tui::fleet::snapshot_from_chips(
                &model.chips,
                &session(),
                model.clock_ms,
            ));
            model
                .daemon_features
                .insert(haider_rpc::FEATURE_SESSION_FLEET_V1.into());
            model.subtree_collapsed = false;
            Hit::FleetSummary
        } else {
            Hit::InboxBand
        };
        let hit_map = vec![(ratatui::layout::Rect::new(0, 0, 5, 1), hit)];
        for kind in [
            ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            ratatui::crossterm::event::MouseEventKind::Up(
                ratatui::crossterm::event::MouseButton::Left,
            ),
        ] {
            dispatch_input(
                &mut model,
                &hit_map,
                Event::Mouse(ratatui::crossterm::event::MouseEvent {
                    kind,
                    column: 1,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                }),
            );
        }
        assert!(
            model.screen == Screen::Fleet || model.inbox_open,
            "{destination}"
        );
        key(&mut model, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(model.composer.text(), DRAFT, "{destination}");
    }
}

#[test]
fn visible_composer_still_accepts_editing_paste_and_inject() {
    let mut model = model();
    model.composer.set_cursor(5);
    key(&mut model, KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert_ne!(model.composer.text(), DRAFT);
    paste(&mut model, "VISIBLE_PASTE");
    assert!(model.composer.text().contains("VISIBLE_PASTE"));
    LiveDriver::new("owner-test").apply(
        &mut model,
        LiveReply::InputInjected {
            session: session(),
            op: haider_rpc::SurfaceInjectOp::Set {
                text: "INJECTED".into(),
            },
        },
    );
    assert_eq!(model.composer.text(), "INJECTED");
}

#[test]
fn visible_composer_keeps_clipboard_drag_and_inject_controls() {
    let mut model = model();
    model.composer.set_cursor(5);
    model.composer.move_left(true);
    key(
        &mut model,
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, haider_tui::app::AppRequest::CopyText(_)))
    );
    model.requests.clear();

    let surface = model.surface_key();
    let revision = model.composer.revision();
    let epoch = model.geometry_epoch.get();
    model.composer_press(0, DRAFT, 0, surface, revision, epoch);
    model.composer_drag_to(5);
    model.composer_release();
    assert!(model.requests.iter().any(
        |request| matches!(request, haider_tui::app::AppRequest::CopyText(text) if text == "KEEP_")
    ));

    let mut model = self::model();
    let mut driver = LiveDriver::new("owner-test");
    for op in [
        haider_rpc::SurfaceInjectOp::Set { text: "INJ".into() },
        haider_rpc::SurfaceInjectOp::Insert { text: "X".into() },
    ] {
        driver.apply(
            &mut model,
            LiveReply::InputInjected {
                session: session(),
                op,
            },
        );
    }
    assert_eq!(model.composer.text(), "INJX");
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: session(),
            op: haider_rpc::SurfaceInjectOp::Clear,
        },
    );
    assert!(model.composer.text().is_empty());
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: session(),
            op: haider_rpc::SurfaceInjectOp::Set {
                text: "SEND".into(),
            },
        },
    );
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: session(),
            op: haider_rpc::SurfaceInjectOp::Submit,
        },
    );
    assert!(model.requests.iter().any(|request| matches!(request, haider_tui::app::AppRequest::SubmitText { text, .. } if text == "SEND")));
}

#[test]
fn zero_option_ask_keeps_the_visible_answer_composer() {
    let mut model = model();
    let mut ask = permission();
    ask.options.clear();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(ask))));
    assert_eq!(model.input_owner(), InputOwner::Composer(Screen::Session));
    key(&mut model, KeyCode::Char('X'), KeyModifiers::NONE);
    assert!(model.composer.text().contains('X'));
}

#[test]
fn visible_detail_wheel_scrolls_its_own_page() {
    let mut model = model();
    model.scroll_back.set(7);
    arm(&mut model, "detail");
    model.tool_detail.as_ref().unwrap().scroll_max.set(50);
    model.handle_wheel(false);
    assert_eq!(model.tool_detail.as_ref().unwrap().scroll, 3);
    assert_eq!(model.scroll_back.get(), 7);
}

#[test]
fn visible_detail_closes_with_one_escape_and_theme_cycle_is_global() {
    let mut model = model();
    arm(&mut model, "detail");
    let original_theme = model.theme;
    key(&mut model, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_ne!(model.theme, original_theme);
    assert!(model.tool_detail.is_some());
    key(&mut model, KeyCode::Esc, KeyModifiers::NONE);
    assert!(model.tool_detail.is_none());
    assert_eq!(model.composer.text(), DRAFT);
}

#[test]
fn e10_search_banner_covers_a_fresh_sticky_transcript_hit() {
    let mut model = AppModel::new();
    for payload in haider_tui::mock::demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();
    terminal
        .draw(|frame| {
            haider_tui::render::render(&model, frame);
        })
        .unwrap();
    model.handle_wheel(true);
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = haider_tui::render::render(&model, frame))
        .unwrap();
    let search_row = hits
        .iter()
        .find_map(|(rect, hit)| matches!(hit, Hit::StickyJump(_)).then_some(rect.y))
        .expect("sticky transcript row before search");

    key(&mut model, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert_eq!(model.input_owner(), InputOwner::Search);
    terminal
        .draw(|frame| hits = haider_tui::render::render(&model, frame))
        .unwrap();
    let mut painted_row = String::new();
    for x in 0..90 {
        painted_row.push_str(terminal.backend().buffer()[(x, search_row)].symbol());
    }
    assert!(
        painted_row.contains("🔎"),
        "search banner was covered: {painted_row}"
    );
    assert!(
        hits.iter()
            .all(|(rect, _)| { rect.y > search_row || rect.y + rect.height <= search_row }),
        "a covered transcript target survived on the search row"
    );
    let before = model.scroll_back.get();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dispatch_input(
            &mut model,
            &hits,
            Event::Mouse(MouseEvent {
                kind,
                column: 10,
                row: search_row,
                modifiers: KeyModifiers::NONE,
            }),
        );
    }
    assert_eq!(model.scroll_back.get(), before);
    assert!(model.tool_detail.is_none());
}

#[test]
fn i_ssh_terminal_renders_above_an_existing_model_picker() {
    let mut model = model();
    arm(&mut model, "ssh-terminal");
    arm(&mut model, "model");
    assert_eq!(model.input_owner(), InputOwner::SshTerminal);
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).unwrap();
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = haider_tui::render::render(&model, frame))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut painted = String::new();
    for y in 0..36 {
        for x in 0..118 {
            painted.push_str(buffer[(x, y)].symbol());
        }
    }
    assert!(painted.contains("interactive PTY"));
    assert!(!painted.contains("MODELS"));
    assert!(hits.is_empty());
}

// Keep this match exhaustive: a new owner needs an explicit agreement row.
fn owner_label(owner: InputOwner) -> &'static str {
    match owner {
        InputOwner::Lockdown => "lockdown",
        InputOwner::ToolDetail => "detail",
        InputOwner::Inbox => "inbox",
        InputOwner::Help => "help",
        InputOwner::Shells => "shells",
        InputOwner::SshList => "ssh list",
        InputOwner::Monitors => "monitors",
        InputOwner::SshTerminal => "ssh terminal",
        InputOwner::SshForm => "ssh form",
        InputOwner::ModelPicker => "model",
        InputOwner::Login => "login",
        InputOwner::TalkSetup => "talk setup",
        InputOwner::CustomProviderForm => "custom provider",
        InputOwner::AccountsReplace => "accounts replace",
        InputOwner::SessionMenu => "session menu",
        InputOwner::SubagentQuestion => "subagent question",
        InputOwner::PlanDoc => "plan",
        InputOwner::FileReviewDoc => "file review",
        InputOwner::Search => "search",
        InputOwner::EffortPicker => "effort",
        InputOwner::ThemePicker => "theme",
        InputOwner::HistoryChooser => "history",
        InputOwner::FleetMemberDetail => "fleet detail",
        InputOwner::Screen(_) => "screen",
        InputOwner::Composer(_) => "composer",
    }
}

fn expected_owner(mode: &str) -> InputOwner {
    match mode {
        "detail" => InputOwner::ToolDetail,
        "help" => InputOwner::Help,
        "search" => InputOwner::Search,
        "inbox" => InputOwner::Inbox,
        "shells" => InputOwner::Shells,
        "ssh" => InputOwner::SshList,
        "ssh-form" => InputOwner::SshForm,
        "ssh-terminal" => InputOwner::SshTerminal,
        "monitors" => InputOwner::Monitors,
        "lockdown" => InputOwner::Lockdown,
        "model" => InputOwner::ModelPicker,
        "theme" => InputOwner::ThemePicker,
        "effort" => InputOwner::EffortPicker,
        "history" => InputOwner::HistoryChooser,
        "permission" => InputOwner::SessionMenu,
        "plan-doc" => InputOwner::PlanDoc,
        "file-review-doc" => InputOwner::FileReviewDoc,
        "login" => InputOwner::Login,
        "accounts-replace" => InputOwner::AccountsReplace,
        "talk-setup" => InputOwner::TalkSetup,
        "custom-form" => InputOwner::CustomProviderForm,
        "fleet-detail" => InputOwner::FleetMemberDetail,
        "fleet" => InputOwner::Screen(Screen::Fleet),
        "sessions" => InputOwner::Screen(Screen::Sessions),
        "accounts" => InputOwner::Screen(Screen::Accounts),
        "providers" => InputOwner::Screen(Screen::Providers),
        "usage" => InputOwner::Screen(Screen::Usage),
        "graph" => InputOwner::Screen(Screen::Graph),
        "tools" => InputOwner::Screen(Screen::Tools),
        "tree" => InputOwner::Screen(Screen::Tree),
        "hooks" => InputOwner::Screen(Screen::Hooks),
        "boot" => InputOwner::Screen(Screen::Boot),
        "session" => InputOwner::Composer(Screen::Session),
        "launcher" => InputOwner::Composer(Screen::Launcher),
        "subagent" => InputOwner::Composer(Screen::Subagent),
        "subagent-question" => InputOwner::SubagentQuestion,
        "aura" => InputOwner::Composer(Screen::Aura),
        "loom" => InputOwner::Composer(Screen::Loom),
        _ => panic!("unknown agreement mode: {mode}"),
    }
}

#[test]
fn i_rendered_hit_map_agrees_with_the_input_owner() {
    for mode in [
        "detail",
        "help",
        "search",
        "inbox",
        "shells",
        "ssh",
        "ssh-form",
        "ssh-terminal",
        "monitors",
        "lockdown",
        "model",
        "theme",
        "effort",
        "history",
        "permission",
        "plan-doc",
        "file-review-doc",
        "login",
        "accounts-replace",
        "talk-setup",
        "custom-form",
        "fleet-detail",
        "fleet",
        "sessions",
        "accounts",
        "providers",
        "usage",
        "graph",
        "tools",
        "tree",
        "hooks",
        "boot",
        "session",
        "launcher",
        "subagent",
        "subagent-question",
        "aura",
        "loom",
    ] {
        let mut model = model();
        arm(&mut model, mode);
        let owner = model.input_owner();
        assert_eq!(
            owner,
            expected_owner(mode),
            "{mode} owner disagreed with state"
        );
        assert!(!owner_label(owner).is_empty());
        let mut terminal = Terminal::new(TestBackend::new(118, 36)).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| hits = haider_tui::render::render(&model, frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let painted = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect::<Vec<_>>()
            .join("");
        let marker = match mode {
            "detail" => Some("no longer in the transcript"),
            "help" => Some("help  esc closes"),
            "search" => Some("🔎"),
            "inbox" => Some("NEEDS YOU"),
            "shells" => Some("no terminal sessions"),
            "ssh" => Some("ssh profiles"),
            "ssh-form" => Some("add SSH profile"),
            "ssh-terminal" => Some("interactive PTY"),
            "monitors" => Some("no active monitors"),
            "lockdown" => Some("provider lockdown"),
            "model" => Some("MODELS"),
            "theme" => Some("theme — how haider"),
            "effort" => Some("effort — reasoning"),
            "permission" => Some("Allow once"),
            "plan-doc" => Some("PLAN"),
            "file-review-doc" => Some("EDIT REVIEW"),
            "login" => Some("API key"),
            "accounts-replace" => Some("Replace this form?"),
            "talk-setup" => Some("talk setup"),
            "custom-form" => Some("add a custom provider"),
            "fleet-detail" => Some("Child"),
            _ => None,
        };
        if let Some(marker) = marker {
            assert!(
                painted.contains(marker),
                "{mode} did not paint its own {marker:?} layer"
            );
        }
        if !matches!(owner, InputOwner::Composer(_)) {
            assert!(
                hits.iter()
                    .all(|(_, hit)| !matches!(hit, Hit::ComposerText { .. })),
                "{mode} published a hidden composer target"
            );
        }
    }
}

#[test]
fn owner_control_chords_reach_ssh_terminal_form_and_login_through_dispatch() {
    for (code, byte) in [('c', 0x03), ('t', 0x14), ('a', 0x01), ('s', 0x13)] {
        let mut model = model();
        arm(&mut model, "ssh-terminal");
        model.ssh_terminal.as_mut().unwrap().shell_id = Some("remote".into());
        model.requests.clear();
        key(&mut model, KeyCode::Char(code), KeyModifiers::CONTROL);
        assert_eq!(model.screen, Screen::Session, "Ctrl+{code}");
        assert_eq!(model.input_owner(), InputOwner::SshTerminal, "Ctrl+{code}");
        assert!(model.requests.iter().any(|request| matches!(request,
            haider_tui::app::AppRequest::SshShellInput { input, .. } if input.as_slice() == [byte]
        )), "Ctrl+{code} did not reach the remote shell");
    }
    let mut model = model();
    arm(&mut model, "ssh-form");
    {
        let form = model.ssh_form.as_mut().unwrap();
        form.name = "prod".into();
        form.host = "prod.test".into();
        form.user = "deploy".into();
        form.credential = "/keys/prod".into();
    }
    model.requests.clear();
    key(&mut model, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, haider_tui::app::AppRequest::SshProfileSave { .. }))
    );
    let mut model = model();
    arm(&mut model, "login");
    key(&mut model, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(model.screen, Screen::Session);
    assert!(model.login.is_none());
    let mut accounts_model = model();
    accounts_model.screen = Screen::Accounts;
    arm(&mut accounts_model, "login");
    key(
        &mut accounts_model,
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    );
    assert_eq!(accounts_model.screen, Screen::Accounts);
    assert!(accounts_model.login.is_none());
}

fn pending_visible_talk_mirror() -> (AppModel, LiveDriver) {
    let mut model = model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.into());
    model.composer.set_text("OLD");
    let mut driver = LiveDriver::new("owner-test");
    let _ = driver.sync_input_mirror(&model);
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            caller_owner: Some("local".into()),
            epoch: 0,
            session: session(),
            input: None,
        },
    );
    let _ = driver.sync_input_mirror(&model);
    model
        .talk
        .begin(haider_stt::config::TranscriptionEngine::Local);
    model.talk.phase = TalkPhase::Listening;
    model.talk.ghost = "GHOSTWORDS".into();
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: session(),
            input: haider_rpc::SurfaceInputWire {
                text: "NEW".into(),
                attachments: vec![],
                revision: 1,
                owner: "foreign".into(),
            },
        },
    );
    assert_eq!(model.composer.text(), "OLD");
    (model, driver)
}

#[test]
fn refused_mirror_edit_cannot_erase_newer_typing_or_paste() {
    for paste in [false, true] {
        let (mut model, mut driver) = pending_visible_talk_mirror();
        if paste {
            model.handle(AppEvent::Paste("PASTED".to_owned().into()));
        } else {
            key(&mut model, KeyCode::Char('x'), KeyModifiers::NONE);
        }
        let local = model.composer.text().to_owned();
        assert!(local.contains("GHOSTWORDS"));
        driver.apply_pending_input(&mut model);
        assert_eq!(model.composer.text(), local);
        let published = driver.sync_input_mirror(&model);
        assert!(published.iter().any(|command| matches!(
            command,
            haider_tui::live::LiveCommand::SurfacePublish { .. }
        )));
    }
}

#[test]
fn cancelled_visible_talk_still_accepts_the_waiting_foreign_edit() {
    let (mut model, mut driver) = pending_visible_talk_mirror();
    key(&mut model, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(model.composer.text(), "OLD");
    driver.apply_pending_input(&mut model);
    assert_eq!(model.composer.text(), "NEW");
}

#[test]
fn waiting_mirror_edit_blocks_publish_before_apply_even_if_local_text_changes() {
    let (mut model, mut driver) = pending_visible_talk_mirror();
    model.talk.phase = TalkPhase::Idle;
    key(&mut model, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(model.composer.text(), "OLDx");
    assert!(driver.sync_input_mirror(&model).is_empty());
    driver.apply_pending_input(&mut model);
    assert_eq!(model.composer.text(), "OLDx");
    assert_eq!(driver.sync_input_mirror(&model).len(), 1);
}

#[test]
fn history_chooser_disappears_when_a_permission_card_takes_input() {
    let mut model = model();
    model
        .prompt_history
        .push_back(haider_tui::session::PromptEntry::local(
            "HISTORY_UNIQUE_SENTINEL".into(),
        ));
    arm(&mut model, "history");
    let paint = |model: &AppModel| {
        let mut terminal = Terminal::new(TestBackend::new(118, 36)).unwrap();
        terminal
            .draw(|frame| {
                let _ = haider_tui::render::render(model, frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect::<Vec<_>>()
            .join("")
    };
    assert!(paint(&model).contains("HISTORY_UNIQUE_SENTINEL"));
    arm(&mut model, "permission");
    assert_eq!(model.input_owner(), InputOwner::SessionMenu);
    let covered = paint(&model);
    assert!(covered.contains("Allow once"));
    assert!(!covered.contains("HISTORY_UNIQUE_SENTINEL"));
}

#[test]
fn ctrl_shift_c_copies_a_mouse_selection_made_on_the_current_overlay() {
    for mode in [
        "help",
        "inbox",
        "monitors",
        "permission",
        "search",
        "ssh-terminal",
    ] {
        let mut model = model();
        arm(&mut model, mode);
        dispatch_input(
            &mut model,
            &[],
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
        );
        dispatch_input(
            &mut model,
            &[],
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 16,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(model.selection.is_some(), "{mode}");
        model.requests.clear();
        key(
            &mut model,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(
            model
                .requests
                .iter()
                .any(|r| matches!(r, haider_tui::app::AppRequest::CopySelection)),
            "{mode}"
        );
    }
}

#[test]
fn i_pairwise_overlay_precedence_paints_the_owner_above_every_lower_owner() {
    // Ordered independently from input_owner. Each pair arms both states;
    // a one-owner-at-a-time render check cannot catch a hidden chooser
    // painted over an active card.
    let ranked = [
        ("lockdown", "provider lockdown"),
        ("detail", "no longer in the transcript"),
        ("inbox", "NEEDS YOU"),
        ("help", "help  esc closes"),
        ("shells", "no terminal sessions"),
        ("ssh", "ssh profiles"),
        ("monitors", "no active monitors"),
        ("model", "MODELS"),
        ("login", "API key"),
        ("talk-setup", "talk setup"),
        ("permission", "Allow once"),
        ("search", "🔎"),
        ("effort", "effort — reasoning"),
        ("theme", "theme — how haider"),
        ("history", "↑↓ / digits choose"),
    ];
    for upper in 0..ranked.len() {
        for lower in upper + 1..ranked.len() {
            let (higher, marker) = ranked[upper];
            let (lower_name, _) = ranked[lower];
            let mut model = model();
            arm(&mut model, lower_name);
            arm(&mut model, higher);
            assert_eq!(
                model.input_owner(),
                expected_owner(higher),
                "{higher} over {lower_name}"
            );
            let mut terminal = Terminal::new(TestBackend::new(118, 36)).unwrap();
            terminal
                .draw(|frame| {
                    let _ = haider_tui::render::render(&model, frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let painted = (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                painted.contains(marker),
                "{higher} over {lower_name}: missing painted owner {marker:?}"
            );
            if lower_name == "history" && higher == "permission" {
                assert!(!painted.contains("↑↓ / digits choose"));
            }
        }
    }
}

#[test]
fn i_owner_by_input_kind_matrix_keeps_parked_drafts_unchanged() {
    const OWNERS: &[&str] = &[
        "detail",
        "help",
        "search",
        "inbox",
        "shells",
        "ssh",
        "ssh-form",
        "ssh-terminal",
        "monitors",
        "lockdown",
        "model",
        "theme",
        "effort",
        "history",
        "permission",
        "plan-doc",
        "file-review-doc",
        "login",
        "accounts-replace",
        "talk-setup",
        "custom-form",
        "fleet-detail",
        "fleet",
        "sessions",
        "accounts",
        "providers",
        "usage",
        "graph",
        "tools",
        "tree",
        "hooks",
        "boot",
        "subagent-question",
    ];
    for mode in OWNERS {
        for input in ["control", "paste", "inject", "stale-composer-click"] {
            let mut model = model();
            arm(&mut model, mode);
            assert_eq!(model.input_owner(), expected_owner(mode), "{mode} state");
            let surface = model.surface_key();
            let revision = model.composer.revision();
            let epoch = model.geometry_epoch.get();
            match input {
                "control" => key(&mut model, KeyCode::Char('u'), KeyModifiers::CONTROL),
                "paste" => model.handle(AppEvent::Paste("MATRIX_PASTE".to_owned().into())),
                "inject" => {
                    LiveDriver::new("matrix").apply(
                        &mut model,
                        LiveReply::InputInjected {
                            session: session(),
                            op: haider_rpc::SurfaceInjectOp::Set {
                                text: "MATRIX_SET".into(),
                            },
                        },
                    );
                }
                "stale-composer-click" => {
                    model.composer_press(0, DRAFT, 0, surface, revision, epoch)
                }
                _ => unreachable!(),
            }
            assert_eq!(model.composer.text(), DRAFT, "{mode} × {input}");
        }
    }
    for mode in ["session", "launcher", "subagent", "aura", "loom"] {
        for input in ["control", "paste", "inject", "composer-click"] {
            let mut model = model();
            arm(&mut model, mode);
            assert!(matches!(model.input_owner(), InputOwner::Composer(_)));
            let surface = model.surface_key();
            let revision = model.composer.revision();
            let epoch = model.geometry_epoch.get();
            match input {
                "control" => {
                    key(&mut model, KeyCode::Char('u'), KeyModifiers::CONTROL);
                    assert!(model.composer.text().is_empty(), "{mode} × {input}");
                }
                "paste" => {
                    model.handle(AppEvent::Paste("MATRIX_PASTE".to_owned().into()));
                    assert!(
                        model.composer.text().contains("MATRIX_PASTE"),
                        "{mode} × {input}"
                    );
                }
                "inject" => {
                    LiveDriver::new("matrix").apply(
                        &mut model,
                        LiveReply::InputInjected {
                            session: session(),
                            op: haider_rpc::SurfaceInjectOp::Set {
                                text: "MATRIX_SET".into(),
                            },
                        },
                    );
                    let expected = if mode == "session" {
                        "MATRIX_SET"
                    } else {
                        DRAFT
                    };
                    assert_eq!(model.composer.text(), expected, "{mode} × {input}");
                }
                "composer-click" => {
                    model.composer_press(0, DRAFT, 0, surface, revision, epoch);
                    assert_eq!(model.composer.cursor(), 0, "{mode} × {input}");
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn a_mouse_selection_from_a_covered_owner_is_not_copied() {
    let mut model = model();
    dispatch_input(
        &mut model,
        &[],
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }),
    );
    dispatch_input(
        &mut model,
        &[],
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 16,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert!(model.selection.is_some());
    arm(&mut model, "help");
    model.requests.clear();
    key(
        &mut model,
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(
        model
            .requests
            .iter()
            .all(|request| !matches!(request, haider_tui::app::AppRequest::CopySelection))
    );
}

#[test]
fn back_chip_closes_fleet_detail_and_navigates_under_model_picker() {
    let mut model = model();
    arm(&mut model, "fleet-detail");
    stale_pointer(
        &mut model,
        Hit::BackChip,
        MouseEventKind::Down(MouseButton::Left),
    );
    stale_pointer(
        &mut model,
        Hit::BackChip,
        MouseEventKind::Up(MouseButton::Left),
    );
    assert!(model.fleet.detail.is_none());
    assert_eq!(model.screen, Screen::Fleet);
    let mut picker_model = model();
    arm(&mut picker_model, "model");
    stale_pointer(
        &mut picker_model,
        Hit::BackChip,
        MouseEventKind::Down(MouseButton::Left),
    );
    stale_pointer(
        &mut picker_model,
        Hit::BackChip,
        MouseEventKind::Up(MouseButton::Left),
    );
    assert_eq!(picker_model.screen, Screen::Launcher);
}

#[test]
fn slash_palette_is_not_painted_over_a_non_composer_owner() {
    for mode in [
        "search",
        "theme",
        "effort",
        "history",
        "login",
        "talk-setup",
    ] {
        let mut model = model();
        model.composer.set_text("/model");
        assert!(model.palette_open(), "plain composer control");
        arm(&mut model, mode);
        assert!(!model.palette_open(), "{mode}");
    }
}

#[test]
fn refused_mirror_edit_does_not_erase_a_newer_attachment() {
    let (mut model, mut driver) = pending_visible_talk_mirror();
    model
        .composer
        .push_attachment(haider_tui::composer::PendingAttachment {
            upload: 17,
            label: "new.txt".into(),
            kind: haider_tui::composer::PendingKind::File {
                name: "new.txt".into(),
                lines: 1,
            },
            bytes: Some(4),
            artifact: None,
            carried: None,
        });
    model.talk.phase = TalkPhase::Idle;
    driver.apply_pending_input(&mut model);
    assert_eq!(model.composer.text(), "OLD");
    assert_eq!(model.composer.attachments().len(), 1);
}

#[test]
fn refused_mirror_edit_does_not_erase_a_newer_injected_edit() {
    let (mut model, mut driver) = pending_visible_talk_mirror();
    model.talk.phase = TalkPhase::Idle;
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: session(),
            op: haider_rpc::SurfaceInjectOp::Set {
                text: "INJECTED".into(),
            },
        },
    );
    assert_eq!(model.composer.text(), "INJECTED");
    driver.apply_pending_input(&mut model);
    assert_eq!(model.composer.text(), "INJECTED");
}
