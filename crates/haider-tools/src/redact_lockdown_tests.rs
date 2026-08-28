#![allow(clippy::expect_used)]

use super::redact_lockdown_text;

/// MUTATION CHECK: bypass the forced lockdown redactor for sandbox reads.
/// Expected failure: a credential-shaped value reaches the restricted model.
#[test]
fn lockdown_redaction_is_unconditional() {
    let redacted = redact_lockdown_text(
        "ordinary text\napi=sk-abcdefghijklmnopQRSTUV\n-----BEGIN PRIVATE KEY-----\nAA==\n",
    );
    assert!(redacted.contains("ordinary text"));
    assert!(!redacted.contains("sk-abcdefghijklmnopQRSTUV"));
    assert!(!redacted.contains("AA=="));
    assert!(redacted.contains("[REDACTED:"));
}
