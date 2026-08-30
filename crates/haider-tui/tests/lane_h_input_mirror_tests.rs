//! Lane H — the TUI is a watcher and publisher of one volatile composer.
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;

use haider_protocol::ids::SessionId;
use haider_rpc::{RequestBody, ResponseBody, SurfaceInputWire, WireFrame};
use haider_tui::app::{AppEvent, AppModel, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_frame, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::launcher_model;

fn sid(value: &str) -> SessionId {
    SessionId::new(value)
}

fn input(text: &str, revision: u64) -> SurfaceInputWire {
    owned("opaque-daemon-connection", text, revision)
}

fn owned(owner: &str, text: &str, revision: u64) -> SurfaceInputWire {
    SurfaceInputWire {
        text: text.to_owned(),
        attachments: Vec::new(),
        revision,
        owner: owner.to_owned(),
    }
}

fn assert_no_publish(commands: &[LiveCommand]) {
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, LiveCommand::SurfacePublish { .. })),
        "expected no publish, got {commands:?}"
    );
}

fn mirror_model(session: &str) -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid(session));
    model.open_session(&sid(session));
    assert_eq!(model.screen, Screen::Session);
    model
}

fn only_publish(commands: Vec<LiveCommand>) -> (String, u64) {
    let [
        LiveCommand::SurfacePublish {
            input: Some((text, _attachments, revision)),
            status: None,
            ..
        },
    ] = commands.as_slice()
    else {
        panic!("expected one input publish, got {commands:?}");
    };
    (text.clone(), *revision)
}

fn assert_one_watch(commands: &[LiveCommand], session: &SessionId) {
    let watches = commands
        .iter()
        .filter_map(|command| match command {
            LiveCommand::SurfaceWatch { session } => Some(session),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(watches, vec![session]);
}

#[test]
fn remote_apply_replaces_through_the_reducer_without_recording_history() {
    let mut model = mirror_model("mirror-a");
    model.composer.set_text("local draft");
    model.composer.record_submitted("kept history");
    model
        .prompt_history
        .push_front(haider_tui::session::PromptEntry::committed(
            "journal prompt".to_owned(),
            4,
        ));
    let prompt_history = model.prompt_history.clone();
    let mut driver = LiveDriver::new("lane-h");

    let initial = driver.sync_input_mirror(&model);
    assert_one_watch(&initial, &sid("mirror-a"));
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            session: sid("mirror-a"),
            input: Some(input("remote replacement", 7)),
        },
    );

    assert_eq!(model.composer.text(), "remote replacement");
    assert_eq!(model.composer.cursor(), "remote replacement".len());
    assert_eq!(model.prompt_history, prompt_history);
    assert!(
        model.requests.is_empty(),
        "remote text emits no durable work"
    );
    assert!(
        driver.sync_input_mirror(&model).is_empty(),
        "an applied remote baseline is not immediately republished"
    );

    assert!(model.composer.history_prev());
    assert_eq!(
        model.composer.text(),
        "kept history",
        "the remote replacement never entered composer history"
    );
}

#[test]
fn stale_and_echo_revisions_drop_and_the_next_local_publish_wins() {
    let mut model = mirror_model("mirror-b");
    model.composer.set_text("local");
    let mut driver = LiveDriver::new("lane-h");

    // Fresh binding: watch FIRST, no publish of any kind (rev934 P1-2).
    let initial = driver.sync_input_mirror(&model);
    assert_one_watch(&initial, &sid("mirror-b"));
    assert_no_publish(&initial);
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            session: sid("mirror-b"),
            input: None,
        },
    );
    // Empty surface adopted; the non-empty local stash genuinely differs
    // post-adoption, so it publishes now.
    let (text, local_revision) = only_publish(driver.sync_input_mirror(&model));
    assert_eq!(text, "local");

    // Our accepted publish echoes back with the daemon-stamped owner:
    // revision AND text match names the lane as SELF — and every later
    // frame in that lane drops, whatever its revision (rev934 P1-1).
    model.composer.move_left(false);
    let moved_cursor = model.composer.cursor();
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: sid("mirror-b"),
            input: owned("conn-self", "local", local_revision),
        },
    );
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: sid("mirror-b"),
            input: owned("conn-self", "self echo must drop", local_revision + 40),
        },
    );
    assert_eq!(model.composer.text(), "local");
    assert_eq!(model.composer.cursor(), moved_cursor);

    // A foreign lane applies; duplicate/stale revisions IN THAT LANE drop.
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: sid("mirror-b"),
            input: owned("conn-remote", "remote", 6),
        },
    );
    assert_eq!(model.composer.text(), "remote");
    assert_eq!(model.composer.cursor(), "remote".len());

    model.composer.move_left(false);
    let moved_cursor = model.composer.cursor();
    for revision in [5, 6] {
        driver.apply(
            &mut model,
            LiveReply::SurfaceInput {
                session: sid("mirror-b"),
                input: owned("conn-remote", "stale", revision),
            },
        );
    }
    assert_eq!(model.composer.text(), "remote");
    assert_eq!(model.composer.cursor(), moved_cursor);

    // The next local edit publishes on OUR monotone lane.
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('!'),
        KeyModifiers::NONE,
    )));
    let (text, next_revision) = only_publish(driver.sync_input_mirror(&model));
    assert_eq!(text, "remot!e");
    assert!(next_revision > local_revision, "our lane stays monotone");
}

