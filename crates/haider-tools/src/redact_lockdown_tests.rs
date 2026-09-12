#![allow(clippy::expect_used)]

use super::redact_lockdown_text;

/// MUTATION CHECK: bypass the forced lockdown redactor for sandbox reads.
/// Expected failure: a credential-shaped value reaches the restricted model.
#[test]
fn lockdown_redaction_is_unconditional() {
    let redacted = redact_lockdown_text(
        "ordinary text\napi=sk-abcdefghijklmnopQRSTUV\n-----BEGIN\x20PRIVATE KEY-----\nAA==\n",
    );
    assert!(redacted.contains("ordinary text"));
    assert!(!redacted.contains("sk-abcdefghijklmnopQRSTUV"));
    assert!(!redacted.contains("AA=="));
    assert!(redacted.contains("[REDACTED:"));
}

#[test]
fn lockdown_retains_legacy_identifier_and_carrier_redaction() {
    for value in [
        "01a0e893-52bc-7def-89ab-0123456789cd",
        "7931bed71e96041039c9f85710778c4a00019ab0",
        "859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4",
        "/workspace/harness/state/evidence/971-redaction/result.md",
    ] {
        assert_ne!(redact_lockdown_text(value), value);
    }
}
