//! Usage decision ownership and restoration regressions.
//!
//! Class: a deferred spending card owns an input event only if it was
//! visible at the start of that event and still owns input at dispatch.
//! Rows prefixed `ctl_` are over-correction controls; rows prefixed `var_`
//! are class variants that must hold; rows prefixed `rep_` report behaviour
//! (they print and assert only the spend-safety invariant).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{AgentId, CredentialAlias, LeaseId, MenuId, SessionId};
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_rpc::SurfaceInjectOp;
use haider_tui::app::{
    AppEvent, AppModel, ChipModel, Hit, Pasted, PromptBacktrack, RuntimeMode, Screen,
};
use haider_tui::composer::{PendingAttachment, PendingKind};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::projection::SessionProjection;
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

mod common;
use common::{key, launcher_model, run_slash, submit};

fn card(id: &str) -> Menu {
    Menu {
        id: MenuId::new(id),
        kind: MenuKind::DeferredApproval {
            approval_id: format!("approval-{id}"),
            effect: EffectClass::UsageResetApply,
            account: Some(CredentialAlias::new("work-chatgpt")),
            binding: None,
        },
        title: "Apply a usage reset credit?".into(),
        body: vec!["Account: work-chatgpt".into()],
        options: vec![
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: None,
                decision: Some(DecisionKind::RejectOnce),
            },
            MenuOption {
                key: "approve".into(),
                label: "Apply one reset".into(),
                detail: None,
                decision: Some(DecisionKind::AllowOnce),
            },
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "deferred_approval_v1".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn blocking_permission(id: &str) -> Menu {
    Menu {
        id: MenuId::new(id),
        kind: MenuKind::Permission {
            effect_summary: "write synthetic.txt".into(),
            file_review: None,
        },
        title: "Allow write?".into(),
        body: vec![],
        options: vec![
            MenuOption {
                key: "reject".into(),
                label: "Reject".into(),
                detail: None,
                decision: Some(DecisionKind::RejectOnce),
            },
            MenuOption {
                key: "allow".into(),
                label: "Allow once".into(),
                detail: None,
                decision: Some(DecisionKind::AllowOnce),
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "permission".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn session() -> AppModel {
    let mut model = launcher_model();
    submit(&mut model, "work on the repo");
    model.outbox.clear();
    model.composer = haider_tui::composer::Composer::default();
    model.requests.clear();
    model.turn_active = false;
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        "provider_models_v1",
        "session_model_select_v1",
        "session_effort_select_v1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    model.daemon_version = Some("synthetic".into());
    let mut summaries = seed_provider_summaries();
    let anthropic = summaries
        .iter_mut()
        .find(|row| row.provider == "anthropic")
        .expect("anthropic");
    for detail in &mut anthropic.model_details {
        detail.supported_efforts = vec!["low".into(), "medium".into(), "high".into()];
        detail.default_effort = Some("medium".into());
    }
    model.providers.apply_snapshot(summaries, 1);
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.identity.provider = "anthropic".into();
    model.identity.model_short = "claude-opus-5".into();
    model.launcher_identity_pinned = true;
    model
}

fn open(model: &mut AppModel, menu: Menu) {
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
}

fn answers(model: &AppModel) -> Vec<(String, Option<String>)> {
    model
        .outbox
        .iter()
        .map(|a| (a.answer.menu.to_string(), a.answer.option_key.clone()))
        .collect()
}

fn approvals(model: &AppModel) -> usize {
    model
        .outbox
        .iter()
        .filter(|a| a.answer.option_key.as_deref() == Some("approve"))
        .count()
}

fn painted_at(model: &AppModel, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn painted(model: &AppModel) -> String {
    painted_at(model, 118, 36)
}

fn card_painted(model: &AppModel) -> bool {
    painted(model).contains("Apply one reset")
}

fn ctrl(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

/// Picker rendered over a pending card at both gate sizes: the picker paints,
/// the card does not, and render agrees with dispatch (select_menu is None).
#[test]
fn var_picker_over_card_paints_picker_at_both_sizes() {
    for (slash, marker) in [
        ("/theme", "theme"),
        ("/effort", "ffort"),
        ("/model", "odel"),
    ] {
        let mut model = session();
        run_slash(&mut model, slash);
        open(&mut model, card("og22-paint"));
        for (w, h) in [(118, 36), (80, 24)] {
            let text = painted_at(&model, w, h);
            println!(
                "[{slash} {w}x{h}] card_painted={} select_menu={}",
                text.contains("Apply one reset"),
                model.select_menu().is_some()
            );
            assert!(
                !text.contains("Apply one reset"),
                "{slash} {w}x{h}: card painted under picker"
            );
            assert!(
                text.to_lowercase().contains(marker),
                "{slash} {w}x{h}: picker not painted\n{text}"
            );
            assert!(model.select_menu().is_none());
        }
    }
}

/// Theme picker open over a card: paste, hover/click on a stale card hit,
/// injected Set("")+Submit and Enter must not answer the card.
#[test]
fn var_picker_open_every_input_kind_answers_nothing() {
    let mut model = session();
    run_slash(&mut model, "/theme");
    assert!(model.theme_picker.is_some());
    let c = card("og22-kinds");
    open(&mut model, c.clone());
    model.handle_hover(Some(Hit::MenuOption {
        menu: c.id.clone(),
        index: 1,
    }));
    model.handle_hit(Hit::MenuOption {
        menu: c.id.clone(),
        index: 1,
    });
    let sid = SessionId::new("og22-kinds");
    model.active_session = Some(sid.clone());
    let mut driver = LiveDriver::new("og22");
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: sid.clone(),
            op: SurfaceInjectOp::Set {
                text: String::new(),
            },
        },
    );
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: sid.clone(),
            op: SurfaceInjectOp::Submit,
        },
    );
    model.handle(AppEvent::SurfaceInputReplace {
        text: String::new(),
    });
    model.handle(key(KeyCode::Down));
    println!(
        "after hover/click/inject/mirror/Down: theme_open={} owner={:?} answers={:?}",
        model.theme_picker.is_some(),
        model.menu_selection_owner,
        answers(&model)
    );
    assert!(model.theme_picker.is_some(), "theme picker keeps Down");
    assert!(
        model.menu_selection_owner.is_none(),
        "no card navigation while picker open"
    );
    model.handle(AppEvent::Paste(Pasted::new(
        "pasted while the picker is up".into(),
    )));
    model.handle(key(KeyCode::Enter));
    println!(
        "after paste+Enter: theme_open={} composer={:?} answers={:?}",
        model.theme_picker.is_some(),
        model.composer.text(),
        answers(&model)
    );
    assert!(
        answers(&model).is_empty(),
        "no answer while the picker owned input: {:?}",
        answers(&model)
    );
}

/// Search (Ctrl+F) over a navigated card: the Apply navigation is retired
/// and, after Esc, the card is back at Deny.
#[test]
fn var_search_over_navigated_card_retires_apply() {
    let mut model = session();
    let c = card("og22-search");
    open(&mut model, c.clone());
    assert!(card_painted(&model));
    model.handle(key(KeyCode::Down));
    assert_eq!(model.menu_display_selection(&c), 1);
    model.handle(ctrl('f'));
    let hidden = !card_painted(&model);
    model.handle(key(KeyCode::Char('x')));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Esc));
    let shown = model
        .projection
        .open_menu()
        .map(|m| model.menu_display_selection(m));
    println!(
        "search: card_hidden_while_open={hidden} search_open={} shown_after_esc={shown:?} card_painted={}",
        model.transcript_search.is_some(),
        card_painted(&model)
    );
    model.handle(key(KeyCode::Enter));
    println!("search: answers={:?}", answers(&model));
    assert!(hidden, "card must not paint under search");
    assert_eq!(
        approvals(&model),
        0,
        "search round trip must not keep Apply"
    );
}

/// HistoryChooser open when the card arrives: chooser keeps Down/Enter.
#[test]
fn var_history_chooser_over_card_keeps_its_keys() {
    let mut model = session();
    model.prompt_history = [
        haider_tui::session::PromptEntry::committed("recent synthetic prompt".into(), 20),
        haider_tui::session::PromptEntry::committed("older synthetic prompt".into(), 10),
    ]
    .into();
    let now = std::time::Instant::now();
    model.handle_at(key(KeyCode::Esc), now);
    model.handle_at(key(KeyCode::Esc), now + std::time::Duration::from_millis(5));
    assert!(
        model.backtrack.is_some(),
        "seeded history must open the chooser"
    );
    open(&mut model, card("og22-chooser"));
    let painted_card = card_painted(&model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    println!(
        "chooser: card_painted_under_chooser={painted_card} chooser_open={} composer={:?} answers={:?}",
        model.backtrack.is_some(),
        model.composer.text(),
        answers(&model)
    );
    assert!(!painted_card);
    assert!(answers(&model).is_empty());
    assert!(model.backtrack.is_none());
    assert_eq!(model.composer.text(), "older synthetic prompt");
}

/// A blocking menu that arrives after Apply navigation: Enter answers the
/// blocking menu's option 0, never its option 1 (no stale index).
#[test]
fn var_blocking_menu_after_navigated_card_starts_at_option_zero() {
    let mut model = session();
    open(&mut model, card("og22-before-blocking"));
    model.handle(key(KeyCode::Down));
    open(&mut model, blocking_permission("og22-blocking"));
    model.handle(key(KeyCode::Enter));
    println!(
        "blocking-after-card: answers={:?} open_after={:?}",
        answers(&model),
        model.projection.open_menu().map(|m| m.id.to_string())
    );
    assert_eq!(
        answers(&model),
        vec![("og22-blocking".to_owned(), Some("reject".to_owned()))]
    );
}

/// REPORT: a card that arrives while a blocking menu is open. The single
/// projection menu slot is replaced by the card; the blocking menu is no
/// longer painted. Spend safety still holds (Enter = deny).
#[test]
fn rep_card_after_blocking_menu_single_slot() {
    let mut model = session();
    open(&mut model, blocking_permission("og22-blocking-first"));
    open(&mut model, card("og22-card-second"));
    let text = painted(&model);
    println!(
        "card-after-blocking: open_menu={:?} blocking_painted={} card_painted={}",
        model.projection.open_menu().map(|m| m.id.to_string()),
        text.contains("Allow write?") || text.contains("Allow once"),
        text.contains("Apply one reset")
    );
    model.handle(key(KeyCode::Enter));
    println!("card-after-blocking: answers={:?}", answers(&model));
    assert_eq!(approvals(&model), 0);
}

/// REPORT: Enter repeat after an explicit approve. The TUI must not emit a
/// second approve for the same card from a bare repeat; Down+Enter repeat is
/// printed (the daemon's first-committed-wins CAS is the end guard).
#[test]
fn rep_enter_repeat_after_approve() {
    let mut model = session();
    open(&mut model, card("og22-repeat"));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    model.handle(key(KeyCode::Enter));
    let bare = answers(&model);
    println!("Down,Enter,Enter: answers={bare:?}");
    assert_eq!(
        approvals(&model),
        1,
        "bare Enter repeat emitted a second approve: {bare:?}"
    );
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        answers(&model).len(),
        1,
        "locally answered card is retired after the first answer"
    );
}

/// Replacement card while the old one is navigated: Enter denies the new one.
#[test]
fn var_replacement_card_denies() {
    let mut model = session();
    open(&mut model, card("og22-a"));
    model.handle(key(KeyCode::Down));
    open(&mut model, card("og22-b"));
    let shown = model
        .projection
        .open_menu()
        .map(|m| model.menu_display_selection(m));
    model.handle(key(KeyCode::Enter));
    println!("replacement: shown={shown:?} answers={:?}", answers(&model));
    assert_eq!(
        answers(&model),
        vec![("og22-b".to_owned(), Some("deny".to_owned()))]
    );
}

/// Late visibility inside one event: a draft hides the card; Backspace to
/// empty (card becomes visible during that event) must not be a card key,
/// and the next Enter answers Deny only.
#[test]
fn var_late_visibility_backspace_then_enter_is_deny() {
    let mut model = session();
    model.handle(key(KeyCode::Char('x')));
    open(&mut model, card("og22-late"));
    assert!(!card_painted(&model));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Backspace));
    model.handle(key(KeyCode::Enter));
    println!("late: answers={:?}", answers(&model));
    assert_eq!(approvals(&model), 0);
}

