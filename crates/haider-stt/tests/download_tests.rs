//! Download laws: streaming progress, sha256-before-atomic-rename, and the
//! shared-install short-circuit.

#![allow(clippy::expect_used)]

mod common;

use common::{CannedResponse, sha256_hex, spawn_http_fixture};
use haider_stt::SttError;
use haider_stt::download::{
    DOWNLOAD_TEMP_SUFFIX, DownloadProgress, DownloadSpec, DownloadState, install,
};

fn spec<'a>(url: &'a str, sha256: &'a str) -> DownloadSpec<'a> {
    DownloadSpec {
        url,
        sha256,
        file_name: "ggml-test.bin",
    }
}

/// MUTATION CHECK: rename the temp file before hashing (verify-after-
/// rename), or skip verification entirely. Expected runtime failure: the
/// corrupted-download law below finds the final file present, or this law's
/// digest/progress assertions fail.
#[tokio::test]
async fn verified_download_streams_progress_and_installs_atomically() {
    let body: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
    let digest = sha256_hex(&body);
    let fixture = spawn_http_fixture(vec![(
        "/model.bin".to_owned(),
        CannedResponse::ok_bytes(body.clone()),
    )])
    .await;
    let dir = tempfile::tempdir().expect("temp dir");
    let mut events: Vec<DownloadProgress> = Vec::new();
    let client = reqwest::Client::new();
    let url = format!("{}/model.bin", fixture.origin);
    let installed = install(&client, dir.path(), spec(&url, &digest), |progress| {
        events.push(progress);
    })
    .await
    .expect("verified install succeeds");
    assert_eq!(installed, dir.path().join("ggml-test.bin"));
    assert_eq!(
        std::fs::read(&installed).expect("installed bytes"),
        body,
        "installed file carries the exact served bytes"
    );
    assert!(
        !dir.path()
            .join(format!("ggml-test.bin{DOWNLOAD_TEMP_SUFFIX}"))
            .exists(),
        "temp file is gone after install"
    );
    // Progress: starts with Starting, ends with Done, and the Downloading
    // byte counts are monotonically increasing up to the full length.
    assert_eq!(
        events.first().map(|event| event.state),
        Some(DownloadState::Starting)
    );
    assert_eq!(
        events.last().map(|event| event.state),
        Some(DownloadState::Done)
    );
    let downloading: Vec<&DownloadProgress> = events
        .iter()
        .filter(|event| event.state == DownloadState::Downloading)
        .collect();
    assert!(!downloading.is_empty(), "at least one downloading emission");
    let mut last = 0;
    for event in &downloading {
        assert!(event.downloaded_bytes >= last, "byte counts are monotonic");
        last = event.downloaded_bytes;
        assert_eq!(event.total_bytes, Some(body.len() as u64));
    }
    assert_eq!(last, body.len() as u64, "final progress covers every byte");
}

/// MUTATION CHECK: on checksum mismatch, keep the temp file or fall through
/// to rename anyway. Expected runtime failure: the typed mismatch error
/// disappears, the temp file survives, or the final path exists.
#[tokio::test]
async fn corrupted_download_is_refused_and_leaves_nothing() {
    let body = b"not the promised bytes".to_vec();
    let fixture = spawn_http_fixture(vec![(
        "/model.bin".to_owned(),
        CannedResponse::ok_bytes(body),
    )])
    .await;
    let dir = tempfile::tempdir().expect("temp dir");
    let client = reqwest::Client::new();
    let url = format!("{}/model.bin", fixture.origin);
    let expected = "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002";
    let error = install(&client, dir.path(), spec(&url, expected), |_| {})
        .await
        .expect_err("mismatched digest must refuse");
    match error {
        SttError::ChecksumMismatch {
            expected: e,
            actual,
        } => {
            assert_eq!(e, expected);
            assert_eq!(actual, sha256_hex(b"not the promised bytes"));
        }
        other => panic!("expected ChecksumMismatch, got {other:?}"),
    }
    assert!(
        !dir.path().join("ggml-test.bin").exists(),
        "the final path must never exist after a failed verification"
    );
    assert!(
        !dir.path()
            .join(format!("ggml-test.bin{DOWNLOAD_TEMP_SUFFIX}"))
            .exists(),
        "the temp file is removed after a failed verification"
    );
}

/// The shared-install law: an already-present final file short-circuits
/// with ZERO network requests (the ADE may have installed it; never
/// re-download a 465 MB model).
///
/// MUTATION CHECK: remove the existence short-circuit. Expected runtime
/// failure: the fixture hit counter is non-zero (and the sentinel bytes are
/// clobbered).
#[tokio::test]
async fn existing_model_short_circuits_without_network() {
    let fixture = spawn_http_fixture(vec![(
        "/model.bin".to_owned(),
        CannedResponse::ok_bytes(b"remote".to_vec()),
    )])
    .await;
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("ggml-test.bin"), b"already-installed")
        .expect("preinstalled model");
    let client = reqwest::Client::new();
    let url = format!("{}/model.bin", fixture.origin);
    let digest = sha256_hex(b"remote");
    let installed = install(&client, dir.path(), spec(&url, &digest), |_| {})
        .await
        .expect("short-circuit succeeds");
    assert_eq!(
        std::fs::read(&installed).expect("bytes"),
        b"already-installed",
        "existing bytes stay untouched"
    );
    assert_eq!(
        fixture.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no network request may be made for an installed model"
    );
}

/// A non-success HTTP status is a typed download error, not a hash failure.
#[tokio::test]
async fn http_error_status_is_a_typed_download_error() {
    let fixture = spawn_http_fixture(vec![(
        "/model.bin".to_owned(),
        CannedResponse::status_only(500),
    )])
    .await;
    let dir = tempfile::tempdir().expect("temp dir");
    let client = reqwest::Client::new();
    let url = format!("{}/model.bin", fixture.origin);
    let error = install(&client, dir.path(), spec(&url, "00"), |_| {})
        .await
        .expect_err("500 must fail");
    assert!(matches!(error, SttError::Download(message) if message.contains("500")));
}
