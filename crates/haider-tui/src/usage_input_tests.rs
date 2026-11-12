//! Usage plugs into the shared runtime; it must not implement its own dwell
//! or queue policy. These tests exercise the stack through that runtime.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::input_typeahead_tests::{k, loop_present};
use super::*;
use crate::app::{ChipModel, Hit, InputOwner, Screen};
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::MenuId;
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use ratatui::crossterm::event::{KeyEvent, MouseEvent};

fn card(id: &str) -> Menu {
    Menu {
        id: MenuId::new(id),
        kind: MenuKind::DeferredApproval {
            approval_id: id.into(),
            effect: EffectClass::UsageResetApply,
            account: None,
            binding: None,
        },
        title: "Spend one synthetic reset credit?".into(),
        body: vec![],
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

fn stack(child: bool, count: usize) -> AppModel {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    if child {
        let mut chip = ChipModel::from_seed(crate::script::ChipSeed {
            agent: "synthetic-child".into(),
            parent: None,
            ros: None,
            callsign: "Child".into(),
            hon: "(r)",
            full: "Synthetic child".into(),
            name: "synthetic task".into(),
            model: "synthetic".into(),
            device: "local".into(),
            state: crate::script::ChipDisplayState::InputRequired,
            tokens: 0,
            prefill: vec![],
        });
        chip.transcript = crate::projection::SessionProjection::new();
        m.chips.push(chip);
        m.screen = Screen::Subagent;
        m.view_path = vec!["synthetic-child".into()];
    }
    for i in 0..count {
        projection(&mut m).apply(&EventPayload::MenuOpened(card(&format!("card-{i}"))));
    }
    m
}

fn projection(m: &mut AppModel) -> &mut crate::projection::SessionProjection {
    if m.screen == Screen::Subagent {
        &mut m.chips[0].transcript
    } else {
        &mut m.projection
    }
}

fn paint_at(m: &mut AppModel, w: u16, h: u16, at: Instant) -> Vec<(ratatui::layout::Rect, Hit)> {
    let previous = m.pointer_presentation.borrow().clone();
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    let mut hits = vec![];
    term.draw(|f| hits = render(m, f)).unwrap();
    let owner = m.pointer_input_owner();
    if previous.as_ref().is_none_or(|(old, _)| old != &owner) {
        *m.pointer_presentation.borrow_mut() = Some((owner, at));
    }
    let text: String = term
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    if m.input_owner() == InputOwner::SpendCard {
        let pending = projection(m).pending_menu_count();
        assert!(
            text.contains(&format!("{pending} pending")),
            "missing count: {text}"
        );
        println!("usage stack {w}x{h}: {pending} pending");
    }
    hits
}

fn click(m: &mut AppModel, hits: &[(ratatui::layout::Rect, Hit)], at: Instant) {
    let (rect, _) = hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::MenuOption { index: 1, .. }))
        .unwrap();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dispatch_input_at(
            m,
            hits,
            Event::Mouse(MouseEvent {
                kind,
                column: rect.x + 1,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }),
            at,
        );
    }
}

#[test]
fn usage_stacked_double_click_spends_once_and_a_deliberate_later_click_works() {
    for child in [false, true] {
        for count in [2, 3] {
            for (w, h) in [(118, 36), (80, 24)] {
                let mut m = stack(child, count);
                let start = Instant::now();
                let hits = paint_at(&mut m, w, h, start);
                let first = start + Duration::from_millis(500);
                click(&mut m, &hits, first);
                assert_eq!(m.outbox.len(), 1, "first click child={child}");
                assert_eq!(projection(&mut m).pending_menu_count(), count - 1);
                let next = paint_at(&mut m, w, h, first + Duration::from_millis(10));
                click(&mut m, &next, first + Duration::from_millis(166));
                assert_eq!(
                    m.outbox.len(),
                    1,
                    "double-click crossed stack child={child}"
                );
                assert_eq!(projection(&mut m).pending_menu_count(), count - 1);
                let mature = paint_at(&mut m, w, h, first + Duration::from_millis(200));
                click(&mut m, &mature, first + Duration::from_millis(411));
                assert_eq!(m.outbox.len(), 2, "deliberate mature successor is usable");
                assert!(
                    m.outbox
                        .iter()
                        .all(|a| a.answer.option_key.as_deref() == Some("approve"))
                );
            }
        }
    }
}

