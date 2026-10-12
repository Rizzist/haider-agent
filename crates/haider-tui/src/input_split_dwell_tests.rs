#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::input_typeahead_tests::{decision, k, paint};
use super::*;
use crate::app::{AppEvent, AppModel, Screen};
use ratatui::crossterm::event::{KeyEvent, MouseEvent};

fn set_shown_at(m: &AppModel, at: Instant) {
    *m.pointer_presentation.borrow_mut() = Some((m.pointer_input_owner(), at));
    *m.text_question_presentation.borrow_mut() =
        m.free_text_question_owner().map(|owner| (owner, at));
}

fn dispatch(
    m: &mut AppModel,
    event: Event,
    stamp: Option<(PresentedInput, u64)>,
    at: Instant,
) -> bool {
    let accepted = dispatch_queued_input(
        m,
        &[],
        QueuedInput {
            event,
            presented: stamp,
            received_at: at,
        },
    );
    paint(m);
    accepted
}

#[test]
fn split_reads_keep_every_intermediate_queue_frame() {
    // Every event boundary of 2/3 reads, including opener, closer, paste and
    // key repeats. A later read uses an intermediate, already superseded
    // frame, exactly like a reader outrunning the dispatch loop.
    let variants = [
        vec![
            k(KeyCode::Char('/')),
            k(KeyCode::Char('t')),
            k(KeyCode::Char('h')),
            k(KeyCode::Char('e')),
            k(KeyCode::Char('m')),
            k(KeyCode::Char('e')),
            k(KeyCode::Enter),
            k(KeyCode::Down),
            k(KeyCode::Down),
        ],
        vec![
            k(KeyCode::Char('@')),
            k(KeyCode::Char('a')),
            k(KeyCode::Char('b')),
            Event::Paste("cd".into()),
        ],
        vec![
            Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            k(KeyCode::Char('a')),
            k(KeyCode::Char('b')),
            k(KeyCode::Esc),
            Event::Paste("text".into()),
        ],
    ];
    for (variant, events) in variants.iter().enumerate() {
        for split in 1..events.len() {
            for third in split..events.len() {
                let mut m = AppModel::new();
                m.screen = Screen::Session;
                paint(&mut m);
                let original = m.painted_stamp();
                let mut middle = None;
                let mut last = None;
                for (i, event) in events.iter().enumerate() {
                    let stamp = if i < split {
                        original.clone()
                    } else if i < third {
                        middle.clone()
                    } else {
                        last.clone().or_else(|| middle.clone())
                    };
                    assert!(
                        dispatch(&mut m, event.clone(), stamp, Instant::now()),
                        "variant={variant} split={split}/{third} event={i} flash={:?}",
                        m.flash
                    );
                    if i == 0 {
                        middle = m.painted_stamp();
                    }
                    if i == split {
                        last = m.painted_stamp();
                    }
                }
                if variant == 0 {
                    assert_eq!(m.theme_picker.map(|p| p.selection), Some(2));
                }
                if variant == 1 {
                    assert_eq!(m.composer.text(), "@abcd");
                }
                if variant == 2 {
                    assert_eq!(m.composer.text(), "text");
                }
            }
        }
    }
}

#[test]
fn split_chain_current_frame_does_not_forget_older_queued_frames() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    paint(&mut m);
    let original = m.painted_stamp();
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('@')),
        original.clone(),
        Instant::now()
    ));
    let middle = m.painted_stamp();
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('a')),
        middle.clone(),
        Instant::now()
    ));
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('b')),
        original,
        Instant::now()
    ));
    assert!(dispatch(
        &mut m,
        Event::Paste("cd".into()),
        middle,
        Instant::now()
    ));
    assert_eq!(m.composer.text(), "@abcd");
}

#[test]
fn outside_content_and_resize_retire_the_whole_split_chain() {
    for resize in [false, true] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        paint(&mut m);
        let original = m.painted_stamp();
        assert!(dispatch(
            &mut m,
            k(KeyCode::Char('@')),
            original,
            Instant::now()
        ));
        let middle = m.painted_stamp();
        assert!(dispatch(
            &mut m,
            k(KeyCode::Char('a')),
            middle.clone(),
            Instant::now()
        ));
        if resize {
            dispatch_input(&mut m, &[], Event::Resize(80, 24));
        } else {
            m.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
                HarnessStatus::Ready,
            ))));
        }
        paint(&mut m);
        assert!(!dispatch(
            &mut m,
            k(KeyCode::Char('b')),
            middle,
            Instant::now()
        ));
        assert_eq!(m.composer.text(), "@a");
        assert!(m.flash.as_deref().unwrap().contains("Input changed"));
    }
}

fn file_review_decision(id: &str) -> haider_protocol::menu::Menu {
    use haider_protocol::file_review::{FileDiffHunk, FileReview, FileReviewOperation};
    let mut menu = decision(id);
    if let haider_protocol::menu::MenuKind::Permission { file_review, .. } = &mut menu.kind {
        *file_review = Some(FileReview {
            effect: haider_protocol::ids::EffectId::new("synthetic"),
            path: "synthetic.rs".into(),
            operation: FileReviewOperation::Edit,
            old_digest: None,
            new_digest: "blake3:synthetic".into(),
            added: 0,
            removed: 0,
            hunks: (0..3)
                .map(|i| FileDiffHunk {
                    old_start: i + 1,
                    old_lines: 0,
                    new_start: i + 1,
                    new_lines: 0,
                    lines: vec![],
                })
                .collect(),
            truncated: false,
        });
    }
    menu
}

#[test]
fn review_hunk_letters_remain_navigation_during_decision_dwell() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    let mut menu = file_review_decision("review");
    menu.options[0].key = "n".into();
    menu.options[1].key = "p".into();
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
    paint(&mut m);
    let t = Instant::now();
    set_shown_at(&m, t);
    for (i, (code, hunk)) in [('n', 1), ('p', 0), (']', 1), ('[', 0)]
        .into_iter()
        .enumerate()
    {
        dispatch_input_at(
            &mut m,
            &[],
            k(KeyCode::Char(code)),
            t + Duration::from_millis(100 + i as u64 * 50),
        );
        assert!(m.outbox.is_empty());
        assert!(m.flash.is_none(), "{code} is navigation");
        assert_eq!(m.file_review_hunk.get(), hunk);
        paint(&mut m);
    }
    dispatch_input_at(&mut m, &[], k(KeyCode::Enter), t + AppModel::DECISION_DWELL);
    assert_eq!(m.outbox.len(), 1);
}

#[test]
fn committing_keys_share_card_dwell_navigation_and_default() {
    for kind in ["permission", "question", "plan", "file", "child"] {
        for code in [KeyCode::Enter, KeyCode::Char('1')] {
            for gap in [0, 120, 250, 399, 400, 401] {
                let t = Instant::now() + Duration::from_secs(5);
                let mut m = AppModel::new();
                m.screen = Screen::Session;
                let mut menu = if kind == "file" {
                    file_review_decision("A")
                } else {
                    decision("A")
                };
                menu.options.swap(0, 1); // Default is a committing Allow option.
                menu.options[0].key = "y".into();
                menu.options[1].key = "n".into();
                if kind == "question" {
                    menu.kind = haider_protocol::menu::MenuKind::Question;
                }
                if kind == "plan" {
                    menu.origin = "plan".into();
                }
                let mut b = menu.clone();
                b.id = haider_protocol::ids::MenuId::new("B");
                if kind == "child" {
                    let mut chip = super::runtime_pointer_tests::question_chip("child", "A");
                    crate::session::chip_apply(&mut chip, &EventPayload::MenuOpened(menu), 0);
                    m.chips.push(chip);
                    m.screen = Screen::Subagent;
                    m.view_path = vec!["child".into()];
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
                }
                paint(&mut m);
                set_shown_at(&m, t - Duration::from_secs(1));
                dispatch_input_at(&mut m, &[], k(code), t);
                assert_eq!(m.outbox.len(), 1, "{kind} {code:?}");
                if kind == "child" {
                    crate::session::chip_apply(&mut m.chips[0], &EventPayload::MenuOpened(b), 0);
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(b))));
                }
                m.menu_selection = 0; // The live scope router resets a newly opened child's selection too.
                paint(&mut m);
                set_shown_at(&m, t);
                assert_eq!(m.menu_selection, 0, "default remains unchanged");
                let navigation = if matches!(kind, "plan" | "file") {
                    KeyCode::Tab
                } else {
                    KeyCode::Down
                };
                dispatch_input_at(&mut m, &[], k(navigation), t + Duration::from_millis(20));
                assert_eq!(m.menu_selection, 1, "navigation during dwell");
                paint(&mut m);
                dispatch_input_at(&mut m, &[], k(code), t + Duration::from_millis(gap));
                assert_eq!(
                    m.outbox.len(),
                    if gap < 400 { 1 } else { 2 },
                    "{kind} {code:?} gap={gap}"
                );
                if gap < 400 {
                    assert!(
                        m.flash
                            .as_deref()
                            .unwrap()
                            .contains("New request — press again")
                    );
                }
            }
        }
    }
}

