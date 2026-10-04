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
    fail_conceal: bool,
}

impl PresenceRenderer for FakeRenderer {
    fn send(&mut self, command: &PresenceCommand) -> bool {
        self.recorded.commands.lock().unwrap().push(command.clone());
        !(self.fail_conceal && matches!(command, PresenceCommand::Conceal { .. }))
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
    // Conceal tests intentionally wait through the 600 ms ack bound. Keep
    // their fixture's idle timer out of that interleaving; the dedicated
    // idle-expiry test below uses the shorter non-capturable fixture.
    let idle_timeout = if capturable {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(200)
    };
    let presence = CuPresence::new(idle_timeout, Duration::from_millis(20));
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
                fail_conceal: false,
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
        // Nobody acks: a capturable capture is refused after the bound.
        let guard = lease.conceal_for_capture().await;
        assert_eq!(guard.is_err(), capturable);
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

/// Polls a future once without a runtime wake-up; true while it is pending.
fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> bool {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    future.poll(&mut context).is_pending()
}

/// Records conceal/reveal only, and whether a Conceal was ever answered.
fn conceal_log(recorded: &Recorded) -> Vec<&'static str> {
    recorded
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter_map(|command| match command {
            PresenceCommand::Conceal { .. } => Some("conceal"),
            PresenceCommand::Reveal { .. } => Some("reveal"),
            _ => None,
        })
        .collect()
}

fn conceal_seq(recorded: &Recorded) -> u64 {
    recorded
        .commands
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|command| match command {
            PresenceCommand::Conceal { seq, .. } => Some(*seq),
            _ => None,
        })
        .expect("a conceal was sent")
}

/// Verifier finding B2 (8c614f6c), cancellation: a capture cancelled while
/// waiting for the conceal ack (Esc/Stop) left no guard, so the indicator —
/// and its Stop control — stayed hidden while ANOTHER session kept the
/// surface. The hold is now taken before the first await.
/// MUTATION CHECK: construct the guard after the ack wait. Expected runtime
/// failure: no "reveal" after the cancelled capture.
#[tokio::test]
async fn capture_cancelled_before_the_conceal_ack_still_reveals() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    // Session B keeps the screen (e.g. a long wait/typing action).
    let (stop_b, _) = counting_hook();
    let lease_b = PresenceLease::new(Arc::clone(&presence), "s/b".into(), stop_b);
    let _b_action = lease_b
        .begin_computer(
            &ComputerAction::Wait { ms: 60_000 },
            None,
            &haider_tools::ComputerCancelToken::new(),
        )
        .await
        .expect("begin b");
    // Session A starts a screenshot; nobody acks the conceal.
    let (stop_a, _) = counting_hook();
    let lease_a = PresenceLease::new(Arc::clone(&presence), "s/a".into(), stop_a);
    {
        let capture = lease_a.conceal_for_capture();
        tokio::pin!(capture);
        // Poll once: the Conceal is sent and the future parks on the ack.
        assert!(poll_once(capture.as_mut()), "waiting for the helper's ack");
        assert_eq!(conceal_log(&recorded), vec!["conceal"]);
        // Esc: the capture future is dropped mid-wait.
    }
    assert_eq!(
        conceal_log(&recorded),
        vec!["conceal", "reveal"],
        "the cancelled capture releases its hold and reveals"
    );
    assert_eq!(presence.capture_holders(PresenceSurface::Screen), 0);
    lease_a.end(true);
    assert!(
        presence.is_shown(PresenceSurface::Screen),
        "B still controls the screen"
    );
}