/// Controls: pickers still work without a card; explicit Down+Enter on a
/// painted card approves exactly once; ordinary blocking menu digits answer.
#[test]
fn ctl_explicit_visible_down_enter_approves_once() {
    let mut model = session();
    open(&mut model, card("og22-ctl"));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        answers(&model),
        vec![("og22-ctl".to_owned(), Some("approve".to_owned()))]
    );
}

#[test]
fn ctl_picker_closes_then_card_is_answerable_by_fresh_gestures() {
    let mut model = session();
    run_slash(&mut model, "/theme");
    open(&mut model, card("og22-after-picker"));
    model.handle(key(KeyCode::Esc));
    assert!(model.theme_picker.is_none());
    assert!(card_painted(&model), "card paints once the picker closes");
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        answers(&model),
        vec![("og22-after-picker".to_owned(), Some("approve".to_owned()))]
    );
}

#[test]
fn ctl_blocking_menu_digit_still_answers() {
    let mut model = session();
    open(&mut model, blocking_permission("og22-digit"));
    model.handle(key(KeyCode::Char('2')));
    assert_eq!(
        answers(&model),
        vec![("og22-digit".to_owned(), Some("allow".to_owned()))]
    );
}

#[test]
fn ctl_typing_behind_card_is_a_message() {
    let mut model = session();
    open(&mut model, card("og22-type"));
    for c in "hello".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(model.composer.text(), "hello");
    model.handle(key(KeyCode::Enter));
    assert!(answers(&model).is_empty());
}