#[test]
fn stale_motion_and_wheel_are_silent_but_clicks_and_keys_flash() {
    for kind in [
        MouseEventKind::Moved,
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight,
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        paint(&mut m);
        let stale = m.painted_stamp();
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            decision("new"),
        ))));
        paint(&mut m);
        let passive = matches!(
            kind,
            MouseEventKind::Moved
                | MouseEventKind::Drag(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        );
        assert!(!dispatch(
            &mut m,
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE
            }),
            stale,
            Instant::now()
        ));
        assert_eq!(m.flash.is_none(), passive, "{kind:?}");
    }
}

#[test]
fn retained_startup_path_delivers_all_events_in_order_with_original_time() {
    let at = Instant::now();
    let queue = Mutex::new(std::collections::VecDeque::new());
    let events = vec![
        k(KeyCode::Char('/')),
        k(KeyCode::Char('t')),
        Event::Paste("heme".into()),
        k(KeyCode::Enter),
        Event::Resize(80, 24),
    ];
    retain_startup_input(&queue, events.clone(), at);
    let mut startup = std::mem::take(&mut *queue.lock().unwrap());
    assert_eq!(startup.len(), events.len());
    for expected in events {
        let (actual, received, retained) = read_input(&mut startup).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(received, at);
        assert!(retained);
    }
    assert!(startup.is_empty());
}

#[test]
fn outside_retirement_preserves_fresh_decision_commit_feedback_and_refuses_every_stamp() {
    for child in [false, true] {
        for text in [false, true] {
            for gap in [0, 120, 399, 400, 401] {
                for event in [
                    k(KeyCode::Enter),
                    k(KeyCode::Char('1')),
                    k(KeyCode::Char('y')),
                    k(KeyCode::Char('N')),
                    k(KeyCode::Down),
                    Event::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::NONE,
                    }),
                    Event::Mouse(MouseEvent {
                        kind: MouseEventKind::Up(MouseButton::Left),
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::NONE,
                    }),
                    Event::Mouse(MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::NONE,
                    }),
                    Event::Paste("edit".into()),
                ] {
                    let mut m = text_question(child, "fresh");
                    if !text {
                        let mut menu = decision("fresh-choice");
                        menu.options[0].key = "y".into();
                        menu.options[1].key = "n".into();
                        if child {
                            crate::session::chip_apply(
                                &mut m.chips[0],
                                &EventPayload::MenuOpened(menu),
                                0,
                            );
                        } else {
                            m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
                        }
                    }
                    paint(&mut m);
                    let t = Instant::now();
                    set_shown_at(&m, t);
                    let stale = m.painted_stamp();
                    m.invalidate_queued_input();
                    let before = m.composer.text().to_owned();
                    let expected = match &event {
                        Event::Key(key)
                            if gap < 400
                                && (key.code == KeyCode::Enter
                                    || (!text && matches!(key.code, KeyCode::Char('1')))) =>
                        {
                            Some("New request")
                        }
                        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => None,
                        Event::Mouse(_) if gap < 400 => Some("New surface"),
                        _ => Some("Input changed"),
                    };
                    assert!(!dispatch(
                        &mut m,
                        event.clone(),
                        stale,
                        t + Duration::from_millis(gap)
                    ));
                    assert!(m.outbox.is_empty());
                    if expected == Some("New request") {
                        assert_eq!(m.flash.as_deref(), Some("· New request — press again"));
                    }
                    assert_eq!(m.composer.text(), before);
                    assert_eq!(m.menu_selection, 0);
                    match expected {
                        Some(copy) => assert!(
                            m.flash.as_deref().is_some_and(|s| s.contains(copy)),
                            "child={child} text={text} gap={gap} {event:?}: {:?}",
                            m.flash
                        ),
                        None => assert!(m.flash.is_none()),
                    }
                }
            }
        }
    }
}

#[test]
fn unpainted_successors_cannot_borrow_a_mature_decision_clock_or_feedback() {
    for kind in [
        "permission",
        "question",
        "plan",
        "file",
        "child",
        "text",
        "child-text",
        "accounts",
    ] {
        for stamped in [false, true] {
            for event in [
                k(KeyCode::Enter),
                k(KeyCode::Char('1')),
                k(KeyCode::Char('y')),
                k(KeyCode::Char('N')),
                k(KeyCode::Down),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                }),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                }),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                }),
                Event::Paste("edit".into()),
            ] {
                let child = matches!(kind, "child" | "child-text");
                let mut menu = if kind == "file" {
                    file_review_decision("first")
                } else {
                    decision("first")
                };
                menu.options[0].key = "y".into();
                menu.options[1].key = "n".into();
                if kind == "question" {
                    menu.kind = haider_protocol::menu::MenuKind::Question;
                }
                if kind == "plan" {
                    menu.origin = "plan".into();
                }
                if matches!(kind, "text" | "child-text") {
                    menu.options.clear();
                }
                let mut m = AppModel::new();
                m.screen = Screen::Session;
                if kind == "accounts" {
                    m.screen = Screen::Accounts;
                    m.accounts.pending_replace = Some(crate::app::AccountAddKind::Ollama);
                } else if child {
                    m.chips.push(super::runtime_pointer_tests::question_chip(
                        "child", "first",
                    ));
                    crate::session::chip_apply(
                        &mut m.chips[0],
                        &EventPayload::MenuOpened(menu.clone()),
                        0,
                    );
                    m.screen = Screen::Subagent;
                    m.view_path = vec!["child".into()];
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
                        menu.clone(),
                    ))));
                }
                paint(&mut m);
                let t = Instant::now();
                set_shown_at(&m, t - Duration::from_secs(1));
                let stamp = stamped.then(|| m.painted_stamp()).flatten();
                menu.id = haider_protocol::ids::MenuId::new("successor");
                if kind == "accounts" {
                    m.accounts.pending_replace = Some(crate::app::AccountAddKind::LmStudio);
                } else if child {
                    crate::session::chip_apply(&mut m.chips[0], &EventPayload::MenuOpened(menu), 0);
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
                }
                // The old clock is mature, but this logical successor has not
                // painted yet. The refusal will become visible on its first paint.
                assert_ne!(m.painted_input(), Some(m.presented_input()));
                let commits = matches!(&event, Event::Key(key) if m.decision_key_commits(key));
                if commits {
                    assert!(!m.decision_key_commit_ready(t), "{kind}");
                }
                let expected = match &event {
                    Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => None,
                    Event::Mouse(_) if stamped => Some("New surface"),
                    Event::Key(_) if stamped && commits => Some("New request"),
                    _ => Some("Input changed"),
                };
                assert!(!dispatch(&mut m, event.clone(), stamp, t));
                assert!(m.outbox.is_empty());
                assert!(m.composer.text().is_empty());
                assert_eq!(m.menu_selection, 0);
                match expected {
                    Some(copy) => assert!(
                        m.flash.as_deref().is_some_and(|s| s.contains(copy)),
                        "{kind} stamped={stamped} {event:?}: {:?}",
                        m.flash
                    ),
                    None => assert!(m.flash.is_none()),
                }
            }
        }
    }
}

#[test]
fn live_child_menu_metadata_is_available_without_demo_beats() {
    let mut chip = super::runtime_pointer_tests::question_chip("child", "old");
    chip.question = None;
    let menu = decision("live");
    crate::session::chip_apply(&mut chip, &EventPayload::MenuOpened(menu), 10);
    assert_eq!(
        chip.question_menu().map(|menu| menu.id.as_str()),
        Some("live")
    );
    assert!(!chip.question.as_ref().unwrap().resolved);
    // Unrelated replies/closures cannot resolve this question.
    crate::session::chip_apply(
        &mut chip,
        &EventPayload::MenuClosed {
            menu: haider_protocol::ids::MenuId::new("other"),
            reason: haider_protocol::menu::MenuCloseReason::Dismissed,
        },
        20,
    );
    assert!(chip.question_menu().is_some());
    crate::session::chip_apply(
        &mut chip,
        &EventPayload::MenuClosed {
            menu: haider_protocol::ids::MenuId::new("live"),
            reason: haider_protocol::menu::MenuCloseReason::Dismissed,
        },
        30,
    );
    assert!(chip.question_menu().is_none());
    assert!(chip.question.as_ref().unwrap().resolved);
    crate::session::chip_apply(
        &mut chip,
        &EventPayload::MenuOpened(decision("successor")),
        40,
    );
    assert_eq!(
        chip.question_menu().map(|menu| menu.id.as_str()),
        Some("successor")
    );
    chip.closed = true;
    assert!(chip.question_menu().is_none());
}

