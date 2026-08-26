#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use haider_protocol::mobile::Point;

#[tokio::test]
async fn frame_codec_round_trips_big_endian_json_without_a_socket() {
    let expected = Envelope {
        id: 42,
        body: json!({"type": "ack", "ok": true}),
    };
    let encoded = encode_frame(&expected).expect("encode frame");
    assert_eq!(
        u32::from_be_bytes(encoded[..4].try_into().expect("prefix")) as usize,
        encoded.len() - 4,
    );
    let (mut writer, mut reader) = tokio::io::duplex(1024);
    writer.write_all(&encoded).await.expect("write duplex");
    assert_eq!(read_frame(&mut reader).await.expect("read frame"), expected);
}

#[tokio::test]
async fn frame_codec_rejects_oversized_length_before_allocating_payload() {
    let (mut writer, mut reader) = tokio::io::duplex(16);
    writer
        .write_all(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes())
        .await
        .expect("write prefix");
    let error = read_frame(&mut reader).await.expect_err("oversized frame");
    assert!(error.to_string().contains("8 MiB"));
}

#[test]
fn token_comparison_accepts_only_the_complete_exact_token() {
    let token = b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
    assert!(constant_time_token_eq(token, token));
    assert!(!constant_time_token_eq(
        token,
        b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFH"
    ));
    assert!(!constant_time_token_eq(token, &token[..token.len() - 1]));
    let mut longer = token.to_vec();
    longer.push(b'x');
    assert!(!constant_time_token_eq(token, &longer));
}

#[test]
fn hello_validation_rejects_wrong_tokens_and_wrong_ids() {
    let hello = Envelope {
        id: 1,
        body: json!({"type": "hello", "token": "wrong", "apkVersion": "test"}),
    };
    assert_eq!(validate_hello(&hello, b"right"), Err("invalid token"));
    let wrong_id = Envelope {
        id: 2,
        body: json!({"type": "hello", "token": "right", "apkVersion": "test"}),
    };
    assert_eq!(validate_hello(&wrong_id, b"right"), Err("invalid hello"));
}

