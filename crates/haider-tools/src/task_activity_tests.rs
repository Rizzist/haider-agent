#![allow(clippy::expect_used)]

use super::TaskOutputBuffer;

#[test]
fn activity_redacts_complete_lines_across_every_chunk_boundary_and_tail_eviction() {
    let output = b"first\nsk-abcdefghijklmnopQRSTUV\n";
    for split in 0..output.len() {
        let mut buffer = TaskOutputBuffer::new(3, 3);
        buffer.append(&output[..split]);
        assert!(!buffer.progress_line().unwrap_or("").contains("sk-"));
        buffer.append(&output[split..]);
        assert!(buffer.progress_line().expect("line").contains("[REDACTED:"));
        assert!(
            !buffer
                .progress_line()
                .expect("line")
                .contains("abcdefghijklmnop")
        );
        assert_eq!(buffer.total_bytes(), output.len() as u64);
    }
    let mut buffer = TaskOutputBuffer::new(1, 1);
    for line in [
        concat!("-----BEGIN ", "PRIVATE KEY-----\n"),
        "AA==\n",
        "-----END PRIVATE KEY-----\n",
    ] {
        buffer.append(line.as_bytes());
        assert_eq!(buffer.progress_line(), Some("[REDACTED:private_key]"));
    }
    buffer.append(b"back to work\npartial-secret");
    assert_eq!(buffer.progress_line(), Some("back to work"));
}

#[test]
fn activity_line_is_utf8_bounded_and_overlong_lines_fail_closed() {
    let mut buffer = TaskOutputBuffer::new(0, 0);
    buffer.append(format!("{}\n", "界".repeat(100)).as_bytes());
    assert_eq!(buffer.progress_line().expect("line").len(), 255);
    buffer.append(&[b'x'; 16384]);
    buffer.append(b"\n");
    assert_eq!(buffer.progress_line(), None);
    assert!(buffer.progress_stdout.pending.len() <= 4096);
    buffer.append(b"short material after unknown oversized line\n");
    assert_eq!(buffer.progress_line(), Some("[REDACTED:private_key]"));
}

#[test]
fn activity_normalizes_terminal_escapes_before_secret_redaction() {
    let mut buffer = TaskOutputBuffer::new(0, 0);
    buffer.append(b"\x1b[32mchecking\x1b[0m\n");
    assert_eq!(buffer.progress_line(), Some("checking"));
    buffer.append(b"sk-abcdef\x1b[32mghijklmnopQRSTUV\x1b[0m\n");
    assert!(
        buffer
            .progress_line()
            .expect("redacted line")
            .contains("[REDACTED:")
    );
    assert!(!buffer.progress_line().expect("line").contains("abcdef"));
}

#[test]
fn activity_preserves_pem_state_hidden_by_terminal_escapes() {
    for closing_escape in [b"".as_slice(), b"\x07", b"\x1b\\"] {
        let mut buffer = TaskOutputBuffer::new(0, 0);
        buffer.append(concat!("\x1b]0;-----BEGIN ", "PRIVATE KEY-----\n").as_bytes());
        assert_eq!(buffer.progress_line(), Some("[REDACTED:private_key]"));
        buffer.append(b"AA==");
        buffer.append(closing_escape);
        buffer.append(b"\n");
        assert_eq!(buffer.progress_line(), Some("[REDACTED:private_key]"));
        buffer.append(b"-----END PRIVATE KEY-----\n");
        assert_eq!(buffer.progress_line(), Some("[REDACTED:private_key]"));
        buffer.append(b"checking again\n");
        assert_eq!(buffer.progress_line(), Some("checking again"));
    }
    let mut buffer = TaskOutputBuffer::new(0, 0);
    buffer.append(b"-----BE\x1b[32mGIN PRIVATE KEY-----\nAA==\n");
    assert_eq!(buffer.progress_line(), Some("[REDACTED:private_key]"));
    buffer.append(b"-----END PRIVATE KEY-----\nchecking again\n");
    assert_eq!(buffer.progress_line(), Some("checking again"));
    buffer.append(b"\x1b]0;sk-abcdefghijklmnopQRSTUV\x07suffix\n");
    assert_eq!(buffer.progress_line(), Some("[REDACTED:task_line]"));
}

#[test]
fn activity_streams_keep_partial_secrets_and_pem_state_separate() {
    use haider_protocol::item::OutputStream::{Stderr, Stdout};
    let mut buffer = TaskOutputBuffer::new(1000, 1000);
    buffer.append_stream(Stdout, b"-----BEGIN ");
    buffer.append_stream(Stderr, b"diagnostic\n");
    assert_eq!(buffer.progress_line(), Some("diagnostic"));
    buffer.append_stream(Stdout, b"PRIVATE KEY-----\nAA==\n");
    assert_eq!(buffer.progress_line(), Some("[REDACTED:private_key]"));
    buffer.append_stream(Stderr, b"stderr stays readable\n");
    assert_eq!(buffer.progress_line(), Some("stderr stays readable"));
    buffer.append_stream(Stdout, b"-----END PRIVATE KEY-----\n");
    buffer.append_stream(Stderr, b"sk-abc");
    buffer.append_stream(Stdout, b"checking\n");
    assert_eq!(buffer.progress_line(), Some("checking"));
    buffer.append_stream(Stderr, b"defghijklmnopQRSTUV\n");
    assert!(
        buffer
            .progress_line()
            .expect("safe line")
            .contains("[REDACTED:")
    );
    // Committed output is redacted per stream before it reaches the bounds:
    // stderr lines commit readable while the split stdout PEM line is still
    // pending, and the recombined marker never reaches the tail.
    let tail = buffer.tail_lossy();
    let diagnostic = tail.find("diagnostic\n").expect("stderr committed");
    let redacted = tail.find("[REDACTED:private_key]").expect("PEM redacted");
    assert!(
        diagnostic < redacted,
        "stderr commits before the split stdout line completes"
    );
    assert!(tail.contains("stderr stays readable\n"));
    assert!(!tail.contains("PRIVATE KEY"), "split PEM never commits raw");
    assert!(!tail.contains("sk-abc"), "split token never commits raw");
}
