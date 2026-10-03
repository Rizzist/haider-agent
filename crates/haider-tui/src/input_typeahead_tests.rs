#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::app::{Screen, ThemePicker};
use ratatui::crossterm::event::KeyEvent;

pub(super) fn k(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Present exactly as run_live does after each loop pass.
pub(super) fn loop_present(model: &mut AppModel) {
    if owner_needs_presentation(model) {
        let backend = ratatui::backend::TestBackend::new(118, 36);
        let Ok(mut term) = Terminal::new(backend);
        term.draw(|frame| drop(render(model, frame))).unwrap();
        model.input_refused = false;
        model.dirty = false;
    }
}

pub(super) fn paint(model: &mut AppModel) {
    let backend = ratatui::backend::TestBackend::new(118, 36);
    let Ok(mut term) = Terminal::new(backend);
    term.draw(|frame| drop(render(model, frame))).unwrap();
    model.input_refused = false;
    model.dirty = false;
}

/// A burst: every event was read (and stamped) before the loop handled the
/// first one, which is what a terminal delivering several keys in one read
/// (tmux send-keys, ssh/mosh batching, key repeat while the loop is busy)
/// produces. Returns per-event acceptance.
fn burst(model: &mut AppModel, events: Vec<Event>) -> Vec<bool> {
    let stamp = model.painted_stamp();
    events
        .into_iter()
        .map(|event| {
            let ok = dispatch_queued_input(
                model,
                &[],
                QueuedInput {
                    received_at: Instant::now(),
                    event,
                    presented: stamp.clone(),
                },
            );
            loop_present(model);
            ok
        })
        .collect()
}

/// Human pace: each event is stamped after the previous frame was painted.
fn paced(model: &mut AppModel, events: Vec<Event>) -> Vec<bool> {
    events
        .into_iter()
        .map(|event| {
            let stamp = model.painted_stamp();
            let ok = dispatch_queued_input(
                model,
                &[],
                QueuedInput {
                    received_at: Instant::now(),
                    event,
                    presented: stamp,
                },
            );
            loop_present(model);
            ok
        })
        .collect()
}

fn theme_model() -> AppModel {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.theme_picker = Some(ThemePicker {
        selection: 0,
        prior: m.theme_choice,
    });
    paint(&mut m);
    m
}

#[test]
fn ctl_theme_picker_paced_navigation_applies_every_key() {
    let mut m = theme_model();
    let accepted = paced(&mut m, vec![k(KeyCode::Down), k(KeyCode::Down)]);
    println!(
        "paced theme accepted={accepted:?} selection={:?}",
        m.theme_picker.map(|p| p.selection)
    );
    assert_eq!(accepted, vec![true, true]);
    assert_eq!(m.theme_picker.map(|p| p.selection), Some(2));
}

#[test]
fn var_theme_picker_typeahead_down_down_is_not_silently_dropped() {
    let mut m = theme_model();
    let accepted = burst(
        &mut m,
        vec![k(KeyCode::Down), k(KeyCode::Down), k(KeyCode::Down)],
    );
    println!(
        "burst theme accepted={accepted:?} selection={:?} flash={:?}",
        m.theme_picker.map(|p| p.selection),
        m.flash
    );
    assert_eq!(
        m.theme_picker.map(|p| p.selection),
        Some(3),
        "type-ahead navigation lost: {accepted:?}"
    );
}

#[test]
fn var_sessions_list_typeahead_down_down_is_not_silently_dropped() {
    let mut m = AppModel::new();
    for name in ["s-one", "s-two", "s-three", "s-four"] {
        m.upsert_live_session(&haider_protocol::ids::SessionId::new(name));
    }
    m.screen = Screen::Sessions;
    m.session_browser_sel = 0;
    paint(&mut m);
    let rows = m.session_browser_rows().len();
    let accepted = burst(&mut m, vec![k(KeyCode::Down), k(KeyCode::Down)]);
    println!(
        "burst sessions rows={rows} accepted={accepted:?} sel={}",
        m.session_browser_sel
    );
    assert!(rows >= 3, "fixture: {rows} rows");
    assert_eq!(
        m.session_browser_sel, 2,
        "type-ahead list navigation lost: {accepted:?}"
    );
}

#[test]
fn var_ssh_form_tab_then_text_typeahead_is_not_silently_dropped() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.ssh_open = true;
    paint(&mut m);
    paced(&mut m, vec![k(KeyCode::Char('a'))]);
    assert!(m.ssh_form.is_some(), "fixture: form open");
    let focus0 = m.ssh_form.as_ref().map(|f| f.focus);
    let accepted = burst(
        &mut m,
        vec![
            k(KeyCode::Char('n')),
            k(KeyCode::Tab),
            k(KeyCode::Char('h')),
            k(KeyCode::Char('x')),
        ],
    );
    let form = m.ssh_form.as_ref().unwrap();
    println!(
        "burst ssh-form focus0={focus0:?} accepted={accepted:?} focus={}",
        form.focus
    );
    assert_eq!(
        accepted,
        vec![true, true, true, true],
        "form type-ahead lost after Tab"
    );
}