#[test]
fn split_repeat_and_closers_match_paced_input_on_every_owner() {
    use crate::app::{EffortPicker, PromptBacktrack, ThemePicker, TranscriptSearch};
    for cover in [
        "theme", "effort", "model", "sessions", "inbox", "ssh", "history", "search",
    ] {
        let make = || {
            let mut m = AppModel::new();
            m.screen = Screen::Session;
            match cover {
                "theme" => {
                    m.theme_picker = Some(ThemePicker {
                        selection: 0,
                        prior: m.theme_choice,
                    })
                }
                "effort" => m.effort_picker = Some(EffortPicker::default()),
                "model" => {
                    m.providers.providers.push(haider_rpc::ProviderSummaryWire {
                        provider: "synthetic".into(),
                        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
                        endpoint: None,
                        response_open_timeout_ms: None,
                        chunk_idle_timeout_ms: None,
                        semantic_progress_timeout_ms: None,
                        models: (0..4).map(|i| format!("model-{i}")).collect(),
                        model_details: vec![],
                        catalog: haider_rpc::ProviderCatalogKindWire::Unknown,
                        inventory: haider_rpc::ModelInventoryWire::Static,
                        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
                        auth_methods: vec![haider_protocol::credential::AuthMethod::ApiKey],
                        availability: haider_rpc::ProviderAvailabilityWire::Available,
                        availability_reason: None,
                        default_model: None,
                        enabled: true,
                        trust: haider_rpc::ProviderTrustWire::Full,
                    });
                    m.open_model_picker(String::new());
                    assert!(m.model_picker_rows().len() >= 4);
                }
                "sessions" => {
                    for name in ["a", "b", "c", "d"] {
                        m.upsert_live_session(&haider_protocol::ids::SessionId::new(name));
                    }
                    m.screen = Screen::Sessions;
                }
                "inbox" => {
                    for i in 0..4 {
                        m.chips.push(super::runtime_pointer_tests::question_chip(
                            &format!("child-{i}"),
                            &format!("question-{i}"),
                        ));
                    }
                    m.inbox_open = true;
                    assert!(m.inbox_rows().len() >= 4);
                }
                "ssh" => {
                    m.ssh_profiles = (0..4)
                        .map(|i| haider_rpc::SshProfileWire {
                            name: format!("profile-{i}"),
                            description: None,
                            host: "127.0.0.1".into(),
                            port: 1,
                            user: "synthetic".into(),
                            default_cwd: None,
                            host_key: None,
                            last_used_ms: None,
                            multiplexing: false,
                            in_scope: true,
                        })
                        .collect();
                    m.ssh_open = true;
                }
                "history" => m.backtrack = Some(PromptBacktrack { selection: 0 }),
                _ => m.transcript_search = Some(TranscriptSearch::default()),
            }
            paint(&mut m);
            m
        };
        for close in [false, true] {
            let mut slow = make();
            let mut paced = make();
            let events = if close {
                vec![
                    k(KeyCode::Esc),
                    k(KeyCode::Char('h')),
                    Event::Paste("i".into()),
                ]
            } else {
                vec![k(KeyCode::Down), k(KeyCode::Down), k(KeyCode::Down)]
            };
            let first = slow.painted_stamp();
            let mut middle = None;
            for (i, event) in events.into_iter().enumerate() {
                let read_stamp = if i < 2 { first.clone() } else { middle.clone() };
                assert!(
                    dispatch(&mut slow, event.clone(), read_stamp, Instant::now()),
                    "{cover} close={close} event={i}"
                );
                if i == 0 {
                    middle = slow.painted_stamp();
                }
                let current = paced.painted_stamp();
                assert!(dispatch(&mut paced, event, current, Instant::now()));
            }
            if !close {
                match cover {
                    "inbox" => assert_eq!(slow.inbox_cursor, 3),
                    "ssh" => assert_eq!(slow.ssh_cursor, 3),
                    "model" => assert_eq!(slow.model_picker.as_ref().unwrap().selection, 3),
                    _ => {}
                }
            }
            assert_eq!(
                slow.presented_input(),
                paced.presented_input(),
                "{cover} close={close}"
            );
            assert_eq!(
                slow.composer.text(),
                paced.composer.text(),
                "{cover} close={close}"
            );
        }
    }
}

#[test]
fn split_mixed_pointer_reports_do_not_drop_typing() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    paint(&mut m);
    let initial = m.painted_stamp();
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('@')),
        initial.clone(),
        Instant::now()
    ));
    let middle = m.painted_stamp();
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('a')),
        initial,
        Instant::now()
    ));
    // Pointer reports retain the stronger pointer-stability gate. Passive
    // reports can be dropped without flashing or breaking the key cohort.
    let pointer = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    dispatch(&mut m, pointer, middle.clone(), Instant::now());
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('b')),
        middle,
        Instant::now()
    ));
    assert_eq!(m.composer.text(), "@ab");
}

#[test]
fn child_free_text_question_renders_edits_and_obeys_commit_dwell() {
    for (w, h) in [(118, 36), (80, 24)] {
        let mut m = AppModel::new();
        m.chips
            .push(super::runtime_pointer_tests::question_chip("child", "text"));
        let mut menu = decision("text");
        menu.options.clear();
        menu.title = "Child free-text question".into();
        crate::session::chip_apply(&mut m.chips[0], &EventPayload::MenuOpened(menu), 0);
        m.screen = Screen::Subagent;
        m.view_path = vec!["child".into()];
        let t = Instant::now();
        let backend = ratatui::backend::TestBackend::new(w, h);
        let Ok(mut term) = Terminal::new(backend);
        term.draw(|frame| drop(render(&m, frame))).unwrap();
        let text = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Child free-text question"));
        assert_eq!(
            m.input_owner(),
            crate::app::InputOwner::Composer(Screen::Subagent)
        );
        set_shown_at(&m, t);
        dispatch_input_at(
            &mut m,
            &[],
            Event::Paste("typed answer".into()),
            t + Duration::from_millis(10),
        );
        paint(&mut m);
        dispatch_input_at(
            &mut m,
            &[],
            k(KeyCode::Enter),
            t + Duration::from_millis(120),
        );
        assert!(m.outbox.is_empty());
        assert_eq!(m.composer.text(), "typed answer");
        dispatch_input_at(
            &mut m,
            &[],
            k(KeyCode::Enter),
            t + Duration::from_millis(400),
        );
        assert_eq!(m.outbox.len(), 1);
        assert_eq!(m.outbox[0].answer.value.as_deref(), Some("typed answer"));
        assert!(
            !m.requests
                .iter()
                .any(|r| matches!(r, crate::app::AppRequest::ChipSubmit { .. }))
        );
    }
}

#[test]
fn restored_and_same_id_decisions_rearm_keyboard_dwell_while_tab_navigates() {
    for restore in [false, true] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        let t = Instant::now();
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            decision("same"),
        ))));
        paint(&mut m);
        set_shown_at(&m, t - Duration::from_secs(1));
        if restore {
            m.theme_picker = Some(crate::app::ThemePicker {
                selection: 0,
                prior: m.theme_choice,
            });
            m.help_open = true;
            paint(&mut m);
            m.help_open = false;
            m.theme_picker = None;
        } else {
            m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
                decision("same"),
            ))));
        }
        paint(&mut m);
        set_shown_at(&m, t);
        dispatch_input_at(&mut m, &[], k(KeyCode::Tab), t + Duration::from_millis(100));
        assert_eq!(m.menu_selection, 1);
        paint(&mut m);
        dispatch_input_at(
            &mut m,
            &[],
            k(KeyCode::Enter),
            t + Duration::from_millis(120),
        );
        assert!(m.outbox.is_empty());
        dispatch_input_at(
            &mut m,
            &[],
            k(KeyCode::Enter),
            t + Duration::from_millis(400),
        );
        assert_eq!(m.outbox.len(), 1);
    }
}

