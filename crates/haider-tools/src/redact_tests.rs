#![allow(clippy::expect_used)]

use super::{
    is_sensitive_path, is_token_config_path, redact_private_key_lines, redact_text,
    redact_text_bounded, token_config_contains_secret,
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
        "-----BEGIN\x20OPENSSH PRIVATE KEY-----\n",
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
        "-----BEGIN\x20PRIVATE KEY-----\n",
        "AA==\n",
        "-----END PRIVATE KEY-----\n",
    ));
    assert_eq!(short_body.replacements, 3);
    assert!(!short_body.text.contains("AA=="));
}

#[test]
fn bounded_redaction_is_the_exact_full_redaction_prefix() {
    let input = format!(
        "éprefix {} middle sk-abcdefghijklmnopQRSTUV suffix {}",
        "aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY ".repeat(512),
        "tail ".repeat(512),
    );
    let full = redact_text(&input);
    for limit in [0, 1, 2, 3, 7, 31, 8 * 1024, input.len() * 2] {
        let bounded = redact_text_bounded(&input, limit);
        let mut end = limit.min(full.text.len());
        while end > 0 && !full.text.is_char_boundary(end) {
            end -= 1;
        }
        assert_eq!(bounded.text, full.text[..end]);
        assert_eq!(bounded.replacements, full.replacements);
        assert_eq!(bounded.full_len, full.text.len());
    }
}

#[test]
fn orchestration_identifiers_survive_but_secrets_win_over_shapes() {
    use base64::Engine as _;
    let identifiers = [
        "01a0e893-52bc-7def-89ab-0123456789cd",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "7931bed71e96041039c9f85710778c4a00019ab0",
        "859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4",
        "/workspace/harness/state/evidence/971-redaction/01a0e893-52bc-7def-89ab-0123456789cd/result.md",
        "state/evidence/971-redaction/01a0e893-52bc-7def-89ab-0123456789cd/result.md",
    ];
    for value in identifiers {
        assert_eq!(redact_private_key_lines(value).text, value, "{value}");
        assert_eq!(
            redact_private_key_lines(&format!("{value}.")).text,
            format!("{value}.")
        );
        let wrapped = format!("thread={value}\n");
        assert_eq!(redact_private_key_lines(&wrapped).text, wrapped);
        let encoded = base64::engine::general_purpose::STANDARD.encode(value);
        assert_eq!(
            redact_private_key_lines(&encoded).text,
            encoded,
            "encoded {value}"
        );
    }
    for value in [
        "token=859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4",
        "Authorization: Bearer 859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4",
        "password=\"ordinary secret phrase\"",
        "/workspace/sk-abcdefghijklmnopQRSTUV/log.txt",
        "xoxp-1234567890-abcdefghij",
        "aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
        "QWxhZGRpbjpPcGVuU2VzYW1lU2VjcmV0QmxvYg==",
    ] {
        assert_ne!(redact_private_key_lines(value).text, value, "{value}");
    }
}

#[test]
fn explicit_pointer_allow_list_is_exact_and_keeps_secret_checks() {
    let paths = super::ExplicitReadPaths::new(Path::new("handoff.md"));
    let id = "thread-01a0e893-52bc-7def-89ab-0123456789cd";
    assert_eq!(paths.redact(Path::new("handoff.md"), id).text, id);
    assert_ne!(paths.redact(Path::new("other.md"), id).text, id);
    for value in [
        "sk-abcdefghijklmnopQRSTUV",
        "aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
    ] {
        assert_ne!(paths.redact(Path::new("handoff.md"), value).text, value);
    }
}

#[test]
fn secret_context_covers_bearer_digest_and_quoted_phrase() {
    for value in [
        "Authorization: Bearer 859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4",
        "password=\"ordinary secret phrase\"",
    ] {
        let text = redact_text(value).text;
        assert!(!text.contains("859c9fd"));
        assert!(!text.contains("ordinary secret phrase"));
        assert!(text.contains("[REDACTED:secret_value]"));
    }
}

