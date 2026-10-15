#![allow(clippy::unwrap_used)]

mod common;

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
    runtime::dispatch_input_at,
    script::{ChipDisplayState, ChipSeed},
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::Rect,
};

// These legacy controls exercise deliberate interaction with an already
// presented surface. Fresh-owner timing is tested in runtime_pointer_tests.
fn dispatch_input(model: &mut AppModel, hits: &[(Rect, Hit)], event: Event) {
    dispatch_input_at(
        model,
        hits,
        event,
        std::time::Instant::now() + std::time::Duration::from_millis(500),
    );
}

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
            model.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
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

// ======================================================================
// Opus 11 gate probes, corrected for the Ctrl+C form UX and retained as regressions.
// ======================================================================

fn frame(model: &AppModel, w: u16, h: u16) -> Vec<(Rect, Hit)> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let mut hits = Vec::new();
    terminal
        .draw(|f| hits = haider_tui::render::render(model, f))
        .unwrap();
    hits
}

fn mouse(model: &mut AppModel, hits: &[(Rect, Hit)], kind: MouseEventKind, x: u16, y: u16) {
    dispatch_input(
        model,
        hits,
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }),
    );
}

fn dkey(model: &mut AppModel, hits: &[(Rect, Hit)], code: KeyCode) {
    dispatch_input(
        model,
        hits,
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
    );
}

fn answers(model: &AppModel) -> Vec<(String, Option<String>)> {
    model
        .outbox
        .iter()
        .map(|a| (a.answer.menu.to_string(), a.answer.option_key.clone()))
        .collect()
}

fn option_cell(hits: &[(Rect, Hit)], index: usize) -> Option<(u16, u16)> {
    hits.iter().find_map(|(r, h)| match h {
        Hit::MenuOption { index: i, .. } if *i == index => Some((r.x + 2, r.y)),
        _ => None,
    })
}

fn card(id: &str) -> Menu {
    let mut menu = permission();
    menu.id = MenuId::new(id);
    menu.title = format!("Allow {id}?");
    for option in &mut menu.options {
        option.key = format!("{}-{id}", option.key);
    }
    menu
}

fn open(model: &mut AppModel, menu: Menu) {
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
}

/// Close whatever cover is up with the user's own Esc key, as a real user
/// would while still holding the mouse button.
fn close_cover(model: &mut AppModel, hits: &[(Rect, Hit)]) {
    for _ in 0..3 {
        if model.input_owner() == InputOwner::SessionMenu {
            return;
        }
        dkey(model, hits, KeyCode::Esc);
    }
}

// ---------------- B1: press -> release target binding ----------------