#[test]
fn queued_keyboard_dwell_uses_ingress_time_and_ordinary_edits_still_work() {
    let t = Instant::now() - Duration::from_secs(1);
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        decision("ingress"),
    ))));
    paint(&mut m);
    set_shown_at(&m, t);
    let stamp = m.painted_stamp();
    dispatch(
        &mut m,
        k(KeyCode::Enter),
        stamp,
        t + Duration::from_millis(120),
    );
    assert!(
        m.outbox.is_empty(),
        "dispatch occurred after dwell; ingress was early"
    );
    let stamp = m.painted_stamp();
    dispatch(
        &mut m,
        k(KeyCode::Enter),
        stamp,
        t + Duration::from_millis(400),
    );
    assert_eq!(m.outbox.len(), 1);
    // Draft Enter, text, paste and Shift+Enter stay responsive on ordinary
    // composers. Free-text card edits do not count as committing gestures.
    let mut q = decision("text");
    q.options.clear();
    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(q))));
    paint(&mut m);
    set_shown_at(&m, Instant::now());
    dispatch_input(&mut m, &[], k(KeyCode::Char('a')));
    paint(&mut m);
    dispatch_input(&mut m, &[], Event::Paste("b".into()));
    paint(&mut m);
    dispatch_input(
        &mut m,
        &[],
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
    );
    assert_eq!(m.composer.text(), "ab\n");
    assert_eq!(m.outbox.len(), 1);
}

#[test]
fn letter_option_keys_and_unmapped_digits_never_commit() {
    for c in ['y', 'Y', 'n', 'N'] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        let mut menu = decision("letters");
        menu.options[0].key = "y".into();
        menu.options[1].key = "n".into();
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
        paint(&mut m);
        let t = Instant::now();
        set_shown_at(&m, t);
        let mut repeat = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        dispatch_input_at(
            &mut m,
            &[],
            Event::Key(repeat),
            t + Duration::from_millis(120),
        );
        assert!(m.outbox.is_empty());
        dispatch_input_at(
            &mut m,
            &[],
            k(KeyCode::Char('9')),
            t + Duration::from_millis(130),
        );
        assert!(m.outbox.is_empty());
        assert!(m.flash.is_none());
        dispatch_input_at(
            &mut m,
            &[],
            Event::Key(repeat),
            t + Duration::from_millis(400),
        );
        assert!(m.outbox.is_empty());
        assert!(m.flash.is_none());
    }
}

#[test]
fn modified_keys_that_reach_menu_commits_share_dwell() {
    for modifiers in (0..64)
        .map(KeyModifiers::from_bits_retain)
        .filter(|flags| !flags.contains(KeyModifiers::CONTROL))
    {
        for code in [
            KeyCode::Enter,
            KeyCode::Char('1'),
            KeyCode::Char('y'),
            KeyCode::Char('β'),
        ] {
            let mut m = AppModel::new();
            m.screen = Screen::Session;
            let mut menu = decision("modified");
            menu.options[0].key = if code == KeyCode::Char('β') {
                "β".into()
            } else {
                "y".into()
            };
            m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
            paint(&mut m);
            let t = Instant::now();
            set_shown_at(&m, t);
            let event = Event::Key(KeyEvent::new(code, modifiers));
            dispatch_input_at(&mut m, &[], event.clone(), t + Duration::from_millis(120));
            assert!(m.outbox.is_empty(), "{code:?} {modifiers:?}");
            dispatch_input_at(&mut m, &[], event, t + AppModel::DECISION_DWELL);
            assert_eq!(
                m.outbox.len(),
                usize::from(matches!(code, KeyCode::Enter | KeyCode::Char('1'))),
                "{code:?} {modifiers:?}"
            );
        }
    }
}

#[test]
fn intermediate_pointer_frames_on_the_queues_new_surface_remain_valid() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    paint(&mut m);
    let original = m.painted_stamp();
    for c in "/theme".chars() {
        dispatch(
            &mut m,
            k(KeyCode::Char(c)),
            original.clone(),
            Instant::now(),
        );
    }
    dispatch(&mut m, k(KeyCode::Enter), original.clone(), Instant::now());
    let picker = m.painted_stamp();
    dispatch(&mut m, k(KeyCode::Down), picker.clone(), Instant::now());
    dispatch(&mut m, k(KeyCode::Down), original, Instant::now());
    let mouse = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 2,
        row: 2,
        modifiers: KeyModifiers::NONE,
    });
    let hits = [(
        ratatui::layout::Rect::new(1, 1, 4, 4),
        crate::app::Hit::ThemeOption(2),
    )];
    assert!(dispatch_queued_input(
        &mut m,
        &hits,
        QueuedInput {
            event: mouse.clone(),
            presented: picker.clone(),
            received_at: Instant::now()
        }
    ));
    paint(&mut m);
    dispatch(&mut m, k(KeyCode::Esc), picker.clone(), Instant::now());
    assert!(!dispatch_queued_input(
        &mut m,
        &hits,
        QueuedInput {
            event: mouse,
            presented: picker.clone(),
            received_at: Instant::now()
        }
    ));
    assert!(
        m.flash.is_none(),
        "old surface's passive report stays silent"
    );
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('h')),
        picker,
        Instant::now()
    ));
    assert_eq!(m.composer.text(), "h");
}

#[test]
fn long_retained_startup_cohort_keeps_its_original_stamp_after_history_rollover() {
    let mut m = AppModel::new();
    m.screen = Screen::Launcher;
    paint(&mut m);
    let original = m.painted_stamp();
    let text = format!("/{}", "x".repeat(INPUT_FRAME_HISTORY * 2));
    for c in text.chars() {
        assert!(dispatch(
            &mut m,
            k(KeyCode::Char(c)),
            original.clone(),
            Instant::now()
        ));
    }
    assert_eq!(m.composer.text(), text);
}

fn text_question(child: bool, id: &str) -> AppModel {
    let mut m = AppModel::new();
    let mut menu = decision(id);
    menu.options.clear();
    if child {
        m.chips
            .push(super::runtime_pointer_tests::question_chip("child", id));
        crate::session::chip_apply(&mut m.chips[0], &EventPayload::MenuOpened(menu), 0);
        m.screen = Screen::Subagent;
        m.view_path = vec!["child".into()];
    } else {
        m.screen = Screen::Session;
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
    }
    m
}

#[test]
fn free_text_frames_distinguish_questions_and_incarnations_but_preserve_answer_edits() {
    for child in [false, true] {
        for popup in [false, true] {
            for same_id in [false, true] {
                let mut m = text_question(child, "first");
                if popup {
                    m.composer.insert_str("/theme ");
                }
                let before = m.presented_input();
                if !popup {
                    m.composer.insert_str("ordinary answer");
                    assert_eq!(
                        m.presented_input(),
                        before,
                        "answer edit retired its target"
                    );
                }
                let mut next = decision(if same_id { "first" } else { "second" });
                next.options.clear();
                if child {
                    crate::session::chip_apply(&mut m.chips[0], &EventPayload::MenuOpened(next), 0);
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(next))));
                }
                assert_eq!(m.input_owner(), before.owner);
                assert_ne!(
                    m.presented_input(),
                    before,
                    "replacement reused frame target: child={child} popup={popup} same_id={same_id}"
                );
            }
        }
    }
}

#[test]
fn empty_mentions_submit_questions_only_after_card_dwell() {
    for child in [false, true] {
        for nonempty in [false, true] {
            let mut m = text_question(child, "text");
            m.composer.insert_str("@missing");
            m.mention_completion = Some(crate::app::MentionCompletion {
                candidates: if nonempty {
                    vec!["found.rs".into()]
                } else {
                    vec![]
                },
                ..Default::default()
            });
            let t = Instant::now();
            m.record_painted_input_at(t);
            dispatch_input_at(
                &mut m,
                &[],
                k(KeyCode::Enter),
                t + Duration::from_millis(120),
            );
            assert!(m.outbox.is_empty());
            if nonempty {
                assert!(m.flash.is_none());
                assert!(m.composer.text().contains("found.rs"));
            } else {
                assert!(m.flash.as_deref().unwrap().contains("New request"));
                dispatch_input_at(&mut m, &[], k(KeyCode::Enter), t + AppModel::DECISION_DWELL);
                assert_eq!(m.outbox.len(), 1);
                assert_eq!(m.outbox[0].answer.value.as_deref(), Some("@missing"));
            }
        }
    }
}

