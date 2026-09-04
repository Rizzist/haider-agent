#![allow(clippy::expect_used)]

//! Cross-platform, no-bind connection regressions.
//!
//! `tokio::io::duplex` reaches the production framing, writer, hub RPC, and
//! worker handoff after the transport-specific peer gate. This keeps Windows
//! cancellation failures reproducible in sandboxes that cannot bind a named
//! pipe or Unix socket.

use super::*;
use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::{RunState, SessionState};
use haider_rpc::{
    AttachMode, CancelStatus, Capability, CapabilitySet, ClientKind, CommandId, Hello, RequestBody,
    RequestId, ResponseBody, WIRE_PROTOCOL_VERSION,
};
use haider_store::SessionCreateCommand;
use std::collections::VecDeque;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

const FRAME_LIMIT: usize = 1024 * 1024;

#[cfg(unix)]
const CANCELLABLE_COMMAND: &str = "printf started; sleep 30";
#[cfg(windows)]
const CANCELLABLE_COMMAND: &str =
    "[Console]::Out.Write('started');[Console]::Out.Flush();while($true){Start-Sleep -Seconds 1}";

struct DuplexClient {
    stream: DuplexStream,
    decoder: uds_codec::Decoder,
    pending: VecDeque<WireFrame>,
    ping_nonce: u64,
}

impl DuplexClient {
    fn new(stream: DuplexStream) -> Self {
        Self {
            stream,
            decoder: uds_codec::Decoder::new(FRAME_LIMIT),
            pending: VecDeque::new(),
            ping_nonce: 0,
        }
    }

    async fn send(&mut self, frame: &WireFrame) {
        let bytes = uds_codec::encode(frame, FRAME_LIMIT).expect("duplex frame encodes");
        self.stream
            .write_all(&bytes)
            .await
            .expect("duplex frame writes");
    }

    async fn request(&mut self, request: &str, body: RequestBody) {
        self.send(&WireFrame::Request {
            request_id: RequestId::new(request),
            body,
        })
        .await;
    }

    /// Production clients ping while waiting. Preserve that contract here so
    /// a deliberately slow Windows process startup cannot turn the R9
    /// read-idle law into a misleading cancellation EOF.
    async fn next(&mut self) -> Option<WireFrame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        loop {
            let mut buffer = [0_u8; 4096];
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.stream.read(&mut buffer),
            )
            .await
            {
                Ok(Ok(0)) => return None,
                Ok(Ok(read)) => {
                    let batch = self.decoder.push(&buffer[..read]);
                    assert!(batch.error.is_none(), "server sent an invalid duplex frame");
                    self.pending.extend(batch.frames);
                    if let Some(frame) = self.pending.pop_front() {
                        return Some(frame);
                    }
                }
                Ok(Err(error)) => panic!("duplex frame read: {error}"),
                Err(_) => {
                    self.ping_nonce = self.ping_nonce.saturating_add(1);
                    self.send(&WireFrame::Ping {
                        nonce: self.ping_nonce,
                    })
                    .await;
                }
            }
        }
    }

    async fn handshake(&mut self) {
        self.send(&WireFrame::Hello(Hello {
            protocol_min: WIRE_PROTOCOL_VERSION,
            protocol_max: WIRE_PROTOCOL_VERSION,
            client_name: "duplex-cancellation-test".into(),
            client_version: "test".into(),
            client_instance_id: "duplex-cancellation-client".into(),
            client_kind: ClientKind::Headless,
            capabilities_requested: CapabilitySet::from([Capability::View, Capability::Control]),
            max_receive_frame: u32::try_from(FRAME_LIMIT).expect("frame limit fits"),
            encodings: Vec::new(),
        }))
        .await;
        assert!(matches!(self.next().await, Some(WireFrame::Welcome(_))));
    }
}

fn connection_context(
    hub: crate::session_hub::SessionHub,
    writers: WriterRegistry,
) -> ConnectionContext {
    ConnectionContext {
        profile_id: "duplex-cancellation-profile".into(),
        instance_id: "duplex-cancellation-instance".into(),
        daemon_generation: 1,
        frame_limit: FRAME_LIMIT,
        outbound_queue_capacity: 64,
        outbound_queued_bytes: 4 * FRAME_LIMIT,
        max_connections: 1,
        handshake_timeout: std::time::Duration::from_secs(10),
        writers,
        owner_uid: u32::MAX,
        hub,
        shutdown: crate::lifecycle::ShutdownHandle::channel().0,
        endpoint_path: std::path::PathBuf::from("duplex://cancellation"),
        pid_file_path: std::path::PathBuf::from("duplex://haiderd.pid"),
        idle_ttl_ms: None,
        warm: false,
        readiness: crate::lifecycle::ready_for_tests(),
    }
}

fn create_command(session_id: &SessionId, workspace: &std::path::Path) -> SessionCreateCommand {
    SessionCreateCommand {
        command_id: "duplex-create".into(),
        request_digest: "duplex-create-digest".into(),
        request_json: r#"{"fixture":"duplex-cancellation"}"#.into(),
        session_id: session_id.clone(),
        cwd: workspace.to_string_lossy().into_owned(),
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4_096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system-v1".into(),
        event_id: EventId::new("duplex-created"),
        device_id: DeviceId::new("duplex-cancellation-test"),
    }
}