#[test]
fn ctl_composer_burst_typing_is_kept() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.composer.clear();
    paint(&mut m);
    let accepted = burst(
        &mut m,
        "hello".chars().map(|c| k(KeyCode::Char(c))).collect(),
    );
    println!(
        "burst composer accepted={accepted:?} text={:?}",
        m.composer.text()
    );
    assert_eq!(m.composer.text(), "hello");
}

#[test]
fn esc_then_typing_burst_after_picker() {
    // Characterization only (not a verdict row): Esc closes a picker, typed
    // text follows in the same read.
    let mut m = theme_model();
    m.composer.clear();
    let mut events = vec![k(KeyCode::Esc)];
    events.extend("hi".chars().map(|c| k(KeyCode::Char(c))));
    let accepted = burst(&mut m, events);
    assert_eq!(accepted, vec![true; 3]);
    assert_eq!(m.composer.text(), "hi");
}

#[test]
fn ctl_external_owner_change_between_stamp_and_dispatch_refuses_key() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.composer.clear();
    paint(&mut m);
    let stamp = m.painted_stamp();
    // An owner arrives from outside the input queue and is presented.
    m.theme_picker = Some(ThemePicker {
        selection: 0,
        prior: m.theme_choice,
    });
    loop_present(&mut m);
    let ok = dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            received_at: Instant::now(),
            event: k(KeyCode::Enter),
            presented: stamp,
        },
    );
    println!(
        "external change: accepted={ok} picker_open={}",
        m.theme_picker.is_some()
    );
    assert!(!ok);
    assert!(m.theme_picker.is_some());
}

#[test]
fn ssh_form_two_tabs_and_text_are_kept() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.ssh_open = true;
    paint(&mut m);
    paced(&mut m, vec![k(KeyCode::Char('a'))]);
    assert_eq!(
        burst(
            &mut m,
            vec![
                k(KeyCode::Tab),
                k(KeyCode::Tab),
                k(KeyCode::Char('x')),
                k(KeyCode::Char('y')),
                k(KeyCode::Char('z'))
            ]
        ),
        vec![true; 5]
    );
    assert_eq!(m.ssh_form.as_ref().unwrap().host, "xyz");
}

#[test]
fn slash_mentions_search_and_startup_keep_the_whole_burst() {
    for text in ["/theme", "@path", "startup keys"] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.composer.clear();
        // Production paints before starting its input reader. No initial
        // key is read/stamped until this presentation exists.
        paint(&mut m);
        assert_eq!(
            burst(&mut m, text.chars().map(|c| k(KeyCode::Char(c))).collect()),
            vec![true; text.len()]
        );
        assert_eq!(m.composer.text(), text);
    }
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    paint(&mut m);
    assert_eq!(
        burst(
            &mut m,
            vec![
                Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
                k(KeyCode::Char('h')),
                k(KeyCode::Char('i'))
            ]
        ),
        vec![true; 3]
    );
    assert_eq!(m.transcript_search.as_ref().unwrap().query, "hi");
}

