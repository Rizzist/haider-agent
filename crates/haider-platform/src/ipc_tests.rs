#![allow(clippy::expect_used)]

use std::path::Path;

#[test]
fn peer_artifact_names_are_fixed_and_portably_short() {
    let paths = super::peer_endpoint_paths(
        Path::new("/tmp/haider/01234567890123456789"),
        "session-with-a-deliberately-long-stable-identifier",
        super::PeerEndpointKind::Haider,
    )
    .expect("peer paths fit their budgets");
    for path in [&paths.socket, &paths.manifest, &paths.mailbox] {
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.len() <= super::RUNTIME_ARTIFACT_BASENAME_MAX_BYTES)
        );
    }
    assert_eq!(
        paths
            .socket
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::len),
        Some(17)
    );
}

#[test]
#[cfg(unix)]
fn over_budget_unix_socket_fails_with_observed_length_and_limit() {
    let path = Path::new("/tmp").join("x".repeat(super::UNIX_SOCKET_PATH_MAX_BYTES));
    let error = super::validate_unix_socket_path(&path).expect_err("path exceeds sun_path");
    let super::EndpointError::PathTooLong { length, limit, .. } = error else {
        panic!("expected typed path-length error");
    };
    assert!(length > limit);
    assert_eq!(limit, super::UNIX_SOCKET_PATH_MAX_BYTES);
}