#[test]
fn usage_replacement_and_question_successor_require_their_own_pointer_dwell() {
    for question in [false, true] {
        let mut m = stack(false, 2);
        if question {
            let mut menu = card("question");
            menu.kind = MenuKind::Permission {
                effect_summary: "synthetic write".into(),
                file_review: None,
            };
            menu.blocking = true;
            m.projection.apply(&EventPayload::MenuOpened(menu));
        }
        let start = Instant::now();
        let hits = paint_at(&mut m, 118, 36, start);
        let now = start + Duration::from_millis(500);
        if question {
            click(&mut m, &hits, now);
            let answer = m.outbox.last().unwrap().answer.clone();
            m.projection.apply(&EventPayload::MenuAnswered(answer));
        } else {
            m.projection
                .apply(&EventPayload::MenuOpened(card("replacement")));
        }
        let before = m.outbox.len();
        let next = paint_at(&mut m, 118, 36, now);
        click(&mut m, &next, now + Duration::from_millis(1));
        assert_eq!(m.outbox.len(), before);
        let mature = paint_at(&mut m, 118, 36, now + Duration::from_millis(2));
        click(&mut m, &mature, now + AppModel::DECISION_DWELL);
        assert_eq!(m.outbox.len(), before + 1);
    }
}

#[test]
fn usage_hidden_card_keeps_blocking_menu_identity_but_retires_queued_press() {
    let mut m = stack(false, 1);
    let mut question = card("question");
    question.kind = MenuKind::Permission {
        effect_summary: "synthetic write".into(),
        file_review: None,
    };
    question.blocking = true;
    question.origin = "permission".into();
    question.options[1].key = "allow".into();
    m.projection
        .apply(&EventPayload::MenuOpened(question.clone()));
    m.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    let start = Instant::now();
    let hits = paint_at(&mut m, 118, 36, start);
    let owner = m.pointer_input_owner();
    let (rect, _) = hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::MenuOption { index: 1, .. }))
        .unwrap();
    let mouse = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        })
    };
    dispatch_input_at(
        &mut m,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left)),
        start + Duration::from_millis(500),
    );
    // An outside fact keeps the shared queue barrier even when its menu
    // stays below the visible question. Only the visible identity is stable.
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        card("hidden-card"),
    ))));
    let next = paint_at(&mut m, 118, 36, start + Duration::from_millis(600));
    assert_eq!(
        m.pointer_input_owner(),
        owner,
        "hidden arrival replaced the visible question identity"
    );
    assert_eq!(
        m.menu_selection, 1,
        "hidden arrival reset the visible choice"
    );
    dispatch_input_at(
        &mut m,
        &next,
        mouse(MouseEventKind::Up(MouseButton::Left)),
        start + Duration::from_millis(700),
    );
    assert!(
        m.outbox.is_empty(),
        "outside content must retain Toolview's queued-press barrier"
    );
    click(&mut m, &next, start + Duration::from_millis(700));
    assert_eq!(
        m.outbox.len(),
        1,
        "a fresh press on the unchanged mature question must work"
    );
    assert_eq!(m.outbox[0].answer.menu, question.id);
    assert_eq!(m.outbox[0].answer.option_key.as_deref(), Some("allow"));
    assert_eq!(m.projection.pending_menu_count(), 3);
}

