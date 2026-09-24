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

#[test]
fn model_path_helper_masks_assignment_bearing_names() {
    assert_eq!(
        super::model_visible_path(std::path::Path::new("password=amber-moonset.txt")),
        "[REDACTED:sensitive_path]"
    );
    assert_eq!(
        super::model_visible_path(std::path::Path::new("nested/public.txt")),
        "nested/public.txt"
    );
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
        ranged_body.text, "[REDACTED:high_entropy]\n",
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
fn default_redaction_labels_match_detected_classes_golden() {
    use base64::Engine as _;
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let jwt = format!(
        "{}.{}.{}",
        encoder.encode(br#"{"alg":"HS256","typ":"JWT"}"#),
        encoder.encode(br#"{"sub":"synthetic-user"}"#),
        encoder.encode("synthetic-signature")
    );
    let input = [
        "AKIAABCDEFGHIJKLMNOP".to_owned(),
        "sk-abcdefghijklmnopQRSTUV".to_owned(),
        "ghp_abcdefghijklmnopqrstuvwxyz1234".to_owned(),
        "xoxb-1234567890-abcdefghij".to_owned(),
        vendor_fixture(&["glpat", "-"], VENDOR_PAYLOAD_20),
        vendor_fixture(&["npm", "_"], VENDOR_PAYLOAD_36),
        vendor_fixture(&["sk", "_live", "_"], VENDOR_PAYLOAD_26),
        vendor_fixture(&["AI", "za"], VENDOR_PAYLOAD_35),
        jwt,
        "eyJabcdefghijk.eyJabcdefghijk.abcdefghijkl".to_owned(),
        "Authorization: Bearer 859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4"
            .to_owned(),
        "api_key=ordinary-fixture-value".to_owned(),
        "api_key=eyJabcdefghijk.eyJabcdefghijk.abcdefghijkl".to_owned(),
        "password=ordinary-fixture-value".to_owned(),
        "aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY".to_owned(),
        "QWxhZGRpbjpPcGVuU2Vz".to_owned(),
    ]
    .join("\n");
    let actual = super::redact_output_text(&format!("{input}\n"));
    assert_eq!(
        actual,
        include_str!("../tests/fixtures/redaction_labels_v1.golden")
    );
    assert!(!actual.contains("ordinary-fixture-value"));
    assert!(!actual.contains("synthetic-signature"));
}

#[test]
fn explicit_context_overrides_generic_jwt_lookalike_label() {
    let lookalike = "eyJabcdefghijk.eyJabcdefghijk.abcdefghijkl";
    for (input, expected) in [
        (
            format!("api_key={lookalike}"),
            "api_key=[REDACTED:api_key]".to_owned(),
        ),
        (
            format!("Bearer {lookalike}"),
            "Bearer [REDACTED:bearer_token]".to_owned(),
        ),
        (
            format!("https://owner:{lookalike}@example.test"),
            "https://owner:[REDACTED:password]@example.test".to_owned(),
        ),
    ] {
        assert_eq!(super::redact_output_text(&input), expected);
    }
}

#[test]
fn failed_jwt_shape_uses_entropy_only_when_detector_accepts_it() {
    let low = "eyJaaaaaaaa.aaaaaaaa.aaaaaaaa";
    assert!(!super::looks_high_entropy(low));
    assert_eq!(redact_text(low).text, "[REDACTED:secret_value]");
    assert_eq!(
        redact_private_key_lines(low).text,
        "[REDACTED:secret_value]"
    );
    assert_eq!(
        redact_text(&format!("api_key={low}")).text,
        "api_key=[REDACTED:api_key]"
    );
    assert_eq!(
        redact_text(&format!("https://owner:{low}@example.test")).text,
        "https://owner:[REDACTED:password]@example.test"
    );
}

#[test]
fn assignment_label_comes_from_terminal_matched_field() {
    let input = concat!(
        "PASSWORD_RESET_TOKEN=synthetic-phrase\n",
        "API_KEY_PASSWORD=synthetic-phrase\n",
        "notapassword_token=synthetic-phrase\n",
        "Authorization: Bearer synthetic-phrase\n",
    );
    let expected = concat!(
        "PASSWORD_RESET_TOKEN=[REDACTED:secret_value]\n",
        "API_KEY_PASSWORD=[REDACTED:password]\n",
        "notapassword_token=[REDACTED:secret_value]\n",
        "Authorization: [REDACTED:bearer_token]\n",
    );
    assert_eq!(redact_private_key_lines(input).text, expected);
    assert_eq!(redact_text(input).text, expected);
}

#[test]
fn pem_body_is_masked_in_bounded_linewise_and_process_rendering() {
    for input in [
        "password=-----BEGIN\x20PRIVATE KEY-----\nVGVzdA==\n-----END PRIVATE KEY-----\n",
        "password=foo-----BEGIN\x20PRIVATE KEY-----\nVGVzdA==\n-----END PRIVATE KEY-----\n",
        "password=\"-----BEGIN\x20PRIVATE KEY-----\nVGVzdA==\n-----END PRIVATE KEY----- trailing-secret\"\n",
    ] {
        for rendered in [
            redact_text_bounded(input, usize::MAX).text,
            redact_private_key_lines(input).text,
            super::redact_output_text(input),
        ] {
            assert!(!rendered.contains("VGVzdA=="), "{rendered}");
            assert!(!rendered.contains("trailing-secret"), "{rendered}");
            assert!(
                !rendered.contains("-----END PRIVATE KEY-----"),
                "{rendered}"
            );
            assert!(rendered.contains("[REDACTED:"), "{rendered}");
        }
        assert_eq!(
            redact_text_bounded(input, usize::MAX).text,
            redact_private_key_lines(input).text,
            "PEM assignment overlap must agree across bounded and linewise paths"
        );
    }
}

#[test]
fn paged_capture_does_not_reclassify_an_existing_marker() {
    for marker in [
        "[REDACTED:api_key]",
        "[REDACTED:password]",
        "[REDACTED:private_key]",
    ] {
        let safe = format!("token={marker}\n");
        assert_eq!(super::redact_output_text(&safe), safe);
    }
    assert_eq!(
        super::redact_output_text("token=[REDACTED:unknown]suffix\n"),
        "token=[REDACTED:secret_value]\n"
    );
}

#[test]
fn marker_table_bytes_are_pinned() {
    use super::SecretKind::*;
    for (kind, marker) in [
        (PrivateKey, "[REDACTED:private_key]"),
        (PrivateKeyMaterial, "[REDACTED:private_key_material]"),
        (AwsAccessKey, "[REDACTED:aws_access_key]"),
        (ApiKey, "[REDACTED:api_key]"),
        (GithubToken, "[REDACTED:github_token]"),
        (SlackToken, "[REDACTED:slack_token]"),
        (GitlabToken, "[REDACTED:gitlab_token]"),
        (NpmToken, "[REDACTED:npm_token]"),
        (StripeApiKey, "[REDACTED:stripe_api_key]"),
        (GoogleApiKey, "[REDACTED:google_api_key]"),
        (Jwt, "[REDACTED:jwt]"),
        (BearerToken, "[REDACTED:bearer_token]"),
        (BasicAuth, "[REDACTED:basic_auth]"),
        (Password, "[REDACTED:password]"),
        (SecretValue, "[REDACTED:secret_value]"),
        (HighEntropy, "[REDACTED:high_entropy]"),
    ] {
        assert_eq!(kind.marker(), marker);
    }
}

#[test]
fn every_context_class_keeps_its_label_on_each_quoted_line() {
    for (label, kind) in [
        ("password=", "password"),
        ("api_key=", "api_key"),
        ("Bearer ", "bearer_token"),
        ("Basic ", "basic_auth"),
        ("secret=", "secret_value"),
    ] {
        let marker = format!("[REDACTED:{kind}]");
        assert_eq!(
            super::redact_output_text(&format!("{label}'single quoted\nsecond line\nthird' after")),
            format!("{label}{marker}\n{marker}\n{marker} after"),
            "{label}"
        );
    }
}

#[test]
fn authorization_scheme_labels_accept_whitespace_variants() {
    let token = "abcdefghijklmnopqrstuvwxyz0123456789";
    for (scheme, kind) in [
        ("Bearer", "bearer_token"),
        ("bEaReR", "bearer_token"),
        ("Basic", "basic_auth"),
        ("bAsIc", "basic_auth"),
    ] {
        for gap in [
            " ", "  ", "\t", "\t ", "\r", "\u{000b}", "\u{000c}", "\u{00a0}", "\u{2003}",
            "\u{2009}",
        ] {
            let input = format!("Authorization: {scheme}{gap}{token}");
            assert_eq!(
                super::redact_output_text(&input),
                format!("Authorization: [REDACTED:{kind}]"),
                "{scheme:?} {gap:?}"
            );
            let standalone = format!("{scheme}{gap}{token}");
            assert_eq!(
                super::redact_output_text(&standalone),
                format!("{scheme}{gap}[REDACTED:{kind}]"),
                "standalone {scheme:?} {gap:?}"
            );
        }
    }

    assert_eq!(
        super::redact_output_text(&format!("Authorization: Bearer{token}")),
        "Authorization: [REDACTED:secret_value]"
    );
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
    // thread-UUID is now a standard carrier; session-UUID remains call-local.
    let id = "session-01a0e893-52bc-7def-89ab-0123456789cd";
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
    for (value, kind) in [
        (
            "Authorization: Bearer 859c9fd11efbc93ee3d6b5458111b031d4a6477f405146d65d543732901bdfc4",
            "bearer_token",
        ),
        ("password=\"ordinary secret phrase\"", "password"),
    ] {
        let text = redact_text(value).text;
        assert!(!text.contains("859c9fd"));
        assert!(!text.contains("ordinary secret phrase"));
        assert!(text.contains(&format!("[REDACTED:{kind}]")));
    }
}

/// A passphrase is ambiguous when it appears alone, but an explicit
/// passphrase assignment is credential context. Classify that context before
/// the code-identifier exemption so labels cannot make an entropy-bearing
/// value look public.
#[test]
fn passphrase_assignment_family_precedes_identifier_exemptions() {
    let secret = "quartz-jumping-vexed-fibers";
    let paths = super::ExplicitReadPaths::new(Path::new("handoff.md"));
    for input in [
        format!("passphrase={secret}"),
        format!("pass_phrase='{secret}'"),
        format!("PASSPHRASE=\"{secret}\""),
        format!("ssh_passphrase={secret}"),
        format!(r#"{{"passphrase":"{secret}"}}"#),
        format!(r#"{{"pass_phrase":"{secret}"}}"#),
        format!(r#"{{"SSH_PASSPHRASE":"{secret}"}}"#),
    ] {
        for output in [
            redact_text(&input),
            paths.redact(Path::new("handoff.md"), &input),
        ] {
            assert!(!output.text.contains(secret), "{input}: {}", output.text);
            assert!(
                output.text.contains("[REDACTED:password]"),
                "{input}: {}",
                output.text
            );
        }
    }

    let quoted = format!("\"{secret}\"");
    for input in [secret, quoted.as_str()] {
        assert_eq!(
            redact_text(input).text,
            input,
            "the owner-ratified bare-passphrase tradeoff remains unchanged"
        );
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
        let kind = match label {
            "OPENAI_API_KEY=" => "api_key",
            "Bearer " => "bearer_token",
            "Basic " => "basic_auth",
            _ => "secret_value",
        };
        assert!(output.contains(&format!("[REDACTED:{kind}]")));
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

#[test]
fn url_userinfo_redacts_only_passwords_before_carrier_exemptions() {
    let paths = super::ExplicitReadPaths::new(Path::new("handoff.md"));
    for (input, expected) in [
        (
            "https://owner:fixturepass@example.test/repo",
            "https://owner:[REDACTED:password]@example.test/repo",
        ),
        (
            "postgres://owner:p%40ssw0rd@db.test/app",
            "postgres://owner:[REDACTED:password]@db.test/app",
        ),
        (
            "https://owner:01a0e893-52bc-7def-89ab-0123456789cd@example.test/repo",
            "https://owner:[REDACTED:password]@example.test/repo",
        ),
        (
            "https://own%65r:p%3Ass%2Fword@example.test/repo",
            "https://own%65r:[REDACTED:password]@example.test/repo",
        ),
        (
            "https://owner:pa'ss:word@example.test/repo",
            "https://owner:[REDACTED:password]@example.test/repo",
        ),
        (
            "https://owner:sk-abcdefghijklmnopQRSTUV@example.test/repo",
            "https://owner:[REDACTED:api_key]@example.test/repo",
        ),
        (
            "https://owner@example.test/repo",
            "https://owner@example.test/repo",
        ),
        ("https://a.test/u:p@repo", "https://a.test/u:p@repo"),
    ] {
        assert_eq!(redact_text(input).text, expected, "{input}");
        assert_eq!(paths.redact(Path::new("handoff.md"), input).text, expected);
    }
}

#[test]
fn escaped_credential_quotes_consume_through_the_real_closing_quote() {
    let paths = super::ExplicitReadPaths::new(Path::new("handoff.md"));
    for input in [
        r#"password="abc\"SYNTHETICTAIL987" after"#,
        r#"password='abc\'SYNTHETICTAIL987' after"#,
        r#"password="abc\\SYNTHETICTAIL987" after"#,
        r#"password='abc\\SYNTHETICTAIL987' after"#,
        r#"password="abc\\\"SYNTHETICTAIL987" after"#,
        r#"password="abc\\" after"#,
        r#"password='abc\\' after"#,
    ] {
        for output in [
            redact_text(input),
            paths.redact(Path::new("handoff.md"), input),
        ] {
            assert_eq!(output.text, "password=[REDACTED:password] after", "{input}");
        }
    }
}

#[test]
fn named_public_carriers_survive_standard_output_but_secret_context_wins() {
    for value in [
        "HEAD:752dfaa79475887978ffeb8eaa73134d7a933c7d",
        "urn:uuid:01a0e893-52bc-7def-89ab-0123456789cd",
        "thread-01a0e893-52bc-7def-89ab-0123456789cd",
        "01a0e893-52bc-7def-89ab-0123456789cd.result.md",
        "QmYwAPJzv5CZsnAzt8auVZRnGi2CQCqK4HCb2jdPrFAgDq",
        "run_id=aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
    ] {
        assert_eq!(super::redact_output_text(value), value);
        for (label, kind) in [
            ("password=", "password"),
            ("Bearer ", "bearer_token"),
            ("secret=", "secret_value"),
        ] {
            assert_eq!(
                redact_text(&format!("{label}{value}")).text,
                format!("{label}[REDACTED:{kind}]")
            );
        }
    }
    for value in [
        "HEAD:aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
        "urn:uuid:aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
        "thread-aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
        "aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY.result.md",
        "QmYwAPJzv5CZsnAzt8auVZRnGi2CQCqK4HCb2jdPrFAgD0",
        "QmYwAPJzv5CZsnAzt8auVZRnGi2CQCqK4HCb2jdPrFAgDqx",
        "aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
        "other_run_id=aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
        "run_id=sk-abcdefghijklmnopQRSTUV",
    ] {
        assert!(!super::is_cid_v0(value), "{value}");
        assert_ne!(redact_text(value).text, value, "{value}");
    }
    // A base58 value with the wrong multihash header is not a public CID.
    assert!(!super::is_cid_v0(
        "Qm11111111111111111111111111111111111111111111"
    ));
}

/// 972 identifier false-positive corpus (AX-1 experience report, finding F2):
/// released 0.0.971 replaced ordinary `unittest -v` identifiers with
/// `[REDACTED:high_entropy]` in both process output and `fs_read`. Valid
/// code-identifier shapes survive standard redaction; random tokens,
/// credential contexts, and the lockdown classifier are unchanged.
#[test]
fn identifier_false_positive_corpus_survives_standard_redaction() {
    let paths = super::ExplicitReadPaths::new(Path::new("testrun.txt"));
    let mut failures = Vec::new();
    for value in [
        // The exact identifier shapes finding F2 observed inside 0.0.971.
        "test_unreadable_file_exit_code",
        "test_tie_break_alphabetical",
        "test_wordfreq.OrderingTests",
        "test_unreadable_file_exit_code (test_wordfreq.OrderingTests) ... ok",
        "test_wordfreq.OrderingTests.test_tie_break_alphabetical",
        // Broader identifier families at candidate length.
        "test_unreadable_file_exit_code_path",
        "haider_tools::redact::redact_output_text",
        "HAIDER_MAX_PROCESS_OUTPUT_BYTES",
        "lane-972-redaction-precision",
        "assertRaisesRegexMessageMatches",
        "com.example.wordfreq.CliSmokeTests",
        "process_exec_output_redaction_v2",
        "normalize_URLs_and_paths_helper",
    ] {
        for (mode, output) in [
            ("standard", super::redact_output_text(value)),
            ("fs_read", redact_private_key_lines(value).text),
            (
                "explicit",
                paths.redact(Path::new("testrun.txt"), value).text,
            ),
        ] {
            if output != value {
                failures.push(format!("{mode}: {value:?} -> {output:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    // Identifier shape never exempts random tokens: interleaved case/digit
    // runs, single-word noise, and encoded credentials all still redact.
    for value in [
        "aB3d_E5fG7_hI9jK1mN3pQ5rS7tU9vW1",
        "qwertyuiopasdfghjklzxcvbnmqw",
        "Ab1Cd2Ef3Gh4Ij5Kl6Mn7Pq8St9Uv0",
        "test-aB3dE5fG7hI9jK1mN3pQ5rS7tU9",
        "c2stYWJjZGVmZ2hpamtsbW5vcFFSU1RVVg",
    ] {
        assert_eq!(
            redact_text(value).text,
            "[REDACTED:high_entropy]",
            "{value}"
        );
    }
    // Vendor-prefix bypass corpus from independent verification. Known-format
    // precedence may choose a more specific marker than generic entropy.
    for value in verifier_bypass_fixtures() {
        let value = value.as_str();
        let output = redact_text(value).text;
        assert_ne!(output, value, "{value}");
        assert!(!output.contains(value), "{value}: {output}");
    }
    // Credential context still wins over identifier shape.
    for input in [
        "password=test_unreadable_file_exit_code",
        "Bearer test_unreadable_file_exit_code",
        "api_key: test_wordfreq.OrderingTests",
    ] {
        let output = redact_text(input).text;
        assert!(!output.contains("test_"), "{input}");
        let kind = if input.starts_with("api_key") {
            "api_key"
        } else if input.starts_with("Bearer") {
            "bearer_token"
        } else {
            "password"
        };
        assert!(output.contains(&format!("[REDACTED:{kind}]")), "{input}");
    }
    assert_ne!(
        super::redact_lockdown_text("test_unreadable_file_exit_code_path"),
        "test_unreadable_file_exit_code_path",
        "lockdown keeps its historical classifier"
    );
}

/// Builds a synthetic vendor-format fixture at runtime so the scanner-shaped
/// literal never exists at rest (GitHub push protection and similar scanners
/// match file contents; the runtime bytes are asserted by length/prefix).
fn vendor_fixture(prefix_parts: &[&str], payload: &str) -> String {
    let mut value: String = prefix_parts.concat();
    value.push_str(payload);
    value
}

const VENDOR_PAYLOAD_20: &str = "QWERTYUIOPASDFGHJKLZ";
const VENDOR_PAYLOAD_26: &str = "QWERTYUIOPASDFGHJKLZXCVBNM";
const VENDOR_PAYLOAD_35: &str = "QWERTYUIOPASDFGHJKLZXCVBNMQWERTYUIO";
const VENDOR_PAYLOAD_36: &str = "QWERTYUIOPASDFGHJKLZXCVBNMQWERTYUIOP";

fn verifier_bypass_fixtures() -> [String; 4] {
    [
        vendor_fixture(&["glpat", "-"], VENDOR_PAYLOAD_20),
        vendor_fixture(&["npm", "_"], VENDOR_PAYLOAD_36),
        vendor_fixture(&["sk", "_live", "_"], VENDOR_PAYLOAD_26),
        vendor_fixture(&["AI", "za"], VENDOR_PAYLOAD_35),
    ]
}

/// Permanent regressions for vendor-prefixed secrets that previously looked
/// like two code-identifier words: a lowercase prefix and one long acronym.
#[test]
fn vendor_secret_formats_precede_identifier_exemption() {
    let paths = super::ExplicitReadPaths::new(Path::new("testrun.txt"));
    let verifier_bypasses = verifier_bypass_fixtures();
    for value in verifier_bypasses.iter().map(String::as_str) {
        let known = super::extended_secret_regex()
            .and_then(|regex| regex.find(value))
            .map(|found| found.as_str());
        assert_eq!(known, Some(value), "known-format span: {value}");
        for output in [
            super::redact_output_text(value),
            paths.redact(Path::new("testrun.txt"), value).text,
        ] {
            assert_ne!(output, value, "{value}");
            assert!(!output.contains(value), "{value}: {output}");
        }
    }
    let gitlab_siblings = [
        "gloas", "gldt", "glrt", "glrtr", "glcbt", "glptt", "glft", "glimt", "glagent", "glwt",
        "glsoat", "glffct",
    ]
    .map(|prefix| vendor_fixture(&[prefix, "-"], VENDOR_PAYLOAD_20));
    let stripe_siblings = [
        ["pk", "_live", "_"],
        ["sk", "_test", "_"],
        ["rk", "_live", "_"],
    ]
    .map(|parts| vendor_fixture(&parts, VENDOR_PAYLOAD_26));
    for value in gitlab_siblings
        .iter()
        .chain(stripe_siblings.iter())
        .map(String::as_str)
    {
        let known = super::extended_secret_regex()
            .and_then(|regex| regex.find(value))
            .map(|found| found.as_str());
        assert_eq!(known, Some(value), "known-format span: {value}");
    }
}

#[test]
fn long_uppercase_payload_is_not_a_code_identifier_word() {
    let value = "vendor_QWERTYUIOPASDFGHJKLZ";
    assert!(!super::is_code_identifier(value));
    assert_eq!(redact_text(value).text, "[REDACTED:high_entropy]");

    for identifier in ["HTTPServer", "parse_JSON_value", "SQLX_query_builder"] {
        assert!(super::is_code_identifier(identifier), "{identifier}");
    }
}

#[test]
fn multiline_quoted_values_preserve_lines_and_hide_every_secret_fragment() {
    let paths = super::ExplicitReadPaths::new(Path::new("handoff.md"));
    for quote in ['\'', '"'] {
        for newline in ["\n", "\r\n", "\\\n", "\\\r\n"] {
            for escaped in ["", "\\", "\\\\\\"] {
                let input = format!(
                    "password={quote}abc{newline}SYNTHETICTAIL987{escaped}{quote}last{quote} after"
                );
                // Odd backslashes escape the first quote; the final quote closes it.
                let suffix = if escaped.is_empty() {
                    format!("last{quote} after")
                } else {
                    " after".into()
                };
                let expected = format!("password=[REDACTED:password]\n[REDACTED:password]{suffix}");
                for output in [
                    redact_text(&input),
                    paths.redact(Path::new("handoff.md"), &input),
                    redact_private_key_lines(&input),
                ] {
                    assert_eq!(output.text, expected, "{input:?}");
                }
                for limit in [0, 1, 28, 29, 30, 54, 128] {
                    let bounded = redact_text_bounded(&input, limit);
                    assert_eq!(bounded.text, super::utf8_prefix(&expected, limit));
                    assert_eq!(bounded.full_len, expected.len());
                }
            }
        }
    }
}

#[test]
fn multiline_quote_window_is_byte_bounded_and_fails_closed_on_overflow() {
    let paths = super::ExplicitReadPaths::new(Path::new("handoff.md"));
    for quote in ['\'', '"'] {
        for length in [
            super::QUOTED_SECRET_MAX_BYTES - 3,
            super::QUOTED_SECRET_MAX_BYTES - 2,
        ] {
            // Opening + payload + LF + closing: exactly at the limit, then one over.
            let input = format!(
                "password={quote}{}\n{quote} after\nPUBLIC",
                "a".repeat(length)
            );
            let expected = if length == super::QUOTED_SECRET_MAX_BYTES - 3 {
                "password=[REDACTED:password]\n[REDACTED:password] after\nPUBLIC"
            } else {
                "password=[REDACTED:password]\n[REDACTED:password]\n[REDACTED:password]"
            };
            for output in [
                redact_text(&input),
                paths.redact(Path::new("handoff.md"), &input),
                redact_private_key_lines(&input),
            ] {
                assert_eq!(output.text, expected);
            }
        }
    }
}

#[test]
fn unterminated_multiline_quotes_and_nested_contexts_fail_closed() {
    for input in [
        "password=\"abc\n\nSYNTHETICTAIL987",
        "password='abc\\\n\nSYNTHETICTAIL987",
        "password=\"abc\n-----BEGIN\x20PRIVATE KEY-----\nSYNTHETICTAIL987",
        "password=\"abc\npassword=inner SYNTHETICTAIL987",
    ] {
        let expected = redact_text(input).text;
        assert!(!expected.contains("SYNTHETICTAIL987"));
        assert_eq!(expected.matches('\n').count(), input.matches('\n').count());
        assert_eq!(redact_private_key_lines(input).text, expected);
    }
}