#[test]
fn completion_edits_preserve_mature_question_age_and_replacements_rearm_it() {
    for child in [false, true] {
        for restore in [false, true] {
            let mut m = text_question(child, "same");
            let t = Instant::now();
            m.record_painted_input_at(t - Duration::from_secs(1));
            m.composer.insert_str("@missing");
            m.mention_completion = Some(crate::app::MentionCompletion::default());
            m.record_painted_input_at(t);
            // A fresh completion popup does not make the visible question new.
            assert!(m.decision_key_commit_ready(t + Duration::from_millis(50)));
            if restore {
                m.help_open = true;
                m.record_painted_input_at(t);
                m.help_open = false;
            } else {
                let mut menu = decision("same");
                menu.options.clear();
                if child {
                    crate::session::chip_apply(&mut m.chips[0], &EventPayload::MenuOpened(menu), 0);
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
                }
            }
            m.record_painted_input_at(t);
            dispatch_input_at(
                &mut m,
                &[],
                k(KeyCode::Enter),
                t + Duration::from_millis(120),
            );
            assert!(m.outbox.is_empty(), "child={child} restore={restore}");
            dispatch_input_at(&mut m, &[], k(KeyCode::Enter), t + AppModel::DECISION_DWELL);
            assert_eq!(m.outbox.len(), 1, "child={child} restore={restore}");
        }
    }
}

#[test]
fn a_question_replaced_under_the_same_palette_rearms_pointer_dwell() {
    for (w, h) in [(118, 36), (80, 24)] {
        let mut m = text_question(false, "same");
        m.composer.insert_str("/theme ");
        let prior_theme = m.theme_choice;
        let t = Instant::now();
        m.record_painted_input_at(t - Duration::from_secs(1));
        let mut menu = decision("same");
        menu.options.clear();
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
        let Ok(mut term) = Terminal::new(ratatui::backend::TestBackend::new(w, h));
        let mut hits = vec![];
        term.draw(|f| hits = render(&m, f)).unwrap();
        let t = Instant::now();
        m.record_painted_input_at(t);
        let (rect, _) = hits
            .iter()
            .find(|(_, hit)| matches!(hit, crate::app::Hit::PaletteRow(crate::commands::PaletteItem::Arg { cmd: "theme", value, .. }) if value == "dark"))
            .unwrap();
        let column = rect.x;
        let row = rect.y;
        let event = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        dispatch_input_at(
            &mut m,
            &hits,
            event(MouseEventKind::Down(MouseButton::Left)),
            t + Duration::from_millis(120),
        );
        dispatch_input_at(
            &mut m,
            &hits,
            event(MouseEventKind::Up(MouseButton::Left)),
            t + Duration::from_millis(120),
        );
        assert_eq!(m.composer.text(), "/theme ");
        assert_eq!(m.theme_choice, prior_theme);
        assert!(m.outbox.is_empty());
        // The refused click may hover/select its row. Publish that input
        // stamp before a later gesture, as the runtime does after dispatch.
        term.draw(|f| hits = render(&m, f)).unwrap();
        dispatch_input_at(
            &mut m,
            &hits,
            event(MouseEventKind::Down(MouseButton::Left)),
            t + AppModel::DECISION_DWELL,
        );
        dispatch_input_at(
            &mut m,
            &hits,
            event(MouseEventKind::Up(MouseButton::Left)),
            t + AppModel::DECISION_DWELL,
        );
        assert_eq!(m.composer.text(), "");
        assert_eq!(
            m.theme_choice,
            crate::theme::ThemeChoice::Fixed(crate::theme::ThemeKey::Dark)
        );
        assert!(m.outbox.is_empty());
    }
}

#[test]
fn native_permission_action_keys_obey_the_shared_dwell() {
    use haider_protocol::permission::{
        PermissionGrantAction, PermissionGrantNeeded, SystemPermission,
    };
    for code in [
        KeyCode::Enter,
        KeyCode::Char('r'),
        KeyCode::Char('R'),
        KeyCode::Char('o'),
        KeyCode::Char('O'),
    ] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.active_session = Some(haider_protocol::ids::SessionId::new("synthetic"));
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
        let t = Instant::now();
        m.record_painted_input_at(t);
        dispatch_input_at(&mut m, &[], k(code), t + Duration::from_millis(120));
        assert!(m.requests.is_empty() && m.outbox.is_empty(), "{code:?}");
        assert!(m.flash.as_deref().unwrap().contains("New request"));
        m.record_painted_input_at(t + Duration::from_millis(120));
        dispatch_input_at(&mut m, &[], k(code), t + AppModel::DECISION_DWELL);
        assert_eq!(m.requests.len() + m.outbox.len(), 1, "{code:?}");
    }
}

fn native_grant_model(paired: bool) -> AppModel {
    use haider_protocol::permission::{
        PermissionGrantAction, PermissionGrantNeeded, SystemPermission,
    };
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    m.active_session = Some(haider_protocol::ids::SessionId::new("synthetic"));
    if paired {
        let mut menu = decision("grant");
        menu.options[0].key = "retry".into();
        menu.options[1].key = "deny".into();
        m.projection.apply(&EventPayload::MenuOpened(menu));
    }
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
    m
}

fn replace_native_grant(m: &mut AppModel, replacement: &str) {
    let mut card = m.projection.permission_card().unwrap().clone();
    match replacement {
        "request" => card.request_id = "next-native-grant".into(),
        "binding" => {
            card.request_seq += 1;
            card.opening_generation += 1;
            card.call_id = "next-call".into();
            card.effect_id = haider_protocol::ids::EffectId::new("next-effect");
            card.permission = haider_protocol::permission::SystemPermission::Accessibility;
        }
        "reissue" => {}
        "repaint" => return,
        _ => unreachable!(),
    }
    m.projection.set_permission_card(card);
}

fn assert_native_grant_action(m: &AppModel, settings: bool) {
    let card = m.projection.permission_card().unwrap();
    if settings {
        assert!(m.outbox.is_empty());
        assert!(matches!(m.requests.as_slice(),
            [crate::app::AppRequest::OpenPermissionSettings { request_id, permission, .. }]
                if request_id == &card.request_id && permission == &card.permission));
    } else {
        assert!(m.requests.is_empty());
        assert_eq!(m.outbox.len(), 1);
        assert_eq!(m.outbox[0].answer.menu, card.menu_id);
    }
}

#[test]
fn mature_native_grant_replacements_retire_queued_action_keys() {
    // Dwell and target binding are independent protections: the old key must
    // stay stale after the replacement has been painted and fully matured.
    for paired in [false, true] {
        for replacement in ["request", "binding", "reissue", "repaint"] {
            for gap in [400, 1000, 120] {
                for code in [
                    KeyCode::Char('o'),
                    KeyCode::Char('O'),
                    KeyCode::Enter,
                    KeyCode::Char('r'),
                    KeyCode::Char('R'),
                    KeyCode::Char('1'),
                    KeyCode::Char('2'),
                ] {
                    let settings = matches!(code, KeyCode::Char('o' | 'O'));
                    if !paired && !settings {
                        continue;
                    }
                    let mut m = native_grant_model(paired);
                    let t = Instant::now();
                    m.record_painted_input_at(t - Duration::from_secs(1));
                    let old = m.painted_stamp();
                    replace_native_grant(&mut m, replacement);
                    m.record_painted_input_at(t);
                    dispatch(&mut m, k(code), old, t + Duration::from_millis(gap));
                    if replacement == "repaint" {
                        assert_native_grant_action(&m, settings);
                        continue;
                    }
                    assert!(
                        m.requests.is_empty() && m.outbox.is_empty(),
                        "old key crossed native grant: paired={paired} {replacement} gap={gap} {code:?}"
                    );
                    let current = m.painted_stamp();
                    dispatch(
                        &mut m,
                        k(code),
                        current,
                        t + Duration::from_millis(gap.max(400)),
                    );
                    assert_native_grant_action(&m, settings);
                }
            }
        }
    }
}