#[test]
fn usage_same_id_option_changes_retire_the_presented_pointer_target() {
    for child in [false, true] {
        let mut m = stack(child, 1);
        let start = Instant::now();
        let hits = paint_at(&mut m, 118, 36, start);
        let (rect, _) = hits
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::MenuOption { index: 1, .. }))
            .unwrap();
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: rect.x + 1,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            })
        };
        dispatch_input_at(
            &mut m,
            &hits,
            mouse(MouseEventKind::Down(MouseButton::Left)),
            start + Duration::from_millis(500),
        );
        let mut changed = card("card-0");
        changed.options[1].key = "changed-effect".into();
        changed.options[1].label = "Changed operation".into();
        // Exercise ingestion identity itself, independently of the shell's
        // additional outside-event queue invalidation.
        projection(&mut m).apply(&EventPayload::MenuOpened(changed));
        let next = paint_at(&mut m, 118, 36, start + Duration::from_millis(600));
        dispatch_input_at(
            &mut m,
            &next,
            mouse(MouseEventKind::Up(MouseButton::Left)),
            start + Duration::from_millis(700),
        );
        assert!(
            m.outbox.is_empty(),
            "same-id menu retargeted a press child={child}"
        );
        let mature = paint_at(&mut m, 118, 36, start + Duration::from_millis(701));
        click(&mut m, &mature, start + Duration::from_millis(1001));
        assert_eq!(m.outbox.len(), 1, "fresh changed operation remains usable");
        assert_eq!(
            m.outbox[0].answer.option_key.as_deref(),
            Some("changed-effect")
        );
    }
}

#[test]
fn usage_same_id_option_changes_also_retire_old_apply_navigation() {
    for child in [false, true] {
        let mut m = stack(child, 1);
        m.handle(crate::app::AppEvent::Key(
            ratatui::crossterm::event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        ));
        let old = projection(&mut m).open_menu().unwrap().clone();
        assert_eq!(m.menu_display_selection(&old), 1);
        assert_eq!(m.menu_display_selection(&card("unrelated-menu")), 0);
        let mut changed = old;
        changed.options[1].key = "changed-effect".into();
        projection(&mut m).apply(&EventPayload::MenuOpened(changed.clone()));
        assert_eq!(
            m.menu_display_selection(&changed),
            0,
            "new incarnation starts at Deny"
        );
        let now = Instant::now();
        let hits = paint_at(&mut m, 118, 36, now);
        dispatch_input_at(
            &mut m,
            &hits,
            k(KeyCode::Enter),
            now + AppModel::DECISION_DWELL,
        );
        assert_eq!(m.outbox.len(), 1);
        assert_eq!(m.outbox[0].answer.option_key.as_deref(), Some("deny"));
    }
    let mut m = stack(true, 1);
    m.handle(crate::app::AppEvent::Key(
        ratatui::crossterm::event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    ));
    let mut other = stack(true, 1).chips.remove(0);
    other.agent = "other-child".into();
    m.chips.push(other);
    m.view_path = vec!["other-child".into()];
    let menu = m.viewed_chip().unwrap().question_menu().unwrap();
    assert_eq!(
        m.menu_display_selection(menu),
        0,
        "shared owner identity includes the child context even when ids and incarnations coincide"
    );
}

#[test]
fn usage_defaulted_navigation_cannot_hide_a_selection_change_from_queued_enter() {
    for child in [false, true] {
        for replacement in [false, true] {
            let mut m = stack(child, 1);
            m.handle(crate::app::AppEvent::Key(
                ratatui::crossterm::event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            ));
            if replacement {
                let mut changed = card("card-0");
                changed.options[1].key = "changed-effect".into();
                projection(&mut m).apply(&EventPayload::MenuOpened(changed));
            } else {
                m.handle(crate::app::AppEvent::Key(
                    ratatui::crossterm::event::KeyEvent::new(
                        KeyCode::Char('f'),
                        KeyModifiers::CONTROL,
                    ),
                ));
                m.handle(crate::app::AppEvent::Key(
                    ratatui::crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                ));
            }
            let now = Instant::now();
            paint_at(&mut m, 118, 36, now);
            let menu = projection(&mut m).open_menu().unwrap().clone();
            assert_eq!(m.menu_display_selection(&menu), 0);
            let deny = m.painted_stamp();
            assert!(dispatch_queued_input(
                &mut m,
                &[],
                QueuedInput {
                    event: k(KeyCode::Down),
                    presented: deny.clone(),
                    received_at: now + Duration::from_secs(1),
                }
            ));
            loop_present(&mut m);
            assert_eq!(m.menu_display_selection(&menu), 1);
            assert!(
                !dispatch_queued_input(
                    &mut m,
                    &[],
                    QueuedInput {
                        event: k(KeyCode::Enter),
                        presented: deny,
                        received_at: now + Duration::from_secs(1),
                    }
                ),
                "an old Deny frame cannot confirm Apply child={child} replacement={replacement}"
            );
            assert!(m.outbox.is_empty());
            loop_present(&mut m);
            let apply = m.painted_stamp();
            assert!(
                dispatch_queued_input(
                    &mut m,
                    &[],
                    QueuedInput {
                        event: k(KeyCode::Enter),
                        presented: apply,
                        received_at: now + Duration::from_secs(2),
                    }
                ),
                "a fresh informed confirm remains usable"
            );
            assert_eq!(m.outbox.len(), 1);
            assert_eq!(
                m.outbox[0].answer.option_key.as_deref(),
                Some(if replacement {
                    "changed-effect"
                } else {
                    "approve"
                })
            );
        }
    }
}

