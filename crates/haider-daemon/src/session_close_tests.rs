//! Close witnesses use completion channels, not elapsed-time guesses.
use super::*;

async fn attach(hub: &SessionHub, session: &SessionId, mode: AttachMode) -> AttachmentId {
    let RegisterResult::Registered(registration) = hub
        .register("close-test", session.clone(), 0, mode)
        .await
        .expect("register")
    else {
        panic!("attachment refused");
    };
    let id = registration.attachment_id.clone();
    hub.spawn_replay(
        registration,
        0,
        false,
        Arc::new(CapturingFrameSink::default()),
    )
    .expect("replay");
    id
}

async fn close(hub: &SessionHub, id: AttachmentId) -> Result<SessionId, HaiderError> {
    hub.close_attachment_session(
        id,
        "close-test".into(),
        Arc::new(CapturingFrameSink::default()),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_close_refuses_orphan_recovery_until_its_last_journal_write() {
    use crate::tasks::TaskFacade;
    use haider_protocol::ids::TaskId;
    use haider_protocol::task::TaskStarted;
    use haider_tools::PidLiveness;

    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    for supervised in [false, true] {
        let session = SessionId::new(format!("close-orphan-{supervised}"));
        let run = RunId::new(format!("prior-run-{supervised}"));
        let task_id = TaskId::new(format!("orphan-{supervised}"));
        hub.create_internal_session(create_command(&session, session.as_str()))
            .await
            .expect("create");
        let started = TaskStarted {
            task: task_id.clone(),
            name: "prior daemon task".into(),
            command: "synthetic orphan; never executed".into(),
            pid: 999_999_990,
            started_at_ms: 1,
        };
        let mut facts = [
            crate::tasks::test_task_fact_envelope(
                &hub,
                &session,
                &run,
                &format!("orphan-started-{supervised}"),
                started.to_payload_value().expect("started payload"),
            ),
            crate::tasks::test_task_fact_envelope(
                &hub,
                &session,
                &run,
                &format!("prior-run-done-{supervised}"),
                serde_json::to_value(haider_protocol::EventPayload::RunState(
                    haider_protocol::state::RunState::Done,
                ))
                .expect("terminal payload"),
            ),
        ];
        hub.append(&mut facts).await.expect("prior daemon history");
        let attachment = attach(&hub, &session, AttachMode::Control).await;
        let before = store.read(&session, 0, 256).await.expect("before recovery");
        let (entered, at_probe) = oneshot::channel();
        let entered = std::sync::Mutex::new(Some(entered));
        let (release, released) = std::sync::mpsc::channel();
        let released = std::sync::Mutex::new(released);
        let probe = move |_| {
            if let Some(entered) = entered.lock().expect("probe signal").take() {
                let _ = entered.send(());
            }
            released
                .lock()
                .expect("probe gate")
                .recv()
                .expect("release probe");
            PidLiveness::Dead
        };
        let facade = TaskFacade::new(hub.clone());
        let adoption = if supervised {
            facade
                .start_session_adoption_with_probe(&session, probe)
                .expect("start recovery");
            None
        } else {
            let session = session.clone();
            Some(tokio::spawn(async move {
                facade.adopt_session_with_probe(&session, probe).await
            }))
        };
        // The injected probe blocks outside the store lock, before a Running
        // entry or completion exists. The other runtime worker can close.
        at_probe.await.expect("recovery reached orphan reaping");
        assert_eq!(hub.task_registry().running_count(&session), 0);
        let refused = close(&hub, attachment.clone()).await;
        release.send(()).expect("release orphan recovery");
        if let Some(adoption) = adoption {
            adoption
                .await
                .expect("adoption joined")
                .expect("adoption succeeded");
        }
        hub.task_registry()
            .join_session(&session)
            .await
            .expect("tracked recovery joined");
        assert_eq!(
            refused
                .expect_err("active recovery has no close witness")
                .code,
            ErrorCode::Busy
        );
        let recovered = store
            .read(&session, 0, 256)
            .await
            .expect("recovered history");
        assert_eq!(
            recovered.len(),
            before.len() + 1,
            "orphan completion preceded close"
        );
        assert!(hub.task_registry().get(&session, &task_id).is_some());
        assert_eq!(
            close(&hub, attachment).await.expect("settled close"),
            session
        );
        assert!(
            !lock(&hub.inner.actors)
                .expect("actors")
                .contains_key(&session)
        );
        assert!(hub.task_registry().get(&session, &task_id).is_none());
        assert_eq!(
            store.read(&session, 0, 256).await.expect("after witness"),
            recovered
        );
    }
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn supervisor_adoption_owns_activity_before_first_poll_and_refuses_a_close_fence() {
    use crate::tasks::TaskFacade;
    use haider_tools::PidLiveness;
    use std::future::Future;
    use std::task::Poll;

    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-startup-adoption");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let facade = TaskFacade::new(hub.clone());
    let activity = hub.try_session_activity(&session).expect("activity");
    let serial = Arc::clone(tokio::sync::OwnedRwLockReadGuard::rwlock(&activity));
    drop(activity);
    facade
        .start_session_adoption_with_probe(&session, |_| PidLiveness::Dead)
        .expect("start");
    // This current-thread runtime has not polled the spawned task yet.
    assert!(
        serial.clone().try_write_owned().is_err(),
        "startup admission owns the lease synchronously"
    );
    hub.task_registry()
        .join_session(&session)
        .await
        .expect("startup task joined");
    let fence = serial
        .clone()
        .try_write_owned()
        .expect("joined recovery released activity");
    assert_eq!(
        facade
            .start_session_adoption_with_probe(&session, |_| panic!("fenced probe ran"))
            .expect_err("late startup is refused before spawning")
            .code,
        ErrorCode::Busy
    );
    let mut cancelled = Box::pin(facade.start_session_adoption_when_available(&session));
    std::future::poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    // Supervisor retirement drops this lease wait; no detached retry survives.
    drop(cancelled);
    drop(fence);
    assert!(serial.clone().try_write_owned().is_ok());
    hub.task_registry()
        .join_session(&session)
        .await
        .expect("no late recovery task");
    let fence = serial
        .clone()
        .try_write_owned()
        .expect("refused close fence");
    let mut resumed = Box::pin(facade.start_session_adoption_when_available(&session));
    std::future::poll_fn(|cx| {
        assert!(resumed.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(fence);
    resumed
        .await
        .expect("a refused close resumes supervisor recovery");
    assert!(serial.clone().try_write_owned().is_err());
    hub.task_registry()
        .join_session(&session)
        .await
        .expect("resumed recovery joined");
    let before = store.read(&session, 0, 256).await.expect("history");
    let attachment = attach(&hub, &session, AttachMode::Control).await;
    close(&hub, attachment).await.expect("close");
    assert!(
        !lock(&hub.inner.actors)
            .expect("actors")
            .contains_key(&session)
    );
    assert_eq!(
        store.read(&session, 0, 256).await.expect("after witness"),
        before
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn detach_rpc_requires_control_and_emits_close_witness_only_after_eviction() {
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-rpc");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    for (control, suffix) in [(false, "view"), (true, "control")] {
        let sink = Arc::new(CapturingFrameSink::default());
        let capabilities = if control {
            CapabilitySet::from([
                haider_rpc::Capability::View,
                haider_rpc::Capability::Control,
            ])
        } else {
            CapabilitySet::from([haider_rpc::Capability::View])
        };
        let connection = hub
            .open_connection(
                capabilities,
                sink.clone(),
                crate::accounts::ConnectionTransport::LocalSameUid,
            )
            .expect("connection");
        connection
            .request(
                RequestId::new("attach"),
                RequestBody::SessionAttach {
                    session_id: session.clone(),
                    after_seq: 0,
                    mode: if control {
                        AttachMode::Control
                    } else {
                        AttachMode::View
                    },
                    sealed_replay: false,
                },
            )
            .await
            .expect("attach");
        let attachment_id = sink
            .0
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match frame {
                WireFrame::Response {
                    body: ResponseBody::SessionAttach { attachment_id, .. },
                    ..
                } => Some(attachment_id.clone()),
                _ => None,
            })
            .expect("attachment response");
        connection
            .request(
                RequestId::new(suffix),
                RequestBody::SessionDetach {
                    attachment_id: attachment_id.clone(),
                    close_session: true,
                },
            )
            .await
            .expect("close request");
        let response = sink
            .0
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match frame {
                WireFrame::Response { request_id, body } if request_id.as_str() == suffix => {
                    Some(body.clone())
                }
                _ => None,
            })
            .expect("close response");
        if control {
            assert!(
                matches!(response, ResponseBody::SessionDetach { closed_session_id: Some(ref closed), .. } if *closed == session)
            );
            assert!(
                !lock(&hub.inner.actors)
                    .expect("actors")
                    .contains_key(&session)
            );
        } else {
            assert!(
                matches!(response, ResponseBody::Error { ref code, .. } if code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED)
            );
            hub.detach(&attachment_id).await.expect("detach view");
        }
        connection.close().await.expect("connection closes");
    }
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn native_close_releases_owned_registries_and_reopens_identical_history() {
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    for ordinal in 0..32 {
        let session = SessionId::new(format!("close-{ordinal}"));
        hub.create_internal_session(create_command(&session, session.as_str()))
            .await
            .expect("create");
        let before = store.read(&session, 0, 256).await.expect("history");
        let old_actor = hub.actor_for(session.clone()).await.expect("actor");
        hub.schedule_idle_derived_state_release(session.clone(), before.last().expect("event").seq);
        assert_eq!(
            close(&hub, attach(&hub, &session, AttachMode::Control).await)
                .await
                .expect("close"),
            session
        );
        assert!(old_actor.commands.is_closed());
        assert!(
            !lock(&hub.inner.actors)
                .expect("actors")
                .contains_key(&session)
        );
        assert!(
            !lock(&hub.inner.session_actor_tasks)
                .expect("joins")
                .contains_key(&session)
        );
        assert!(
            !lock(&hub.inner.idle_release_tasks)
                .expect("idle joins")
                .contains_key(&session)
        );
        assert_eq!(lock(&hub.inner.attachment_slots).expect("slots").total, 0);
        assert!(lock(&hub.inner.replay_tasks).expect("replays").is_empty());
        assert_eq!(
            store
                .read(&session, 0, 256)
                .await
                .expect("preserved history"),
            before
        );
        assert!(
            store
                .session_metadata(&session)
                .await
                .expect("metadata")
                .is_some()
        );
        let reopened = attach(&hub, &session, AttachMode::Control).await;
        assert_eq!(close(&hub, reopened).await.expect("reclose"), session);
    }
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn native_close_refuses_view_foreign_other_attached_and_active_sessions() {
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-refusals");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let view = attach(&hub, &session, AttachMode::View).await;
    assert!(close(&hub, view.clone()).await.is_err());
    let control = attach(&hub, &session, AttachMode::Control).await;
    assert_eq!(
        close(&hub, control.clone())
            .await
            .expect_err("other attachment")
            .code,
        ErrorCode::Busy
    );
    hub.detach(&view).await.expect("detach view");
    assert!(
        hub.close_attachment_session(
            control.clone(),
            "foreign".into(),
            Arc::new(CapturingFrameSink::default())
        )
        .await
        .is_err()
    );
    hub.accept_internal_turn(accept_command(
        &session,
        &RunId::new("active"),
        store.worker_generation(),
        "active",
    ))
    .await
    .expect("accept");
    assert_eq!(
        close(&hub, control.clone())
            .await
            .expect_err("active run")
            .code,
        ErrorCode::Busy
    );
    assert!(
        lock(&hub.inner.attachments)
            .expect("attachments")
            .contains_key(&control)
    );
    assert!(
        !hub.actor_for(session.clone())
            .await
            .expect("still live")
            .closing
            .load(Ordering::Acquire)
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn native_close_joins_detached_replays_and_refuses_late_replay_admission() {
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-detached-replay");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let RegisterResult::Registered(view) = hub
        .register("close-test", session.clone(), 0, AttachMode::View)
        .await
        .expect("view")
    else {
        panic!("view refused");
    };
    let view_id = view.attachment_id.clone();
    let (release, resume) = oneshot::channel();
    let task = tokio::spawn(async move {
        resume.await.expect("release detached replay");
        drop(view);
        ReplayCompletion::Complete
    });
    lock(&hub.inner.replay_tasks).expect("replays").insert(
        view_id.clone(),
        ReplayTask {
            sessions: HashSet::from([session.clone()]),
            task,
        },
    );
    hub.detach(&view_id).await.expect("detach view");
    let descendant_root = SessionId::new("descendant-root");
    let DescendantRegisterResult::Registered {
        attachment_id: descendant_id,
        ..
    } = hub
        .register_descendant_attachment("close-test", descendant_root.clone(), HashSet::new())
        .expect("descendant registration")
    else {
        panic!("descendant refused");
    };
    let (release_descendant, resume_descendant) = oneshot::channel();
    lock(&hub.inner.replay_tasks).expect("replays").insert(
        descendant_id.clone(),
        ReplayTask {
            sessions: HashSet::from([descendant_root]),
            task: tokio::spawn(async move {
                resume_descendant.await.expect("release descendant");
                ReplayCompletion::Complete
            }),
        },
    );
    assert!(
        hub.track_descendant_attachment_session(&descendant_id, session.clone())
            .expect("live cohort growth")
    );
    hub.detach_descendant(&descendant_id)
        .expect("detach descendant");
    let RegisterResult::Registered(control) = hub
        .register("close-test", session.clone(), 0, AttachMode::Control)
        .await
        .expect("control")
    else {
        panic!("control refused");
    };
    let id = control.attachment_id.clone();
    let closing_hub = hub.clone();
    let mut caller = tokio::spawn(async move { close(&closing_hub, id).await });
    // Observe transfer of the detached replay's join handle. An implementation
    // that only joins the caller's attachment completes early and fails here;
    // no wall-clock sleep or timing assumption controls the barrier.
    while lock(&hub.inner.replay_tasks)
        .expect("replays")
        .contains_key(&view_id)
    {
        tokio::select! {
            result = &mut caller => panic!("close skipped detached replay: {result:?}"),
            () = tokio::task::yield_now() => {},
        }
    }
    assert!(!caller.is_finished(), "replay still owns its registration");
    assert!(
        !lock(&hub.inner.replay_tasks)
            .expect("replays")
            .contains_key(&descendant_id),
        "close must join the detached descendant's final dynamic cohort"
    );
    release.send(()).expect("release replay");
    release_descendant.send(()).expect("release descendant");
    caller.await.expect("close task").expect("close");
    // Model a preempted attach handler resuming after its response was sent
    // and a native close already removed that attachment.
    hub.spawn_replay(control, 0, false, Arc::new(CapturingFrameSink::default()))
        .expect("already detached replay is a no-op");
    assert!(lock(&hub.inner.replay_tasks).expect("replays").is_empty());
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn cancelled_close_caller_does_not_cancel_the_owned_join_barrier() {
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-cancelled-caller");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let RegisterResult::Registered(registration) = hub
        .register("close-test", session.clone(), 0, AttachMode::Control)
        .await
        .expect("register")
    else {
        panic!("attachment refused");
    };
    let id = registration.attachment_id.clone();
    let mut cancel = lock(&hub.inner.attachments).expect("attachments")[&id]
        .cancel
        .subscribe();
    let (reached, waiting) = oneshot::channel();
    let (release, resume) = oneshot::channel();
    // A real owned replay task deliberately retains its registration until
    // released. The close acknowledgement MUST wait for this join.
    let replay = tokio::spawn(async move {
        cancel.changed().await.expect("close cancels replay");
        reached.send(()).expect("close observed");
        resume.await.expect("release replay");
        drop(registration);
        ReplayCompletion::Complete
    });
    lock(&hub.inner.replay_tasks).expect("replays").insert(
        id.clone(),
        ReplayTask {
            sessions: HashSet::from([session.clone()]),
            task: replay,
        },
    );
    let closing_hub = hub.clone();
    let mut caller = tokio::spawn(async move { close(&closing_hub, id).await });
    tokio::select! {
        reached = waiting => reached.expect("close reached replay join"),
        result = &mut caller => panic!("close returned before replay join: {result:?}"),
    }
    assert!(!caller.is_finished(), "ack cannot outrun replay ownership");
    assert!(matches!(hub.actor_for(session.clone()).await,
        Err(SessionHubError::Store(error)) if error.code == ErrorCode::Busy));
    assert!(
        !lock(&hub.inner.deleting_sessions)
            .expect("deletions")
            .contains(&session),
        "temporary eviction must never advertise permanent deletion"
    );
    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    release.send(()).expect("release join");
    // Join the same hub-owned operation a daemon shutdown would join.
    let tasks = std::mem::take(&mut *lock(&hub.inner.session_close_tasks).expect("close tasks"));
    for task in tasks {
        task.await.expect("owned close finishes");
    }
    assert!(
        !lock(&hub.inner.actors)
            .expect("actors")
            .contains_key(&session)
    );
    assert!(
        !lock(&hub.inner.closing_sessions)
            .expect("fences")
            .contains(&session)
    );
    assert!(
        store
            .session_metadata(&session)
            .await
            .expect("metadata")
            .is_some()
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[cfg(unix)]
#[tokio::test]
async fn native_close_joins_peer_socket_and_does_not_republish_until_reopened() {
    use tokio::io::AsyncReadExt as _;

    // Keep Unix endpoint paths below the sockaddr_un ceiling on macOS.
    let root = tempfile::Builder::new()
        .prefix("hclose-peer-")
        .tempdir_in("/tmp")
        .expect("short root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-peer");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let runtime = root.path().join("runtime");
    let _prepared = haider_platform::prepare_runtime_directory(&runtime).expect("runtime");
    let peer = crate::peer::PeerService::start(runtime.clone(), &hub)
        .await
        .expect("peer service");
    hub.install_peer_service(Arc::clone(&peer))
        .expect("install");
    assert_eq!(peer.list().await.expect("publish").len(), 1);
    let paths = haider_platform::peer_endpoint_paths(
        &runtime,
        session.as_str(),
        haider_platform::PeerEndpointKind::Haider,
    )
    .expect("peer paths");
    let mut stream = tokio::net::UnixStream::connect(&paths.socket)
        .await
        .expect("live peer socket");
    close(&hub, attach(&hub, &session, AttachMode::Control).await)
        .await
        .expect("close");
    let mut byte = [0];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
        .await
        .expect("closed peer cannot retain a handshake reader");
    assert!(matches!(read, Ok(0)) || read.is_err());
    assert!(!paths.socket.exists());
    assert!(!paths.manifest.exists());
    assert!(peer.list().await.expect("reconcile closed").is_empty());
    let reopened = attach(&hub, &session, AttachMode::Control).await;
    assert_eq!(peer.list().await.expect("republish reopened").len(), 1);
    close(&hub, reopened).await.expect("reclose");
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn native_close_preserves_dormant_hook_outbox_and_refuses_live_hook_ownership() {
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let profile = tempfile::tempdir().expect("hook profile");
    let (service, engine) = crate::hooks::HookEngine::start(
        profile.path().canonicalize().expect("canonical profile"),
        store.clone(),
        hub.clone(),
    )
    .await
    .expect("hook engine");
    hub.install_hooks(service).expect("install hooks");
    let session = SessionId::new("close-hook-outbox");
    let mut command = create_command(&session, session.as_str());
    command.cwd = root
        .path()
        .canonicalize()
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(command).await.expect("create");
    let attachment = attach(&hub, &session, AttachMode::Control).await;
    let activity = hub.try_session_activity(&session).expect("hook ownership");
    assert_eq!(
        close(&hub, attachment.clone())
            .await
            .expect_err("live hook")
            .code,
        ErrorCode::Busy
    );
    drop(activity);
    close(&hub, attachment)
        .await
        .expect("close with dormant hook history");
    assert!(
        store
            .has_pending_hook_dispatches(&session)
            .await
            .expect("outbox retained")
    );
    engine.shutdown().await;
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

fn close_timer_request(
    hub: &SessionHub,
    session: &SessionId,
) -> crate::monitor::MonitorClientRegistrationRequest {
    crate::monitor::MonitorClientRegistrationRequest {
        command_id: haider_rpc::CommandId::new("close-monitor-register"),
        session_id: session.clone(),
        worker_generation: hub.worker_generation(),
        source: haider_rpc::MonitorSourceWire::Timer {
            interval_ms: 60_000,
        },
        filter: None,
        action: haider_rpc::MonitorActionWire {
            report: true,
            follow_up: None,
        },
        occurrence: haider_rpc::MonitorOccurrenceWire::Every,
        lifetime: haider_rpc::MonitorLifetimeWire::Session,
    }
}

#[tokio::test]
async fn native_close_after_monitor_remove_joins_the_real_timer_runner() {
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-removed-monitor");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let attachment = attach(&hub, &session, AttachMode::Control).await;
    let monitors = hub.inner_monitor();
    let registered = monitors
        .client_register(&hub, close_timer_request(&hub, &session))
        .await;
    let haider_rpc::MonitorRegisterOutcomeWire::Registered { monitor } = registered.outcome else {
        panic!("registration refused: {registered:?}");
    };
    assert_eq!(
        close(&hub, attachment.clone())
            .await
            .expect_err("live timer has ownership")
            .code,
        ErrorCode::Busy
    );
    let removed = monitors
        .client_remove(
            &hub,
            haider_rpc::CommandId::new("close-monitor-remove"),
            session.clone(),
            hub.worker_generation(),
            monitor.monitor_id,
        )
        .await;
    assert!(
        matches!(
            removed.outcome,
            haider_rpc::MonitorRemoveOutcomeWire::Removed { .. }
        ),
        "{removed:?}"
    );
    assert!(!monitors.has_session_resources(&session));
    // Pause only after durable setup. Removal must cancel and join before the
    // timer's next scheduled tick; virtual time makes this a semantic bound,
    // independent of build-machine wall load. A missing runner cancellation
    // leaves the real60s timer alive and fails at the half-interval deadline.
    tokio::time::pause();
    tokio::time::timeout(
        Duration::from_millis(60_000 / 2),
        monitors.join_session_tasks(&session),
    )
    .await
    .expect("removed runner must stop before its next tick")
    .expect("actual monitor task joins");
    tokio::time::resume();
    let history = store.read(&session, 0, 256).await.expect("history");
    assert_eq!(close(&hub, attachment).await.expect("closed"), session);
    assert_eq!(
        store
            .read(&session, 0, 256)
            .await
            .expect("preserved history"),
        history
    );
    assert!(
        !lock(&hub.inner.actors)
            .expect("actors")
            .contains_key(&session)
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn native_close_refuses_monitor_registration_between_commit_and_publication() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-registering-monitor");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let attachment = attach(&hub, &session, AttachMode::Control).await;
    let monitors = hub.inner_monitor();
    let mut register =
        Box::pin(monitors.client_register(&hub, close_timer_request(&hub, &session)));
    loop {
        // The registration is manually driven on this current-thread runtime.
        // Its append runs in the actor while the client continuation stays
        // unpolled; inspect committed facts before polling that continuation.
        assert!(
            matches!(
                register
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ),
            "registration must reach its post-append suspension"
        );
        let history = store
            .read(&session, 0, 256)
            .await
            .expect("committed history");
        if history.iter().any(|envelope| {
            envelope
                .event_id
                .as_str()
                .starts_with("monitor-client-registered-")
        }) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        !monitors.has_session_resources(&session),
        "runner and projection are not published yet"
    );
    assert_eq!(
        close(&hub, attachment.clone())
            .await
            .expect_err("in-flight registration must refuse the witness")
            .code,
        ErrorCode::Busy
    );
    let registered = register.await;
    let haider_rpc::MonitorRegisterOutcomeWire::Registered { monitor } = registered.outcome else {
        panic!("registration refused: {registered:?}");
    };
    let removed = monitors
        .client_remove(
            &hub,
            haider_rpc::CommandId::new("close-monitor-remove"),
            session.clone(),
            hub.worker_generation(),
            monitor.monitor_id,
        )
        .await;
    assert!(
        matches!(
            removed.outcome,
            haider_rpc::MonitorRemoveOutcomeWire::Removed { .. }
        ),
        "{removed:?}"
    );
    monitors
        .join_session_tasks(&session)
        .await
        .expect("removed task joins");
    let history = store
        .read(&session, 0, 256)
        .await
        .expect("history before close");
    close(&hub, attachment).await.expect("settled close");
    assert_eq!(
        store
            .read(&session, 0, 256)
            .await
            .expect("history after witness"),
        history
    );
    assert!(
        !lock(&hub.inner.actors)
            .expect("actors")
            .contains_key(&session)
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[cfg(unix)]
#[tokio::test]
async fn native_close_waits_for_cancelled_process_owner_cleanup() {
    use crate::native_process::{NativeOwner, NativeProcess};
    use tokio::io::AsyncReadExt;
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-cancelled-native-owner");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let attachment = attach(&hub, &session, AttachMode::Control).await;
    let activity = hub.try_session_activity(&session).expect("lease");
    let owner = NativeOwner::new(&hub, &session, activity);
    let (release, gate) = oneshot::channel();
    let (ready, started) = oneshot::channel();
    let actor = tokio::spawn(NativeOwner::scope(Some(owner), async move {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "printf r; read line"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        haider_platform::configure_process_group(&mut command);
        let mut child = NativeProcess::register(command.spawn().expect("child")).expect("group");
        let pid = child.id();
        let mut byte = [0];
        child
            .take_stdout()
            .expect("stdout")
            .read_exact(&mut byte)
            .await
            .expect("native readiness");
        child.delay_cleanup(gate);
        ready.send(pid).expect("started");
        std::future::pending::<()>().await;
    }));
    let pid = started.await.expect("actual child readiness");
    actor.abort();
    assert!(actor.await.expect_err("actor cancelled").is_cancelled());
    assert_eq!(
        close(&hub, attachment.clone())
            .await
            .expect_err("cleanup still owns session")
            .code,
        ErrorCode::Busy
    );
    assert!(
        haider_platform::process_leader_exited(
            haider_platform::process_id(Some(pid)).expect("pid")
        )
        .is_ok(),
        "child identity remains owned even after actor cancellation"
    );
    release.send(()).expect("allow actual cleanup");
    hub.task_registry()
        .join_session(&session)
        .await
        .expect("real cleanup join");
    assert_eq!(
        close(&hub, attachment).await.expect("native witness"),
        session
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_dead_recovery_outcome_still_requires_native_absence() {
    use crate::tasks::TaskFacade;
    use haider_protocol::{ids::TaskId, task::TaskStarted};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-already-dead-recovery");
    let run = RunId::new("prior-already-dead-run");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "printf r; read line"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    haider_platform::configure_process_group(&mut command);
    let mut child = command.spawn().expect("fixture-owned child");
    let pid = i32::try_from(child.id().expect("pid")).expect("pid fits");
    let mut byte = [0];
    child
        .stdout
        .as_mut()
        .expect("stdout")
        .read_exact(&mut byte)
        .await
        .expect("ready");
    let started = TaskStarted {
        task: TaskId::new("already-dead-task"),
        name: "owned child".into(),
        command: "fixture-owned shell".into(),
        pid,
        started_at_ms: 1,
    };
    let mut facts = [
        crate::tasks::test_task_fact_envelope(
            &hub,
            &session,
            &run,
            "already-dead-start",
            started.to_payload_value().expect("payload"),
        ),
        crate::tasks::test_task_fact_envelope(
            &hub,
            &session,
            &run,
            "already-dead-terminal",
            serde_json::to_value(haider_protocol::EventPayload::RunState(
                haider_protocol::state::RunState::Done,
            ))
            .expect("terminal"),
        ),
    ];
    hub.append(&mut facts).await.expect("prior-life facts");
    let attachment = attach(&hub, &session, AttachMode::Control).await;
    // AlreadyDead also originates from a signal result. Model that outcome
    // with the existing reaper seam while the independent native probe sees
    // the real, blocked child. No foreign process or credential is needed.
    TaskFacade::new(hub.clone())
        .adopt_session_with_probe(&session, |_| haider_tools::PidLiveness::Dead)
        .await
        .expect("recovery result");
    let refused = close(&hub, attachment.clone()).await;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"release\n")
        .await
        .expect("release child");
    assert!(child.wait().await.expect("reap fixture").success());
    hub.task_registry()
        .join_session(&session)
        .await
        .expect("absence observer joined");
    assert_eq!(
        refused
            .expect_err("outcome alone cannot release ownership")
            .code,
        ErrorCode::Busy
    );
    assert_eq!(
        close(&hub, attachment)
            .await
            .expect("native absence witness"),
        session
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_direct_recovery_keeps_its_native_owner() {
    use crate::tasks::TaskFacade;
    use haider_protocol::{ids::TaskId, task::TaskStarted};
    use tokio::io::AsyncReadExt;
    let root = tempfile::tempdir().expect("root");
    let (store, hub) = open_retention_test_hub(root.path()).await.expect("hub");
    let session = SessionId::new("close-cancelled-orphan-recovery");
    let run = RunId::new("prior-cancelled-recovery-run");
    hub.create_internal_session(create_command(&session, session.as_str()))
        .await
        .expect("create");
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "trap '' TERM; printf r; read line"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    haider_platform::configure_process_group(&mut command);
    let mut child = command.spawn().expect("fixture-owned orphan");
    let pid = i32::try_from(child.id().expect("pid")).expect("pid fits");
    let mut byte = [0];
    child
        .stdout
        .as_mut()
        .expect("stdout")
        .read_exact(&mut byte)
        .await
        .expect("TERM-ignoring child ready");
    let started = TaskStarted {
        task: TaskId::new("cancelled-recovery-task"),
        name: "owned orphan".into(),
        command: "fixture-owned shell".into(),
        pid,
        started_at_ms: 1,
    };
    let mut facts = [
        crate::tasks::test_task_fact_envelope(
            &hub,
            &session,
            &run,
            "cancelled-recovery-start",
            started.to_payload_value().expect("payload"),
        ),
        crate::tasks::test_task_fact_envelope(
            &hub,
            &session,
            &run,
            "cancelled-recovery-terminal",
            serde_json::to_value(haider_protocol::EventPayload::RunState(
                haider_protocol::state::RunState::Done,
            ))
            .expect("terminal"),
        ),
    ];
    hub.append(&mut facts).await.expect("prior-life facts");
    let attachment = attach(&hub, &session, AttachMode::Control).await;
    let facade = TaskFacade::with_kill_grace(hub.clone(), std::time::Duration::from_millis(200));
    let (entered, at_probe) = oneshot::channel();
    let entered = std::sync::Mutex::new(Some(entered));
    let (release, gate) = std::sync::mpsc::channel();
    let gate = std::sync::Mutex::new(gate);
    let recovery_session = session.clone();
    let caller = tokio::spawn(async move {
        facade
            .adopt_session_with_probe(&recovery_session, move |pid| {
                entered
                    .lock()
                    .expect("entered")
                    .take()
                    .expect("single probe")
                    .send(())
                    .expect("probe notification");
                gate.lock().expect("gate").recv().expect("release");
                haider_tools::probe_group_liveness(pid)
            })
            .await
    });
    at_probe.await.expect("recovery reached actual orphan");
    caller.abort();
    // The caller can be joined while admitted recovery remains at its probe.
    // This deadline is only a failing watchdog; the successful witness is join.
    let joined = tokio::time::timeout(std::time::Duration::from_secs(2), caller).await;
    let refused = if joined.is_ok() {
        Some(close(&hub, attachment.clone()).await)
    } else {
        None
    };
    // Release the injected probe before any assertion, including on a failed
    // cancellation watchdog. Busy is checked while admission is explicitly
    // held, without racing the duration of the real TERM grace interval.
    release
        .send(())
        .expect("release recovery even on failed join");
    assert!(
        joined
            .expect("caller cancellation must not own recovery")
            .expect_err("caller cancelled")
            .is_cancelled()
    );
    assert_eq!(hub.task_registry().running_count(&session), 0);
    assert_eq!(
        refused
            .expect("close attempted")
            .expect_err("cancelled caller left recovery ownership intact")
            .code,
        ErrorCode::Busy
    );
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("owned recovery sends KILL")
        .expect("reap owned fixture");
    assert!(!status.success());
    hub.task_registry()
        .join_session(&session)
        .await
        .expect("adoption joined");
    // A recovery owner can register its final read-only exit observer while
    // the first cohort is being joined. Drain that cohort before the witness.
    hub.task_registry()
        .join_session(&session)
        .await
        .expect("exit observer joined");
    assert_eq!(
        close(&hub, attachment)
            .await
            .expect("closed after real exit"),
        session
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}