#[test]
fn token_file_is_written_without_a_secret_copy_in_the_error_path() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = write_mobile_token(directory.path(), "token-sentinel").expect("write token");
    assert_eq!(
        std::fs::read_to_string(path).expect("read token"),
        "token-sentinel"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(directory.path().join(TOKEN_FILE_NAME))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn apk_accessibility_and_sms_shapes_translate_to_protocol_types() {
    let nodes = translate_a11y_tree(json!({
        "type": "a11yTree",
        "nodes": [{
            "text": "Send",
            "contentDesc": "Send message",
            "className": "android.widget.Button",
            "resourceId": "com.example:id/send",
            "bounds": [10, 20, 30, 40],
            "clickable": true
        }]
    }))
    .expect("a11y tree");
    assert_eq!(nodes[0].id, "com.example:id/send#0");
    assert_eq!(nodes[0].bounds.right, 30);

    let messages = translate_sms_list(json!({
        "type": "smsList",
        "messages": [{"address": "+1555", "body": "hello", "ts": 123, "read": true}]
    }))
    .expect("sms list");
    assert_eq!(messages[0].id, "apk-sms-123-0");
    assert_eq!(messages[0].folder, "inbox");
}

#[tokio::test]
async fn apk_pushes_reach_their_typed_routes() {
    let state = TransportState::new();
    let (commands, _receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
    let (monitor_streams, mut monitor_stream_receiver) =
        mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
    let (close, _close_receiver) = watch::channel(false);
    state
        .install_connection(Arc::new(ConnectionHandle {
            id: 9,
            commands,
            monitor_streams,
            close,
        }))
        .await;
    let monitor_sources = MonitorSourceHub::new();
    let mut monitor_sms = monitor_sources.subscribe(haider_tools::MonitorSourceKind::Sms);
    state.install_monitor_source_hub(monitor_sources);
    let mut incoming_sms = state.incoming_sms.subscribe();
    route_push(
        &state,
        9,
        json!({
            "type": "sms.incoming",
            "address": "+1555",
            "body": "new message",
            "ts": 456
        }),
    )
    .await
    .expect("route SMS push");
    assert_eq!(
        incoming_sms.recv().await.expect("incoming SMS"),
        IncomingSmsPush {
            _push_type: "sms.incoming".into(),
            address: "+1555".into(),
            body: "new message".into(),
            ts: 456,
        }
    );
    let monitor_event = monitor_sms.recv().await.expect("monitor SMS source event");
    assert!(matches!(
        monitor_event.payload,
        crate::MonitorEventPayload::Sms(crate::SmsIncomingEvent {
            address,
            body,
            received_at_ms: 456,
        }) if address == "+1555" && body == "new message"
    ));
    let monitor_chat_id = state
        .send_monitor_chat("monitor report".into())
        .await
        .expect("queue monitor chat stream");
    assert!(monitor_chat_id < 0);
    assert_eq!(
        monitor_stream_receiver
            .recv()
            .await
            .expect("monitor chat stream"),
        MonitorChatStream {
            id: monitor_chat_id,
            text: "monitor report".into(),
        }
    );
    route_push(
        &state,
        9,
        json!({"type": "capabilities.changed", "granted": ["smsRead", "accessibility", "smsRead"]}),
    )
    .await
    .expect("route capability push");
    assert_eq!(
        read_lock(&state.capabilities).as_slice(),
        ["accessibility", "smsRead"]
    );
    assert!(state.capabilities_seen.load(Ordering::Acquire));
    assert_eq!(mutex_lock(&state.recent_sms).len(), 1);
}

#[tokio::test]
async fn last_authenticated_connection_closes_prior_and_resets_push_state() {
    let state = TransportState::new();
    let (first_commands, _first_receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
    let (first_monitor_streams, _first_monitor_stream_receiver) =
        mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
    let (first_close, first_close_receiver) = watch::channel(false);
    state
        .install_connection(Arc::new(ConnectionHandle {
            id: 1,
            commands: first_commands,
            monitor_streams: first_monitor_streams,
            close: first_close,
        }))
        .await;
    state.set_capabilities(vec!["smsRead".into()]);
    state.record_incoming_sms(IncomingSmsPush {
        _push_type: "sms.incoming".into(),
        address: "+1555".into(),
        body: "old".into(),
        ts: 1,
    });

    let (second_commands, _second_receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
    let (second_monitor_streams, _second_monitor_stream_receiver) =
        mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
    let (second_close, _second_close_receiver) = watch::channel(false);
    state
        .install_connection(Arc::new(ConnectionHandle {
            id: 2,
            commands: second_commands,
            monitor_streams: second_monitor_streams,
            close: second_close,
        }))
        .await;
    assert!(*first_close_receiver.borrow());
    assert!(state.is_current(2));
    assert!(!state.capabilities_seen.load(Ordering::Acquire));
    assert!(read_lock(&state.capabilities).is_empty());
    assert!(mutex_lock(&state.recent_sms).is_empty());
}

async fn connected_backend() -> (
    ApkMobileBackend,
    mpsc::Receiver<ActorCommand>,
    watch::Receiver<bool>,
) {
    let state = Arc::new(TransportState::new());
    let (commands, receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
    let (monitor_streams, _monitor_stream_receiver) = mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
    let (close, close_receiver) = watch::channel(false);
    state
        .install_connection(Arc::new(ConnectionHandle {
            id: 7,
            commands,
            monitor_streams,
            close,
        }))
        .await;
    state.set_capabilities(vec![
        "accessibility".into(),
        "screenCapture".into(),
        "smsRead".into(),
    ]);
    (ApkMobileBackend { state }, receiver, close_receiver)
}

#[tokio::test]
async fn backend_translates_screen_capture_without_a_socket() {
    let (backend, mut commands, _close) = connected_backend().await;
    let responder = tokio::spawn(async move {
        let ActorCommand::Request { body, reply, .. } =
            commands.recv().await.expect("capability request");
        assert_eq!(body, json!({"type": "screen.capture"}));
        reply
            .send(Ok(json!({
                "type": "png",
                "base64": STANDARD.encode(b"png-bytes")
            })))
            .expect("reply receiver");
    });
    let output = backend
        .execute(&MobileAction::Screenshot {}, &MobileCancelToken::new())
        .await
        .expect("screenshot output");
    assert_eq!(output, MobileOutput::Screenshot(b"png-bytes".to_vec()));
    responder.await.expect("responder");
}

#[tokio::test]
async fn backend_preserves_control_effect_on_swipe_request() {
    let (backend, mut commands, _close) = connected_backend().await;
    let responder = tokio::spawn(async move {
        let ActorCommand::Request { body, reply, .. } =
            commands.recv().await.expect("capability request");
        assert_eq!(body["type"], "a11y.swipe");
        assert_eq!(body["x1"], 1);
        assert_eq!(body["y2"], 4);
        assert_eq!(body["ms"], DEFAULT_SWIPE_MS);
        assert_eq!(body["control"], true);
        reply
            .send(Ok(json!({"type": "ack", "ok": true})))
            .expect("reply receiver");
    });
    let output = backend
        .execute(
            &MobileAction::Swipe {
                from: Point { x: 1, y: 2 },
                to: Point { x: 3, y: 4 },
            },
            &MobileCancelToken::new(),
        )
        .await
        .expect("swipe output");
    assert_eq!(output, MobileOutput::Ack);
    responder.await.expect("responder");
}

#[tokio::test]
async fn backend_emits_remaining_apk_request_shapes_exactly() {
    let (backend, mut commands, _close) = connected_backend().await;
    let responder = tokio::spawn(async move {
        let cases = [
            (
                json!({"type": "a11y.snapshot"}),
                json!({"type": "a11yTree", "nodes": []}),
            ),
            (
                json!({"type": "a11y.tap", "x": 10, "y": 20, "control": true}),
                json!({"type": "ack", "ok": true}),
            ),
            (
                json!({"type": "a11y.text", "text": "hello", "control": true}),
                json!({"type": "ack", "ok": true}),
            ),
            (
                json!({"type": "app.open", "pkg": "com.example", "control": true}),
                json!({"type": "ack", "ok": true}),
            ),
            (
                json!({"type": "sms.list", "sinceMs": 1000, "limit": 5}),
                json!({"type": "smsList", "messages": []}),
            ),
        ];
        for (expected, response) in cases {
            let ActorCommand::Request { body, reply, .. } =
                commands.recv().await.expect("capability request");
            assert_eq!(body, expected);
            reply.send(Ok(response)).expect("reply receiver");
        }
    });
    let cancel = MobileCancelToken::new();
    assert_eq!(
        backend
            .execute(&MobileAction::A11yTree {}, &cancel)
            .await
            .expect("snapshot"),
        MobileOutput::A11yTree(Vec::new())
    );
    for action in [
        MobileAction::Tap {
            element_id: None,
            x: Some(10),
            y: Some(20),
        },
        MobileAction::Type {
            text: "hello".into(),
        },
        MobileAction::OpenApp {
            package: Some("com.example".into()),
            name: None,
        },
    ] {
        assert_eq!(
            backend
                .execute(&action, &cancel)
                .await
                .expect("control ack"),
            MobileOutput::Ack
        );
    }
    backend.state.record_incoming_sms(IncomingSmsPush {
        _push_type: "sms.incoming".into(),
        address: "+1555".into(),
        body: "just arrived".into(),
        ts: 1500,
    });
    let sms = backend
        .execute(
            &MobileAction::SmsRead {
                folder: Some("inbox".into()),
                since: Some("1000".into()),
                limit: Some(5),
            },
            &cancel,
        )
        .await
        .expect("SMS list");
    assert!(
        matches!(sms, MobileOutput::SmsList(messages) if messages.len() == 1 && messages[0].body == "just arrived")
    );
    responder.await.expect("responder");
}

#[tokio::test]
async fn known_missing_apk_grant_is_a_typed_prepare_failure() {
    let (backend, _commands, _close) = connected_backend().await;
    backend
        .state
        .capabilities_seen
        .store(false, Ordering::Release);
    assert!(matches!(
        backend
            .prepare(&MobileAction::A11yTree {}, &MobileCancelToken::new())
            .await,
        Err(MobileError::Unavailable { .. })
    ));
    backend.state.set_capabilities(vec!["accessibility".into()]);
    assert!(
        backend
            .prepare(&MobileAction::A11yTree {}, &MobileCancelToken::new())
            .await
            .is_ok()
    );
    let error = backend
        .prepare(&MobileAction::Screenshot {}, &MobileCancelToken::new())
        .await
        .expect_err("missing screen grant");
    assert!(matches!(error, MobileError::Unavailable { .. }));
}

#[tokio::test]
async fn false_ack_is_a_typed_backend_failure() {
    let (backend, mut commands, _close) = connected_backend().await;
    let responder = tokio::spawn(async move {
        let ActorCommand::Request { reply, .. } =
            commands.recv().await.expect("capability request");
        reply
            .send(Ok(json!({"type": "ack", "ok": false})))
            .expect("reply receiver");
    });
    let error = backend
        .execute(
            &MobileAction::Type {
                text: "hello".into(),
            },
            &MobileCancelToken::new(),
        )
        .await
        .expect_err("false ack");
    assert!(matches!(error, MobileError::Backend { .. }));
    responder.await.expect("responder");
}

#[tokio::test]
async fn disconnected_backend_is_a_typed_unavailable_failure() {
    let backend = ApkMobileBackend {
        state: Arc::new(TransportState::new()),
    };
    let error = backend
        .execute(&MobileAction::Screenshot {}, &MobileCancelToken::new())
        .await
        .expect_err("disconnected backend");
    assert!(matches!(error, MobileError::Unavailable { .. }));
}

#[tokio::test(start_paused = true)]
async fn backend_timeout_is_a_typed_failure() {
    let (backend, _commands, _close) = connected_backend().await;
    let error = backend
        .execute(&MobileAction::Screenshot {}, &MobileCancelToken::new())
        .await
        .expect_err("request timeout");
    assert!(matches!(error, MobileError::Backend { message } if message.contains("timed out")));
}

struct StreamingTestBridge;

#[async_trait]
impl MobileChatBridge for StreamingTestBridge {
    async fn handle(
        &self,
        command: ChatCommand,
        responder: ChatResponder,
    ) -> Result<(), MobileChatError> {
        assert!(matches!(command, ChatCommand::Send { text } if text == "hello"));
        responder
            .send(ChatEvent::Delta {
                text: "checking".into(),
                segment: "thinking",
            })
            .await?;
        responder
            .send(ChatEvent::Delta {
                text: "world".into(),
                segment: "answer",
            })
            .await?;
        responder.send(ChatEvent::Done).await
    }
}

#[tokio::test]
async fn chat_send_streams_same_id_delta_and_done_shapes() {
    let state = Arc::new(TransportState::new());
    state.install_chat_bridge(Arc::new(StreamingTestBridge));
    let (output, mut frames) = mpsc::channel(CHAT_OUTPUT_CAPACITY);
    let (commands, command_receiver) = mpsc::channel(CHAT_COMMAND_CAPACITY);
    let (stop, stop_receiver) = watch::channel(false);
    let worker = tokio::spawn(run_bridge_commands(command_receiver, stop_receiver));
    dispatch_bridge_frame(
        Envelope {
            id: 77,
            body: json!({"type": "chat.send", "text": "hello"}),
        },
        &state,
        &output,
        &commands,
    )
    .await
    .expect("dispatch chat request");
    assert_eq!(
        frames.recv().await.expect("thinking delta"),
        Envelope {
            id: 77,
            body: json!({
                "type": "chat.delta",
                "text": "checking",
                "segment": "thinking",
            }),
        }
    );
    assert_eq!(
        frames.recv().await.expect("answer delta"),
        Envelope {
            id: 77,
            body: json!({
                "type": "chat.delta",
                "text": "world",
                "segment": "answer",
            }),
        }
    );
    assert_eq!(
        frames.recv().await.expect("done"),
        Envelope {
            id: 77,
            body: json!({"type": "chat.done"}),
        }
    );
    stop.send_replace(true);
    worker.await.expect("stop chat worker");
}

#[tokio::test]
async fn monitor_chat_stream_uses_negative_id_delta_then_done() {
    let (mut daemon, mut apk) = tokio::io::duplex(8 * 1024);
    write_monitor_chat_stream(
        &mut daemon,
        MonitorChatStream {
            id: -9,
            text: "SMS from +1555:\nship it".into(),
        },
    )
    .await
    .expect("write monitor chat stream");
    assert_eq!(
        read_frame(&mut apk).await.expect("monitor delta"),
        Envelope {
            id: -9,
            body: json!({
                "type": "chat.delta",
                "text": "SMS from +1555:\nship it",
                "segment": "answer",
            }),
        }
    );
    assert_eq!(
        read_frame(&mut apk).await.expect("monitor done"),
        Envelope {
            id: -9,
            body: json!({"type": "chat.done"}),
        }
    );
}

struct OrderedTestBridge;

#[async_trait]
impl MobileChatBridge for OrderedTestBridge {
    async fn handle(
        &self,
        command: ChatCommand,
        responder: ChatResponder,
    ) -> Result<(), MobileChatError> {
        let ChatCommand::Send { text } = command else {
            return Err(MobileChatError::invalid("expected chat.send"));
        };
        if text == "first" {
            tokio::task::yield_now().await;
        }
        responder.send(ChatEvent::Done).await
    }
}

#[tokio::test]
async fn bridge_worker_preserves_socket_request_order() {
    let state = Arc::new(TransportState::new());
    state.install_chat_bridge(Arc::new(OrderedTestBridge));
    let (output, mut frames) = mpsc::channel(CHAT_OUTPUT_CAPACITY);
    let (commands, command_receiver) = mpsc::channel(CHAT_COMMAND_CAPACITY);
    let (stop, stop_receiver) = watch::channel(false);
    let worker = tokio::spawn(run_bridge_commands(command_receiver, stop_receiver));
    for (id, text) in [(41, "first"), (42, "second")] {
        dispatch_bridge_frame(
            Envelope {
                id,
                body: json!({"type": "chat.send", "text": text}),
            },
            &state,
            &output,
            &commands,
        )
        .await
        .expect("enqueue ordered chat request");
    }
    assert_eq!(frames.recv().await.expect("first terminal").id, 41);
    assert_eq!(frames.recv().await.expect("second terminal").id, 42);
    stop.send_replace(true);
    worker.await.expect("stop ordered chat worker");
}

#[tokio::test]
#[ignore = "requires a host that permits numeric loopback socket binds"]
async fn host_loopback_gate_authenticates_and_advertises_capabilities() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut server = MobileTransportServer::start(directory.path(), None)
        .await
        .expect("start loopback server");
    let token = std::fs::read_to_string(directory.path().join(TOKEN_FILE_NAME))
        .expect("read bootstrap token");
    let mut rejected = TcpStream::connect(server.address())
        .await
        .expect("connect rejected client");
    write_frame(
        &mut rejected,
        &Envelope {
            id: 1,
            body: json!({"type": "hello", "token": "wrong", "apkVersion": "test"}),
        },
    )
    .await
    .expect("write rejected hello");
    let rejection = read_frame(&mut rejected)
        .await
        .expect("read auth rejection");
    assert_eq!(rejection.id, 1);
    assert_eq!(rejection.body["type"], "authReject");
    let mut closed = [0_u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), rejected.read(&mut closed))
            .await
            .expect("auth rejection close timeout")
            .expect("read auth rejection close"),
        0
    );

    let mut stream = TcpStream::connect(server.address())
        .await
        .expect("connect loopback client");
    write_frame(
        &mut stream,
        &Envelope {
            id: 1,
            body: json!({"type": "hello", "token": token, "apkVersion": "test"}),
        },
    )
    .await
    .expect("write hello");
    let response = read_frame(&mut stream).await.expect("read auth response");
    assert_eq!(response.id, 1);
    assert_eq!(response.body["type"], "authOk");
    assert!(response.body["capabilities"].is_array());
    server.shutdown().await;
}