#[test]
fn var_b1a_press_before_permission_card_must_not_answer_it() {
    let mut bad = Vec::new();
    for (w, h) in [(118, 36), (80, 24)] {
        for index in [0usize, 1] {
            let mut m = model();
            let before = frame(&m, w, h);
            let mut future = model();
            open(&mut future, card("b1a"));
            let (x, y) = option_cell(&frame(&future, w, h), index).unwrap();
            mouse(
                &mut m,
                &before,
                MouseEventKind::Down(MouseButton::Left),
                x,
                y,
            );
            let armed = m.mouse_down.is_some();
            open(&mut m, card("b1a"));
            let after = frame(&m, w, h);
            mouse(&mut m, &after, MouseEventKind::Up(MouseButton::Left), x, y);
            println!(
                "B1a {w}x{h} option{index} at ({x},{y}) press_armed={armed} answers={:?}",
                answers(&m)
            );
            if !m.outbox.is_empty() {
                bad.push((w, h, index));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "press made before the card existed answered it: {bad:?}"
    );
}

#[test]
fn var_b1b_press_on_card_a_must_not_answer_replacement_b() {
    let mut bad = Vec::new();
    for (w, h) in [(118, 36), (80, 24)] {
        let mut m = model();
        open(&mut m, card("A"));
        let a = frame(&m, w, h);
        let (x, y) = option_cell(&a, 1).unwrap();
        mouse(&mut m, &a, MouseEventKind::Down(MouseButton::Left), x, y);
        open(&mut m, card("B"));
        let b = frame(&m, w, h);
        mouse(&mut m, &b, MouseEventKind::Up(MouseButton::Left), x, y);
        println!(
            "B1b {w}x{h} projection={:?} answers={:?}",
            m.projection.open_menu().map(|m| m.id.to_string()),
            answers(&m)
        );
        if answers(&m).iter().any(|(menu, _)| menu == "B") {
            bad.push((w, h));
        }
    }
    assert!(bad.is_empty(), "press on A answered B: {bad:?}");
}

#[test]
fn var_b1c_deny_press_must_not_become_allow_after_resize() {
    let mut bad = Vec::new();
    for (w, h) in [(118u16, 36u16), (80, 24)] {
        for shrink in 1..=3u16 {
            let mut m = model();
            open(&mut m, card("R"));
            let before = frame(&m, w, h);
            let (x, y) = option_cell(&before, 1).unwrap(); // Deny
            mouse(
                &mut m,
                &before,
                MouseEventKind::Down(MouseButton::Left),
                x,
                y,
            );
            dispatch_input(&mut m, &before, Event::Resize(w, h - shrink));
            let after = frame(&m, w, h - shrink);
            mouse(&mut m, &after, MouseEventKind::Up(MouseButton::Left), x, y);
            println!("B1c {w}x{h} shrink {shrink}: answers={:?}", answers(&m));
            if answers(&m)
                .iter()
                .any(|(_, k)| k.as_deref() == Some("allow-R"))
            {
                bad.push((w, h, shrink));
            }
        }
    }
    assert!(bad.is_empty(), "Deny press became Allow: {bad:?}");
}

#[test]
fn press_is_retired_on_resize_even_when_the_option_stays_under_the_pointer() {
    let mut m = model();
    open(&mut m, card("resize"));
    let hits = frame(&m, 118, 36);
    let (x, y) = option_cell(&hits, 0).unwrap();
    mouse(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), x, y);
    assert!(m.mouse_down.is_some());
    dispatch_input(&mut m, &hits, Event::Resize(118, 36));
    assert!(m.mouse_down.is_none());
    mouse(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), x, y);
    assert!(m.outbox.is_empty());
}

#[test]
fn press_is_retired_when_the_same_hit_reflows_to_a_new_rectangle() {
    let mut m = model();
    open(&mut m, card("reflow"));
    let hits = frame(&m, 118, 36);
    let (x, y) = option_cell(&hits, 0).unwrap();
    mouse(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), x, y);
    let mut reflowed = hits.clone();
    let (rect, _) = reflowed
        .iter_mut()
        .find(|(rect, hit)| {
            x >= rect.x
                && x < rect.x + rect.width
                && y >= rect.y
                && y < rect.y + rect.height
                && matches!(hit, Hit::MenuOption { .. })
        })
        .unwrap();
    rect.width += 1;
    mouse(
        &mut m,
        &reflowed,
        MouseEventKind::Up(MouseButton::Left),
        x,
        y,
    );
    assert!(m.outbox.is_empty());
}

