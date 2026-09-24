#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use haider_protocol::computer::ScreenPoint;
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Recorded {
    commands: StdMutex<Vec<PresenceCommand>>,
    spawned: AtomicUsize,
    closed: AtomicUsize,
    sink: StdMutex<Option<EventSink>>,
}

struct FakeRenderer {
    recorded: Arc<Recorded>,
    acks: bool,
}

impl PresenceRenderer for FakeRenderer {
    fn send(&mut self, command: &PresenceCommand) -> bool {
        self.recorded.commands.lock().unwrap().push(command.clone());
        true
    }
    fn acknowledges_pointers(&self) -> bool {
        self.acks
    }
    fn close(&mut self) {
        self.recorded.closed.fetch_add(1, Ordering::SeqCst);
    }
}

fn presence_with(surface: PresenceSurface, acks: bool) -> (Arc<CuPresence>, Arc<Recorded>) {
    let presence = CuPresence::new(Duration::from_millis(200), Duration::from_millis(20));
    let recorded = Arc::new(Recorded::default());
    let factory_record = Arc::clone(&recorded);
    presence.register_renderer(
        surface,
        Arc::new(move |sink| {
            factory_record.spawned.fetch_add(1, Ordering::SeqCst);
            *factory_record.sink.lock().unwrap() = Some(sink);
            Some(Box::new(FakeRenderer {
                recorded: Arc::clone(&factory_record),
                acks,
            }) as Box<dyn PresenceRenderer>)
        }),
    );
    (presence, recorded)
}

fn counting_hook() -> (StopHook, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let hook_count = Arc::clone(&count);
    (
        Arc::new(move || {
            hook_count.fetch_add(1, Ordering::SeqCst);
        }),
        count,
    )
}

fn click() -> ComputerAction {
    ComputerAction::LeftClick { x: 5, y: 6 }
}