/// Verifier finding B2 (8c614f6c), overlap: two sessions' captures sent
/// independent Conceal/Reveal, so the first to finish revealed the surface
/// while the other capture was still running.
/// MUTATION CHECK: reveal on every guard drop instead of the last. Expected
/// runtime failure: a "reveal" while capture B is still held.
#[tokio::test]
async fn overlapping_captures_reveal_only_when_the_last_one_retires() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop_a, _) = counting_hook();
    let (stop_b, _) = counting_hook();
    let lease_a = PresenceLease::new(Arc::clone(&presence), "s/a".into(), stop_a);
    let lease_b = PresenceLease::new(Arc::clone(&presence), "s/b".into(), stop_b);
    for lease in [&lease_a, &lease_b] {
        drop(
            lease
                .begin_computer(
                    &ComputerAction::Screenshot,
                    None,
                    &haider_tools::ComputerCancelToken::new(),
                )
                .await
                .expect("begin"),
        );
    }
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    // A's capture starts; the helper acks promptly.
    let capture_a = lease_a.conceal_for_capture();
    tokio::pin!(capture_a);
    assert!(poll_once(capture_a.as_mut()));
    sink(PresenceEvent::Ack {
        seq: conceal_seq(&recorded),
    });
    let guard_a = capture_a.await;
    // B's capture overlaps: the surface is already concealed and acked, so
    // B neither re-conceals nor waits.
    let started = Instant::now();
    let guard_b = lease_b.conceal_for_capture().await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "no second ack wait"
    );
    assert_eq!(
        conceal_log(&recorded),
        vec!["conceal"],
        "one Conceal for both"
    );
    assert_eq!(presence.capture_holders(PresenceSurface::Screen), 2);
    drop(guard_a);
    assert_eq!(
        conceal_log(&recorded),
        vec!["conceal"],
        "A finishing must not reveal while B is capturing"
    );
    drop(guard_b);
    assert_eq!(conceal_log(&recorded), vec!["conceal", "reveal"]);
    lease_a.end(false);
    lease_b.end(false);
}

// Reviewer interleaving probes kept as permanent regression coverage.

/// B2 class probe: a LATE ack for an earlier, timed-out Conceal must not
/// release a newer Conceal's wait (the helper handles Conceal(1), Reveal,
/// Conceal(2) in order, so ack(1) arriving after Conceal(2) was sent says
/// nothing about Conceal(2)). Expected on a correct implementation: PASS.
#[tokio::test]
async fn regression_stale_conceal_ack_does_not_release_a_newer_conceal() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/stale".into(), stop);
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    // Capture 1: the helper is slow; the bounded wait elapses unacked.
    let guard_1 = lease.conceal_for_capture().await;
    let seq_1 = conceal_seq(&recorded);
    drop(guard_1);
    assert_eq!(conceal_log(&recorded), vec!["conceal", "reveal"]);
    // Capture 2 sends a new Conceal and parks on its ack.
    let capture_2 = lease.conceal_for_capture();
    tokio::pin!(capture_2);
    assert!(poll_once(capture_2.as_mut()));
    let seq_2 = conceal_seq(&recorded);
    assert_ne!(seq_1, seq_2);
    // The helper finally acks Conceal 1 (it has not yet run Reveal/Conceal 2).
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    sink(PresenceEvent::Ack { seq: seq_1 });
    let early = tokio::time::timeout(Duration::from_millis(250), capture_2.as_mut()).await;
    assert!(
        early.is_err(),
        "stale ack for Conceal {seq_1} released capture 2 before Conceal {seq_2} was acked"
    );
    lease.end(false);
}

/// B2 class probe: the run ends (Hide retires the renderer) while its
/// capture still holds; another session's action recreates the renderer,
/// which must be concealed, and the last hold reveals it. No leak.
#[tokio::test]
async fn regression_session_end_while_held_recreated_renderer_concealed_no_leak() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop_a, _) = counting_hook();
    let lease_a = PresenceLease::new(Arc::clone(&presence), "s/a".into(), stop_a);
    drop(
        lease_a
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin a"),
    );
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    let capture = lease_a.conceal_for_capture();
    tokio::pin!(capture);
    assert!(poll_once(capture.as_mut()));
    sink(PresenceEvent::Ack {
        seq: conceal_seq(&recorded),
    });
    let guard_a = capture.await;
    lease_a.end(true);
    assert_eq!(
        recorded.closed.load(Ordering::SeqCst),
        1,
        "Hide retired the renderer"
    );
    let (stop_b, _) = counting_hook();
    let lease_b = PresenceLease::new(Arc::clone(&presence), "s/b".into(), stop_b);
    let _b = lease_b
        .begin_computer(&click(), None, &haider_tools::ComputerCancelToken::new())
        .await
        .expect("begin b");
    assert_eq!(recorded.spawned.load(Ordering::SeqCst), 2);
    assert_eq!(
        conceal_log(&recorded),
        vec!["conceal", "conceal"],
        "recreated renderer concealed"
    );
    let commands = recorded.commands.lock().unwrap().clone();
    let retired = commands
        .iter()
        .rposition(|command| matches!(command, PresenceCommand::Hide { .. }))
        .unwrap();
    assert!(
        matches!(
            commands.get(retired + 1),
            Some(PresenceCommand::Conceal { .. })
        ),
        "Conceal must be the recreated renderer's first command"
    );
    let second_conceal = commands
        .iter()
        .rposition(|command| matches!(command, PresenceCommand::Conceal { .. }))
        .unwrap();
    let second_show = commands
        .iter()
        .rposition(|command| matches!(command, PresenceCommand::Show { .. }))
        .unwrap();
    assert!(
        second_conceal < second_show,
        "recreated renderer must be concealed before Show"
    );
    drop(guard_a);
    assert_eq!(conceal_log(&recorded), vec!["conceal", "conceal", "reveal"]);
    assert_eq!(presence.capture_holders(PresenceSurface::Screen), 0);
    drop(_b);
    lease_b.end(false);
}