#[test]
fn esc_from_search_history_and_pickers_keeps_following_text() {
    for cover in ["search", "history", "theme", "effort"] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.composer.clear();
        match cover {
            "search" => m.transcript_search = Some(crate::app::TranscriptSearch::default()),
            "history" => m.backtrack = Some(crate::app::PromptBacktrack { selection: 0 }),
            "theme" => {
                m.theme_picker = Some(ThemePicker {
                    selection: 0,
                    prior: m.theme_choice,
                })
            }
            _ => m.effort_picker = Some(crate::app::EffortPicker::default()),
        }
        paint(&mut m);
        assert_eq!(
            burst(
                &mut m,
                vec![
                    k(KeyCode::Esc),
                    k(KeyCode::Char('h')),
                    k(KeyCode::Char('i'))
                ]
            ),
            vec![true; 3],
            "{cover}"
        );
        assert_eq!(m.composer.text(), "hi", "{cover}");
    }
}

#[test]
fn outside_change_breaks_an_already_rebased_chain_even_if_owner_returns() {
    for return_to_owner in [false, true] {
        let mut m = theme_model();
        let original = m.painted_stamp();
        assert!(dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event: k(KeyCode::Down),
                presented: original.clone()
            }
        ));
        loop_present(&mut m);
        m.help_open = true;
        paint(&mut m);
        if return_to_owner {
            m.help_open = false;
            paint(&mut m);
        }
        assert!(!dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event: k(KeyCode::Down),
                presented: original
            }
        ));
        assert_eq!(m.theme_picker.as_ref().unwrap().selection, 1);
        assert!(m.flash.as_deref().unwrap().contains("Input changed"));
    }
}

#[test]
fn focus_navigation_and_focus_loss_retire_a_press_without_an_owner_change() {
    for event in [k(KeyCode::Tab), Event::FocusLost] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.composer.clear();
        paint(&mut m);
        let owner = m.presented_input();
        dispatch_input(
            &mut m,
            &[],
            Event::Mouse(event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(m.mouse_down.is_some());
        dispatch_input(&mut m, &[], event);
        assert_eq!(m.presented_input(), owner);
        assert!(m.mouse_down.is_none());
        assert!(m.mouse_press_owner.is_none());
    }
}

pub(crate) fn decision(id: &str) -> haider_protocol::menu::Menu {
    use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
    Menu {
        id: haider_protocol::ids::MenuId::new(id),
        kind: MenuKind::Permission {
            effect_summary: "synthetic".into(),
            file_review: None,
        },
        title: "Confirm?".into(),
        body: vec![],
        options: vec![
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: None,
                decision: Some(DecisionKind::RejectOnce),
            },
            MenuOption {
                key: "apply".into(),
                label: "Apply".into(),
                detail: None,
                decision: Some(DecisionKind::AllowOnce),
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "test".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

#[test]
fn down_enter_requires_fresh_confirmation_and_flashes() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        decision("one"),
    ))));
    paint(&mut m);
    let stamp = m.painted_stamp();
    assert!(dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            event: k(KeyCode::Down),
            presented: stamp.clone(),
            received_at: Instant::now(),
        }
    ));
    loop_present(&mut m);
    assert!(!dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            event: k(KeyCode::Enter),
            presented: stamp,
            received_at: Instant::now(),
        }
    ));
    assert_eq!(m.menu_selection, 1);
    assert!(m.outbox.is_empty());
    assert_eq!(
        m.flash.as_deref(),
        Some("· Selection changed: press Enter again to confirm")
    );
    assert!(
        owner_needs_presentation(&m),
        "the refusal must paint immediately"
    );
    loop_present(&mut m);
    assert!(!owner_needs_presentation(&m));
    // Deliberate confirmation also waits for the shared new-card dwell.
    let stamp = m.painted_stamp();
    assert!(dispatch_queued_input(
        &mut m,
        &[],
        QueuedInput {
            event: k(KeyCode::Enter),
            presented: stamp,
            received_at: Instant::now() + AppModel::DECISION_DWELL,
        }
    ));
    assert_eq!(m.outbox.len(), 1);
}

