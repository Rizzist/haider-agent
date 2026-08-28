#![allow(clippy::expect_used)]

use super::{
    is_sensitive_path, is_token_config_path, redact_private_key_lines, redact_text,
    token_config_contains_secret,
};
use std::path::Path;

#[test]
fn sensitive_paths_cover_key_and_state_families() {
    for path in [
        ".env",
        ".env.production",
        "keys/server.pem",
        "id_rsa.pub",
        "infra/main.tfstate",
        ".aws/credentials",
        ".npmrc",
        ".pypirc",
    ] {
        assert!(is_sensitive_path(Path::new(path)), "{path}");
    }
    assert!(!is_sensitive_path(Path::new("src/environment.rs")));
    assert!(is_token_config_path(Path::new(".npmrc")));
    assert!(!token_config_contains_secret(
        b"registry=https://example.invalid\n"
    ));
    assert!(token_config_contains_secret(
        b"//registry/:_authToken=secret\n"
    ));
}

/// MUTATION CHECK: remove any known-shape branch or the generic entropy pass.
/// Expected failure: a literal credential survives in the preview.
#[test]
fn known_and_high_entropy_tokens_are_redacted_deterministically() {
    let input = concat!(
        "aws=AKIAABCDEFGHIJKLMNOP\n",
        "openai=sk-abcdefghijklmnopQRSTUV\n",
        "github=ghp_abcdefghijklmnopqrstuvwxyz1234\n",
        "slack=xoxb-1234567890-abcdefghij\n",
        "jwt=eyJabcdefghijk.eyJabcdefghijk.abcdefghijkl\n",
        "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        "QWxhZGRpbjpPcGVuU2VzYW1lU2VjcmV0QmxvYg==\n",
        "-----END OPENSSH PRIVATE KEY-----\n",
        "blob=aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY\n",
        "hex=0123456789abcdef0123456789abcdef0123456789abcdef\n",
    );
    let first = redact_text(input);
    let second = redact_text(input);
    assert_eq!(first, second);
    assert_eq!(first.replacements, 8);
    assert!(!first.text.contains("AKIA"));
    assert!(first.text.contains("[REDACTED:private_key]"));
    assert!(first.text.contains("[REDACTED:high_entropy]"));
    assert!(!first.text.contains("QWxhZGRp"));
    assert!(!first.text.contains("0123456789abcdef"));

    let ranged_body = redact_text("QWxhZGRpbjpPcGVuU2Vz\n");
    assert_eq!(
        ranged_body.text, "[REDACTED:private_key_material]\n",
        "a range or line-oriented search cannot bypass PEM-body redaction"
    );

    let short_body = redact_private_key_lines(concat!(
        "-----BEGIN PRIVATE KEY-----\n",
        "AA==\n",
        "-----END PRIVATE KEY-----\n",
    ));
    assert_eq!(short_body.replacements, 3);
    assert!(!short_body.text.contains("AA=="));
}