/// A direct shell cancellation must answer the RPC, close the synthetic run,
/// and leave the same connection usable. This pins the exact path implicated
/// by the Windows EOF symptom without a platform socket or external daemon.
#[tokio::test]
async fn shell_cancel_settles_and_keeps_the_duplex_connection_open() {
    let root = tempfile::tempdir().expect("duplex profile");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("duplex workspace");
    let store = haider_core::SqliteStoreHandle::open(root.path().join("store"))
        .await
        .expect("duplex store opens");
    let hub = crate::session_hub::SessionHub::new(
        store.clone(),
        crate::session_hub::SessionHubConfig::default(),
    )
    .expect("duplex hub opens");
    let session_id = SessionId::new("duplex-cancellation-session");
    let created = hub
        .create_internal_session(create_command(&session_id, &workspace))
        .await
        .expect("duplex session commits");
    let manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("duplex manager installs");

    let (server, client) = tokio::io::duplex(4 * FRAME_LIMIT);
    let (server_reader, server_writer) = tokio::io::split(server);
    let (writers, mut registered_writers) = mpsc::unbounded_channel();
    let writer_registry = tokio::spawn(async move {
        while let Some(writer) = registered_writers.recv().await {
            let _ = writer.await;
        }
    });
    let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
    let serve_task = tokio::spawn(serve_io(
        server_reader,
        server_writer,
        connection_context(hub.clone(), writers),
        drain_rx,
    ));
    let mut client = DuplexClient::new(client);
    tokio::time::timeout(std::time::Duration::from_secs(10), client.handshake())
        .await
        .expect("duplex handshake completes");
    client
        .request(
            "duplex-attach",
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut attached = false;
        let mut caught_up = false;
        while !(attached && caught_up) {
            match client.next().await.expect("duplex attach stays open") {
                WireFrame::Response {
                    body: ResponseBody::SessionAttach { .. },
                    ..
                } => attached = true,
                WireFrame::AttachCaughtUp { .. } => caught_up = true,
                _ => {}
            }
        }
    })
    .await
    .expect("duplex attach completes");

    client
        .request(
            "duplex-shell-start",
            RequestBody::ShellExec {
                command_id: CommandId::new("duplex-shell-command"),
                session_id: session_id.clone(),
                worker_generation: created.worker_generation,
                command: CANCELLABLE_COMMAND.into(),
                cwd: None,
            },
        )
        .await;
    let (run_id, item_id) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        async {
            let mut accepted = None;
            let mut started = false;
            loop {
                match client.next().await.expect("shell start keeps connection open") {
                    WireFrame::Response {
                        body:
                            ResponseBody::ShellExec {
                                run_id: Some(run_id),
                                item_id,
                                ..
                            },
                        ..
                    } => accepted = Some((run_id, item_id)),
                    WireFrame::Event { envelope, .. }
                        if envelope.payload.decode_event().is_ok_and(
                            |payload| {
                                matches!(
                                    payload,
                                    EventPayload::Item(ItemEvent::Delta {
                                        delta: ItemDelta::CommandOutput { chunk_b64, .. },
                                        ..
                                    }) if base64::engine::general_purpose::STANDARD
                                        .decode(chunk_b64.as_bytes())
                                        .is_ok_and(|bytes| bytes.windows(7).any(|window| window == b"started"))
                                )
                            },
                        ) => started = true,
                    _ => {}
                }
                if started && let Some(accepted) = accepted {
                    break accepted;
                }
            }
        },
    )
    .await
    .expect("shell process starts within the Windows-honest bound");

    client
        .request(
            "duplex-shell-cancel",
            RequestBody::TurnCancel {
                command_id: CommandId::new("duplex-shell-cancel-command"),
                session_id: session_id.clone(),
                worker_generation: created.worker_generation,
                run_id: run_id.clone(),
            },
        )
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut response = false;
        let mut completed = false;
        let mut terminal = false;
        let mut idle = false;
        while !(response && completed && terminal && idle) {
            match client
                .next()
                .await
                .expect("cancellation keeps connection open")
            {
                WireFrame::Response {
                    body:
                        ResponseBody::TurnCancel {
                            run_id: cancelled,
                            status: CancelStatus::Accepted,
                            ..
                        },
                    ..
                } if cancelled == run_id => response = true,
                WireFrame::Event { envelope, .. } => {
                    let Ok(payload) = envelope.payload.decode_event() else {
                        continue;
                    };
                    completed |= matches!(
                        &payload,
                        EventPayload::Item(ItemEvent::Completed {
                            item_id: completed,
                            item: TurnItem::CommandExecution {
                                status: ToolStatus::Cancelled,
                                ..
                            },
                        }) if completed == &item_id
                    );
                    terminal |= envelope.run_id.as_ref() == Some(&run_id)
                        && payload == EventPayload::RunState(RunState::Cancelled);
                    idle |= payload
                        == EventPayload::SessionState(SessionState::Idle { interrupted: true });
                }
                _ => {}
            }
        }
    })
    .await
    .expect("shell cancellation settles within the process-group bound");

    client.send(&WireFrame::Ping { nonce: 99 }).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if matches!(client.next().await, Some(WireFrame::Pong { nonce: 99 })) {
                break;
            }
        }
    })
    .await
    .expect("post-cancel connection answers Ping");

    drop(client);
    let _ = serve_task.await.expect("duplex serve task joins");
    writer_registry.await.expect("duplex writer registry joins");
    manager.shutdown().await.expect("duplex manager shuts down");
    hub.shutdown().await.expect("duplex hub shuts down");
    store.close().await.expect("duplex store closes");
}