#[test]
fn var_b1d_press_under_a_cover_must_not_answer_the_card_it_reveals() {
    let mut bad = Vec::new();
    for cover in [
        "detail", "inbox", "help", "model", "ssh", "ssh-form", "shells", "monitors",
    ] {
        for (w, h) in [(118, 36), (80, 24)] {
            let mut m = model();
            arm(&mut m, "permission");
            let shown = frame(&m, w, h);
            let (x, y) = option_cell(&shown, 0).unwrap(); // Allow once
            arm(&mut m, cover);
            let covered_owner = m.input_owner();
            let hidden = frame(&m, w, h);
            let hidden_has_option = hidden
                .iter()
                .any(|(_, h)| matches!(h, Hit::MenuOption { .. }));
            mouse(
                &mut m,
                &hidden,
                MouseEventKind::Down(MouseButton::Left),
                x,
                y,
            );
            let mut current = hidden.clone();
            for _ in 0..4 {
                if m.input_owner() == InputOwner::SessionMenu {
                    break;
                }
                dkey(&mut m, &current, KeyCode::Esc);
                current = frame(&m, w, h);
            }
            assert_eq!(
                m.input_owner(),
                InputOwner::SessionMenu,
                "{cover} did not reveal the card at {w}x{h}"
            );
            let revealed = frame(&m, w, h);
            mouse(
                &mut m,
                &revealed,
                MouseEventKind::Up(MouseButton::Left),
                x,
                y,
            );
            println!(
                "B1d {cover} {w}x{h} owner_at_press={covered_owner:?} hidden_frame_option={hidden_has_option} owner_at_release={:?} answers={:?}",
                m.input_owner(),
                answers(&m)
            );
            if !m.outbox.is_empty() {
                bad.push((cover, w, h));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "press under a cover answered the revealed card: {bad:?}"
    );
}

#[test]
fn session_menu_preempts_local_picker_state_and_remains_actionable() {
    // These four local surfaces sit below a newly arrived permission card.
    // They cannot be used as a cover over that card: the card is already the
    // painted owner, and its visible answer remains a positive control.
    for picker in ["theme", "effort", "search", "history"] {
        let mut m = model();
        arm(&mut m, "permission");
        arm(&mut m, picker);
        assert_eq!(m.input_owner(), InputOwner::SessionMenu, "{picker}");
        let hits = frame(&m, 118, 36);
        assert!(option_cell(&hits, 0).is_some(), "{picker} hid the card");
        dkey(&mut m, &hits, KeyCode::Enter);
        assert_eq!(
            answers(&m),
            vec![("input-owner-permission".into(), Some("allow".into()))],
            "{picker}"
        );
    }
}

#[test]
fn ctl_b1_visible_card_click_answers_once() {
    for (w, h) in [(118, 36), (80, 24)] {
        let mut m = model();
        arm(&mut m, "permission");
        let hits = frame(&m, w, h);
        let (x, y) = option_cell(&hits, 1).unwrap();
        mouse(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), x, y);
        mouse(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), x, y);
        assert_eq!(
            answers(&m),
            vec![("input-owner-permission".into(), Some("deny".into()))],
            "{w}x{h}"
        );
    }
}

#[test]
fn ctl_b1_visible_card_click_after_redraw_of_same_card_answers() {
    // A cadence redraw of the SAME card between press and release must not
    // break an ordinary click (over-correction control for a B1 repair).
    let mut m = model();
    arm(&mut m, "permission");
    let a = frame(&m, 118, 36);
    let (x, y) = option_cell(&a, 0).unwrap();
    mouse(&mut m, &a, MouseEventKind::Down(MouseButton::Left), x, y);
    m.dirty = true;
    let b = frame(&m, 118, 36);
    mouse(&mut m, &b, MouseEventKind::Up(MouseButton::Left), x, y);
    assert_eq!(
        answers(&m),
        vec![("input-owner-permission".into(), Some("allow".into()))]
    );
}

#[test]
fn a_press_cannot_survive_a_different_painted_owner_and_return_to_the_same_card() {
    let mut m = model();
    open(&mut m, card("returned"));
    let card_hits = frame(&m, 118, 36);
    let (x, y) = option_cell(&card_hits, 0).unwrap();
    mouse(
        &mut m,
        &card_hits,
        MouseEventKind::Down(MouseButton::Left),
        x,
        y,
    );
    m.help_open = true;
    frame(&m, 118, 36);
    m.help_open = false;
    let returned = frame(&m, 118, 36);
    mouse(
        &mut m,
        &returned,
        MouseEventKind::Up(MouseButton::Left),
        x,
        y,
    );
    assert!(m.outbox.is_empty());
}

// ---------------- B2: keys vs the presented owner ----------------

#[test]
fn var_b2a_keys_before_card_is_painted_must_not_answer_it() {
    let mut rows = Vec::new();
    for keys in [
        vec![KeyCode::Enter],
        vec![KeyCode::Char('1')],
        vec![KeyCode::Down, KeyCode::Enter],
    ] {
        let mut m = model();
        m.composer.clear();
        let last = frame(&m, 118, 36); // no card painted
        open(&mut m, card("unpainted"));
        for k in &keys {
            dkey(&mut m, &last, *k);
        }
        println!("B2a keys={keys:?} answers={:?}", answers(&m));
        if !m.outbox.is_empty() {
            rows.push(format!("{keys:?}"));
        }
    }
    assert!(
        rows.is_empty(),
        "keys typed before the card was painted answered it: {rows:?}"
    );
}

