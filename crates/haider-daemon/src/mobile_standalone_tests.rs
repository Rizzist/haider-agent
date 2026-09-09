#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use std::os::unix::fs::PermissionsExt;

async fn connect(path: &std::path::Path) -> tokio::net::UnixStream {
    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    write_frame(
        &mut stream,
        &Envelope {
            id: 1,
            body: json!({"type":"hello", "apkVersion":"971-test"}),
        },
    )
    .await
    .unwrap();
    let hello = read_frame(&mut stream).await.unwrap();
    assert_eq!(
        hello,
        Envelope {
            id: 1,
            body: json!({"type":"authOk", "capabilities":SERVER_CAPABILITIES})
        }
    );
    stream
}

#[tokio::test]
async fn mobile_standalone_uds_hello_reconnect_push_request_and_cleanup() {
    let root = tempfile::Builder::new()
        .prefix("m")
        .tempdir_in("/tmp")
        .unwrap();
    // The endpoint helper owns both the runtime directory and its containing
    // root; the shared system temporary directory is not an owner-private root.
    let root_path = root.path().canonicalize().unwrap().join("runtime");
    let state = Arc::new(TransportState::new());
    let mut server = MobileTransportServer::start_standalone(&root_path, Arc::clone(&state))
        .await
        .unwrap();
    let path = root_path.join("mobile.sock");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut first = connect(&path).await;
    write_frame(
        &mut first,
        &Envelope {
            id: 0,
            body: json!({"type":"capabilities.changed", "granted":["accessibility"]}),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !state.capabilities_seen.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut second = connect(&path).await;
    assert!(!state.capabilities_seen.load(Ordering::Acquire));
    assert!(read_lock(&state.capabilities).is_empty());
    assert!(mutex_lock(&state.recent_sms).entries.is_empty());
    let mut byte = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), first.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let connection = read_lock(&state.connection).clone().unwrap();
    // A capability push may be split across a concurrently dispatched request.
    // Cancelling read_frame itself in select would lose this two-byte prefix.
    let fragmented = encode_frame(&Envelope {
        id: 0,
        body: json!({"type":"capabilities.changed", "granted":["accessibility"]}),
    })
    .unwrap();
    second.write_all(&fragmented[..2]).await.unwrap();
    let (reply, result) = oneshot::channel();
    connection
        .commands
        .send(ActorCommand::Request {
            body: json!({"type":"a11y.snapshot"}),
            reply,
            deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        })
        .await
        .unwrap();
    let request = read_frame(&mut second).await.unwrap();
    assert!(request.id > 0);
    assert_eq!(request.body["type"], "a11y.snapshot");
    second.write_all(&fragmented[2..]).await.unwrap();
    write_frame(
        &mut second,
        &Envelope {
            id: request.id,
            body: json!({"type":"a11yTree", "nodes":[]}),
        },
    )
    .await
    .unwrap();
    assert_eq!(result.await.unwrap().unwrap()["type"], "a11yTree");
    assert!(state.capabilities_seen.load(Ordering::Acquire));
    // Cancelled work is discarded before dispatch and cannot poison correlation.
    let (reply, cancelled) = oneshot::channel();
    drop(cancelled);
    connection
        .commands
        .send(ActorCommand::Request {
            body: json!({"type":"a11y.tap","x":1,"y":1,"control":true}),
            reply,
            deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        })
        .await
        .unwrap();
    // The cancelled queued action must not reach the APK. A later request is
    // the next frame, with no intervening a11y.tap bytes.
    let (reply, pending) = oneshot::channel();
    connection
        .commands
        .send(ActorCommand::Request {
            body: json!({"type":"a11y.snapshot"}),
            reply,
            deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        })
        .await
        .unwrap();
    let dispatched = read_frame(&mut second).await.unwrap();
    assert_eq!(dispatched.body["type"], "a11y.snapshot");
    drop(pending); // Cancellation after dispatch: a late reply must be discarded.
    write_frame(
        &mut second,
        &Envelope {
            id: dispatched.id,
            body: json!({"type":"a11yTree", "nodes":[]}),
        },
    )
    .await
    .unwrap();
    let (reply, replacement_result) = oneshot::channel();
    connection
        .commands
        .send(ActorCommand::Request {
            body: json!({"type":"a11y.snapshot"}),
            reply,
            deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        })
        .await
        .unwrap();
    let awaiting_replacement = read_frame(&mut second).await.unwrap();
    assert_ne!(awaiting_replacement.id, dispatched.id);
    let mut third = connect(&path).await;
    assert!(
        replacement_result.await.unwrap().is_err(),
        "replacement fails old pending request"
    );
    assert_eq!(second.read(&mut byte).await.unwrap(), 0);
    let connection = read_lock(&state.connection).clone().unwrap();
    let (reply, current_result) = oneshot::channel();
    connection
        .commands
        .send(ActorCommand::Request {
            body: json!({"type":"a11y.snapshot"}),
            reply,
            deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        })
        .await
        .unwrap();
    let current_request = read_frame(&mut third).await.unwrap();
    write_frame(
        &mut third,
        &Envelope {
            id: current_request.id,
            body: json!({"type":"a11yTree", "nodes":[], "fixture":"current"}),
        },
    )
    .await
    .unwrap();
    assert_eq!(current_result.await.unwrap().unwrap()["fixture"], "current");
    let _silent_handshake = tokio::net::UnixStream::connect(&path).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), server.shutdown())
        .await
        .unwrap();
    assert!(!path.exists());
    assert_eq!(third.read(&mut byte).await.unwrap(), 0);
    assert!(!root_path.join("mobile-token").exists());
    // Exact owned stale endpoint recovery uses the same platform helper.
    let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    let mut restarted =
        MobileTransportServer::start_standalone(&root_path, Arc::new(TransportState::new()))
            .await
            .unwrap();
    let _client = connect(&path).await;
    restarted.shutdown().await;
    assert!(!path.exists());
}