/// B2 class probe: Hide (run end / Stop) while a capture awaits its ack
/// releases the wait at once instead of the 600 ms bound, and no hold leaks.
#[tokio::test]
async fn regression_hide_while_awaiting_ack_releases_wait_no_leak() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/h".into(), stop);
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    let capture = lease.conceal_for_capture();
    tokio::pin!(capture);
    assert!(poll_once(capture.as_mut()));
    lease.end(true);
    let started = Instant::now();
    let guard = capture.await;
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "released by the Hide"
    );
    assert!(guard.is_err(), "renderer retirement refuses capture");
    drop(guard);
    assert_eq!(presence.capture_holders(PresenceSurface::Screen), 0);
    assert_eq!(
        conceal_log(&recorded),
        vec!["conceal"],
        "no Reveal to a retired renderer"
    );
}

/// B2 class probe: a panic in the capture path unwinds the guard.
#[tokio::test]
async fn regression_panic_while_holding_releases_the_hold() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop, _) = counting_hook();
    let lease = Arc::new(PresenceLease::new(
        Arc::clone(&presence),
        "s/p".into(),
        stop,
    ));
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    let held = Arc::clone(&lease);
    let joined = tokio::spawn(async move {
        let _guard = held.conceal_for_capture().await;
        panic!("probe: capture path panicked while holding");
    })
    .await;
    assert!(joined.is_err());
    assert_eq!(presence.capture_holders(PresenceSurface::Screen), 0);
    assert_eq!(conceal_log(&recorded), vec!["conceal", "reveal"]);
    lease.end(false);
}

#[tokio::test]
async fn conceal_ack_accepts_only_the_current_sequence_once() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/seq".into(), stop);
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    let capture = lease.conceal_for_capture();
    tokio::pin!(capture);
    assert!(poll_once(capture.as_mut()));
    let seq = conceal_seq(&recorded);
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    for wrong in [seq + 1, 1, u64::MAX] {
        sink(PresenceEvent::Ack { seq: wrong });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), capture.as_mut())
                .await
                .is_err(),
            "unsent/future seq {wrong} released the capture"
        );
    }
    sink(PresenceEvent::Ack { seq });
    let guard = tokio::time::timeout(Duration::from_millis(100), capture.as_mut())
        .await
        .expect("matching ack must release promptly");
    sink(PresenceEvent::Ack { seq }); // Duplicate cannot release a later hold.
    drop(guard);
    let next = lease.conceal_for_capture();
    tokio::pin!(next);
    assert!(poll_once(next.as_mut()));
    sink(PresenceEvent::Ack { seq });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), next.as_mut())
            .await
            .is_err()
    );
    sink(PresenceEvent::Ack {
        seq: conceal_seq(&recorded),
    });
    drop(next.await);
    lease.end(false);
}

#[tokio::test]
async fn overlapping_holders_wait_for_the_same_outstanding_conceal() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop_a, _) = counting_hook();
    let (stop_b, _) = counting_hook();
    let a = PresenceLease::new(Arc::clone(&presence), "s/overlap-a".into(), stop_a);
    let b = PresenceLease::new(Arc::clone(&presence), "s/overlap-b".into(), stop_b);
    for lease in [&a, &b] {
        drop(
            lease
                .begin_computer(
                    &ComputerAction::Screenshot,
                    None,
                    &haider_tools::ComputerCancelToken::new(),
                )
                .await
                .expect("begin"),
        );
    }
    let first = a.conceal_for_capture();
    let second = b.conceal_for_capture();
    tokio::pin!(first, second);
    assert!(poll_once(first.as_mut()));
    assert!(poll_once(second.as_mut()));
    assert_eq!(conceal_log(&recorded), vec!["conceal"]);
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    sink(PresenceEvent::Ack {
        seq: conceal_seq(&recorded),
    });
    let first_guard = tokio::time::timeout(Duration::from_millis(100), first.as_mut())
        .await
        .expect("first");
    let second_guard = tokio::time::timeout(Duration::from_millis(100), second.as_mut())
        .await
        .expect("second");
    drop(first_guard);
    assert_eq!(conceal_log(&recorded), vec!["conceal"]);
    drop(second_guard);
    assert_eq!(conceal_log(&recorded), vec!["conceal", "reveal"]);
    a.end(false);
    b.end(false);
}