#[test]
fn var_b2b_keys_seen_on_card_a_must_not_answer_unpainted_b() {
    let mut m = model();
    open(&mut m, card("A"));
    let last = frame(&m, 118, 36);
    open(&mut m, card("B"));
    dkey(&mut m, &last, KeyCode::Enter);
    println!("B2b answers={:?}", answers(&m));
    assert!(
        !answers(&m).iter().any(|(menu, _)| menu == "B"),
        "Enter aimed at painted A answered unpainted B"
    );
}

#[test]
fn free_text_menu_arrival_changes_the_presented_target_within_the_composer_owner() {
    let mut m = model();
    let before = frame(&m, 118, 36);
    let prior_owner = m.input_owner();
    let mut question = card("free-text");
    question.options.clear();
    open(&mut m, question);
    assert_eq!(m.input_owner(), prior_owner);
    dkey(&mut m, &before, KeyCode::Enter);
    assert!(m.outbox.is_empty(), "unpainted answer line consumed Enter");
    let current = frame(&m, 118, 36);
    dkey(&mut m, &current, KeyCode::Enter);
    assert_eq!(answers(&m), vec![("free-text".into(), None)]);
}

#[test]
fn standalone_permission_grant_replacement_changes_the_presented_target() {
    use haider_protocol::permission::{
        PermissionGrantAction, PermissionGrantNeeded, SystemPermission,
    };
    let card = |request_id: &str| PermissionGrantNeeded {
        request_id: request_id.into(),
        menu_id: MenuId::new("grant-menu"),
        request_seq: 1,
        opening_generation: 1,
        call_id: "synthetic-call".into(),
        effect_id: EffectId::new("synthetic-effect"),
        permission: SystemPermission::ScreenRecording,
        pane_name: "Synthetic pane".into(),
        settings_url: "x-apple.systempreferences:com.apple.preference.security".into(),
        actions: vec![PermissionGrantAction::OpenSettings],
        auto_restart_pending: false,
        poll_timeout_ms: 1000,
    };
    let mut m = model();
    let before = frame(&m, 118, 36);
    m.projection.set_permission_card(card("grant-A"));
    dkey(&mut m, &before, KeyCode::Char('o'));
    assert!(m.requests.is_empty(), "unpainted grant acted on a key");
    let first = frame(&m, 118, 36);
    m.projection.set_permission_card(card("grant-B"));
    dkey(&mut m, &first, KeyCode::Char('o'));
    assert!(
        m.requests.is_empty(),
        "old grant's key acted on replacement"
    );
}

#[test]
fn screen_confirmation_must_be_painted_before_enter_can_commit_it() {
    let mut accounts = model();
    accounts.screen = Screen::Accounts;
    let old = frame(&accounts, 118, 36);
    let alias = "synthetic-account".to_owned();
    accounts.accounts.pending_remove = Some(alias.clone());
    accounts.accounts.message = Some("remove account?".into());
    dkey(&mut accounts, &old, KeyCode::Enter);
    assert_eq!(accounts.accounts.pending_remove, Some(alias.clone()));
    assert!(accounts.requests.is_empty());
    let current = frame(&accounts, 118, 36);
    dkey(&mut accounts, &current, KeyCode::Enter);
    assert!(accounts.accounts.pending_remove.is_none());
    assert!(accounts.requests.iter().any(|request| matches!(
        request,
        haider_tui::app::AppRequest::AccountRemove { alias: removed } if removed == &alias
    )));

    let mut providers = model();
    providers.screen = Screen::Providers;
    let old = frame(&providers, 118, 36);
    providers.providers.pending_remove = Some("synthetic-provider".into());
    providers.providers.message = Some("remove provider?".into());
    dkey(&mut providers, &old, KeyCode::Enter);
    assert_eq!(
        providers.providers.pending_remove.as_deref(),
        Some("synthetic-provider")
    );
    let current = frame(&providers, 118, 36);
    dkey(&mut providers, &current, KeyCode::Enter);
    assert!(providers.providers.pending_remove.is_none());
}

