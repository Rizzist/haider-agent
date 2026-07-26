use haider_protocol::error::ErrorCode;

use crate::keychain::classify_os_status;

#[test]
fn keychain_os_status_classification_is_stable() {
    let cases = [
        // Missing remains typed and permanent; delete handles it idempotently
        // before constructing an error.
        (-25_300, ErrorCode::CredentialMissing, false),
        // Transient I/O, unavailable/locked Keychain, and authentication UI.
        (-36, ErrorCode::Internal, true),
        (-25_291, ErrorCode::Internal, true),
        (-25_293, ErrorCode::Internal, true),
        (-25_308, ErrorCode::Internal, true),
        (-25_315, ErrorCode::Internal, true),
        (-25_320, ErrorCode::Internal, true),
        // Permanent invalid-input, cancellation, and entitlement failures.
        (-50, ErrorCode::Internal, false),
        (-128, ErrorCode::Internal, false),
        (-34_018, ErrorCode::Internal, false),
        // Unknown statuses are not guessed to be safe to retry.
        (-99_999, ErrorCode::Internal, false),
    ];

    for (status, expected_code, expected_retryable) in cases {
        assert_eq!(
            classify_os_status(status),
            (expected_code, expected_retryable),
            "unexpected classification for OSStatus {status}"
        );
    }
}