#[tokio::test]
async fn retired_renderer_ack_cannot_release_recreated_renderer_conceal() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop_a, _) = counting_hook();
    let a = PresenceLease::new(Arc::clone(&presence), "s/restart-a".into(), stop_a);
    drop(
        a.begin_computer(
            &ComputerAction::Screenshot,
            None,
            &haider_tools::ComputerCancelToken::new(),
        )
        .await
        .expect("begin"),
    );
    let capture = a.conceal_for_capture();
    tokio::pin!(capture);
    assert!(poll_once(capture.as_mut()));
    let old_seq = conceal_seq(&recorded);
    let old_sink = recorded.sink.lock().unwrap().clone().expect("old sink");
    old_sink(PresenceEvent::Ack { seq: old_seq });
    let guard = capture.await.expect("first helper verified conceal");
    a.end(true); // Hide retires the first helper while the hold remains.
    let (stop_b, _) = counting_hook();
    let b = PresenceLease::new(Arc::clone(&presence), "s/restart-b".into(), stop_b);
    drop(
        b.begin_computer(&click(), None, &haider_tools::ComputerCancelToken::new())
            .await
            .expect("restart"),
    );
    let new_seq = conceal_seq(&recorded);
    assert_ne!(old_seq, new_seq);
    let next = b.conceal_for_capture();
    tokio::pin!(next);
    assert!(poll_once(next.as_mut()));
    old_sink(PresenceEvent::Ack { seq: old_seq });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), next.as_mut())
            .await
            .is_err()
    );
    let new_sink = recorded.sink.lock().unwrap().clone().expect("new sink");
    new_sink(PresenceEvent::Ack { seq: new_seq });
    drop(next.await);
    drop(guard);
    b.end(false);
}

#[tokio::test]
async fn failed_reconceal_never_shows_the_recreated_renderer() {
    let presence = CuPresence::new(Duration::from_secs(5), Duration::from_millis(20));
    let recorded = Arc::new(Recorded::default());
    presence.register_renderer(
        PresenceSurface::Screen,
        Arc::new({
            let recorded = Arc::clone(&recorded);
            move |sink| {
                let generation = recorded.spawned.fetch_add(1, Ordering::SeqCst);
                *recorded.sink.lock().unwrap() = Some(sink);
                Some(Box::new(FakeRenderer {
                    recorded: Arc::clone(&recorded),
                    acks: false,
                    capturable: true,
                    fail_conceal: generation > 0,
                }) as Box<dyn PresenceRenderer>)
            }
        }),
    );
    let (stop_a, _) = counting_hook();
    let a = PresenceLease::new(Arc::clone(&presence), "s/dead-a".into(), stop_a);
    let _action = a
        .begin_computer(
            &ComputerAction::Screenshot,
            None,
            &haider_tools::ComputerCancelToken::new(),
        )
        .await
        .expect("first renderer");
    let capture = a.conceal_for_capture();
    tokio::pin!(capture);
    assert!(poll_once(capture.as_mut()));
    let first_sink = recorded.sink.lock().unwrap().clone().expect("first sink");
    first_sink(PresenceEvent::Ack {
        seq: conceal_seq(&recorded),
    });
    let guard = capture.await.expect("first helper verified conceal");
    a.end(true);
    let (stop_b, _) = counting_hook();
    let b = PresenceLease::new(Arc::clone(&presence), "s/dead-b".into(), stop_b);
    drop(
        b.begin_computer(&click(), None, &haider_tools::ComputerCancelToken::new())
            .await
            .expect("second action"),
    );
    let commands = recorded.commands.lock().unwrap().clone();
    let retired = commands
        .iter()
        .rposition(|command| matches!(command, PresenceCommand::Hide { .. }))
        .unwrap();
    assert!(
        commands[retired + 1..]
            .iter()
            .all(|command| matches!(command, PresenceCommand::Conceal { .. })),
        "failed Conceal cannot be followed by Show or Pointer"
    );
    assert!(recorded.spawned.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        recorded.closed.load(Ordering::SeqCst),
        recorded.spawned.load(Ordering::SeqCst)
    );
    drop(guard);
    b.end(false);
}

