#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::input_typeahead_tests::{decision, k, loop_present};
use super::*;
use crate::app::{Screen, ThemePicker};
use ratatui::crossterm::event::KeyEvent;

pub(super) fn question_chip(agent: &str, id: &str) -> crate::app::ChipModel {
    use crate::app::{ChipModel, ChipQuestion};
    use crate::script::{ChipDisplayState, ChipSeed};
    let mut chip = ChipModel::from_seed(ChipSeed {
        agent: agent.into(),
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
        prefill: vec![],
    });
    chip.question = Some(ChipQuestion {
        recovery: false,
        text: "Choose?".into(),
        options: vec!["Deny".into(), "Apply".into()],
        resolved: false,
    });
    chip.transcript
        .apply(&EventPayload::MenuOpened(decision(id)));
    chip
}

fn pointer(
    m: &mut AppModel,
    hits: &[(ratatui::layout::Rect, crate::app::Hit)],
    kind: MouseEventKind,
    now: Instant,
) {
    dispatch_input_at(
        m,
        hits,
        Event::Mouse(event::MouseEvent {
            kind,
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }),
        now,
    );
    if owner_needs_presentation(m) {
        m.record_painted_input_at(now);
        m.input_refused = false;
        m.dirty = false;
    }
}

#[test]
fn newly_opened_pickers_allow_preview_but_wait_before_pointer_commit() {
    use crate::app::{EffortPicker, Hit};
    let now = Instant::now();
    let rect = ratatui::layout::Rect::new(1, 1, 8, 3);
    for theme in [false, true] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        let mut providers = crate::mock::seed_provider_summaries();
        let provider = &mut providers[0];
        let detail = &mut provider.model_details[0];
        detail.supported_efforts = vec!["low".into()];
        m.identity.provider = provider.provider.clone();
        m.identity.model_short = detail.name.clone();
        m.providers.apply_snapshot(providers, 1);
        m.record_painted_input_at(now - Duration::from_secs(1));
        let action = if theme {
            m.theme_picker = Some(ThemePicker {
                selection: 0,
                prior: m.theme_choice,
            });
            Hit::ThemeOption(1)
        } else {
            m.effort_picker = Some(EffortPicker::default());
            Hit::EffortOption(1)
        };
        m.record_painted_input_at(now);
        let hits = [(rect, action)];
        for age in [0, 166, 399] {
            let at = now + Duration::from_millis(age);
            pointer(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), at);
            pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), at);
            assert!(m.theme_picker.is_some() || m.effort_picker.is_some());
            assert!(m.requests.is_empty());
            assert_eq!(m.theme_commits, 0);
        }
        assert_eq!(
            if theme {
                m.theme_picker.as_ref().map(|picker| picker.selection)
            } else {
                m.effort_picker.as_ref().map(|picker| picker.selection)
            },
            Some(1)
        );
        let at = now + Duration::from_millis(400);
        pointer(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), at);
        pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), at);
        assert!(m.theme_picker.is_none() && m.effort_picker.is_none());
        assert!(if theme {
            m.theme_commits == 1
        } else {
            m.identity.reasoning.as_deref() == Some("low")
        });
    }
}

#[test]
fn a_palette_appearing_over_the_composer_has_its_own_pointer_dwell() {
    use crate::app::Hit;
    let now = Instant::now();
    let mut m = AppModel::new();
    m.screen = Screen::Launcher;
    m.record_painted_input_at(now - Duration::from_secs(1));
    m.composer.set_text("/theme");
    m.record_painted_input_at(now);
    let hits = [(
        ratatui::layout::Rect::new(1, 1, 8, 3),
        Hit::PaletteRow(m.palette_items()[0].clone()),
    )];
    pointer(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), now);
    pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), now);
    assert!(m.theme_picker.is_none());
    assert!(m.palette_open());
    let at = now + Duration::from_millis(400);
    pointer(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), at);
    pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), at);
    assert!(m.theme_picker.is_some());
}

