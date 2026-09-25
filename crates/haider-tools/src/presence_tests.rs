#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::art;
use super::*;
use haider_protocol::computer::ScreenPoint;
use std::time::{Duration, Instant};

fn key(run: &str) -> String {
    format!("session/{run}")
}

fn point(x: f64, y: f64) -> Option<PresencePoint> {
    Some(PresencePoint { x, y })
}

#[test]
fn first_action_shows_the_surface_once_and_every_action_moves_the_pointer() {
    let mut machine = PresenceMachine::<String>::default();
    let now = Instant::now();
    let (seq, commands) = machine
        .begin_action(
            key("r1"),
            PresenceSurface::Screen,
            PresenceMark::Click,
            point(10.0, 20.0),
            now,
        )
        .expect("not stopped");
    assert_eq!(seq, 1);
    assert_eq!(
        commands,
        vec![
            PresenceCommand::Show {
                surface: PresenceSurface::Screen,
                label: "Haider is controlling this screen".into(),
            },
            PresenceCommand::Pointer {
                surface: PresenceSurface::Screen,
                seq: 1,
                mark: PresenceMark::Click,
                point: point(10.0, 20.0),
            },
        ]
    );
    machine.finish_action(&key("r1"), now);
    let (seq, commands) = machine
        .begin_action(
            key("r1"),
            PresenceSurface::Screen,
            PresenceMark::Type,
            None,
            now,
        )
        .expect("not stopped");
    assert_eq!(seq, 2);
    assert_eq!(
        commands,
        vec![PresenceCommand::Pointer {
            surface: PresenceSurface::Screen,
            seq: 2,
            mark: PresenceMark::Type,
            point: None,
        }],
        "an already-visible surface is never re-shown"
    );
    assert!(machine.is_shown(PresenceSurface::Screen));
}

#[test]
fn run_end_and_cancel_hide_only_after_the_last_lease_on_that_surface() {
    let mut machine = PresenceMachine::<String>::default();
    let now = Instant::now();
    for run in ["r1", "r2"] {
        machine
            .begin_action(
                key(run),
                PresenceSurface::Screen,
                PresenceMark::Move,
                None,
                now,
            )
            .expect("begin");
        machine.finish_action(&key(run), now);
    }
    assert!(
        machine
            .end(&key("r1"), PresenceEndReason::RunEnded)
            .is_empty(),
        "another run still controls the screen"
    );
    assert_eq!(
        machine.end(&key("r2"), PresenceEndReason::Cancelled),
        vec![PresenceCommand::Hide {
            surface: PresenceSurface::Screen,
            reason: PresenceEndReason::Cancelled,
        }]
    );
    assert!(!machine.is_shown(PresenceSurface::Screen));
    assert!(!machine.has_leases());
    assert!(
        machine
            .end(&key("r2"), PresenceEndReason::RunEnded)
            .is_empty(),
        "ending twice is idempotent"
    );
}

#[test]
fn idle_expiry_hides_after_the_timeout_but_never_mid_action_and_the_next_action_reshows() {
    let mut machine = PresenceMachine::<String>::new(Duration::from_secs(30));
    let start = Instant::now();
    machine
        .begin_action(
            key("r1"),
            PresenceSurface::Screen,
            PresenceMark::Wait,
            None,
            start,
        )
        .expect("begin");
    // A 60 s `wait` is still in flight: never idle.
    assert!(machine.tick(start + Duration::from_secs(61)).is_empty());
    let finished = start + Duration::from_secs(61);
    machine.finish_action(&key("r1"), finished);
    assert!(machine.tick(finished + Duration::from_secs(29)).is_empty());
    assert_eq!(
        machine.tick(finished + Duration::from_secs(30)),
        vec![PresenceCommand::Hide {
            surface: PresenceSurface::Screen,
            reason: PresenceEndReason::Idle,
        }]
    );
    let (_, commands) = machine
        .begin_action(
            key("r1"),
            PresenceSurface::Screen,
            PresenceMark::Click,
            None,
            finished + Duration::from_secs(40),
        )
        .expect("the run was idle, not stopped");
    assert!(matches!(commands[0], PresenceCommand::Show { .. }));
}