#[tokio::test]
async fn failed_first_conceal_retires_the_renderer_before_capture() {
    let presence = CuPresence::new(Duration::from_secs(5), Duration::from_millis(20));
    let recorded = Arc::new(Recorded::default());
    presence.register_renderer(
        PresenceSurface::Screen,
        Arc::new({
            let recorded = Arc::clone(&recorded);
            move |sink| {
                recorded.spawned.fetch_add(1, Ordering::SeqCst);
                *recorded.sink.lock().unwrap() = Some(sink);
                Some(Box::new(FakeRenderer {
                    recorded: Arc::clone(&recorded),
                    acks: false,
                    capturable: true,
                    fail_conceal: true,
                }) as Box<dyn PresenceRenderer>)
            }
        }),
    );
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/failed-first".into(), stop);
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    let guard = lease.conceal_for_capture().await;
    assert_eq!(recorded.closed.load(Ordering::SeqCst), 1);
    assert!(guard.is_err(), "failed send must refuse capture");
    assert!(
        !lock(&presence.state)
            .renderers
            .contains_key(&PresenceSurface::Screen)
    );
    drop(guard);
    lease.end(false);
}

#[tokio::test]
async fn only_the_latest_conceal_ack_is_retained_across_timeouts() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/bounded".into(), stop);
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    for _ in 0..3 {
        let guard = lease.conceal_for_capture().await;
        assert!(guard.is_err(), "timeout must refuse the capture");
        drop(guard);
        let seq = conceal_seq(&recorded);
        let state = lock(&presence.state);
        assert_eq!(state.conceal_acks.len(), 1, "previous seqs are purged");
        assert_eq!(state.conceal_acks.get(&seq), Some(&PresenceSurface::Screen));
    }
    lease.end(false);
}

#[tokio::test]
async fn typed_conceal_failure_rejects_capture_without_replacing_visible_renderer() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/verify-failed".into(), stop);
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    let capture = lease.conceal_for_capture();
    tokio::pin!(capture);
    assert!(poll_once(capture.as_mut()));
    let seq = conceal_seq(&recorded);
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    sink(PresenceEvent::ConcealFailed {
        seq,
        message: "window still visible".into(),
    });
    assert!(
        capture.await.is_err(),
        "failed verification must refuse capture"
    );
    assert_eq!(recorded.closed.load(Ordering::SeqCst), 0);
    assert_eq!(conceal_log(&recorded), vec!["conceal", "reveal"]);
    lease.end(false);
}

#[tokio::test]
async fn stale_conceal_failure_cannot_reject_a_new_capture() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/stale-failure".into(), stop);
    drop(
        lease
            .begin_computer(
                &ComputerAction::Screenshot,
                None,
                &haider_tools::ComputerCancelToken::new(),
            )
            .await
            .expect("begin"),
    );
    let sink = recorded.sink.lock().unwrap().clone().expect("sink");
    let first = lease.conceal_for_capture();
    tokio::pin!(first);
    assert!(poll_once(first.as_mut()));
    let old_seq = conceal_seq(&recorded);
    sink(PresenceEvent::Ack { seq: old_seq });
    drop(first.await.expect("verified first capture"));
    let second = lease.conceal_for_capture();
    tokio::pin!(second);
    assert!(poll_once(second.as_mut()));
    let new_seq = conceal_seq(&recorded);
    sink(PresenceEvent::ConcealFailed {
        seq: old_seq,
        message: "stale".into(),
    });
    assert_eq!(recorded.closed.load(Ordering::SeqCst), 0);
    sink(PresenceEvent::Ack { seq: new_seq });
    drop(
        second
            .await
            .expect("stale failure did not reject current capture"),
    );
    lease.end(false);
}

#[tokio::test]
async fn conceal_sequence_rollover_stays_out_of_the_pointer_ack_range() {
    let (presence, recorded) = presence_with_capturable(PresenceSurface::Screen, false, true);
    presence.next_conceal_seq.store(u64::MAX, Ordering::Relaxed);
    let (stop, _) = counting_hook();
    let lease = PresenceLease::new(Arc::clone(&presence), "s/wrap".into(), stop);
    let action = lease
        .begin_computer(
            &ComputerAction::Screenshot,
            None,
            &haider_tools::ComputerCancelToken::new(),
        )
        .await
        .expect("begin");
    for expected in [u64::MAX, CONCEAL_ACK_SEQ_START] {
        let capture = lease.conceal_for_capture();
        tokio::pin!(capture);
        assert!(poll_once(capture.as_mut()));
        assert_eq!(conceal_seq(&recorded), expected);
        let sink = recorded.sink.lock().unwrap().clone().expect("sink");
        sink(PresenceEvent::Ack { seq: expected });
        drop(capture.await);
    }
    drop(action);
    lease.end(false);
}