#[test]
fn fresh_foreign_publisher_low_revision_applies_over_high_local_floor() {
    let mut model = mirror_model("mirror-low");
    let mut driver = LiveDriver::new("lane-h");
    let initial = driver.sync_input_mirror(&model);
    assert_one_watch(&initial, &sid("mirror-low"));
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            session: sid("mirror-low"),
            input: None,
        },
    );

    // Drive the LOCAL publish counter far past 200.
    let mut last_revision = 0;
    for index in 0..200 {
        model.composer.set_text(format!("draft {index}"));
        let (_, revision) = only_publish(driver.sync_input_mirror(&model));
        last_revision = revision;
    }
    assert!(last_revision >= 200);

    // Daemon revision lanes are per-connection: a fresh publisher's
    // revision 1 is not comparable to our floor — it MUST apply (P1-1).
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: sid("mirror-low"),
            input: owned("conn-fresh-ade", "fresh remote draft", 1),
        },
    );
    assert_eq!(model.composer.text(), "fresh remote draft");
}

#[test]
fn ade_draft_survives_tui_attach_and_lands_in_the_composer() {
    let mut model = mirror_model("mirror-attach");
    assert_eq!(model.composer.text(), "");
    let mut driver = LiveDriver::new("lane-h");

    // A just-opened TUI watches first and publishes NOTHING — before the
    // fix it published "" revision 1 and wiped the ADE draft (P1-2).
    let initial = driver.sync_input_mirror(&model);
    assert_one_watch(&initial, &sid("mirror-attach"));
    assert_no_publish(&initial);
    assert!(
        driver.sync_input_mirror(&model).is_empty(),
        "publishes hold while the watch ack is in flight"
    );

    // The ack carries the ADE's in-progress draft: adopted, not wiped.
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            session: sid("mirror-attach"),
            input: Some(owned("conn-ade", "ade draft in progress", 3)),
        },
    );
    assert_eq!(model.composer.text(), "ade draft in progress");
    assert!(
        driver.sync_input_mirror(&model).is_empty(),
        "adoption is not republished"
    );

    // Only a real local edit publishes — and it wins as the newest frame.
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('!'),
        KeyModifiers::NONE,
    )));
    let (text, _) = only_publish(driver.sync_input_mirror(&model));
    assert_eq!(text, "ade draft in progress!");
}

#[test]
fn remote_replace_never_overwrites_a_modal_in_progress() {
    let mut model = mirror_model("mirror-modal");
    let mut driver = LiveDriver::new("lane-h");
    let initial = driver.sync_input_mirror(&model);
    assert_one_watch(&initial, &sid("mirror-modal"));
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            session: sid("mirror-modal"),
            input: None,
        },
    );
    model.composer.set_text("modal answer in flight");

    // The same predicate that gates injected ops gates remote replaces
    // (rev934 P3-5): with the composer not the live Enter target, a remote
    // frame must not clobber it.
    model.help_open = true;
    assert!(!model.accepts_injected_input());
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: sid("mirror-modal"),
            input: owned("conn-remote", "remote clobber", 9),
        },
    );
    assert_eq!(model.composer.text(), "modal answer in flight");
}

#[test]
fn watch_is_issued_on_entry_switch_and_connection_epoch() {
    let mut model = mirror_model("mirror-one");
    model.upsert_live_session(&sid("mirror-two"));
    let mut driver = LiveDriver::new("lane-h");

    let first = driver.sync_input_mirror(&model);
    assert_one_watch(&first, &sid("mirror-one"));
    let first_watch = first
        .iter()
        .find(|command| matches!(command, LiveCommand::SurfaceWatch { .. }))
        .expect("first watch");
    assert_eq!(
        request_body(first_watch.clone()),
        RequestBody::SessionSurfaceWatch {
            session_id: sid("mirror-one")
        }
    );
    assert!(driver.sync_input_mirror(&model).is_empty());

    model.open_session(&sid("mirror-two"));
    let switched = driver.sync_input_mirror(&model);
    assert_one_watch(&switched, &sid("mirror-two"));

    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "test redial".to_owned(),
        },
    );
    driver.apply(
        &mut model,
        LiveReply::Handshake {
            features: BTreeSet::from([haider_rpc::FEATURE_INPUT_MIRROR_V1.to_owned()]),
            version: "test".to_owned(),
        },
    );
    driver.apply(&mut model, LiveReply::Reconnected);
    let redialed = driver.sync_input_mirror(&model);
    assert_one_watch(&redialed, &sid("mirror-two"));
}

#[test]
fn watch_frames_map_to_the_surface_reducer_and_stay_feature_gated() {
    let watch = LiveCommand::SurfaceWatch {
        session: sid("mirror-frame"),
    };
    let context = CommandContext::of(&watch);
    assert_eq!(
        map_response(
            &context,
            ResponseBody::SessionSurfaceWatching {
                session_id: sid("mirror-frame"),
                input: Some(input("baseline", 4)),
                status: None,
            },
        ),
        vec![LiveReply::SurfaceWatching {
            session: sid("mirror-frame"),
            input: Some(input("baseline", 4)),
        }]
    );
    assert_eq!(
        map_frame(WireFrame::SessionSurfaceDelta {
            session_id: sid("mirror-frame"),
            input: Some(input("delta", 5)),
            status: None,
        }),
        vec![LiveReply::SurfaceInput {
            session: sid("mirror-frame"),
            input: input("delta", 5),
        }]
    );
    assert!(
        map_frame(WireFrame::SessionSurfaceDelta {
            session_id: sid("mirror-frame"),
            input: None,
            status: None,
        })
        .is_empty()
    );

    let mut model = mirror_model("mirror-frame");
    let mut driver = LiveDriver::new("lane-h");
    model.daemon_features.clear();
    assert!(driver.sync_input_mirror(&model).is_empty());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_INPUT_MIRROR_V1.to_owned());
    model.screen = Screen::Launcher;
    assert!(driver.sync_input_mirror(&model).is_empty());
    model.screen = Screen::Aura;
    assert!(driver.sync_input_mirror(&model).is_empty());
}