#[test]
fn cursor_and_focus_owners_keep_burst_navigation_like_paced_navigation() {
    use crate::app::{EffortPicker, LoginCard, PromptBacktrack, TranscriptSearch};
    for cover in [
        "inbox",
        "shells",
        "ssh",
        "monitors",
        "effort",
        "history",
        "search",
        "login",
        "model",
        "custom",
        "talk",
        "accounts",
        "providers",
        "hooks",
        "fleet",
    ] {
        let make = || {
            let mut m = AppModel::new();
            m.screen = Screen::Session;
            match cover {
                "inbox" => m.inbox_open = true,
                "shells" => m.shells_open = true,
                "ssh" => m.ssh_open = true,
                "monitors" => m.monitors_open = true,
                "effort" => m.effort_picker = Some(EffortPicker::default()),
                "history" => m.backtrack = Some(PromptBacktrack { selection: 0 }),
                "search" => m.transcript_search = Some(TranscriptSearch::default()),
                "login" => m.login = Some(LoginCard::new("test".into(), "alias".into(), 1)),
                "model" => m.open_model_picker(String::new()),
                "custom" => {
                    m.mode = crate::app::RuntimeMode::Live;
                    m.screen = Screen::Providers;
                    m.daemon_features
                        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.into());
                    m.handle(AppEvent::Key(KeyEvent::new(
                        KeyCode::Char('o'),
                        KeyModifiers::NONE,
                    )));
                }
                "talk" => {
                    m.talk_setup = Some(crate::talk::TalkSetupCard::new(
                        m.talk_config.clone(),
                        false,
                    ))
                }
                "accounts" => m.screen = Screen::Accounts,
                "providers" => m.screen = Screen::Providers,
                "hooks" => m.screen = Screen::Hooks,
                _ => m.screen = Screen::Fleet,
            }
            paint(&mut m);
            m
        };
        let mut queued = make();
        let mut human = make();
        let keys = || vec![k(KeyCode::Down), k(KeyCode::Down), k(KeyCode::Tab)];
        assert_eq!(burst(&mut queued, keys()), vec![true; 3], "{cover}");
        assert_eq!(paced(&mut human, keys()), vec![true; 3], "{cover}");
        assert_eq!(queued.presented_input(), human.presented_input(), "{cover}");
    }
}