#[test]
fn stop_marks_every_lease_on_the_surface_refuses_further_actions_and_hides() {
    let mut machine = PresenceMachine::<String>::default();
    let now = Instant::now();
    machine
        .begin_action(
            key("r1"),
            PresenceSurface::Screen,
            PresenceMark::Type,
            None,
            now,
        )
        .expect("begin");
    machine
        .begin_action(
            key("phone"),
            PresenceSurface::Phone,
            PresenceMark::Click,
            None,
            now,
        )
        .expect("begin");
    let (stopped, commands) = machine.stop(PresenceSurface::Screen);
    assert_eq!(stopped, vec![key("r1")], "only the screen lease is stopped");
    assert_eq!(
        commands,
        vec![
            PresenceCommand::Stopping {
                surface: PresenceSurface::Screen
            },
            PresenceCommand::Hide {
                surface: PresenceSurface::Screen,
                reason: PresenceEndReason::Stopped,
            },
        ]
    );
    assert!(machine.is_stopped(&key("r1")));
    assert_eq!(
        machine.begin_action(
            key("r1"),
            PresenceSurface::Screen,
            PresenceMark::Click,
            None,
            now
        ),
        Err(PresenceRefusal::Stopped),
        "a stopped run can start no further CU action"
    );
    assert!(
        machine.is_shown(PresenceSurface::Phone),
        "the phone is unaffected"
    );
    let (again, commands) = machine.stop(PresenceSurface::Screen);
    assert!(
        again.is_empty() && commands.is_empty(),
        "stop is idempotent"
    );
    // A NEW run may control the screen again after the stopped one ends.
    assert!(
        machine
            .end(&key("r1"), PresenceEndReason::Cancelled)
            .is_empty()
    );
    let (_, commands) = machine
        .begin_action(
            key("r2"),
            PresenceSurface::Screen,
            PresenceMark::Move,
            None,
            now,
        )
        .expect("fresh run");
    assert!(matches!(commands[0], PresenceCommand::Show { .. }));
}

/// Verifier finding (191a8d60): one run that drives the screen and then the
/// phone must stay stoppable from BOTH indicators, and its end must retire
/// both. MUTATION CHECK: store a single surface per lease again. Expected
/// runtime failure: `stop(Screen)` returns no run / end leaves Screen shown.
#[test]
fn one_run_on_two_surfaces_is_stoppable_from_either_and_its_end_hides_both() {
    let now = Instant::now();
    let begin = |machine: &mut PresenceMachine<String>, surface| {
        machine
            .begin_action(key("r1"), surface, PresenceMark::Click, None, now)
            .expect("begin");
        machine.finish_action(&key("r1"), now);
    };
    // Stop pressed on the desktop badge after the run moved to the phone.
    let mut machine = PresenceMachine::<String>::default();
    begin(&mut machine, PresenceSurface::Screen);
    begin(&mut machine, PresenceSurface::Phone);
    let (stopped, commands) = machine.stop(PresenceSurface::Screen);
    assert_eq!(
        stopped,
        vec![key("r1")],
        "the desktop Stop still reaches the run"
    );
    assert!(commands.contains(&PresenceCommand::Hide {
        surface: PresenceSurface::Screen,
        reason: PresenceEndReason::Stopped,
    }));
    assert!(
        commands.contains(&PresenceCommand::Hide {
            surface: PresenceSurface::Phone,
            reason: PresenceEndReason::Stopped,
        }),
        "the stopped run's phone indicator retires too"
    );
    assert_eq!(
        machine.begin_action(
            key("r1"),
            PresenceSurface::Phone,
            PresenceMark::Click,
            None,
            now
        ),
        Err(PresenceRefusal::Stopped),
        "no surface of a stopped run may act"
    );
    // Stop pressed on the phone works the same way.
    let mut machine = PresenceMachine::<String>::default();
    begin(&mut machine, PresenceSurface::Screen);
    begin(&mut machine, PresenceSurface::Phone);
    assert_eq!(machine.stop(PresenceSurface::Phone).0, vec![key("r1")]);
    assert!(!machine.is_shown(PresenceSurface::Screen));
    // A normal run end hides every surface the run used.
    let mut machine = PresenceMachine::<String>::default();
    begin(&mut machine, PresenceSurface::Screen);
    begin(&mut machine, PresenceSurface::Phone);
    let hidden = machine.end(&key("r1"), PresenceEndReason::RunEnded);
    assert_eq!(hidden.len(), 2, "both surfaces retire: {hidden:?}");
    assert!(
        !machine.is_shown(PresenceSurface::Screen) && !machine.is_shown(PresenceSurface::Phone)
    );
    // Idle retires each surface on its own clock.
    let mut machine = PresenceMachine::<String>::new(Duration::from_secs(30));
    begin(&mut machine, PresenceSurface::Screen);
    machine
        .begin_action(
            key("r1"),
            PresenceSurface::Phone,
            PresenceMark::Click,
            None,
            now + Duration::from_secs(20),
        )
        .expect("begin");
    machine.finish_action(&key("r1"), now + Duration::from_secs(20));
    assert_eq!(
        machine.tick(now + Duration::from_secs(31)),
        vec![PresenceCommand::Hide {
            surface: PresenceSurface::Screen,
            reason: PresenceEndReason::Idle,
        }]
    );
    assert!(machine.is_shown(PresenceSurface::Phone));
    assert_eq!(machine.stop(PresenceSurface::Phone).0, vec![key("r1")]);
}

#[test]
fn stopped_leases_are_reclaimed_if_their_run_never_ends() {
    let mut machine = PresenceMachine::<String>::default();
    let now = Instant::now();
    machine
        .begin_action(
            key("r1"),
            PresenceSurface::Screen,
            PresenceMark::Move,
            None,
            now,
        )
        .expect("begin");
    machine.stop_at(PresenceSurface::Screen, now);
    assert!(machine.tick(now + Duration::from_secs(60)).is_empty());
    assert!(
        machine.has_leases(),
        "stopped lease retained to refuse late actions"
    );
    machine.tick(now + STOPPED_LEASE_RETENTION);
    assert!(!machine.has_leases());
}