fn burst(m: &mut AppModel, events: impl IntoIterator<Item = Event>) {
    let stamp = m.painted_stamp();
    for event in events {
        assert!(
            dispatch_queued_input(
                m,
                &[],
                QueuedInput {
                    event,
                    presented: stamp.clone(),
                    received_at: Instant::now(),
                }
            ),
            "lost queued event: {:?}",
            m.flash
        );
        loop_present(m);
    }
}

#[test]
fn usage_external_equal_value_composer_replacement_retires_queued_card_keys() {
    let mut m = stack(false, 2);
    paint_at(&mut m, 118, 36, Instant::now());
    let old = m.painted_stamp();
    m.edit_composer_from_outside(crate::composer::Composer::clear);
    loop_present(&mut m);
    for code in [KeyCode::Down, KeyCode::Enter] {
        assert!(!dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                event: k(code),
                presented: old.clone(),
                received_at: Instant::now() + Duration::from_secs(1),
            }
        ));
        loop_present(&mut m);
    }
    assert!(m.outbox.is_empty());
    assert_eq!(m.menu_selection, 0);
    assert_eq!(m.projection.pending_menu_count(), 2);
}

#[test]
fn usage_child_card_keeps_home_navigation_without_answering_the_stack() {
    let mut child = stack(true, 2);
    assert_eq!(child.input_owner(), InputOwner::SpendCard);
    assert!(child.hit_owned(&Hit::SessionHome));
    assert!(child.hit_owned(&Hit::ChipCrumb(vec!["synthetic-child".into()])));
    child.handle_hit(Hit::SessionHome);
    assert_eq!(child.screen, Screen::Session);
    assert_eq!(child.chips[0].transcript.pending_menu_count(), 2);
    assert!(child.outbox.is_empty());
    let root = stack(false, 2);
    assert!(
        !root.hit_owned(&Hit::SessionHome),
        "root card cannot act on a stale child Home hit"
    );
}

