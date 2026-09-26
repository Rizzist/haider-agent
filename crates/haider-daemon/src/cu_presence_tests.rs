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
    capturable: bool,
}

impl PresenceRenderer for FakeRenderer {
    fn send(&mut self, command: &PresenceCommand) -> bool {
        self.recorded.commands.lock().unwrap().push(command.clone());
        true
    }
    fn acknowledges_pointers(&self) -> bool {
        self.acks
    }
    fn capturable(&self) -> bool {
        self.capturable
    }
    fn close(&mut self) {
        self.recorded.closed.fetch_add(1, Ordering::SeqCst);
    }
}

fn presence_with(surface: PresenceSurface, acks: bool) -> (Arc<CuPresence>, Arc<Recorded>) {
    presence_with_capturable(surface, acks, false)
}

fn presence_with_capturable(
    surface: PresenceSurface,
    acks: bool,
    capturable: bool,
) -> (Arc<CuPresence>, Arc<Recorded>) {
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
                capturable,
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

/// Verifier finding (191a8d60): Stop released the pointer ack before the
/// in-flight token flipped, so a click waiting on the ack could run.
/// MUTATION CHECK: flip the token after `dispatch` and drop the post-wait
/// `is_stopped` re-check. Expected runtime failure: the waiting `begin`
/// returns Ok (the click would execute) or its token is not yet cancelled.
#[tokio::test]
async fn stop_during_the_pointer_ack_wait_refuses_the_click_and_cancels_its_token() {
    for _ in 0..50 {
        let (presence, recorded) = presence_with(PresenceSurface::Screen, true);
        let (stop, stops) = counting_hook();
        let lease = Arc::new(PresenceLease::new(
            Arc::clone(&presence),
            "s/ack-race".into(),
            stop,
        ));
        let token = haider_tools::ComputerCancelToken::new();
        let waiting = tokio::spawn({
            let lease = Arc::clone(&lease);
            let token = token.clone();
            async move {
                let result = lease.begin_computer(&click(), None, &token).await.map(drop);
                // What the dispatcher checks right before `execute`.
                (result, token.is_cancelled())
            }
        });
        // Deterministically wait until the click is parked on its ack.
        while !recorded
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| matches!(command, PresenceCommand::Pointer { .. }))
        {
            tokio::task::yield_now().await;
        }
        let sink = recorded.sink.lock().unwrap().clone().expect("sink");
        sink(PresenceEvent::Stop);
        let (result, cancelled_on_resume) = waiting.await.expect("join");
        assert_eq!(
            result,
            Err(PresenceRefusal::Stopped),
            "the parked click is refused"
        );
        assert!(
            cancelled_on_resume,
            "its token was already cancelled when it resumed"
        );
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }
}