#[tokio::test]
async fn mobile_standalone_kernel_peer_identity_rejects_a_different_owner() {
    let (client, server) = tokio::net::UnixStream::pair().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    assert!(haider_platform::peer_is_owner(&server, uid).unwrap());
    assert!(!haider_platform::peer_is_owner(&server, uid.wrapping_add(1)).unwrap());
    drop(client);
}

#[tokio::test]
async fn mobile_standalone_rejects_token_chat_and_malformed_hello() {
    for body in [
        json!({"type":"hello","apkVersion":"971","token":"synthetic"}),
        json!({"type":"chat.send","text":"fixture"}),
        json!({"type":"hello","apkVersion":""}),
        json!({"type":"hello","apkVersion":"971","bootstrap":true}),
    ] {
        let (mut client, server) = tokio::io::duplex(4096);
        let task = tokio::spawn(serve_io(server, Arc::new(TransportState::new()), None));
        write_frame(&mut client, &Envelope { id: 1, body })
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut client).await.unwrap().body["type"],
            "authReject"
        );
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn mobile_standalone_frame_bounds_and_envelope_shape() {
    for bytes in [
        0u32.to_be_bytes().to_vec(),
        8_388_609u32.to_be_bytes().to_vec(),
    ] {
        assert!(read_frame(&mut bytes.as_slice()).await.is_err());
    }
    for text in [
        r#"{"id":1,"body":{},"token":"fixture"}"#,
        r#"{"id":1.5,"body":{}}"#,
        r#"{"id":1,"body":[]}"#,
        r#"{"id":1,"body":{"type":7}}"#,
        r#"{"id":1,"body":{"type":""}}"#,
    ] {
        let mut bytes = (text.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(text.as_bytes());
        assert!(read_frame(&mut bytes.as_slice()).await.is_err());
    }
}