#[test]
fn mature_native_grant_replacements_retire_queued_clicks_and_held_presses() {
    let mouse = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 2,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })
    };
    for paired in [false, true] {
        for replacement in ["request", "binding", "reissue", "repaint"] {
            for held in [false, true] {
                for hit in [
                    crate::app::Hit::PermissionOpenSettings,
                    crate::app::Hit::PermissionRetry,
                ] {
                    let settings = hit == crate::app::Hit::PermissionOpenSettings;
                    if !paired && !settings {
                        continue;
                    }
                    let mut m = native_grant_model(paired);
                    let hits = [(ratatui::layout::Rect::new(1, 1, 8, 1), hit)];
                    let t = Instant::now();
                    m.record_painted_input_at(t - Duration::from_secs(1));
                    let old = m.painted_stamp();
                    if held {
                        assert!(dispatch_queued_input(
                            &mut m,
                            &hits,
                            QueuedInput {
                                event: mouse(MouseEventKind::Down(MouseButton::Left)),
                                presented: old.clone(),
                                received_at: t,
                            }
                        ));
                    }
                    replace_native_grant(&mut m, replacement);
                    m.record_painted_input_at(t);
                    let at = t + Duration::from_secs(1);
                    let kinds = if held {
                        vec![MouseEventKind::Up(MouseButton::Left)]
                    } else {
                        vec![
                            MouseEventKind::Down(MouseButton::Left),
                            MouseEventKind::Up(MouseButton::Left),
                        ]
                    };
                    for kind in kinds {
                        dispatch_queued_input(
                            &mut m,
                            &hits,
                            QueuedInput {
                                event: mouse(kind),
                                presented: old.clone(),
                                received_at: at,
                            },
                        );
                    }
                    if replacement == "repaint" {
                        assert_native_grant_action(&m, settings);
                        continue;
                    }
                    assert!(
                        m.requests.is_empty() && m.outbox.is_empty(),
                        "old mouse crossed native grant: paired={paired} {replacement} held={held} settings={settings}"
                    );
                    paint(&mut m);
                    let current = m.painted_stamp();
                    for kind in [
                        MouseEventKind::Down(MouseButton::Left),
                        MouseEventKind::Up(MouseButton::Left),
                    ] {
                        dispatch_queued_input(
                            &mut m,
                            &hits,
                            QueuedInput {
                                event: mouse(kind),
                                presented: current.clone(),
                                received_at: at,
                            },
                        );
                    }
                    assert_native_grant_action(&m, settings);
                }
            }
        }
    }
}

