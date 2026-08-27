#![allow(clippy::expect_used)]

use super::*;

#[derive(Default)]
struct RegistryFrames(std::sync::Mutex<Vec<WireFrame>>);

impl FrameSink for RegistryFrames {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("registry frames").push(frame);
        Ok(())
    }
}

async fn wait_for_seal(sink: &RegistryFrames, cursor: u64) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if sink.0.lock().expect("registry frames").iter().any(
                |frame| matches!(frame, WireFrame::LoomRegistryCaughtUp { high_water_cursor, .. } if *high_water_cursor == cursor),
            ) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("registry watch seals its replay");
}

#[tokio::test]
async fn reconnect_replays_requested_suffix_then_follows_live_commits() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let created = hub
        .loom_register_workflow_cas(
            "watch-proof: A -> A\nstep \"one\" :cmd".into(),
            haider_protocol::loom::LoomRevisionExpectation {
                rev: 0,
                digest: None,
            },
        )
        .await
        .expect("create workflow");
    let haider_core::LoomRegistryMutation::Applied {
        value: created,
        publication_cursor: Some(first_cursor),
    } = created
    else {
        panic!("workflow create publishes");
    };
    let revised = hub
        .loom_register_workflow_cas(
            "watch-proof: A -> A\nstep \"two\" :cmd".into(),
            haider_protocol::loom::LoomRevisionExpectation {
                rev: created.rev,
                digest: Some(created.digest),
            },
        )
        .await
        .expect("revise workflow");
    let haider_core::LoomRegistryMutation::Applied {
        value: revised,
        publication_cursor: Some(replay_head),
    } = revised
    else {
        panic!("workflow revision publishes");
    };

    let publications = hub.inner.loom_registry_publications.subscribe();
    let sink = Arc::new(RegistryFrames::default());
    let sink_for_task: Arc<dyn FrameSink> = sink.clone();
    let (cancel, cancel_receiver) = watch::channel(false);
    let task = tokio::spawn(run_loom_registry_watch(
        hub.clone(),
        sink_for_task,
        "registry-watch-proof".into(),
        LoomRegistryReplayWindow {
            after_cursor: first_cursor,
            through_cursor: replay_head,
        },
        publications,
        cancel_receiver,
    ));
    wait_for_seal(&sink, replay_head).await;

    let third = hub
        .loom_register_workflow_cas(
            "watch-proof: A -> A\nstep \"three\" :cmd".into(),
            haider_protocol::loom::LoomRevisionExpectation {
                rev: revised.rev,
                digest: Some(revised.digest),
            },
        )
        .await
        .expect("third workflow revision");
    let haider_core::LoomRegistryMutation::Applied {
        publication_cursor: Some(live_head),
        ..
    } = third
    else {
        panic!("third workflow revision publishes");
    };
    wait_for_seal(&sink, live_head).await;
    cancel.send_replace(true);
    task.await.expect("registry watch stops");

    let frames = sink.0.lock().expect("registry frames");
    let cursors = frames
        .iter()
        .filter_map(|frame| match frame {
            WireFrame::LoomRegistryDelta { delta, .. } => Some(delta.cursor),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        cursors,
        ((first_cursor + 1)..=live_head).collect::<Vec<_>>(),
        "attach replays every requested durable cursor and live commits continue contiguously"
    );
    assert!(matches!(
        frames.as_slice(),
        [.., WireFrame::LoomRegistryCaughtUp { high_water_cursor, .. }] if *high_water_cursor == live_head
    ));

    drop(frames);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}