#[test]
fn a_model_picker_provider_stage_cannot_take_the_second_click() {
    use crate::app::{Hit, RuntimeMode};
    let now = Instant::now();
    let mut m = AppModel::new();
    m.mode = RuntimeMode::Live;
    m.screen = Screen::Session;
    m.active_session = Some(haider_protocol::ids::SessionId::new("model-dwell-fixture"));
    m.daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.into());
    m.daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_MODELS_V1.into());
    m.providers
        .apply_snapshot(crate::mock::seed_provider_summaries(), 1);
    m.accounts
        .apply_snapshot(crate::mock::seed_account_rows(), Some(1));
    m.open_model_picker(String::new());
    m.requests.clear();
    m.record_painted_input_at(now);
    let row = m
        .model_picker_rows()
        .into_iter()
        .find(|row| row.auth == "api" && !row.model.is_empty())
        .expect("API group");
    let rect = ratatui::layout::Rect::new(1, 1, 8, 3);
    let hits = [(
        rect,
        Hit::ModelPickerRow {
            provider: row.provider,
            model: row.model,
            api_group: true,
        },
    )];
    pointer(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), now);
    pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), now);
    assert!(m.model_picker.as_ref().unwrap().provider_stage.is_none());
    let shown = now + Duration::from_millis(400);
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Down(MouseButton::Left),
        shown,
    );
    pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), shown);
    assert!(m.model_picker.as_ref().unwrap().provider_stage.is_some());
    let row = m.model_picker_rows()[0].clone();
    let hits = [(
        rect,
        Hit::ModelPickerRow {
            provider: row.provider,
            model: row.model,
            api_group: false,
        },
    )];
    let second = shown + Duration::from_millis(166);
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Down(MouseButton::Left),
        second,
    );
    pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), second);
    assert!(m.requests.is_empty());
    let deliberate = shown + Duration::from_millis(400);
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Down(MouseButton::Left),
        deliberate,
    );
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Up(MouseButton::Left),
        deliberate,
    );
    assert!(!m.requests.is_empty());
}

#[test]
fn newly_shown_restored_replaced_and_successor_cards_require_pointer_dwell() {
    use crate::app::{Hit, InputOwner};
    use ratatui::layout::Rect;
    for variant in [
        "arrival",
        "successor",
        "same-id-options",
        "restored",
        "question-to-card",
    ] {
        let now = Instant::now();
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            decision("A"),
        ))));
        m.record_painted_input_at(now - Duration::from_secs(1));
        let a_hits = vec![(
            Rect::new(1, 1, 5, 5),
            Hit::MenuOption {
                menu: decision("A").id,
                index: 1,
            },
        )];
        let mut next = decision("B");
        match variant {
            "successor" | "question-to-card" => {
                if variant == "question-to-card" {
                    let mut question = decision("A");
                    question.kind = haider_protocol::menu::MenuKind::Question;
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
                        question,
                    ))));
                    m.record_painted_input_at(now - Duration::from_secs(1));
                }
                pointer(
                    &mut m,
                    &a_hits,
                    MouseEventKind::Down(MouseButton::Left),
                    now,
                );
                pointer(&mut m, &a_hits, MouseEventKind::Up(MouseButton::Left), now);
                assert_eq!(m.outbox.len(), 1, "{variant}: deliberate predecessor click");
            }
            "same-id-options" => {
                next.id = decision("A").id;
                next.options[1].key = "different-effect".into();
            }
            "restored" => {
                m.help_open = true;
                m.record_painted_input_at(now);
                m.help_open = false;
                next = decision("A");
            }
            _ => {}
        }
        let prior = m.outbox.len();
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            next.clone(),
        ))));
        m.record_painted_input_at(now);
        assert_eq!(m.input_owner(), InputOwner::SessionMenu);
        let hits = vec![(
            Rect::new(1, 1, 5, 5),
            Hit::MenuOption {
                menu: next.id,
                index: 1,
            },
        )];
        for age in [0, 166, 399] {
            let at = now + Duration::from_millis(age);
            pointer(&mut m, &hits, MouseEventKind::Moved, at);
            pointer(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), at);
            pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), at);
            assert_eq!(m.outbox.len(), prior, "{variant} at {age} ms");
            assert!(m.flash.as_deref().unwrap().contains("New surface"));
        }
        // Cadence redraw and keyboard selection do not restart the dwell.
        m.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        m.record_painted_input_at(now + Duration::from_millis(399));
        pointer(
            &mut m,
            &hits,
            MouseEventKind::Down(MouseButton::Left),
            now + Duration::from_millis(400),
        );
        pointer(
            &mut m,
            &hits,
            MouseEventKind::Up(MouseButton::Left),
            now + Duration::from_millis(400),
        );
        assert_eq!(
            m.outbox.len(),
            prior + 1,
            "{variant}: deliberate follow-up click"
        );
    }
}