// Astra 24: use the actual terminal mouse dispatcher, not a pre-resolved Hit.
fn a25_frame(model: &AppModel, w: u16, h: u16) -> Vec<(ratatui::layout::Rect, Hit)> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let mut hits = Vec::new();
    terminal
        .draw(|f| hits = haider_tui::render::render(model, f))
        .unwrap();
    hits
}
// Deliberate legacy controls use the shared runtime clock seam for dwell.
// Ownership and press identity checks still run unchanged.
fn a25_mouse(
    model: &mut AppModel,
    hits: &[(ratatui::layout::Rect, Hit)],
    down: bool,
    x: u16,
    y: u16,
) {
    use ratatui::crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    haider_tui::runtime::dispatch_input_at(
        model,
        hits,
        Event::Mouse(MouseEvent {
            kind: if down {
                MouseEventKind::Down(MouseButton::Left)
            } else {
                MouseEventKind::Up(MouseButton::Left)
            },
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }),
        std::time::Instant::now() + std::time::Duration::from_secs(1),
    );
}
fn a25_apply_point(hits: &[(ratatui::layout::Rect, Hit)]) -> (u16, u16) {
    let (r, _) = hits
        .iter()
        .find(|(_, h)| matches!(h, Hit::MenuOption { index: 1, .. }))
        .unwrap();
    (r.x + 2, r.y)
}
#[test]
fn a25_mouse_press_before_card_arrives_must_not_approve() {
    let mut unexpected = Vec::new();
    for (w, h) in [(118, 36), (80, 24)] {
        let mut model = session();
        let before = a25_frame(&model, w, h);
        let mut future = session();
        open(&mut future, card("a25-new"));
        let after = a25_frame(&future, w, h);
        let (x, y) = a25_apply_point(&after);
        a25_mouse(&mut model, &before, true, x, y);
        assert!(
            model.mouse_down.is_some(),
            "press did not begin outside composer"
        );
        open(&mut model, card("a25-new"));
        let after = a25_frame(&model, w, h);
        a25_mouse(&mut model, &after, false, x, y);
        println!(
            "A24 absent-to-card {w}x{h} at ({x},{y}) answers={:?}",
            answers(&model)
        );
        if !answers(&model).is_empty() {
            unexpected.push((w, h));
        }
    }
    assert!(
        unexpected.is_empty(),
        "press began before card existed: {unexpected:?}"
    );
}
#[test]
fn a25_mouse_press_on_card_a_must_not_approve_replacement_b() {
    let mut bad = Vec::new();
    for (w, h) in [(118, 36), (80, 24)] {
        let mut model = session();
        open(&mut model, card("a25-A"));
        let before = a25_frame(&model, w, h);
        let (x, y) = a25_apply_point(&before);
        a25_mouse(&mut model, &before, true, x, y);
        open(&mut model, card("a25-B"));
        let after = a25_frame(&model, w, h);
        a25_mouse(&mut model, &after, false, x, y);
        println!("A24 replacement {w}x{h} answers={:?}", answers(&model));
        if !answers(&model).is_empty() {
            bad.push((w, h));
        }
    }
    assert!(bad.is_empty(), "press targeted A, released over B: {bad:?}");
}