#[test]
fn credential_context_overrides_digest_and_identifier_exemptions() {
    let digest = "859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4";
    for label in [
        "OPENAI_API_KEY=",
        "AWS_SECRET_ACCESS_KEY=",
        "Bearer ",
        "Basic ",
        "refresh_token: ",
    ] {
        let output = redact_text(&format!("{label}{digest}")).text;
        assert!(!output.contains(digest), "{label}");
        assert!(output.contains("[REDACTED:secret_value]"));
    }
    for (value, kind) in [
        ("ASIA0123456789ABCDEF", "aws_access_key"),
        ("github_pat_abcdefghijklmnopQRSTUV", "github_token"),
        ("xoxc-1234567890-abcdefghij", "slack_token"),
        ("xoxd-1234567890-abcdefghij", "slack_token"),
        ("xoxe-1234567890-abcdefghij", "slack_token"),
    ] {
        assert_eq!(redact_text(value).text, format!("[REDACTED:{kind}]"));
    }
}

#[test]
fn owner_classifier_before_after_table() {
    use base64::Engine as _;
    let examples = [
        ("thread UUID", "01a0e893-52bc-7def-89ab-0123456789cd".to_owned()),
        ("thread ULID", "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
        ("git SHA", "7931bed71e96041039c9f85710778c4a00019ab0".to_owned()),
        ("SHA-256", "859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4".to_owned()),
        ("evidence path", "/workspace/harness/state/evidence/971-redaction/01a0e893-52bc-7def-89ab-0123456789cd/result.md".to_owned()),
        ("base64 UUID", base64::engine::general_purpose::STANDARD.encode("01a0e893-52bc-7def-89ab-0123456789cd")),
        ("API key fixture", "sk-abcdefghijklmnopQRSTUV".to_owned()),
        ("random fixture", "aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY".to_owned()),
    ];
    for (name, value) in examples {
        let legacy = super::redact_lockdown_text(&value);
        let current = redact_private_key_lines(&value).text;
        assert_ne!(legacy, value, "legacy {name}");
        assert_eq!(
            current != value,
            matches!(name, "API key fixture" | "random fixture"),
            "{name}"
        );
        eprintln!("classifier | {name} | {legacy} | {current}");
    }
}

#[test]
fn slash_prefixed_random_base64_is_not_mistaken_for_a_path() {
    let value = "/aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY";
    assert_eq!(redact_text(value).text, "[REDACTED:high_entropy]");
}

#[test]
fn uuid_shaped_credentials_in_auth_fields_are_never_identifier_exemptions() {
    let value = "01a0e893-52bc-7def-89ab-0123456789cd";
    let paths = super::ExplicitReadPaths::new(Path::new("handoff.md"));
    for label in [
        "_authToken",
        "_auth",
        "MY_AUTH_TOKEN",
        "clientSecret",
        "privateKey",
        "credentials",
    ] {
        let input = format!("{label}={value}");
        for output in [
            redact_text(&input),
            paths.redact(Path::new("handoff.md"), &input),
        ] {
            assert!(!output.text.contains(value), "{label}");
            assert!(output.text.contains("[REDACTED:secret_value]"));
        }
    }
}

#[test]
fn slash_bearing_random_tokens_require_more_than_a_path_separator() {
    for value in [
        "/aB3dE5fG7hI9/jK1mN3pQ5rS7tU9vW1xY",
        "aB3dE5fG7hI9/jK1mN3pQ5rS7tU9vW1xY",
    ] {
        assert_eq!(redact_text(value).text, "[REDACTED:high_entropy]");
    }
    for path in [
        "/Users/owner/Developer/harness/worktrees/971-redaction",
        "state/evidence/971-redaction",
        "~/harness/worktrees/971-redaction",
        "./build/artifacts/abcdefghijklmNOPQRSTUVWXYZ",
        r"C:\Users\owner\Developer\harness\worktree",
        r"state\evidence\971-redaction\result.md",
    ] {
        assert_eq!(redact_text(path).text, path);
    }
}