#[test]
fn resolving_a_native_permission_overlay_rearms_the_revealed_decision() {
    use crate::app::Hit;
    use haider_protocol::permission::{
        PermissionGrantAction, PermissionGrantNeeded, SystemPermission,
    };
    for matching in [false, true] {
        let now = Instant::now();
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.projection
            .apply(&EventPayload::MenuOpened(decision("grant")));
        m.projection.set_permission_card(PermissionGrantNeeded {
            request_id: "native-grant".into(),
            menu_id: decision("grant").id,
            request_seq: 1,
            opening_generation: 1,
            call_id: "synthetic-call".into(),
            effect_id: haider_protocol::ids::EffectId::new("synthetic-effect"),
            permission: SystemPermission::ScreenRecording,
            pane_name: "Fixture".into(),
            settings_url: "fixture-settings".into(),
            actions: vec![PermissionGrantAction::OpenSettings],
            auto_restart_pending: false,
            poll_timeout_ms: 1000,
        });
        m.record_painted_input_at(now - Duration::from_secs(1));
        m.projection.resolve_permission_card(if matching {
            "native-grant"
        } else {
            "other-grant"
        });
        m.record_painted_input_at(now);
        assert_eq!(m.projection.permission_card().is_none(), matching);
        assert_eq!(
            m.inbox_counts().asks,
            1,
            "paired native/menu card is one ask"
        );
        assert_eq!(
            m.pointer_commit_ready(now + Duration::from_millis(166)),
            !matching,
            "only the matching resolution replaces the visible card"
        );
        if matching {
            let hits = [(
                ratatui::layout::Rect::new(1, 1, 5, 5),
                Hit::MenuOption {
                    menu: decision("grant").id,
                    index: 1,
                },
            )];
            for age in [166, 400] {
                let at = now + Duration::from_millis(age);
                pointer(&mut m, &hits, MouseEventKind::Down(MouseButton::Left), at);
                pointer(&mut m, &hits, MouseEventKind::Up(MouseButton::Left), at);
                assert_eq!(m.outbox.len(), usize::from(age == 400));
            }
        }
    }
}

#[test]
fn subagent_successor_card_obeys_the_same_pointer_dwell() {
    use crate::app::Hit;
    let now = Instant::now();
    let mut m = AppModel::new();
    let chip = question_chip("child", "A");
    m.chips.push(chip);
    m.screen = Screen::Subagent;
    m.view_path = vec!["child".into()];
    m.record_painted_input_at(now - Duration::from_secs(1));
    let hits = |id| {
        vec![(
            ratatui::layout::Rect::new(1, 1, 5, 5),
            Hit::MenuOption {
                menu: decision(id).id,
                index: 1,
            },
        )]
    };
    pointer(
        &mut m,
        &hits("A"),
        MouseEventKind::Down(MouseButton::Left),
        now,
    );
    pointer(
        &mut m,
        &hits("A"),
        MouseEventKind::Up(MouseButton::Left),
        now,
    );
    assert_eq!(m.outbox.len(), 1);
    m.chips[0]
        .transcript
        .apply(&EventPayload::MenuOpened(decision("B")));
    m.chips[0].question.as_mut().unwrap().resolved = false;
    m.chips[0].state = crate::script::ChipDisplayState::InputRequired;
    m.record_painted_input_at(now);
    pointer(
        &mut m,
        &hits("B"),
        MouseEventKind::Down(MouseButton::Left),
        now + Duration::from_millis(166),
    );
    pointer(
        &mut m,
        &hits("B"),
        MouseEventKind::Up(MouseButton::Left),
        now + Duration::from_millis(166),
    );
    assert_eq!(m.outbox.len(), 1);
    pointer(
        &mut m,
        &hits("B"),
        MouseEventKind::Down(MouseButton::Left),
        now + Duration::from_millis(400),
    );
    pointer(
        &mut m,
        &hits("B"),
        MouseEventKind::Up(MouseButton::Left),
        now + Duration::from_millis(400),
    );
    assert_eq!(m.outbox.len(), 2);
}