#[test]
fn var_b2c_queued_keys_after_cover_closes_must_not_answer_unpainted_card() {
    let mut bad = Vec::new();
    for cover in ["detail", "inbox", "help", "model", "ssh-form"] {
        let mut m = model();
        arm(&mut m, "permission");
        arm(&mut m, cover);
        let last = frame(&m, 118, 36); // cover painted, card not
        // One burst: close the cover, then the next keys, with no frame between.
        close_cover(&mut m, &last);
        dkey(&mut m, &last, KeyCode::Enter);
        println!("B2c {cover}: answers={:?}", answers(&m));
        if !m.outbox.is_empty() {
            bad.push(cover);
        }
    }
    assert!(
        bad.is_empty(),
        "queued Enter answered a card that was never painted: {bad:?}"
    );
}

#[test]
fn ctl_b2_painted_card_enter_answers() {
    let mut m = model();
    arm(&mut m, "permission");
    let last = frame(&m, 118, 36);
    dkey(&mut m, &last, KeyCode::Enter);
    assert_eq!(
        answers(&m),
        vec![("input-owner-permission".into(), Some("allow".into()))]
    );
}

// ---------------- Owner control chords: modifiers must not become text ----------------

#[test]
fn var_custom_provider_form_ctrl_chords_do_not_type_letters() {
    let mut bad = Vec::new();
    for c in ['c', 't', 'u', 'w', 'a', 'k'] {
        let mut m = model();
        arm(&mut m, "custom-form");
        m.custom_add.as_mut().unwrap().focus = haider_tui::app::CustomField::Key;
        let hits = frame(&m, 118, 36);
        dispatch_input(
            &mut m,
            &hits,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        );
        let card = m.custom_add.as_ref().unwrap();
        assert_eq!(card.masked_key_len(), 1, "plain masked-key input must work");
        let before = (
            card.name.clone(),
            card.origin.clone(),
            card.cursor,
            card.masked_key_len(),
        );
        dispatch_input(
            &mut m,
            &hits,
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
        );
        let after = m.custom_add.as_ref().map(|card| {
            (
                card.name.clone(),
                card.origin.clone(),
                card.cursor,
                card.masked_key_len(),
            )
        });
        println!(
            "custom form Ctrl+{c}: before={before:?} after={after:?} owner={:?}",
            m.input_owner()
        );
        if (c == 'c' && after.is_some()) || (c != 'c' && after.as_ref() != Some(&before)) {
            bad.push(c);
        }
    }
    assert!(
        bad.is_empty(),
        "Ctrl chords typed literal letters into the custom provider form: {bad:?}"
    );
}

#[test]
fn var_talk_setup_ctrl_chords_do_not_type_letters() {
    let mut bad = Vec::new();
    for c in ['c', 't', 'u'] {
        let mut m = model();
        arm(&mut m, "talk-setup");
        m.talk_setup.as_mut().unwrap().stage = haider_tui::talk::SetupStage::Language;
        let before = m.talk_setup.as_ref().unwrap().language.clone();
        key(&mut m, KeyCode::Char(c), KeyModifiers::CONTROL);
        let after = m.talk_setup.as_ref().map(|card| card.language.clone());
        println!("talk setup (language) Ctrl+{c}: before={before:?} after={after:?}");
        if (c == 'c' && after.is_some()) || after.as_deref().is_some_and(|a| a != before) {
            bad.push(c);
        }
    }
    assert!(
        bad.is_empty(),
        "Ctrl chords typed literal letters into talk setup: {bad:?}"
    );
}

#[test]
fn ctl_custom_provider_form_plain_typing_still_edits() {
    let mut m = model();
    arm(&mut m, "custom-form");
    let before = m
        .custom_add
        .as_ref()
        .map(|c| (c.name.clone(), c.origin.clone(), c.masked_key_len()));
    key(&mut m, KeyCode::Char('x'), KeyModifiers::NONE);
    let after = m
        .custom_add
        .as_ref()
        .map(|c| (c.name.clone(), c.origin.clone(), c.masked_key_len()));
    assert_ne!(
        before, after,
        "plain typing must still edit the focused field"
    );
}