#[test]
fn queued_mouse_down_from_card_a_cannot_arm_unpainted_card_b() {
    let mut model = session();
    open(&mut model, card("queued-A"));
    let first = a25_frame(&model, 118, 36);
    let (x, y) = a25_apply_point(&first);
    open(&mut model, card("queued-B"));
    a25_mouse(&mut model, &first, true, x, y);
    let second = a25_frame(&model, 118, 36);
    a25_mouse(&mut model, &second, false, x, y);
    assert!(answers(&model).is_empty());
}
#[test]
fn a25_mouse_press_under_picker_must_not_approve_after_picker_closes() {
    let mut bad = Vec::new();
    for slash in ["/theme", "/effort", "/model"] {
        let mut preview = session();
        open(&mut preview, card("a25-covered"));
        let card_hits = a25_frame(&preview, 118, 36);
        let (x, y) = a25_apply_point(&card_hits);
        let mut model = session();
        run_slash(&mut model, slash);
        open(&mut model, card("a25-covered"));
        let before = a25_frame(&model, 118, 36);
        assert!(!card_painted(&model));
        a25_mouse(&mut model, &before, true, x, y);
        model.handle(key(KeyCode::Esc));
        assert!(card_painted(&model));
        let after = a25_frame(&model, 118, 36);
        a25_mouse(&mut model, &after, false, x, y);
        println!("A24 picker-to-card {slash} answers={:?}", answers(&model));
        if !answers(&model).is_empty() {
            bad.push(slash);
        }
    }
    assert!(bad.is_empty(), "press began under overlay: {bad:?}");
}
#[test]
fn a25_mouse_visible_card_control_and_hidden_on_release() {
    for hide in [false, true] {
        let mut model = session();
        open(&mut model, card("a25-control"));
        let before = a25_frame(&model, 118, 36);
        let (x, y) = a25_apply_point(&before);
        a25_mouse(&mut model, &before, true, x, y);
        if hide {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
            )));
            assert!(!card_painted(&model), "hidden-on-release precondition");
        }
        let after = a25_frame(&model, 118, 36);
        a25_mouse(&mut model, &after, false, x, y);
        println!(
            "A24 visible-at-down hidden_at_up={hide} answers={:?}",
            answers(&model)
        );
        assert_eq!(approvals(&model), usize::from(!hide));
    }
}
#[test]
fn a25_blocking_menu_must_remain_above_later_card_and_restore_after_answer() {
    let mut model = session();
    open(&mut model, blocking_permission("a25-blocking"));
    open(&mut model, card("a25-deferred"));
    let before = painted(&model);
    model.handle(key(KeyCode::Enter));
    println!(
        "A24 blocking-then-card blocking_painted={} spend_painted={} answers={:?}",
        before.contains("Allow once"),
        before.contains("Apply one reset"),
        answers(&model)
    );
    assert_eq!(
        answers(&model),
        vec![("a25-blocking".into(), Some("reject".into()))],
        "blocking menu must retain input"
    );
}