#[tokio::test]
async fn first_computer_action_spawns_the_overlay_and_run_end_retires_it() {
    let (presence, recorded) = presence_with(PresenceSurface::Screen, false);
    let (stop, stops) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/r1".into(), stop);
    let cancel = haider_tools::ComputerCancelToken::new();
    {
        let _guard = lease
            .begin_computer(&ComputerAction::Screenshot, None, &cancel)
            .await
            .expect("begin");
        assert!(presence.is_shown(PresenceSurface::Screen));
    }
    {
        let _guard = lease
            .begin_computer(&click(), Some(PresencePoint { x: 50.0, y: 60.0 }), &cancel)
            .await
            .expect("begin");
    }
    assert_eq!(
        recorded.spawned.load(Ordering::SeqCst),
        1,
        "one overlay per session"
    );
    lease.end(false);
    let commands = recorded.commands.lock().unwrap().clone();
    assert!(
        matches!(commands.first(), Some(PresenceCommand::Show { label, .. }) if label == "Haider is controlling this screen")
    );
    assert!(commands.iter().any(|command| matches!(command,
        PresenceCommand::Pointer { mark: PresenceMark::Click, point: Some(point), .. } if point.x == 50.0)));
    assert_eq!(
        commands.last(),
        Some(&PresenceCommand::Hide {
            surface: PresenceSurface::Screen,
            reason: PresenceEndReason::RunEnded,
        })
    );
    assert_eq!(
        recorded.closed.load(Ordering::SeqCst),
        1,
        "overlay process closed at run end"
    );
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn overlay_stop_cancels_the_in_flight_action_the_run_and_refuses_later_actions() {
    let (presence, recorded) = presence_with(PresenceSurface::Screen, false);
    let (stop, stops) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/r2".into(), stop);
    let in_flight = haider_tools::ComputerCancelToken::new();
    let guard = lease
        .begin_computer(
            &ComputerAction::Type {
                text: "a long sentence".into(),
            },
            None,
            &in_flight,
        )
        .await
        .expect("begin");
    // The overlay helper reports a human Stop click.
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    sink(PresenceEvent::Stop);
    assert!(
        in_flight.is_cancelled(),
        "the in-flight typing is cancelled at once"
    );
    assert_eq!(stops.load(Ordering::SeqCst), 1, "the run cancel hook fired");
    drop(guard);
    let next = haider_tools::ComputerCancelToken::new();
    assert_eq!(
        lease.begin_computer(&click(), None, &next).await.err(),
        Some(PresenceRefusal::Stopped),
        "no further CU action may start in a stopped run"
    );
    let commands = recorded.commands.lock().unwrap().clone();
    assert!(commands.contains(&PresenceCommand::Stopping {
        surface: PresenceSurface::Screen
    }));
    assert!(commands.contains(&PresenceCommand::Hide {
        surface: PresenceSurface::Screen,
        reason: PresenceEndReason::Stopped,
    }));
    // The cancelled run ends; a fresh run may control the screen again.
    lease.end(true);
    let (stop, _) = counting_hook();
    let fresh = PresenceLease::new(Arc::clone(&presence), "s/r3".into(), stop);
    fresh
        .begin_computer(&click(), None, &next)
        .await
        .expect("new run is not stopped");
    assert_eq!(recorded.spawned.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn idle_expiry_hides_the_overlay_and_the_next_action_reshows_it() {
    let (presence, recorded) = presence_with(PresenceSurface::Screen, false);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/idle".into(), stop);
    let cancel = haider_tools::ComputerCancelToken::new();
    drop(
        lease
            .begin_computer(&ComputerAction::MouseMove { x: 1, y: 2 }, None, &cancel)
            .await
            .expect("begin"),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while presence.is_shown(PresenceSurface::Screen) {
        assert!(
            Instant::now() < deadline,
            "idle ticker never retired presence"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        recorded
            .commands
            .lock()
            .unwrap()
            .contains(&PresenceCommand::Hide {
                surface: PresenceSurface::Screen,
                reason: PresenceEndReason::Idle,
            })
    );
    drop(
        lease
            .begin_computer(&click(), None, &cancel)
            .await
            .expect("idle is not stopped"),
    );
    assert!(presence.is_shown(PresenceSurface::Screen));
    assert_eq!(recorded.spawned.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn pointer_input_waits_for_the_overlay_ack_but_never_longer_than_the_bound() {
    let (presence, recorded) = presence_with(PresenceSurface::Screen, true);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/ack".into(), stop);
    let cancel = haider_tools::ComputerCancelToken::new();
    // Nobody acknowledges: the bounded wait elapses and control proceeds.
    let started = Instant::now();
    drop(
        lease
            .begin_computer(&click(), None, &cancel)
            .await
            .expect("begin"),
    );
    let waited = started.elapsed();
    assert!(waited >= POINTER_ACK_TIMEOUT && waited < POINTER_ACK_TIMEOUT * 4);
    // A prompt ack releases the action immediately.
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    let acker = tokio::spawn({
        let recorded = Arc::clone(&recorded);
        async move {
            loop {
                let seq =
                    recorded.commands.lock().unwrap().iter().rev().find_map(
                        |command| match command {
                            PresenceCommand::Pointer { seq, .. } if *seq > 1 => Some(*seq),
                            _ => None,
                        },
                    );
                if let Some(seq) = seq {
                    sink(PresenceEvent::Ack { seq });
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    });
    let started = Instant::now();
    drop(
        lease
            .begin_computer(
                &ComputerAction::LeftClickDrag {
                    from: ScreenPoint { x: 1, y: 1 },
                    to: ScreenPoint { x: 2, y: 2 },
                },
                None,
                &cancel,
            )
            .await
            .expect("begin"),
    );
    acker.await.expect("acker");
    assert!(started.elapsed() < POINTER_ACK_TIMEOUT);
    // Typing never waits (no pointer input is posted).
    let started = Instant::now();
    drop(
        lease
            .begin_computer(&ComputerAction::Type { text: "x".into() }, None, &cancel)
            .await
            .expect("begin"),
    );
    assert!(started.elapsed() < POINTER_ACK_TIMEOUT);
}

#[tokio::test]
async fn phone_stop_only_stops_phone_runs_and_sms_reads_raise_no_presence() {
    let presence = CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    let (screen_stop, screen_stops) = counting_hook();
    let (phone_stop, phone_stops) = counting_hook();
    let desktop = PresenceLease::new(Arc::clone(&presence), "s/desk".into(), screen_stop);
    let phone = PresenceLease::new(Arc::clone(&presence), "s/phone".into(), phone_stop);
    let computer_cancel = haider_tools::ComputerCancelToken::new();
    drop(
        desktop
            .begin_computer(&click(), None, &computer_cancel)
            .await
            .expect("desk"),
    );
    let mobile_cancel = haider_tools::MobileCancelToken::new();
    assert!(
        phone
            .begin_mobile(
                &MobileAction::SmsRead {
                    folder: None,
                    since: None,
                    limit: None,
                },
                &mobile_cancel,
            )
            .await
            .expect("sms")
            .is_none(),
        "reading SMS is not controlling the phone"
    );
    assert!(!presence.is_shown(PresenceSurface::Phone));
    drop(
        phone
            .begin_mobile(
                &MobileAction::Tap {
                    element_id: None,
                    x: Some(3),
                    y: Some(4),
                },
                &mobile_cancel,
            )
            .await
            .expect("tap"),
    );
    assert!(presence.is_shown(PresenceSurface::Phone));
    assert_eq!(presence.stop(PresenceSurface::Phone), 1);
    assert_eq!(phone_stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        screen_stops.load(Ordering::SeqCst),
        0,
        "the desktop run keeps going"
    );
    assert!(presence.is_shown(PresenceSurface::Screen));
    assert_eq!(
        phone
            .begin_mobile(
                &MobileAction::Tap {
                    element_id: None,
                    x: Some(3),
                    y: Some(4),
                },
                &mobile_cancel,
            )
            .await
            .err(),
        Some(PresenceRefusal::Stopped)
    );
}

#[tokio::test]
async fn dropping_a_lease_without_close_still_retires_presence() {
    let (presence, recorded) = presence_with(PresenceSurface::Screen, false);
    {
        let (stop, _) = counting_hook();
        let lease = PresenceLease::new(Arc::clone(&presence), "s/leak".into(), stop);
        drop(
            lease
                .begin_computer(&click(), None, &haider_tools::ComputerCancelToken::new())
                .await
                .expect("begin"),
        );
    }
    assert!(!presence.is_shown(PresenceSurface::Screen));
    assert_eq!(recorded.closed.load(Ordering::SeqCst), 1);
}
