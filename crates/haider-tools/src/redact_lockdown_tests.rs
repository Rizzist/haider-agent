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

#[test]
fn lockdown_passphrase_bytes_remain_frozen() {
    let secret = "quartz-jumping-vexed-fibers";
    assert_eq!(
        redact_lockdown_text(&format!("passphrase={secret}")),
        "[REDACTED:high_entropy]"
    );
    for input in [
        format!("passphrase=\"{secret}\""),
        format!(r#"{{"passphrase":"{secret}"}}"#),
    ] {
        assert_eq!(redact_lockdown_text(&input), input);
    }
}

#[test]
fn lockdown_label_bytes_remain_frozen_across_default_label_changes() {
    assert_eq!(
        redact_lockdown_text(
            "eyJabcdefghijk.eyJabcdefghijk.abcdefghijkl\npassphrase=quartz-jumping-vexed-fibers\n"
        ),
        "[REDACTED:jwt]\n[REDACTED:high_entropy]\n"
    );
}