#[test]
fn wire_vocabulary_round_trips_and_rejects_unknown_fields() {
    let command = PresenceCommand::Pointer {
        surface: PresenceSurface::Screen,
        seq: 7,
        mark: PresenceMark::DoubleClick,
        point: point(1.5, 2.0),
    };
    let line = serde_json::to_string(&command).expect("encode");
    assert_eq!(
        line,
        r#"{"op":"pointer","surface":"screen","seq":7,"mark":"double_click","point":{"x":1.5,"y":2.0}}"#
    );
    assert_eq!(
        parse_command_line(&line).expect("line").expect("parse"),
        command
    );
    assert!(parse_command_line("   ").is_none());
    let conceal = PresenceCommand::Conceal {
        surface: PresenceSurface::Screen,
        seq: 9,
    };
    let line = serde_json::to_string(&conceal).expect("encode");
    assert_eq!(line, r#"{"op":"conceal","surface":"screen","seq":9}"#);
    assert_eq!(
        parse_command_line(&line).expect("line").expect("parse"),
        conceal
    );
    assert_eq!(
        serde_json::to_string(&PresenceCommand::Reveal {
            surface: PresenceSurface::Screen
        })
        .expect("encode"),
        r#"{"op":"reveal","surface":"screen"}"#
    );
    assert!(
        parse_command_line(r#"{"op":"hide","surface":"screen","reason":"idle","x":1}"#)
            .expect("line")
            .is_err()
    );
    assert_eq!(encode_event(&PresenceEvent::Stop), r#"{"event":"stop"}"#);
    let ready: PresenceEvent =
        serde_json::from_str(r#"{"event":"ready","platform":"macos","capture_excluded":true}"#)
            .expect("ready");
    assert_eq!(
        ready,
        PresenceEvent::Ready {
            platform: "macos".into(),
            capture_excluded: true
        }
    );
}

#[test]
fn marks_follow_the_action_and_sms_never_raises_phone_presence() {
    assert_eq!(
        PresenceMark::for_computer_action(&ComputerAction::Screenshot),
        PresenceMark::Observe
    );
    assert_eq!(
        PresenceMark::for_computer_action(&ComputerAction::LeftClickDrag {
            from: ScreenPoint { x: 1, y: 1 },
            to: ScreenPoint { x: 2, y: 2 },
        }),
        PresenceMark::Drag
    );
    assert!(PresenceMark::Click.posts_pointer_input());
    assert!(!PresenceMark::Type.posts_pointer_input());
    assert_eq!(
        PresenceMark::for_mobile_action(&MobileAction::SmsRead {
            folder: None,
            since: None,
            limit: None
        }),
        None
    );
    assert_eq!(
        PresenceMark::for_mobile_action(&MobileAction::Tap {
            element_id: None,
            x: Some(1),
            y: Some(2)
        }),
        Some(PresenceMark::Click)
    );
}

#[test]
fn pointer_art_has_a_gold_body_dark_outline_and_transparent_surroundings() {
    let bitmap = art::pointer(2.0);
    assert_eq!((bitmap.width, bitmap.height), (48, 68));
    // Inside the arrow body (design point (5, 12) + tip margin) is gold.
    let body = bitmap.pixel(
        ((art::POINTER_TIP + 4.0) * 2.0) as u32,
        ((art::POINTER_TIP + 12.0) * 2.0) as u32,
    );
    assert_eq!(body, art::POINTER_FILL);
    // Just right of the tip is the dark outline or halo, never gold.
    let tip = bitmap.pixel(
        (art::POINTER_TIP * 2.0) as u32,
        (art::POINTER_TIP * 2.0 + 1.0) as u32,
    );
    assert_ne!(tip[..3], art::POINTER_FILL[..3]);
    assert!(tip[3] > 0);
    // Far corner is fully transparent (the overlay is click-through there).
    assert_eq!(bitmap.pixel(bitmap.width - 1, 0)[3], 0);
    let bgra = bitmap.premultiplied_bgra();
    assert_eq!(bgra.len(), bitmap.rgba.len());
}

#[test]
fn badge_art_and_stop_hit_testing_agree() {
    let bitmap = art::badge(1.0);
    let (x, y, width, height) = art::BADGE_STOP_RECT;
    let center = ((x + width / 2.0) as u32, (y + height / 2.0) as u32);
    assert_eq!(bitmap.pixel(center.0, center.1), art::STOP_FILL);
    assert!(art::badge_stop_hit(x + width / 2.0, y + height / 2.0));
    assert!(
        !art::badge_stop_hit(40.0, 18.0),
        "the label area is not Stop"
    );
    assert_eq!(bitmap.pixel(0, 0)[3], 0, "rounded corner is transparent");
    let ring = art::click_ring(1.0);
    assert_eq!(ring.pixel(22, 22)[3], 0, "ring centre is clear");
}