#[test]
fn ctl_ssh_terminal_and_form_and_login_chords() {
    // Re-assert the r2 repairs through dispatch_input (not handle_key).
    for (code, byte) in [('c', 0x03u8), ('t', 0x14)] {
        let mut m = model();
        arm(&mut m, "ssh-terminal");
        m.ssh_terminal.as_mut().unwrap().shell_id = Some("fake-local".into());
        common::mark_ssh_terminal_running(&mut m, "fake-local");
        m.requests.clear();
        dispatch_input(
            &mut m,
            &[],
            Event::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)),
        );
        assert_eq!(m.screen, Screen::Session);
        assert!(m.requests.iter().any(|r| matches!(r,
            haider_tui::app::AppRequest::SshShellInput { input, .. } if input.as_slice() == [byte])));
    }
    let mut m = model();
    arm(&mut m, "login");
    dispatch_input(
        &mut m,
        &[],
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(m.login.is_none());
    assert_eq!(m.screen, Screen::Session);
}

#[test]
fn var_ctrl_shift_c_without_selection_is_not_an_owner_keystroke() {
    // Pre-toolview, Ctrl+Shift+C always returned before any owner (copy chord,
    // "never navigate/quit when there is no selection"). The r2 early owner
    // route now hands it to text-field owners when nothing is selected.
    let cs = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    let mut bad = Vec::new();
    let mut m = model();
    arm(&mut m, "ssh-terminal");
    m.ssh_terminal.as_mut().unwrap().shell_id = Some("fake-local".into());
    m.requests.clear();
    dispatch_input(
        &mut m,
        &[],
        Event::Key(KeyEvent::new(KeyCode::Char('C'), cs)),
    );
    let sent: Vec<Vec<u8>> = m
        .requests
        .iter()
        .filter_map(|r| match r {
            haider_tui::app::AppRequest::SshShellInput { input, .. } => {
                Some(input.as_slice().to_vec())
            }
            _ => None,
        })
        .collect();
    println!("ssh terminal Ctrl+Shift+C (no selection) sent={sent:?}");
    if sent.iter().any(|b| b.as_slice() == [0x03]) {
        bad.push("ssh-terminal sends ^C interrupt");
    }
    let mut m = model();
    arm(&mut m, "custom-form");
    let before = m
        .custom_add
        .as_ref()
        .map(|c| (c.name.clone(), c.origin.clone()));
    dispatch_input(
        &mut m,
        &[],
        Event::Key(KeyEvent::new(KeyCode::Char('C'), cs)),
    );
    let after = m
        .custom_add
        .as_ref()
        .map(|c| (c.name.clone(), c.origin.clone()));
    println!("custom form Ctrl+Shift+C before={before:?} after={after:?}");
    if before != after {
        bad.push("custom-form types C");
    }
    assert!(bad.is_empty(), "{bad:?}");
}