#[test]
fn usage_stack_keeps_every_character_when_local_input_opens_or_closes_owners() {
    for child in [false, true] {
        for text in [
            "/theme",
            "/attach /tmp/usage-synthetic.txt",
            "/attach \"/tmp/usage file.txt\"",
            "@child",
            "2",
            "hello burst world",
            "سلام 🌿",
        ] {
            let mut m = stack(child, 3);
            paint_at(&mut m, 118, 36, Instant::now());
            burst(&mut m, text.chars().map(|c| k(KeyCode::Char(c))));
            assert_eq!(m.composer.text(), text, "child={child}");
            assert!(m.outbox.is_empty());
            assert_eq!(projection(&mut m).pending_menu_count(), 3);
        }
    }
    // Local closing keys can transfer subsequent edit text to the composer;
    // inherited runtime tests separately prohibit externally replaced owners.
    for closing in [
        "search-escape",
        "theme-escape",
        "theme-enter",
        "history-escape",
    ] {
        let mut m = stack(false, 3);
        if closing == "search-escape" {
            paint_at(&mut m, 118, 36, Instant::now());
            burst(
                &mut m,
                std::iter::once(Event::Key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char('f'),
                    KeyModifiers::CONTROL,
                )))
                .chain("query".chars().map(|c| k(KeyCode::Char(c)))),
            );
            assert_eq!(m.transcript_search.as_ref().unwrap().query, "query");
        } else if closing == "history-escape" {
            m.backtrack = Some(crate::app::PromptBacktrack { selection: 0 });
        } else {
            m.handle(crate::app::AppEvent::Paste(crate::app::Pasted::new(
                "/theme".into(),
            )));
            m.handle(crate::app::AppEvent::Key(
                ratatui::crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ));
        }
        let expected = match closing {
            "search-escape" => InputOwner::Search,
            "history-escape" => InputOwner::HistoryChooser,
            _ => InputOwner::ThemePicker,
        };
        assert_eq!(m.input_owner(), expected, "closing fixture: {closing}");
        paint_at(&mut m, 118, 36, Instant::now());
        let close = if closing == "theme-enter" {
            KeyCode::Enter
        } else {
            KeyCode::Esc
        };
        burst(
            &mut m,
            std::iter::once(k(close)).chain("after close".chars().map(|c| k(KeyCode::Char(c)))),
        );
        assert_eq!(m.composer.text(), "after close");
        assert!(m.outbox.is_empty());
        assert_eq!(m.projection.pending_menu_count(), 3, "closing={closing}");
    }
}

#[test]
fn usage_spend_card_clipboard_edits_share_the_composer_without_answering() {
    for child in [false, true] {
        for modifiers in [
            Some(KeyModifiers::CONTROL),
            Some(KeyModifiers::SUPER),
            Some(KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Some(KeyModifiers::SUPER | KeyModifiers::SHIFT),
            None,
        ] {
            for (width, height) in [(118, 36), (80, 24)] {
                let mut model = stack(child, 3);
                let shown = Instant::now();
                let hits = paint_at(&mut model, width, height, shown);
                dispatch_input_at(
                    &mut model,
                    &hits,
                    k(KeyCode::Down),
                    shown + Duration::from_millis(500),
                );
                let hits = paint_at(
                    &mut model,
                    width,
                    height,
                    shown + Duration::from_millis(501),
                );
                let right_click = |kind| {
                    Event::Mouse(MouseEvent {
                        kind,
                        column: 1,
                        row: 1,
                        modifiers: KeyModifiers::NONE,
                    })
                };
                let event = match modifiers {
                    Some(modifiers) => Event::Key(KeyEvent::new(KeyCode::Char('v'), modifiers)),
                    None => right_click(MouseEventKind::Down(MouseButton::Right)),
                };
                dispatch_input_at(&mut model, &hits, event, shown + Duration::from_millis(600));
                if modifiers.is_none() {
                    dispatch_input_at(
                        &mut model,
                        &hits,
                        right_click(MouseEventKind::Up(MouseButton::Right)),
                        shown + Duration::from_millis(601),
                    );
                }
                assert_eq!(
                    model
                        .requests
                        .iter()
                        .filter(|request| matches!(request, crate::app::AppRequest::ClipboardRead))
                        .count(),
                    1,
                    "child={child}, modifiers={modifiers:?}"
                );
                model.handle(AppEvent::Paste(crate::app::Pasted::new(
                    "synthetic clipboard text".into(),
                )));
                assert_eq!(model.composer.text(), "synthetic clipboard text");
                assert!(model.outbox.is_empty());
                assert_eq!(projection(&mut model).pending_menu_count(), 3);
            }
        }
    }
    // A blocking permission has no composer edit surface.
    let mut model = stack(false, 2);
    let mut permission = card("clipboard-permission");
    permission.kind = MenuKind::Permission {
        effect_summary: "synthetic write".into(),
        file_review: None,
    };
    permission.blocking = true;
    model
        .projection
        .apply(&EventPayload::MenuOpened(permission));
    model.request_clipboard_paste();
    assert!(model.requests.is_empty());
    assert!(model.outbox.is_empty());
    assert_eq!(model.projection.pending_menu_count(), 3);
}