#[test]
fn spend_owner_plugin_only_owns_input_when_visible_and_below_local_covers() {
    use crate::app::{EffortPicker, InputOwner, PromptBacktrack, TranscriptSearch};
    use haider_protocol::permission::{
        PermissionGrantAction, PermissionGrantNeeded, SystemPermission,
    };
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.composer.clear();
    let card = decision("usage-plugin");
    assert_eq!(
        m.spend_card_input_owner(&card.id),
        InputOwner::Composer(Screen::Session)
    );
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        card.clone(),
    ))));
    assert_eq!(m.spend_card_input_owner(&card.id), InputOwner::SpendCard);
    assert_eq!(
        m.spend_card_input_owner(&decision("another-card").id),
        InputOwner::SessionMenu
    );
    m.projection.set_permission_card(PermissionGrantNeeded {
        request_id: "visible-native-grant".into(),
        menu_id: card.id.clone(),
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
    assert_eq!(m.spend_card_input_owner(&card.id), InputOwner::SessionMenu);
    m.projection.resolve_permission_card("visible-native-grant");
    assert_eq!(m.spend_card_input_owner(&card.id), InputOwner::SpendCard);
    m.screen = Screen::Usage;
    assert_eq!(
        m.spend_card_input_owner(&card.id),
        InputOwner::Screen(Screen::Usage)
    );
    m.screen = Screen::Session;
    m.composer.set_text("draft");
    assert_eq!(
        m.spend_card_input_owner(&card.id),
        InputOwner::Composer(Screen::Session)
    );
    m.composer.clear();
    m.theme_picker = Some(ThemePicker {
        selection: 0,
        prior: m.theme_choice,
    });
    assert_eq!(m.spend_card_input_owner(&card.id), InputOwner::ThemePicker);
    m.effort_picker = Some(EffortPicker {
        selection: 0,
        pending: None,
        error: None,
    });
    assert_eq!(m.spend_card_input_owner(&card.id), InputOwner::EffortPicker);
    m.transcript_search = Some(TranscriptSearch::default());
    assert_eq!(m.spend_card_input_owner(&card.id), InputOwner::Search);
    m.transcript_search = None;
    m.effort_picker = None;
    m.theme_picker = None;
    m.backtrack = Some(PromptBacktrack { selection: 0 });
    assert_eq!(
        m.spend_card_input_owner(&card.id),
        InputOwner::HistoryChooser
    );
    m.help_open = true;
    assert_eq!(m.spend_card_input_owner(&card.id), InputOwner::Help);
}

#[test]
fn outside_plain_draft_replacements_break_the_queue_even_when_text_is_identical() {
    for initial in ["", "plain"] {
        for identical in [false, true] {
            let mut m = AppModel::new();
            m.screen = Screen::Session;
            m.composer.set_text(initial);
            paint(&mut m);
            let source = m.painted_stamp();
            assert!(dispatch_queued_input(
                &mut m,
                &[],
                QueuedInput {
                    received_at: Instant::now(),
                    event: k(KeyCode::Char('q')),
                    presented: source.clone(),
                }
            ));
            loop_present(&mut m);
            let replacement = if identical {
                m.composer.text().to_owned()
            } else {
                "foreign".to_owned()
            };
            // Foreign mirrors and injected Set edits share this reducer.
            m.handle(AppEvent::SurfaceInputReplace {
                text: replacement.clone(),
            });
            assert!(owner_needs_presentation(&m));
            loop_present(&mut m);
            assert!(!dispatch_queued_input(
                &mut m,
                &[],
                QueuedInput {
                    received_at: Instant::now(),
                    event: k(KeyCode::Char('x')),
                    presented: source,
                }
            ));
            assert_eq!(m.composer.text(), replacement);
            assert!(m.flash.as_deref().unwrap().contains("Input changed"));
            loop_present(&mut m);
            let current = m.painted_stamp();
            assert!(dispatch_queued_input(
                &mut m,
                &[],
                QueuedInput {
                    received_at: Instant::now(),
                    event: k(KeyCode::Char('x')),
                    presented: current,
                }
            ));
            assert_eq!(m.composer.text(), format!("{replacement}x"));
        }
    }
}

#[test]
fn outside_popup_replacement_rearms_pointer_dwell_but_local_typing_keeps_it() {
    let now = Instant::now();
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.composer.set_text("/theme");
    assert!(m.palette_open());
    m.record_painted_input_at(now - Duration::from_secs(1));
    m.composer.set_text("/the");
    m.record_painted_input_at(now);
    assert!(m.pointer_commit_ready(now));
    m.handle(AppEvent::SurfaceInputReplace {
        text: "/the".into(),
    });
    m.record_painted_input_at(now);
    assert!(!m.pointer_commit_ready(now + Duration::from_millis(399)));
    assert!(m.pointer_commit_ready(now + Duration::from_millis(400)));
}

#[test]
fn same_id_card_replacement_breaks_a_rebased_queue() {
    for change in ["option", "meaning", "title"] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        let mut card = decision("same-id");
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            card.clone(),
        ))));
        paint(&mut m);
        let stamp = m.painted_stamp();
        assert!(dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event: k(KeyCode::Down),
                presented: stamp.clone()
            }
        ));
        loop_present(&mut m);
        match change {
            "option" => card.options[1].key = "new-effect".into(),
            "meaning" => card.kind = haider_protocol::menu::MenuKind::Question,
            _ => card.title = "Different question".into(),
        }
        m.projection.apply(&EventPayload::MenuOpened(card));
        paint(&mut m);
        assert!(!dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event: k(KeyCode::Down),
                presented: stamp
            }
        ));
        assert!(m.outbox.is_empty());
    }
}

#[test]
fn pending_card_count_includes_covered_decisions_and_retires_by_id() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    let a = decision("A");
    let b = decision("B");
    for card in [a.clone(), b.clone(), b.clone()] {
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(card))));
    }
    assert_eq!(m.projection.pending_menu_count(), 2);
    assert_eq!(m.inbox_counts().asks, 2);
    for (width, height) in [(118, 36), (80, 24)] {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let Ok(mut term) = Terminal::new(backend);
        term.draw(|frame| drop(render(&m, frame))).unwrap();
        let screen = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("2 asks"), "{width}x{height}: {screen}");
    }
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuClosed {
        menu: b.id,
        reason: haider_protocol::menu::MenuCloseReason::Cancelled,
    })));
    assert_eq!(m.inbox_counts().asks, 1);
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuClosed {
        menu: a.id,
        reason: haider_protocol::menu::MenuCloseReason::Cancelled,
    })));
    assert_eq!(m.inbox_counts().asks, 0);
}

