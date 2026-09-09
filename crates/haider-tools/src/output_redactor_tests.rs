#![allow(clippy::expect_used)]
use super::OutputRedactor;

#[test]
fn every_chunk_boundary_preserves_secret_redaction_and_plain_output() {
    let input = "begin\néclair\napi=sk-abcdefghijklmnopQRSTUV\n-----BEGIN\x20PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\nend";
    for boundary in 0..=input.len() {
        let mut redactor = OutputRedactor::default();
        let mut text = redactor.push(&input.as_bytes()[..boundary]);
        text.push_str(&redactor.push(&input.as_bytes()[boundary..]));
        text.push_str(&redactor.finish());
        assert_eq!(
            text,
            crate::redact_output_text(input),
            "boundary {boundary}"
        );
        assert!(!text.contains("sk-"));
        assert!(!text.contains("AA=="));
        assert!(text.ends_with("end"));
    }
}

#[test]
fn stderr_interleaving_does_not_split_stdout_secret_classification() {
    use base64::Engine as _;
    use haider_protocol::item::OutputStream;
    let chunks = [
        (OutputStream::Stdout, "sk-abcdefgh"),
        (OutputStream::Stderr, "notice\n"),
        (OutputStream::Stdout, "ijklmnopQRSTUV\n"),
    ]
    .map(|(stream, bytes)| crate::ProcessOutputChunk {
        stream,
        chunk_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    });
    let output = super::redact_process_output(&chunks).expect("valid chunks");
    assert_eq!(output, "notice\n[REDACTED:api_key]\n");
}

#[test]
fn oversized_unterminated_lines_stay_bounded_and_fail_closed() {
    let mut redactor = OutputRedactor::default();
    assert!(
        redactor
            .push(&vec![b'a'; crate::PROCESS_MAX_OUTPUT_BYTES])
            .is_empty()
    );
    assert_eq!(
        redactor.push(b"extra"),
        "[REDACTED:oversized_output_line]\n"
    );
    assert!(redactor.finish().is_empty());
    let rest = redactor.push(b"\nAA==\n-----END PRIVATE KEY-----\nplain\n");
    assert!(!rest.contains("AA=="));
    assert!(rest.ends_with("plain\n"));
}

#[test]
fn ansi_styling_cannot_hide_a_token_from_the_journal_or_model() {
    let input = "sk-abc\x1b[31mdefghijklmnopQRSTUV\x1b[0m\n";
    for boundary in 0..=input.len() {
        let mut redactor = OutputRedactor::default();
        let mut safe = redactor.push(&input.as_bytes()[..boundary]);
        safe.push_str(&redactor.push(&input.as_bytes()[boundary..]));
        safe.push_str(&redactor.finish());
        assert_eq!(safe, "[REDACTED:api_key]\n", "boundary {boundary}");
    }
}

#[test]
fn non_secret_binary_journal_output_stays_byte_exact_across_chunk_boundaries() {
    let input = b"binary \xff\xfe\nUTF-8 \xc3\xa9\npartial \xf0";
    for boundary in 0..=input.len() {
        let mut redactor = OutputRedactor::default();
        let mut safe = redactor.push_bytes(&input[..boundary]);
        safe.extend(redactor.push_bytes(&input[boundary..]));
        safe.extend(redactor.finish_bytes());
        assert_eq!(safe, input, "boundary {boundary}");
    }
}

#[test]
fn binary_lines_with_known_credentials_still_redact_before_journaling() {
    let mut redactor = OutputRedactor::default();
    let safe = redactor.push_bytes(b"sk-abcdefghijklmnopQRSTUV \xff\n");
    assert!(!safe.windows(3).any(|window| window == b"sk-"));
    assert!(String::from_utf8_lossy(&safe).contains("[REDACTED:api_key]"));
}

#[test]
fn hidden_terminal_control_payloads_never_reenter_the_journal() {
    for input in [
        "plain\x1b]0;sk-abcdefghijklmnopQRSTUV\x07 output\n",
        "plain\x1b]0;sk-abcdefghijklmnopQRSTUV\x1b\\ output\n",
    ] {
        for boundary in 0..=input.len() {
            let mut redactor = OutputRedactor::default();
            let mut safe = redactor.push_bytes(&input.as_bytes()[..boundary]);
            safe.extend(redactor.push_bytes(&input.as_bytes()[boundary..]));
            safe.extend(redactor.finish_bytes());
            assert_eq!(safe, b"plain output\n", "boundary {boundary}");
        }
    }
}

#[test]
fn hidden_pem_delimiters_still_protect_the_visible_body() {
    let input = "\x1b]0;-----BEGIN\x20PRIVATE KEY-----\x07\nAA==\n\x1b]0;-----END PRIVATE KEY-----\x07\nplain\n";
    for boundary in 0..=input.len() {
        let mut redactor = OutputRedactor::default();
        let mut safe = redactor.push(&input.as_bytes()[..boundary]);
        safe.push_str(&redactor.push(&input.as_bytes()[boundary..]));
        safe.push_str(&redactor.finish());
        assert_eq!(
            safe, "[REDACTED:private_key]\n[REDACTED:private_key]\n[REDACTED:private_key]\nplain\n",
            "boundary {boundary}"
        );
    }
}

