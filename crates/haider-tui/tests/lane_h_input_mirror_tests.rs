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
    SurfaceInputWire {
        text: text.to_owned(),
        revision,
        owner: "opaque-daemon-connection".to_owned(),
    }
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
            input: Some((text, revision)),
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
    model.prompt_history.push_front("journal prompt".to_owned());
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

    let initial = driver.sync_input_mirror(&model);
    assert_one_watch(&initial, &sid("mirror-b"));
    let (_, local_revision) = initial
        .iter()
        .find_map(|command| match command {
            LiveCommand::SurfacePublish {
                input: Some((text, revision)),
                ..
            } => Some((text.clone(), *revision)),
            _ => None,
        })
        .expect("initial local publish");
    driver.apply(
        &mut model,
        LiveReply::SurfaceWatching {
            session: sid("mirror-b"),
            input: None,
        },
    );
    model.composer.move_left(false);
    let moved_cursor = model.composer.cursor();

    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: sid("mirror-b"),
            input: input("self echo must drop", local_revision),
        },
    );
    assert_eq!(model.composer.text(), "local");
    assert_eq!(model.composer.cursor(), moved_cursor);

    let remote_revision = local_revision + 2;
    driver.apply(
        &mut model,
        LiveReply::SurfaceInput {
            session: sid("mirror-b"),
            input: input("remote", remote_revision),
        },
    );
    assert_eq!(model.composer.text(), "remote");
    assert_eq!(model.composer.cursor(), "remote".len());

    model.composer.move_left(false);
    let moved_cursor = model.composer.cursor();
    for revision in [remote_revision - 1, remote_revision] {
        driver.apply(
            &mut model,
            LiveReply::SurfaceInput {
                session: sid("mirror-b"),
                input: input("stale", revision),
            },
        );
    }
    assert_eq!(model.composer.text(), "remote");
    assert_eq!(model.composer.cursor(), moved_cursor);

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('!'),
        KeyModifiers::NONE,
    )));
    let (text, next_revision) = only_publish(driver.sync_input_mirror(&model));
    assert_eq!(text, "remot!e");
    assert!(
        next_revision > remote_revision,
        "the local counter jumped beyond the applied remote revision"
    );
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