#[test]
fn a_reader_racing_frame_publication_keeps_the_previous_arrival_stamp() {
    let m = AppModel::new();
    let first = m.presented_input();
    let now = Instant::now();
    let state = Mutex::new(std::collections::VecDeque::from([
        ((first.clone(), 1), now),
        ((first.clone(), 2), now + Duration::from_millis(10)),
    ]));
    assert_eq!(
        input_stamp(&state, now + Duration::from_millis(5)),
        Some((first, 1))
    );
}

#[test]
fn startup_typing_is_preserved_but_unpresented_pointer_and_decision_inputs_are_refused() {
    let before = Instant::now();
    let mut m = AppModel::new();
    m.screen = Screen::Launcher;
    paint(&mut m);
    let frame = Mutex::new(std::collections::VecDeque::from([(
        m.painted_stamp().unwrap(),
        Instant::now(),
    )]));
    let initial = m.painted_stamp();
    let typed = stamped_input(
        &frame,
        k(KeyCode::Char('x')),
        before,
        true,
        initial.as_ref(),
    );
    assert!(dispatch_queued_input(&mut m, &[], typed));
    assert_eq!(m.composer.text(), "x");
    let pointer = stamped_input(
        &frame,
        Event::Mouse(event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }),
        before,
        true,
        initial.as_ref(),
    );
    assert!(pointer.presented.is_none());
    assert!(!dispatch_queued_input(&mut m, &[], pointer));
    m.screen = Screen::Session;
    m.composer.clear();
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        decision("startup-decision"),
    ))));
    paint(&mut m);
    frame
        .lock()
        .unwrap()
        .push_back((m.painted_stamp().unwrap(), Instant::now()));
    let decision_frame = m.painted_stamp();
    let early_enter = stamped_input(
        &frame,
        k(KeyCode::Enter),
        before,
        true,
        decision_frame.as_ref(),
    );
    assert!(early_enter.presented.is_none());
    assert!(!dispatch_queued_input(&mut m, &[], early_enter));
    assert!(m.outbox.is_empty());
    assert!(
        m.flash
            .as_deref()
            .is_some_and(|flash| flash.contains("Input changed"))
    );
    let deliberate = stamped_input(
        &frame,
        k(KeyCode::Enter),
        Instant::now() + AppModel::DECISION_DWELL,
        false,
        None,
    );
    assert!(dispatch_queued_input(&mut m, &[], deliberate));
    assert_eq!(m.outbox.len(), 1);
}

#[test]
fn external_terminal_reports_cannot_rebase_keys_onto_an_outside_owner() {
    for event in [Event::FocusGained, Event::FocusLost, Event::Resize(80, 24)] {
        let mut m = theme_model();
        let stamp = m.painted_stamp();
        m.help_open = true;
        paint(&mut m);
        assert!(dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event,
                presented: stamp.clone(),
            }
        ));
        loop_present(&mut m);
        assert!(!dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event: k(KeyCode::Esc),
                presented: stamp,
            }
        ));
        assert!(m.help_open);
        assert!(
            m.flash
                .as_deref()
                .is_some_and(|flash| flash.contains("Input changed"))
        );
    }
}

#[test]
fn outside_edits_to_a_visible_popup_break_the_queue_chain() {
    for prefix in ["/", "@"] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        paint(&mut m);
        let stamp = m.painted_stamp();
        assert!(dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event: k(KeyCode::Char(prefix.chars().next().unwrap())),
                presented: stamp.clone(),
            }
        ));
        loop_present(&mut m);
        assert_ne!(
            m.painted_stamp(),
            stamp,
            "opening the popup is a presentation boundary"
        );
        // Mirror/inject edits bump the composer revision without going
        // through this queue, even when the same popup remains visible.
        m.composer.insert_str("outside");
        paint(&mut m);
        assert!(!dispatch_queued_input(
            &mut m,
            &[],
            QueuedInput {
                received_at: Instant::now(),
                event: k(KeyCode::Char('x')),
                presented: stamp,
            }
        ));
        assert_eq!(m.composer.text(), format!("{prefix}outside"));
        assert!(
            m.flash
                .as_deref()
                .is_some_and(|flash| flash.contains("Input changed"))
        );
    }
}