fn metadata_provider(models: Vec<String>) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: "synthetic".into(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: None,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models,
        model_details: vec![],
        catalog: haider_rpc::ProviderCatalogKindWire::Unknown,
        inventory: haider_rpc::ModelInventoryWire::Static,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: vec![haider_protocol::credential::AuthMethod::ApiKey],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: None,
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

fn metadata_profiles() -> Vec<haider_rpc::SshProfileWire> {
    ["a", "b", "c"]
        .into_iter()
        .map(|name| haider_rpc::SshProfileWire {
            name: name.into(),
            description: None,
            host: "127.0.0.1".into(),
            port: 1,
            user: "synthetic".into(),
            default_cwd: None,
            host_key: None,
            last_used_ms: None,
            multiplexing: false,
            in_scope: true,
        })
        .collect()
}

#[test]
fn metadata_row_changes_retire_stamps_even_when_the_selected_id_stays_the_same() {
    use crate::live::{LiveDriver, LiveReply};
    for owner in ["ssh", "model", "palette"] {
        for change in ["neighbor", "same-id-binding", "unchanged", "stale"] {
            let mut m = AppModel::new();
            let mut driver = LiveDriver::new("synthetic-metadata");
            let provider = metadata_provider((0..3).map(|i| format!("model-{i}")).collect());
            driver.apply(
                &mut m,
                LiveReply::Providers {
                    providers: vec![provider.clone()],
                    revision: 1,
                },
            );
            driver.apply(
                &mut m,
                LiveReply::SshListed {
                    profiles: metadata_profiles(),
                },
            );
            m.screen = Screen::Session;
            match owner {
                "ssh" => m.ssh_open = true,
                "model" => m.open_model_picker(String::new()),
                _ => {
                    m.identity.provider = "synthetic".into();
                    m.composer.insert_str("/model ");
                }
            }
            paint(&mut m);
            let original = m.painted_stamp();
            dispatch(&mut m, k(KeyCode::Down), original.clone(), Instant::now());
            let middle = m.painted_stamp();
            dispatch(&mut m, k(KeyCode::Up), original, Instant::now());
            let logical = m.presented_input();
            let rows_before = m.input_rows();
            if owner == "palette" {
                assert!(m.palette_items().len() >= 3);
            }
            if owner == "ssh" {
                let mut profiles = metadata_profiles();
                if change == "neighbor" {
                    profiles[2].description = Some("changed neighbor".into());
                }
                if change == "same-id-binding" {
                    profiles[0].port = 2;
                }
                driver.apply(&mut m, LiveReply::SshListed { profiles });
            } else {
                let mut updated = provider;
                if matches!(change, "neighbor" | "stale") {
                    updated.models[2] = "model-9".into();
                }
                if change == "same-id-binding" {
                    updated.models.push("model-3".into());
                }
                driver.apply(
                    &mut m,
                    LiveReply::Providers {
                        providers: vec![updated],
                        revision: if change == "stale" { 0 } else { 2 },
                    },
                );
            }
            assert_eq!(
                m.presented_input(),
                logical,
                "{owner} {change}: current target stays identical"
            );
            let unchanged = matches!(change, "unchanged" | "stale");
            assert_eq!(
                rows_before == m.input_rows(),
                unchanged,
                "{owner} {change}: visible row change"
            );
            paint(&mut m);
            assert_eq!(
                dispatch(&mut m, k(KeyCode::Down), middle, Instant::now()),
                unchanged,
                "{owner} {change}"
            );
            assert_eq!(m.flash.is_none(), unchanged, "{owner} {change}");
        }
    }
}

#[test]
fn irrelevant_metadata_bootstrap_does_not_drop_plain_composer_type_ahead() {
    let mut m = AppModel::new();
    m.screen = Screen::Session;
    paint(&mut m);
    let original = m.painted_stamp();
    dispatch(
        &mut m,
        k(KeyCode::Char('a')),
        original.clone(),
        Instant::now(),
    );
    let mut driver = crate::live::LiveDriver::new("synthetic-bootstrap");
    driver.apply(
        &mut m,
        crate::live::LiveReply::SshListed {
            profiles: metadata_profiles(),
        },
    );
    paint(&mut m);
    assert!(dispatch(
        &mut m,
        k(KeyCode::Char('b')),
        original,
        Instant::now()
    ));
    assert_eq!(m.composer.text(), "ab");
}

#[test]
fn accounts_replace_enter_obeys_new_and_restored_decision_dwell() {
    for restored in [false, true] {
        for modifiers in (0..64)
            .map(KeyModifiers::from_bits_retain)
            .filter(|flags| *flags != KeyModifiers::CONTROL)
        {
            let mut m = AppModel::new();
            m.screen = Screen::Accounts;
            m.accounts.pending_replace = Some(crate::app::AccountAddKind::Ollama);
            paint(&mut m);
            if restored {
                m.help_open = true;
                paint(&mut m);
                m.help_open = false;
                paint(&mut m);
            }
            let t = Instant::now();
            set_shown_at(&m, t);
            let stamp = m.painted_stamp();
            dispatch(
                &mut m,
                Event::Key(KeyEvent::new(KeyCode::Enter, modifiers)),
                stamp,
                t + Duration::from_millis(120),
            );
            assert!(m.accounts.pending_replace.is_some());
            assert!(m.flash.as_deref().unwrap().contains("New request"));
            m.flash = None;
            let stamp = m.painted_stamp();
            dispatch(
                &mut m,
                k(KeyCode::Tab),
                stamp,
                t + Duration::from_millis(250),
            );
            assert!(m.accounts.pending_replace.is_some());
            assert!(m.flash.is_none());
            let stamp = m.painted_stamp();
            dispatch(
                &mut m,
                Event::Key(KeyEvent::new(KeyCode::Enter, modifiers)),
                stamp,
                t + AppModel::DECISION_DWELL,
            );
            assert!(m.accounts.pending_replace.is_none());
        }
    }
}

#[test]
fn long_cohorts_keep_old_intermediate_key_paste_and_pointer_frames() {
    for event in [
        k(KeyCode::Char('z')),
        Event::Paste("z".into()),
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
    ] {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.composer.insert_str("/");
        paint(&mut m);
        let original = m.painted_stamp();
        assert!(dispatch(
            &mut m,
            k(KeyCode::Char('t')),
            original.clone(),
            Instant::now()
        ));
        let intermediate = m.painted_stamp();
        let pointer_surface = m.pointer_input_owner();
        for i in 0..(INPUT_FRAME_HISTORY * 3) {
            assert!(dispatch(
                &mut m,
                k(if i % 2 == 0 {
                    KeyCode::Backspace
                } else {
                    KeyCode::Char('t')
                }),
                original.clone(),
                Instant::now()
            ));
        }
        assert_ne!(m.painted_stamp(), intermediate);
        assert_eq!(m.pointer_input_owner(), pointer_surface);
        assert!(dispatch(
            &mut m,
            event.clone(),
            intermediate,
            Instant::now()
        ));
        if !matches!(event, Event::Mouse(_)) {
            assert!(m.composer.text().ends_with('z'));
        }
        assert!(m.flash.is_none());
    }
}

fn cached_pending_decisions() -> (
    AppModel,
    haider_protocol::ids::SessionId,
    haider_protocol::ids::SessionId,
) {
    let mut m = AppModel::new();
    m.sessions.clear();
    let a = haider_protocol::ids::SessionId::new("active");
    let b = haider_protocol::ids::SessionId::new("background");
    for (id, prefix, n) in [(&a, "a", 3), (&b, "b", 2)] {
        m.upsert_live_session(id);
        for i in 0..n {
            m.session_entry_mut(id)
                .unwrap()
                .projection
                .apply(&EventPayload::MenuOpened(decision(&format!("{prefix}{i}"))));
        }
    }
    m.open_session(&a);
    (m, a, b)
}

#[test]
fn restored_pending_count_includes_cached_sessions_and_not_the_active_copy() {
    let (mut m, a, b) = cached_pending_decisions();
    // A stale stored copy of the active projection must not be added again.
    m.sessions
        .iter_mut()
        .find(|s| s.id == a)
        .unwrap()
        .projection
        .apply(&EventPayload::MenuOpened(decision("stale-active")));
    assert_eq!(m.inbox_counts().asks, 5);
    let bytes = serde_json::to_vec(&crate::demo_store::snapshot(&m)).unwrap();
    let dto = serde_json::from_slice(&bytes).unwrap();
    let mut m = AppModel::new();
    crate::demo_store::hydrate(&mut m, dto);
    m.open_session(&a);
    assert_eq!(m.inbox_counts().asks, 5);
    for (session, id, count) in [
        (&a, "a0", 4),
        (&b, "b0", 3),
        (&a, "a1", 2),
        (&a, "a2", 1),
        (&b, "b1", 0),
    ] {
        let event = EventPayload::MenuClosed {
            menu: haider_protocol::ids::MenuId::new(id),
            reason: haider_protocol::menu::MenuCloseReason::Dismissed,
        };
        if session == &a {
            m.projection.apply(&event);
        } else {
            m.session_entry_mut(session)
                .unwrap()
                .projection
                .apply(&event);
        }
        assert_eq!(m.inbox_counts().asks, count, "{id}");
    }
}

#[test]
fn complete_pending_persistence_advances_demo_format_and_loads_v2() {
    let (m, a, _) = cached_pending_decisions();
    let dir = tempfile::tempdir().unwrap();
    let mut store = crate::demo_store::DemoStore::at(dir.path().join("demo-tui-state.json"));
    store.save(&m);
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
    assert_eq!(
        json["version"], 3,
        "a strict DTO extension advances the format"
    );
    let mut restored = AppModel::new();
    crate::demo_store::hydrate(&mut restored, store.load().unwrap());
    restored.open_session(&a);
    assert_eq!(restored.inbox_counts().asks, 5);

    // A real v2 shape has only the visible menu. It must still load with
    // its known asks and session identities intact, then migrate on save.
    json["version"] = 2.into();
    for session in json["sessions"].as_array_mut().unwrap() {
        session["projection"]
            .as_object_mut()
            .unwrap()
            .remove("pending_menus");
    }
    std::fs::write(store.path(), serde_json::to_vec(&json).unwrap()).unwrap();
    let mut legacy = AppModel::new();
    crate::demo_store::hydrate(&mut legacy, store.load().unwrap());
    legacy.open_session(&a);
    assert_eq!(legacy.inbox_counts().asks, 2);
    assert_eq!(legacy.sessions.len(), m.sessions.len());
    store.save(&legacy);
    let migrated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
    assert_eq!(migrated["version"], 3);
    assert!(migrated["sessions"][0]["projection"]["pending_menus"].is_array());
}

#[test]
fn newer_roster_truth_and_subagent_mirrors_do_not_inflate_cached_ask_counts() {
    let (mut m, _, b) = cached_pending_decisions();
    m.session_kinds
        .insert(b.clone(), haider_rpc::SessionKindWire::Subagent);
    assert_eq!(m.inbox_counts().asks, 3);
    m.session_kinds.remove(&b);
    m.session_attention.insert(
        b.clone(),
        crate::app::SessionAttention {
            seen_at_ms: None,
            last_activity_ms: None,
            waiting_why: None,
            needs_input: None,
        },
    );
    m.session_cache_rates.insert(
        b.clone(),
        crate::app::SessionCacheRates {
            head_seq: 10,
            lifetime_basis_points: None,
            reread_basis_points: None,
        },
    );
    assert_eq!(m.inbox_counts().asks, 3);
    m.session_attention.get_mut(&b).unwrap().needs_input = Some(haider_rpc::NeedsInputWire {
        kind: haider_rpc::NeedsInputKindWire::Question,
        title: "synthetic".into(),
        safe_body: vec![],
        menu_id: None,
        request_seq: None,
        worker_generation: None,
        since_ms: None,
        options: vec![],
        secret_answer: false,
    });
    assert_eq!(m.inbox_counts().asks, 4); // One daemon-reported ask until replay catches up.
    m.session_entry_mut(&b)
        .unwrap()
        .projection
        .set_last_applied(10);
    assert_eq!(m.inbox_counts().asks, 5); // The complete, fresh projection has two.
}

#[test]
fn nonvisual_seen_echo_preserves_split_typing_but_outside_facts_retire_it() {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
    use haider_protocol::ids::{DeviceId, EventId, SessionId};
    use serde_json::json;
    // Valid acknowledgements with either render flag (the daemon uses true),
    // unknown/malformed lookalikes,
    // child variant, a real content fact and a new decision card.
    for variant in 0..10 {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        m.active_session = Some(SessionId::new("synthetic"));
        m.sessions.push(crate::session::SessionState::neutral(
            SessionId::new("background"),
            crate::identity::UiGeneration::new(999),
        ));
        paint(&mut m);
        let stamp = m.painted_stamp();
        dispatch(&mut m, k(KeyCode::Char('@')), stamp, Instant::now());
        let middle = m.painted_stamp();
        dispatch(
            &mut m,
            Event::Paste("ab".into()),
            middle.clone(),
            Instant::now(),
        );
        let payload = match variant {
            0 | 3 | 4 | 7 | 8 | 9 => json!({"type":"session_seen", "seen_at_ms":10}),
            1 => json!({"type":"unknown_seen", "seen_at_ms":10}),
            2 => json!({"type":"session_seen", "seen_at_ms":"bad"}),
            5 => serde_json::to_value(EventPayload::UserMessage {
                text: "outside".into(),
                attachments: vec![],
                mode: Default::default(),
            })
            .unwrap(),
            _ => serde_json::to_value(EventPayload::MenuOpened(decision("outside"))).unwrap(),
        };
        let envelope = EventEnvelope {
            schema_version: 1,
            event_id: EventId::new("seen"),
            seq: 1,
            session_id: SessionId::new(if variant == 7 {
                "background"
            } else {
                "synthetic"
            }),
            branch_id: (variant == 8).then(|| haider_protocol::ids::BranchId::new("branch")),
            run_id: (variant == 9).then(|| haider_protocol::ids::RunId::new("run")),
            agent_id: (variant == 4).then(|| haider_protocol::ids::AgentId::new("child")),
            device_id: DeviceId::new("synthetic"),
            authority_epoch: 1,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 20,
            render: RenderTargets {
                ui: matches!(variant, 3 | 5 | 6),
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: payload.into(),
        };
        let attention_before = m.session_attention.clone();
        assert_eq!(
            m.route_raw(&envelope),
            crate::projection::RawOutcome::Applied
        );
        let seen_ack = matches!(variant, 0 | 3);
        if seen_ack {
            assert_eq!(m.session_attention, attention_before);
            assert_eq!(m.projection.last_applied(), Some(1));
        }
        paint(&mut m);
        assert_eq!(
            dispatch(&mut m, Event::Paste("cd".into()), middle, Instant::now()),
            seen_ack,
            "variant={variant}"
        );
        assert_eq!(
            m.composer.text(),
            if seen_ack { "@abcd" } else { "@ab" },
            "variant={variant}"
        );
    }
}

#[test]
fn modified_control_and_clipboard_chords_match_their_real_dispatch() {
    for child in [false, true] {
        for modifiers in (0..64)
            .map(KeyModifiers::from_bits_retain)
            .filter(|flags| flags.contains(KeyModifiers::CONTROL))
        {
            for code in [KeyCode::Enter, KeyCode::Char('1'), KeyCode::Char('y')] {
                let mut m = AppModel::new();
                m.screen = Screen::Session;
                let mut menu = decision("control");
                menu.options[0].key = "y".into();
                if child {
                    let mut chip = super::runtime_pointer_tests::question_chip("child", "control");
                    crate::session::chip_apply(&mut chip, &EventPayload::MenuOpened(menu), 0);
                    m.chips.push(chip);
                    m.screen = Screen::Subagent;
                    m.view_path = vec!["child".into()];
                } else {
                    m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
                }
                paint(&mut m);
                let t = Instant::now();
                set_shown_at(&m, t);
                let key = KeyEvent::new(code, modifiers);
                assert!(!m.decision_key_commits(&key));
                for gap in [120, 400] {
                    dispatch_input_at(&mut m, &[], Event::Key(key), t + Duration::from_millis(gap));
                    assert!(m.outbox.is_empty(), "child={child} {code:?} {modifiers:?}");
                }
            }
        }
    }
    // A model option named v never turns modified V into a shortcut.
    // The advertised clipboard chords still take their ordinary edit path.
    for modifiers in (0..64)
        .map(KeyModifiers::from_bits_retain)
        .filter(|flags| !flags.contains(KeyModifiers::CONTROL))
    {
        let mut m = AppModel::new();
        m.screen = Screen::Session;
        let mut menu = decision("clipboard-alias");
        menu.options[0].key = "v".into();
        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
        paint(&mut m);
        let t = Instant::now();
        set_shown_at(&m, t);
        let key = KeyEvent::new(KeyCode::Char('V'), modifiers);
        assert!(!m.decision_key_commits(&key));
        for gap in [120, 400] {
            dispatch_input_at(&mut m, &[], Event::Key(key), t + Duration::from_millis(gap));
            assert!(m.outbox.is_empty(), "{modifiers:?}");
        }
    }
    // Bare Ctrl+Enter remains swallowed by the earlier non-composer gate.
    let mut m = AppModel::new();
    m.screen = Screen::Accounts;
    m.accounts.pending_replace = Some(crate::app::AccountAddKind::Ollama);
    paint(&mut m);
    let t = Instant::now();
    set_shown_at(&m, t);
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
    assert!(!m.decision_key_commits(&key));
    for gap in [120, 400] {
        dispatch_input_at(&mut m, &[], Event::Key(key), t + Duration::from_millis(gap));
        assert!(m.accounts.pending_replace.is_some());
        assert!(m.flash.is_none());
    }
}

#[test]
fn typed_prose_never_commits_model_option_letters_at_any_dwell() {
    for kind in ["permission", "question", "plan", "file", "child"] {
        for keys in [["a", "b"], ["y", "n"], ["A", "B"], ["β", "é"], ["o", "r"]] {
            for gap in [0, 120, 399, 400, 401, 1_500, 60_000] {
                for repeat in [false, true] {
                    let mut m = AppModel::new();
                    m.screen = Screen::Session;
                    let mut menu = if kind == "file" {
                        file_review_decision("prose")
                    } else {
                        decision("prose")
                    };
                    menu.options[0].key = keys[0].into();
                    menu.options[1].key = keys[1].into();
                    if kind == "question" {
                        menu.kind = haider_protocol::menu::MenuKind::Question;
                    }
                    if kind == "plan" {
                        menu.origin = "plan".into();
                    }
                    if kind == "child" {
                        let mut chip =
                            super::runtime_pointer_tests::question_chip("child", "prose");
                        crate::session::chip_apply(&mut chip, &EventPayload::MenuOpened(menu), 0);
                        m.chips.push(chip);
                        m.screen = Screen::Subagent;
                        m.view_path = vec!["child".into()];
                    } else {
                        m.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(menu))));
                    }
                    paint(&mut m);
                    let t = Instant::now();
                    set_shown_at(&m, t);
                    for c in "I have a question. YES, No. βé or AB?".chars() {
                        let mut key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
                        if repeat {
                            key.kind = KeyEventKind::Repeat;
                        }
                        assert!(!m.decision_key_commits(&key), "{kind} {keys:?} {c}");
                        let stamp = m.painted_stamp();
                        dispatch(
                            &mut m,
                            Event::Key(key),
                            stamp,
                            t + Duration::from_millis(gap),
                        );
                        assert!(m.outbox.is_empty(), "{kind} {keys:?} gap={gap} {c}");
                    }
                    // Quick picks use display position, never a model's key.
                    dispatch_input_at(
                        &mut m,
                        &[],
                        k(KeyCode::Char('2')),
                        t + Duration::from_secs(61),
                    );
                    assert_eq!(m.outbox.len(), 1);
                    assert_eq!(m.outbox[0].answer.option_index, 1);
                }
            }
        }
    }
}