#[test]
fn switching_chips_with_the_same_menu_coordinate_rearms_pointer_dwell() {
    use crate::app::Hit;
    let now = Instant::now();
    let mut m = AppModel::new();
    m.chips.push(question_chip("first", "shared"));
    m.screen = Screen::Subagent;
    m.view_path = vec!["first".into()];
    m.record_painted_input_at(now - Duration::from_secs(1));
    m.chips.push(question_chip("second", "shared"));
    m.view_path = vec!["second".into()];
    m.record_painted_input_at(now);
    let hits = [(
        ratatui::layout::Rect::new(1, 1, 8, 3),
        Hit::MenuOption {
            menu: decision("shared").id,
            index: 1,
        },
    )];
    let second_click = now + Duration::from_millis(166);
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Down(MouseButton::Left),
        second_click,
    );
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Up(MouseButton::Left),
        second_click,
    );
    assert!(m.outbox.is_empty());
    let deliberate = now + Duration::from_millis(400);
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Down(MouseButton::Left),
        deliberate,
    );
    pointer(
        &mut m,
        &hits,
        MouseEventKind::Up(MouseButton::Left),
        deliberate,
    );
    assert_eq!(m.outbox.len(), 1);
}

#[test]
fn hover_selection_then_a_queued_click_stays_on_the_same_card() {
    use crate::app::Hit;
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        decision("A"),
    ))));
    m.record_painted_input_at(Instant::now() - Duration::from_secs(1));
    let stamp = m.painted_stamp();
    let hits = vec![(
        ratatui::layout::Rect::new(1, 1, 5, 5),
        Hit::MenuOption {
            menu: decision("A").id,
            index: 1,
        },
    )];
    for kind in [
        MouseEventKind::Moved,
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        assert!(dispatch_queued_input(
            &mut m,
            &hits,
            QueuedInput {
                received_at: Instant::now(),
                event: Event::Mouse(event::MouseEvent {
                    kind,
                    column: 2,
                    row: 2,
                    modifiers: KeyModifiers::NONE
                }),
                presented: stamp.clone()
            }
        ));
        m.record_painted_input();
    }
    assert_eq!(m.outbox.len(), 1);
}

#[test]
fn queued_pointer_reports_do_not_retarget_across_a_queue_closed_cover() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        decision("A"),
    ))));
    m.help_open = true;
    m.record_painted_input_at(Instant::now() - Duration::from_secs(1));
    let stamp = m.painted_stamp();
    assert!(dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            received_at: Instant::now(),
            event: k(KeyCode::Esc),
            presented: stamp.clone()
        }
    ));
    m.record_painted_input_at(Instant::now() - Duration::from_secs(1));
    assert!(!dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            received_at: Instant::now(),
            event: Event::Mouse(event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE
            }),
            presented: stamp
        }
    ));
    assert!(m.mouse_down.is_none());
}

#[test]
fn a_delayed_press_started_inside_dwell_stays_refused_after_a_long_hold() {
    use crate::app::Hit;
    let born = Instant::now() - Duration::from_secs(1);
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        decision("delayed"),
    ))));
    m.record_painted_input_at(born);
    let stamp = m.painted_stamp();
    let hits = [(
        ratatui::layout::Rect::new(1, 1, 8, 3),
        Hit::MenuOption {
            menu: haider_protocol::ids::MenuId::new("delayed"),
            index: 1,
        },
    )];
    // Both reports were received earlier. Dispatch now runs after the
    // dwell, while Up itself arrived after it; the press was still early.
    for (kind, age) in [
        (MouseEventKind::Down(MouseButton::Left), 100),
        (MouseEventKind::Up(MouseButton::Left), 600),
    ] {
        dispatch_queued_input(
            &mut m,
            &hits,
            QueuedInput {
                event: Event::Mouse(event::MouseEvent {
                    kind,
                    column: 2,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                }),
                presented: stamp.clone(),
                received_at: born + Duration::from_millis(age),
            },
        );
        loop_present(&mut m);
    }
    assert!(m.outbox.is_empty());
    assert!(
        m.flash
            .as_deref()
            .is_some_and(|flash| flash.contains("click again to confirm"))
    );
    let fresh = m.painted_stamp();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dispatch_queued_input(
            &mut m,
            &hits,
            QueuedInput {
                event: Event::Mouse(event::MouseEvent {
                    kind,
                    column: 2,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                }),
                presented: fresh.clone(),
                received_at: Instant::now(),
            },
        );
        loop_present(&mut m);
    }
    assert_eq!(m.outbox.len(), 1);
}