#[test]
fn retained_startup_input_follows_its_own_cover_but_not_an_outside_cover() {
    for own_cover in [false, true] {
        let before = Instant::now();
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        paint(&mut m);
        let initial = m.painted_stamp();
        let frames = Mutex::new(std::collections::VecDeque::from([(
            initial.clone().unwrap(),
            Instant::now(),
        )]));
        let first = if own_cover {
            Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
        } else {
            k(KeyCode::Char('a'))
        };
        assert!(dispatch_queued_input(
            &mut m,
            &[],
            stamped_input(&frames, first, before, true, initial.as_ref()),
        ));
        if !own_cover {
            m.help_open = true;
        }
        paint(&mut m);
        frames
            .lock()
            .unwrap()
            .push_back((m.painted_stamp().unwrap(), Instant::now()));
        let next = stamped_input(
            &frames,
            k(KeyCode::Char('x')),
            before,
            true,
            initial.as_ref(),
        );
        assert_eq!(next.presented, initial);
        assert_eq!(dispatch_queued_input(&mut m, &[], next), own_cover);
        if own_cover {
            assert_eq!(m.transcript_search.as_ref().unwrap().query, "x");
        } else {
            assert_eq!(m.composer.text(), "a");
            assert!(m.flash.as_deref().unwrap().contains("Input changed"));
        }
    }
}

#[test]
fn startup_confirmation_on_a_non_composer_surface_stays_untrusted() {
    let before = Instant::now();
    let mut m = AppModel::new();
    m.screen = Screen::Accounts;
    m.accounts.pending_remove = Some("synthetic-account".into());
    paint(&mut m);
    let initial = m.painted_stamp();
    let frames = Mutex::new(std::collections::VecDeque::from([(
        initial.clone().unwrap(),
        Instant::now(),
    )]));
    let enter = stamped_input(&frames, k(KeyCode::Enter), before, true, initial.as_ref());
    assert!(enter.presented.is_none());
    assert!(!dispatch_queued_input(&mut m, &[], enter));
    assert!(m.requests.is_empty());
    assert!(m.accounts.pending_remove.is_some());
}

#[test]
fn startup_free_text_question_requires_a_presented_target() {
    for event in [
        k(KeyCode::Char('x')),
        Event::Paste("pasted answer".into()),
        k(KeyCode::Enter),
    ] {
        let before = Instant::now();
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        let mut question = decision("startup-free-text");
        question.options.clear();
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            question,
        ))));
        m.composer.set_text("saved answer");
        paint(&mut m);
        assert_eq!(
            m.input_owner(),
            crate::app::InputOwner::Composer(Screen::Session)
        );
        let initial = startup_input_presentation(&m);
        assert!(initial.is_none(), "an unseen question is a decision target");
        let frames = Mutex::new(std::collections::VecDeque::from([(
            m.painted_stamp().unwrap(),
            Instant::now(),
        )]));
        let early = stamped_input(&frames, event.clone(), before, true, initial.as_ref());
        assert!(!dispatch_queued_input(&mut m, &[], early));
        assert_eq!(m.composer.text(), "saved answer");
        assert!(m.outbox.is_empty());
        assert!(m.flash.as_deref().unwrap().contains("Input changed"));

        // The same event is accepted after presentation and its commit dwell.
        let confirms = matches!(&event, Event::Key(key) if key.code == KeyCode::Enter);
        let fresh = stamped_input(
            &frames,
            event,
            Instant::now() + AppModel::DECISION_DWELL,
            false,
            initial.as_ref(),
        );
        assert!(dispatch_queued_input(&mut m, &[], fresh));
        if confirms {
            assert_eq!(m.outbox.len(), 1);
        } else {
            assert_ne!(m.composer.text(), "saved answer");
        }
    }

    // Ordinary drafts retain their lossless startup cohort.
    let before = Instant::now();
    let mut m = AppModel::new();
    m.screen = Screen::Launcher;
    paint(&mut m);
    let initial = startup_input_presentation(&m);
    assert!(initial.is_some());
    let frames = Mutex::new(std::collections::VecDeque::from([(
        m.painted_stamp().unwrap(),
        Instant::now(),
    )]));
    let early = stamped_input(
        &frames,
        k(KeyCode::Char('x')),
        before,
        true,
        initial.as_ref(),
    );
    assert!(dispatch_queued_input(&mut m, &[], early));
    assert_eq!(m.composer.text(), "x");
}
