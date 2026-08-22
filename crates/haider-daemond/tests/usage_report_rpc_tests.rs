//! U1 wire laws for `usage.report` over the REAL production runtime and a
//! real UnixStream: the welcome advertises `usage_report_v1`, and a
//! View-only client receives a typed report — never an error — on a fresh
//! profile with no accounts.
#![allow(clippy::expect_used)]

mod support;

use haider_daemon::DaemonConfig;
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, Hello, RequestBody, RequestId, ResponseBody,
    WIRE_PROTOCOL_VERSION, WireFrame,
};
use support::{UdsClient as Client, ready, test_root};

/// LAW (usage_report_is_advertised_and_answers_typed_over_uds): the ready
/// daemon's welcome carries `usage_report_v1`; a VIEW-only connection may
/// call `usage.report`; and the fresh-profile answer is a typed empty
/// report (a daemon with no accounts reports no accounts — it never errors
/// and never invents an entry).
#[tokio::test]
async fn usage_report_is_advertised_and_answers_typed_over_uds() {
    let root = test_root("u1-usage-rpc-");
    let config = DaemonConfig::new(
        "usage-report-wire",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready(&config).await;

    let mut client = Client::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("connect");
    client
        .send(
            &WireFrame::Hello(Hello {
                protocol_min: WIRE_PROTOCOL_VERSION,
                protocol_max: WIRE_PROTOCOL_VERSION,
                client_name: "u1-usage-test".into(),
                client_version: "test".into(),
                client_instance_id: "client".into(),
                client_kind: ClientKind::Headless,
                capabilities_requested: CapabilitySet::from([Capability::View]),
                max_receive_frame: u32::try_from(config.frame_limit).expect("frame limit"),
                encodings: Vec::new(),
            }),
            config.frame_limit,
        )
        .await;
    let WireFrame::Welcome(welcome) = client.next().await else {
        panic!("expected a welcome frame");
    };
    assert!(
        welcome
            .features
            .contains(haider_rpc::FEATURE_USAGE_REPORT_V1),
        "welcome must advertise usage_report_v1: {:?}",
        welcome.features
    );

    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("usage-1"),
                body: RequestBody::UsageReport,
            },
            config.frame_limit,
        )
        .await;
    let WireFrame::Response { request_id, body } = client.next_reply().await else {
        panic!("expected a response frame");
    };
    assert_eq!(request_id, RequestId::new("usage-1"));
    let ResponseBody::UsageReport { report } = body else {
        panic!("expected a typed usage.report response, got {body:?}");
    };
    assert!(
        report.accounts.is_empty(),
        "a fresh profile has no accounts to report"
    );

    drop(client);
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