#[test]
fn url_passwords_and_escaped_values_are_safe_at_every_stream_boundary() {
    let input = concat!(
        "https://owner:fixturepass@example.test/repo\n",
        "postgres://owner:p%40ssw0rd@db.test/app\n",
        "password=\"abc\\\"SYNTHETICTAIL987\"\n",
        "password='abc\\'SYNTHETICTAIL987'\n",
        "password=\"abc\\\\SYNTHETICTAIL987\"\n",
        "password='abc\\\\SYNTHETICTAIL987'\n",
    );
    let expected = concat!(
        "https://owner:[REDACTED:secret_value]@example.test/repo\n",
        "postgres://owner:[REDACTED:secret_value]@db.test/app\n",
        "password=[REDACTED:secret_value]\n",
        "password=[REDACTED:secret_value]\n",
        "password=[REDACTED:secret_value]\n",
        "password=[REDACTED:secret_value]\n",
    );
    for boundary in 0..=input.len() {
        let mut redactor = OutputRedactor::default();
        let mut safe = redactor.push_bytes(&input.as_bytes()[..boundary]);
        safe.extend(redactor.push_bytes(&input.as_bytes()[boundary..]));
        safe.extend(redactor.finish_bytes());
        assert_eq!(safe, expected.as_bytes(), "boundary {boundary}");
    }
}

#[test]
fn multiline_quotes_remain_safe_at_every_byte_boundary_and_finish() {
    for quote in ['\'', '"'] {
        for newline in ["\n", "\r\n", "\\\n", "\\\r\n"] {
            let input = format!(
                "password={quote}abc{newline}é\\{quote}SYNTHETICTAIL987{quote} after\npublic\n"
            );
            let expected =
                b"password=[REDACTED:secret_value]\n[REDACTED:secret_value] after\npublic\n";
            for boundary in 0..=input.len() {
                let mut redactor = OutputRedactor::default();
                let mut safe = redactor.push_bytes(&input.as_bytes()[..boundary]);
                safe.extend(redactor.push_bytes(&input.as_bytes()[boundary..]));
                safe.extend(redactor.finish_bytes());
                assert_eq!(safe, expected, "boundary {boundary}, {input:?}");
            }
            let mut redactor = OutputRedactor::default();
            let mut safe = Vec::new();
            for byte in input.bytes() {
                safe.extend(redactor.push_bytes(&[byte]));
            }
            safe.extend(redactor.finish_bytes());
            assert_eq!(safe, expected);
        }
    }
}

#[test]
fn multiline_quote_state_is_per_stream_and_survives_empty_lines() {
    use base64::Engine as _;
    use haider_protocol::item::OutputStream;
    let chunks = [
        (OutputStream::Stdout, "password=\"abc\\\n"),
        (OutputStream::Stderr, "notice \"\n"),
        (OutputStream::Stdout, "\nSYNTHETICTAIL987\" after\n"),
    ]
    .map(|(stream, bytes)| crate::ProcessOutputChunk {
        stream,
        chunk_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    });
    assert_eq!(
        super::redact_process_output(&chunks).expect("chunks"),
        "password=[REDACTED:secret_value]\nnotice \"\n\n[REDACTED:secret_value] after\n"
    );
}

#[test]
fn multiline_quote_overflow_never_reopens_on_a_late_closing_quote() {
    let mut redactor = OutputRedactor::default();
    assert_eq!(
        redactor.push(b"password=\"abc\n"),
        "password=[REDACTED:secret_value]\n"
    );
    let line = vec![b'x'; 1024];
    for _ in 0..2048 {
        let _ = redactor.push(&line);
        let _ = redactor.push(b"\n");
    }
    let output = redactor.push(b"SYNTHETICTAIL987\" after\nPUBLIC\n");
    assert_eq!(output, "[REDACTED:secret_value]\n[REDACTED:secret_value]\n");
    assert!(redactor.pending.len() <= crate::PROCESS_MAX_OUTPUT_BYTES);
}

#[test]
fn oversized_quoted_lines_do_not_recover_at_pem_or_quote_delimiters() {
    for prefix in ["password=\"", "password=\"abc\n"] {
        let mut redactor = OutputRedactor::default();
        let _ = redactor.push(prefix.as_bytes());
        let _ = redactor.push(&vec![b'a'; crate::PROCESS_MAX_OUTPUT_BYTES + 1]);
        let rest = redactor.push(b"\n-----END PRIVATE KEY-----\nSYNTHETICTAIL987\" after\n");
        assert!(!rest.contains("SYNTHETICTAIL987"));
        assert!(!rest.contains(" after"));
        assert!(redactor.pending.len() <= crate::PROCESS_MAX_OUTPUT_BYTES);
    }
}

#[test]
fn quoted_passwords_and_pem_delimiters_cannot_reset_each_others_protection() {
    for input in [
        "password=\"abc -----BEGIN\x20PRIVATE KEY----- -----END PRIVATE KEY-----\nSYNTHETICTAIL987\" after\n",
        "password=\"abc\nSYNTHETICTAIL987\" -----BEGIN\x20PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\npublic\n",
    ] {
        let expected = crate::redact_output_text(input);
        assert!(!expected.contains("SYNTHETICTAIL987"));
        assert!(!expected.contains("AA=="));
        for boundary in 0..=input.len() {
            let mut redactor = OutputRedactor::default();
            let mut output = redactor.push_bytes(&input.as_bytes()[..boundary]);
            output.extend(redactor.push_bytes(&input.as_bytes()[boundary..]));
            output.extend(redactor.finish_bytes());
            assert_eq!(output, expected.as_bytes(), "boundary {boundary}");
        }
    }
}