#[test]
fn a25_keyboard_card_arrives_after_last_draw_before_queued_keys() {
    let mut model = session();
    let previous = a25_frame(&model, 118, 36);
    assert!(
        !previous
            .iter()
            .any(|(_, h)| matches!(h, Hit::MenuOption { .. }))
    );
    // The real live loop applies replies now and draws only on its next 33ms
    // tick. MenuOpened is not an immediate-presentation event.
    open(&mut model, card("a25-not-painted"));
    use ratatui::crossterm::event::Event;
    for code in [KeyCode::Down, KeyCode::Enter] {
        haider_tui::runtime::dispatch_input(
            &mut model,
            &previous,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        );
    }
    println!(
        "A24 no rendered card frame before Down+Enter: answers={:?}",
        answers(&model)
    );
    assert!(
        answers(&model).is_empty(),
        "logically visible card was never painted before input"
    );
}

#[test]
fn a25_pending_card_must_return_when_later_blocking_menu_is_answered() {
    let mut model = session();
    open(&mut model, card("a25-waiting-card"));
    open(&mut model, blocking_permission("a25-later-blocking"));
    assert!(!card_painted(&model));
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        answers(&model),
        vec![("a25-later-blocking".into(), Some("reject".into()))]
    );
    let answer = model.outbox.last().unwrap().answer.clone();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuAnswered(
        answer,
    ))));
    println!(
        "A24 after blocker answered pending_card_painted={} open={:?}",
        card_painted(&model),
        model.projection.open_menu().map(|m| m.id.to_string())
    );
    assert!(
        card_painted(&model),
        "unanswered usage card must remain reachable"
    );
}

#[test]
fn a25_mouse_press_deny_must_not_become_apply_after_resize() {
    let mut bad = Vec::new();
    for (w, h) in [(118, 36), (80, 24)] {
        let mut model = session();
        open(&mut model, card("a25-resize"));
        let before = a25_frame(&model, w, h);
        let (r, _) = before
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::MenuOption { index: 0, .. }))
            .unwrap();
        let (x, y) = (r.x + 2, r.y);
        a25_mouse(&mut model, &before, true, x, y);
        use ratatui::crossterm::event::Event;
        haider_tui::runtime::dispatch_input(&mut model, &before, Event::Resize(w, h - 1));
        let after = a25_frame(&model, w, h - 1);
        a25_mouse(&mut model, &after, false, x, y);
        println!(
            "A24 Down on Deny, shrink one row {w}x{h}: answers={:?}",
            answers(&model)
        );
        if approvals(&model) != 0 {
            bad.push((w, h));
        }
    }
    assert!(bad.is_empty(), "Deny press remapped to Apply: {bad:?}");
}

#[test]
fn a25_mouse_press_blocking_permission_must_not_approve_arriving_spend() {
    let mut model = session();
    open(&mut model, blocking_permission("a25-permission"));
    let before = a25_frame(&model, 118, 36);
    let (x, y) = a25_apply_point(&before);
    a25_mouse(&mut model, &before, true, x, y);
    open(&mut model, card("a25-arriving-credit"));
    let after = a25_frame(&model, 118, 36);
    a25_mouse(&mut model, &after, false, x, y);
    println!(
        "A24 permission press -> usage release answers={:?}",
        answers(&model)
    );
    assert_eq!(
        approvals(&model),
        0,
        "permission click must not become credit approval"
    );
}