/// Verifier finding (191a8d60), daemon side of the same-run two-surface bug.
#[tokio::test]
async fn desktop_stop_reaches_a_run_that_moved_on_to_the_phone() {
    let presence = CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    let (stop, stops) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/two".into(), stop);
    drop(
        lease
            .begin_computer(&click(), None, &haider_tools::ComputerCancelToken::new())
            .await
            .expect("screen"),
    );
    drop(
        lease
            .begin_mobile(
                &MobileAction::Tap {
                    element_id: None,
                    x: Some(1),
                    y: Some(1),
                },
                &haider_tools::MobileCancelToken::new(),
            )
            .await
            .expect("phone"),
    );
    assert_eq!(presence.stop(PresenceSurface::Screen), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(lease.is_stopped());
    assert!(!presence.is_shown(PresenceSurface::Screen));
    assert!(!presence.is_shown(PresenceSurface::Phone));
}

/// Verifier finding (191a8d60), Linux: a capturable indicator (the
/// notification popup) is concealed around every model-facing capture and
/// revealed afterwards; an excluded overlay (macOS/Windows) is left alone.
#[tokio::test]
async fn capturable_indicators_are_concealed_around_model_captures_only() {
    for capturable in [true, false] {
        let (presence, recorded) =
            presence_with_capturable(PresenceSurface::Screen, false, capturable);
        let (stop, _) = counting_hook();
        let lease = PresenceLease::new(Arc::clone(&presence), "s/conceal".into(), stop);
        // As in the dispatcher, the screenshot action stays in flight (so it
        // cannot idle out) for the whole conceal -> capture -> reveal span.
        let action = lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin");
        // Nobody acks: the bounded wait elapses, the capture proceeds.
        let guard = lease.conceal_for_capture().await;
        assert_eq!(guard.is_some(), capturable);
        drop(guard);
        drop(action);
        let commands = recorded.commands.lock().unwrap().clone();
        let concealed = commands
            .iter()
            .any(|command| matches!(command, PresenceCommand::Conceal { .. }));
        let revealed = commands
            .iter()
            .any(|command| matches!(command, PresenceCommand::Reveal { .. }));
        assert_eq!((concealed, revealed), (capturable, capturable));
    }
}

/// Verifier finding (0ddd89a0): a freshly spawned overlay helper was treated
/// as capture-excluded until its `Ready` was read, so the run's first
/// screenshot was never concealed. "No Ready yet" now means capturable.
/// MUTATION CHECK: default the ready state to excluded. Expected runtime
/// failure: the pre-Ready assertion below.
#[test]
fn overlay_ready_state_treats_no_ready_as_capturable() {
    let state = overlay::ReadyState::default();
    assert!(
        state.capturable(),
        "before Ready the helper UI may be on screen"
    );
    state.record(&PresenceEvent::Error {
        message: "noise".into(),
    });
    assert!(state.capturable(), "only Ready decides");
    state.record(&PresenceEvent::Ready {
        platform: "macos".into(),
        capture_excluded: true,
    });
    assert!(!state.capturable());
    state.record(&PresenceEvent::Ready {
        platform: "windows".into(),
        capture_excluded: false,
    });
    assert!(
        state.capturable(),
        "Windows with exclusion refused is capturable"
    );
}

/// End to end with a real child process standing in for the helper: the
/// conceal is sent before `Ready`, and a helper that reports exclusion is
/// not concealed afterwards.
#[cfg(unix)]
#[tokio::test]
async fn first_capture_is_concealed_before_the_helper_reports_ready() {
    use std::io::Read as _;
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("stdin.log");
    // A helper that records its stdin and only says Ready when asked to.
    let script = format!(
        "while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; \
         case \"$line\" in *'\"op\":\"hide\"'*) printf '%s\\n' '{{\"event\":\"ready\",\"platform\":\"t\",\"capture_excluded\":true}}';; esac; done",
        log.display()
    );
    let presence = CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    presence.register_renderer(
        PresenceSurface::Screen,
        Arc::new(move |sink| {
            overlay::spawn_program(
                std::path::Path::new("/bin/sh"),
                &["-c".to_owned(), script.clone()],
                sink,
            )
            .map(|process| Box::new(process) as Box<dyn PresenceRenderer>)
        }),
    );
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/first".into(), stop);
    let action = lease
        .begin_computer(
            &ComputerAction::Screenshot,
            None,
            &haider_tools::ComputerCancelToken::new(),
        )
        .await
        .expect("begin");
    // No Ready has been (or can be) read yet: the first capture must conceal.
    let guard = lease.conceal_for_capture().await;
    assert!(guard.is_some(), "the first capture of a run is concealed");
    drop(guard);
    drop(action);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while Instant::now() < deadline {
        text.clear();
        if let Ok(mut file) = std::fs::File::open(&log) {
            let _ = file.read_to_string(&mut text);
        }
        if text.contains("\"op\":\"reveal\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let show = text.find("\"op\":\"show\"").expect("show sent");
    let conceal = text
        .find("\"op\":\"conceal\"")
        .expect("conceal sent before Ready");
    let reveal = text.find("\"op\":\"reveal\"").expect("reveal sent");
    assert!(show < conceal && conceal < reveal, "{text}");
    lease.end(false);
}