#[test]
fn custom_and_talk_masked_fields_reject_modified_letters() {
    for c in ['t', 'u', 'w', 'a', 'k'] {
        let mut m = model();
        arm(&mut m, "custom-form");
        let card = m.custom_add.as_mut().unwrap();
        card.focus = haider_tui::app::CustomField::Key;
        let before = card.masked_key_len();
        dkey_modified(&mut m, KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(
            m.custom_add.as_ref().unwrap().masked_key_len(),
            before,
            "custom Ctrl+{c}"
        );
    }
    for c in ['t', 'u'] {
        let mut m = model();
        arm(&mut m, "talk-setup");
        m.talk_setup.as_mut().unwrap().stage = haider_tui::talk::SetupStage::DeepgramKey;
        dkey_modified(&mut m, KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(
            m.talk_setup.as_ref().unwrap().key_is_empty(),
            "talk Ctrl+{c}"
        );
    }
}

fn dkey_modified(model: &mut AppModel, code: KeyCode, modifiers: KeyModifiers) {
    dispatch_input(model, &[], Event::Key(KeyEvent::new(code, modifiers)));
}

#[test]
fn terminal_close_accepts_decoded_ctrl_bracket_and_connecting_state() {
    for code in [
        KeyCode::Char('5'),
        KeyCode::Char(']'),
        KeyCode::Char('\u{001d}'),
    ] {
        let mut m = model();
        arm(&mut m, "ssh-terminal");
        m.ssh_terminal.as_mut().unwrap().shell_id = Some("fake-local".into());
        m.requests.clear();
        dkey_modified(&mut m, code, KeyModifiers::CONTROL);
        assert!(
            m.requests.iter().any(|r| matches!(r,
            haider_tui::app::AppRequest::ShellClose { id } if id == "fake-local")),
            "{code:?}"
        );
    }
    let mut m = model();
    arm(&mut m, "ssh-terminal");
    assert!(m.ssh_terminal.as_ref().unwrap().shell_id.is_none());
    dkey_modified(&mut m, KeyCode::Char('5'), KeyModifiers::CONTROL);
    assert!(m.ssh_terminal.is_none(), "connecting terminal must close");
}

#[test]
fn input_from_an_attached_session_does_not_cross_reattach() {
    let mut m = model();
    let old = frame(&m, 118, 36);
    let next = SessionId::new("another-session");
    m.upsert_live_session(&next);
    m.open_session(&next);
    m.composer.clear();
    dispatch_input(
        &mut m,
        &old,
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
    );
    assert_eq!(m.composer.text(), "", "old session's key crossed reattach");
    let current = frame(&m, 118, 36);
    dispatch_input(
        &mut m,
        &current,
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
    );
    assert_eq!(m.composer.text(), "x", "painted session still types");
}

#[test]
fn owner_and_focus_changes_retire_the_pending_press() {
    let mut m = model();
    arm(&mut m, "permission");
    let hits = frame(&m, 118, 36);
    let (x, y) = option_cell(&hits, 1).unwrap();
    mouse(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), x, y);
    assert!(m.mouse_down.is_some());
    m.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert!(m.mouse_down.is_none(), "focus move retained press");
    let moved = frame(&m, 118, 36);
    let (x, y) = option_cell(&moved, 0).unwrap();
    mouse(
        &mut m,
        &moved,
        MouseEventKind::Down(MouseButton::Left),
        x,
        y,
    );
    assert!(m.mouse_down.is_some());
    m.inbox_open = true;
    m.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert!(m.mouse_down.is_none(), "cover close retained press");
}

#[test]
fn queued_enter_cannot_answer_a_selection_that_was_not_presented() {
    let mut m = model();
    arm(&mut m, "permission");
    let first = frame(&m, 118, 36);
    dkey(&mut m, &first, KeyCode::Down);
    dkey(&mut m, &first, KeyCode::Enter);
    assert!(m.outbox.is_empty(), "Enter answered an unseen selection");
    let selected = frame(&m, 118, 36);
    dkey(&mut m, &selected, KeyCode::Enter);
    assert_eq!(
        answers(&m),
        vec![("input-owner-permission".into(), Some("deny".into()))]
    );
}

#[test]
fn queued_mouse_down_cannot_start_a_press_on_a_new_card() {
    let mut m = model();
    open(&mut m, card("A"));
    let old = frame(&m, 118, 36);
    let (x, y) = option_cell(&old, 0).unwrap();
    open(&mut m, card("B"));
    mouse(&mut m, &old, MouseEventKind::Down(MouseButton::Left), x, y);
    assert!(m.mouse_down.is_none(), "old frame armed a press on B");
    let current = frame(&m, 118, 36);
    mouse(
        &mut m,
        &current,
        MouseEventKind::Up(MouseButton::Left),
        x,
        y,
    );
    assert!(m.outbox.is_empty(), "old press answered B");
}

#[test]
fn a_reordered_session_row_does_not_take_an_old_press() {
    let mut m = model();
    let prior = m.active_session.clone();
    let second = SessionId::new("second-session");
    m.upsert_live_session(&second);
    m.screen = Screen::Launcher;
    let first_row = [(Rect::new(0, 0, 20, 1), Hit::AttachSession(session()))];
    let reordered = [(Rect::new(0, 0, 20, 1), Hit::AttachSession(second))];
    mouse(
        &mut m,
        &first_row,
        MouseEventKind::Down(MouseButton::Left),
        1,
        0,
    );
    mouse(
        &mut m,
        &reordered,
        MouseEventKind::Up(MouseButton::Left),
        1,
        0,
    );
    assert_eq!(m.active_session, prior);
}

#[test]
fn dispatch_retires_a_press_after_a_direct_live_projection_change() {
    let mut m = model();
    let old = frame(&m, 118, 36);
    mouse(&mut m, &old, MouseEventKind::Down(MouseButton::Left), 5, 10);
    assert!(m.mouse_down.is_some());
    m.projection.apply(&EventPayload::MenuOpened(card("live")));
    dispatch_input(&mut m, &old, Event::FocusGained);
    assert!(m.mouse_down.is_none());
}