#[test]
fn a25_keyboard_picker_closes_without_card_frame_must_not_arm_apply() {
    let mut model = session();
    run_slash(&mut model, "/theme");
    open(&mut model, card("a25-not-repainted"));
    assert!(model.theme_picker.is_some() && !card_painted(&model));
    let last = a25_frame(&model, 118, 36);
    use ratatui::crossterm::event::Event;
    for code in [KeyCode::Esc, KeyCode::Down, KeyCode::Enter] {
        haider_tui::runtime::dispatch_input(
            &mut model,
            &last,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        );
    }
    println!(
        "A24 picker closed, no card frame: answers={:?}",
        answers(&model)
    );
    assert_eq!(
        approvals(&model),
        0,
        "queued keys approved before restored card was painted"
    );
}

#[test]
fn a25_keyboard_last_frame_card_a_must_not_approve_unpainted_b() {
    let mut model = session();
    open(&mut model, card("a25-painted-A"));
    let last = a25_frame(&model, 118, 36);
    open(&mut model, card("a25-unpainted-B"));
    use ratatui::crossterm::event::Event;
    for code in [KeyCode::Down, KeyCode::Enter] {
        haider_tui::runtime::dispatch_input(
            &mut model,
            &last,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        );
    }
    println!("A24 last frame A, logical B: answers={:?}", answers(&model));
    assert!(
        answers(&model).is_empty(),
        "queued keys answered B while screen still showed A"
    );
}

#[test]
fn a25_pending_card_a_must_return_after_replacement_b_is_resolved() {
    let mut model = session();
    open(&mut model, card("a25-pending-A"));
    open(&mut model, card("a25-pending-B"));
    model.handle(key(KeyCode::Enter));
    let answer = model.outbox.last().unwrap().answer.clone();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuAnswered(
        answer,
    ))));
    println!(
        "A24 B resolved, pending A projection={:?}",
        model.projection.open_menu().map(|m| m.id.to_string())
    );
    assert_eq!(
        model.projection.open_menu().map(|m| m.id.to_string()),
        Some("a25-pending-A".into()),
        "unanswered older card must remain reachable"
    );
}

#[test]
fn a25_mouse_press_under_overlay_or_draft_must_not_confirm_restored_card() {
    let mut bad = Vec::new();
    for kind in ["search", "help", "history", "draft", "attachment"] {
        let mut model = session();
        open(&mut model, card("a25-restored"));
        let shown = a25_frame(&model, 118, 36);
        let (x, y) = a25_apply_point(&shown);
        match kind {
            "search" => model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
            ))),
            "help" => model.help_open = true,
            "history" => model.backtrack = Some(PromptBacktrack { selection: 0 }),
            "attachment" => model.composer.push_attachment(PendingAttachment {
                upload: 1,
                label: "synthetic file".into(),
                kind: PendingKind::File {
                    name: "synthetic.txt".into(),
                    lines: 1,
                },
                bytes: Some(1),
                artifact: None,
                carried: None,
            }),
            _ => model.handle(key(KeyCode::Char('x'))),
        }
        assert!(!card_painted(&model));
        let hidden = a25_frame(&model, 118, 36);
        a25_mouse(&mut model, &hidden, true, x, y);
        match kind {
            "help" => model.help_open = false,
            "history" => model.backtrack = None,
            "draft" => model.handle(key(KeyCode::Backspace)),
            "attachment" => {
                let _ = model.composer.remove_newest_attachment();
            }
            _ => model.handle(key(KeyCode::Esc)),
        }
        assert!(card_painted(&model));
        let restored = a25_frame(&model, 118, 36);
        a25_mouse(&mut model, &restored, false, x, y);
        println!(
            "A25 {kind} press, restored-card release: {:?}",
            answers(&model)
        );
        if !answers(&model).is_empty() {
            bad.push(kind);
        }
    }
    assert!(
        bad.is_empty(),
        "hidden-at-press restored-at-release: {bad:?}"
    );
}

#[test]
fn a25_keyboard_presented_card_control() {
    let mut model = session();
    open(&mut model, card("a25-presented"));
    use ratatui::crossterm::event::Event;
    for code in [KeyCode::Down, KeyCode::Enter] {
        let last = a25_frame(&model, 118, 36);
        haider_tui::runtime::dispatch_input_at(
            &mut model,
            &last,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );
    }
    assert_eq!(
        answers(&model),
        vec![("a25-presented".into(), Some("approve".into()))]
    );
}

#[test]
fn a25_mouse_press_before_card_must_not_deny_either() {
    let mut model = session();
    let before = a25_frame(&model, 80, 24);
    let mut future = session();
    open(&mut future, card("a25-unintended-deny"));
    let after = a25_frame(&future, 80, 24);
    let (r, _) = after
        .iter()
        .find(|(_, h)| matches!(h, Hit::MenuOption { index: 0, .. }))
        .unwrap();
    a25_mouse(&mut model, &before, true, r.x + 2, r.y);
    open(&mut model, card("a25-unintended-deny"));
    let after = a25_frame(&model, 80, 24);
    a25_mouse(&mut model, &after, false, r.x + 2, r.y);
    println!("A25 absent-to-card deny row: {:?}", answers(&model));
    assert!(
        answers(&model).is_empty(),
        "card must not answer at all when absent at press"
    );
}