#[test]
fn stale_cohort_commit_keeps_discarded_edit_feedback() {
    for child in [false, true] {
        for edit in [Event::Paste("answer".into()), k(KeyCode::Char('a'))] {
            for gap in [0, 120, 399, 400, 401, 1_500] {
                let mut m = text_question(child, "discarded");
                paint(&mut m);
                let stale = m.painted_stamp();
                m.invalidate_queued_input();
                paint(&mut m);
                let t = Instant::now();
                set_shown_at(&m, t);
                dispatch(&mut m, edit.clone(), stale.clone(), t);
                dispatch(
                    &mut m,
                    k(KeyCode::Enter),
                    stale.clone(),
                    t + Duration::from_millis(gap),
                );
                assert_eq!(
                    m.flash.as_deref(),
                    Some("· New request — input discarded, type again")
                );
                assert!(m.outbox.is_empty());
                assert!(m.composer.text().is_empty());
                // A new paint alone cannot turn this burst into an informed retry.
                let fresh = m.painted_stamp();
                dispatch(
                    &mut m,
                    k(KeyCode::Enter),
                    fresh,
                    t + Duration::from_millis(gap),
                );
                if gap < 400 {
                    assert_eq!(
                        m.flash.as_deref(),
                        Some("· New request — input discarded, type again")
                    );
                } else {
                    assert!(m.outbox.is_empty());
                }
                // A valid edit starts the user's replacement burst.
                let fresh = m.painted_stamp();
                dispatch(
                    &mut m,
                    Event::Paste("retry".into()),
                    fresh,
                    t + Duration::from_millis(gap),
                );
                assert_eq!(m.composer.text(), "retry");
                // An old stamp no longer claims that replacement was discarded.
                dispatch(
                    &mut m,
                    k(KeyCode::Enter),
                    stale,
                    t + Duration::from_millis(gap),
                );
                assert_eq!(
                    m.flash.as_deref(),
                    Some(if gap < 400 {
                        "· New request — press again"
                    } else {
                        "· Input changed: please try again"
                    })
                );
            }
        }
        let mut m = text_question(child, "no-discard");
        paint(&mut m);
        let stale = m.painted_stamp();
        m.invalidate_queued_input();
        paint(&mut m);
        let t = Instant::now();
        set_shown_at(&m, t);
        dispatch(&mut m, k(KeyCode::Down), stale.clone(), t);
        dispatch(&mut m, k(KeyCode::Enter), stale, t);
        assert_eq!(m.flash.as_deref(), Some("· New request — press again"));
    }
}

#[test]
fn discarded_text_feedback_survives_new_paints_and_passive_reports_before_commit() {
    for child in [false, true] {
        for edit in [Event::Paste("answer".into()), k(KeyCode::Char('a'))] {
            let mut release = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
            release.kind = KeyEventKind::Release;
            for report in [
                None,
                Some(Event::Key(release)),
                Some(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                })),
                Some(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                })),
            ] {
                let mut m = text_question(child, "cross-paint");
                paint(&mut m);
                let old = m.painted_stamp();
                m.invalidate_queued_input();
                paint(&mut m);
                let t = Instant::now();
                set_shown_at(&m, t);
                assert!(!dispatch(&mut m, edit.clone(), old, t));
                if let Some(report) = report {
                    let fresh = m.painted_stamp();
                    assert!(dispatch(
                        &mut m,
                        report,
                        fresh,
                        t + Duration::from_millis(20)
                    ));
                }
                let fresh = m.painted_stamp();
                assert!(dispatch(
                    &mut m,
                    k(KeyCode::Enter),
                    fresh,
                    t + Duration::from_millis(120)
                ));
                assert_eq!(
                    m.flash.as_deref(),
                    Some("· New request — input discarded, type again")
                );
                assert!(m.outbox.is_empty());
                assert!(m.composer.text().is_empty());
                let fresh = m.painted_stamp();
                assert!(dispatch(
                    &mut m,
                    Event::Paste("restored".into()),
                    fresh,
                    t + Duration::from_millis(130)
                ));
                let fresh = m.painted_stamp();
                assert!(dispatch(
                    &mut m,
                    k(KeyCode::Enter),
                    fresh,
                    t + Duration::from_millis(140)
                ));
                assert_eq!(m.flash.as_deref(), Some("· New request — press again"));
                assert_eq!(m.composer.text(), "restored");
            }
        }
    }
}