#[test]
fn repeat_enter_and_double_click_answer_only_once_locally() {
    use ratatui::crossterm::event::Event;
    for mouse in [false, true] {
        let mut model = session();
        open(&mut model, card("answered-once"));
        let frame = a25_frame(&model, 118, 36);
        if mouse {
            let (x, y) = a25_apply_point(&frame);
            for _ in 0..2 {
                a25_mouse(&mut model, &frame, true, x, y);
                a25_mouse(&mut model, &frame, false, x, y);
            }
        } else {
            for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Enter] {
                let last = a25_frame(&model, 118, 36);
                haider_tui::runtime::dispatch_input_at(
                    &mut model,
                    &last,
                    Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                    std::time::Instant::now() + std::time::Duration::from_secs(1),
                );
            }
        }
        assert_eq!(approvals(&model), 1);
        assert_eq!(answers(&model).len(), 1);
        assert!(model.projection.open_menu().is_none());
    }
}

#[test]
fn same_menu_id_with_changed_option_must_not_retarget_mouse_press() {
    let mut model = session();
    open(&mut model, card("same-id"));
    let frame = a25_frame(&model, 118, 36);
    let (x, y) = a25_apply_point(&frame);
    a25_mouse(&mut model, &frame, true, x, y);
    let mut changed = card("same-id");
    changed.options[1].key = "different-effect".into();
    changed.options[1].decision = Some(DecisionKind::RejectOnce);
    open(&mut model, changed);
    let next = a25_frame(&model, 118, 36);
    a25_mouse(&mut model, &next, false, x, y);
    assert!(answers(&model).is_empty());
}

#[test]
fn card_hidden_and_restored_during_a_press_retires_that_press() {
    for (w, h) in [(118, 36), (80, 24)] {
        let mut model = session();
        open(&mut model, card("same-card"));
        let visible = a25_frame(&model, w, h);
        let (x, y) = a25_apply_point(&visible);
        a25_mouse(&mut model, &visible, true, x, y);
        model.help_open = true;
        let hidden = a25_frame(&model, w, h);
        assert!(
            !hidden
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::MenuOption { .. }))
        );
        model.help_open = false;
        let restored = a25_frame(&model, w, h);
        a25_mouse(&mut model, &restored, false, x, y);
        assert!(answers(&model).is_empty());
    }
}

#[test]
fn queued_keyboard_cannot_use_a_changed_menu_with_the_same_id() {
    use ratatui::crossterm::event::Event;
    let mut model = session();
    open(&mut model, card("same-id-keyboard"));
    let old = a25_frame(&model, 118, 36);
    let mut changed = card("same-id-keyboard");
    changed.options[1].key = "changed-effect".into();
    open(&mut model, changed);
    for code in [KeyCode::Down, KeyCode::Enter] {
        haider_tui::runtime::dispatch_input(
            &mut model,
            &old,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        );
    }
    assert!(answers(&model).is_empty());
}

#[test]
fn queued_keys_from_search_history_and_cleared_draft_cannot_transfer_to_card() {
    use ratatui::crossterm::event::Event;
    for owner in ["search", "history", "draft"] {
        let mut model = session();
        open(&mut model, card(owner));
        match owner {
            "search" => model.handle(ctrl('f')),
            "history" => model.backtrack = Some(PromptBacktrack { selection: 0 }),
            _ => model.handle(key(KeyCode::Char('x'))),
        }
        let frame = a25_frame(&model, 118, 36);
        match owner {
            "search" => model.handle(key(KeyCode::Esc)),
            "history" => model.backtrack = None,
            _ => model.handle(key(KeyCode::Backspace)),
        }
        for code in [KeyCode::Down, KeyCode::Enter] {
            haider_tui::runtime::dispatch_input(
                &mut model,
                &frame,
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            );
        }
        assert!(answers(&model).is_empty(), "owner={owner}");
    }
}

#[test]
fn child_projection_restores_its_pending_decisions_without_a_demo_question_mirror() {
    let mut child = ChipModel::from_manifest(&AgentManifest {
        agent: AgentId::new("synthetic-child"),
        role: AgentRole::Subagent,
        task: "synthetic task".into(),
        callsign: Some("Child".into()),
        model_profile: "synthetic".into(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        parent: None,
        placement: Placement::Local,
        coordinates: None,
        cli_scope: None,
        lease: LeaseId::new("synthetic-lease"),
        fencing_epoch: 1,
        attempt: 0,
    });
    child.state = ChipDisplayState::InputRequired;
    let mut pending = card("child-spend");
    pending.scope = MenuScope::Subagent {
        agent: AgentId::new("synthetic-child"),
    };
    let mut blocking = blocking_permission("child-question");
    blocking.scope = pending.scope.clone();
    child.transcript.apply(&EventPayload::MenuOpened(pending));
    child.transcript.apply(&EventPayload::MenuOpened(blocking));
    assert_eq!(
        child.question_menu().map(|menu| menu.id.as_str()),
        Some("child-question")
    );
    child
        .transcript
        .retire_local_answer(&MenuId::new("child-question"));
    assert_eq!(
        child.question_menu().map(|menu| menu.id.as_str()),
        Some("child-spend")
    );
}

#[test]
fn chip_mouse_press_cannot_cross_from_card_a_to_card_b() {
    let mut model = session();
    let mut chip = ChipModel::from_seed(ChipSeed {
        agent: "synthetic-child".into(),
        parent: None,
        ros: None,
        callsign: "Child".into(),
        hon: "(r)",
        full: "Synthetic child".into(),
        name: "synthetic task".into(),
        model: "synthetic".into(),
        device: "local".into(),
        state: ChipDisplayState::InputRequired,
        tokens: 10,
        prefill: Vec::new(),
    });
    let mut a = card("child-A");
    a.scope = MenuScope::Subagent {
        agent: AgentId::new("synthetic-child"),
    };
    chip.transcript.apply(&EventPayload::MenuOpened(a));
    model.chips.push(chip);
    model.screen = Screen::Subagent;
    model.view_path = vec!["synthetic-child".into()];
    let first = a25_frame(&model, 118, 36);
    let (x, y) = a25_apply_point(&first);
    a25_mouse(&mut model, &first, true, x, y);
    let mut b = card("child-B");
    b.scope = MenuScope::Subagent {
        agent: AgentId::new("synthetic-child"),
    };
    model.chips[0]
        .transcript
        .apply(&EventPayload::MenuOpened(b));
    let next = a25_frame(&model, 118, 36);
    a25_mouse(&mut model, &next, false, x, y);
    assert!(answers(&model).is_empty());
    a25_mouse(&mut model, &next, true, x, y);
    a25_mouse(&mut model, &next, false, x, y);
    assert_eq!(
        answers(&model),
        vec![("child-B".into(), Some("approve".into()))]
    );
}

#[test]
fn queued_enter_from_closed_picker_or_search_cannot_submit_composer() {
    use ratatui::crossterm::event::Event;
    for owner in ["theme", "search"] {
        let mut model = session();
        if owner == "theme" {
            run_slash(&mut model, "/theme");
        } else {
            model.handle(ctrl('f'));
        }
        let frame = a25_frame(&model, 118, 36);
        model.handle(key(KeyCode::Esc));
        model.composer.set_text("synthetic draft");
        let before = model.requests.len();
        haider_tui::runtime::dispatch_input(
            &mut model,
            &frame,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(model.requests.len(), before, "owner={owner}");
    }
}

#[test]
fn replayed_menu_arrival_requires_a_new_painted_frame_before_keyboard_answer() {
    use ratatui::crossterm::event::Event;
    let mut model = session();
    let before_reattach = a25_frame(&model, 118, 36);
    model.projection = SessionProjection::new();
    open(&mut model, card("reattached-card"));
    for code in [KeyCode::Down, KeyCode::Enter] {
        haider_tui::runtime::dispatch_input(
            &mut model,
            &before_reattach,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        );
    }
    assert!(answers(&model).is_empty());
    // The production loop paints the navigated selection before a fresh
    // confirmation. The shared clock seam also gives this control its dwell.
    for code in [KeyCode::Down, KeyCode::Enter] {
        let current = a25_frame(&model, 118, 36);
        haider_tui::runtime::dispatch_input_at(
            &mut model,
            &current,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );
    }
    assert_eq!(approvals(&model), 1);
}

#[test]
fn selection_scratch_render_does_not_present_an_arriving_card() {
    use ratatui::crossterm::event::Event;
    let mut model = session();
    let before = a25_frame(&model, 118, 36);
    open(&mut model, card("scratch-arrival"));
    let selection = haider_tui::select::Selection {
        anchor: (0, 0),
        head: (1, 0),
        dragging: false,
    };
    let _ = haider_tui::runtime::rendered_selection_text(
        &model,
        ratatui::layout::Size {
            width: 118,
            height: 36,
        },
        &selection,
    );
    for code in [KeyCode::Down, KeyCode::Enter] {
        haider_tui::runtime::dispatch_input(
            &mut model,
            &before,
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        );
    }
    assert!(answers(&model).is_empty());
}
