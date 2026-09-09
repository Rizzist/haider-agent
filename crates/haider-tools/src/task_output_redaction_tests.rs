use super::TaskOutputBuffer;
use haider_protocol::item::OutputStream;

#[test]
fn live_partial_lines_do_not_commit_unclassified_bytes() {
    let input = b"sk-abcdefghijklmnopQRSTUV\n-----BEGIN\x20PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\nready";
    let safe = crate::redact_output_text(&String::from_utf8_lossy(input));
    for boundary in 0..=input.len() {
        let mut buffer = TaskOutputBuffer::new(4096, 4096);
        buffer.append_stream(OutputStream::Stdout, &input[..boundary]);
        // A read never finalizes the actual redactor's line or PEM state.
        let _ = buffer.live_snapshot();
        buffer.append_stream(OutputStream::Stdout, &input[boundary..]);
        let live = buffer.live_snapshot();
        assert_eq!(live.tail_lossy(), safe, "boundary {boundary}");
        assert!(!buffer.tail_lossy().ends_with("ready"));
        buffer.finish_streams();
        assert_eq!(buffer.retained(), safe.as_bytes());
        assert_eq!(buffer.output_sha256(), live.output_sha256());
    }
}

#[test]
fn live_ready_prompt_remains_immediate_without_newline() {
    let mut buffer = TaskOutputBuffer::new(4096, 4096);
    buffer.append_stream(OutputStream::Stdout, b"ready");
    let live = buffer.live_snapshot();
    assert_eq!(live.total_bytes(), 5);
    assert_eq!(live.tail_lossy(), "ready");
    assert_eq!(live.read_from(2, 4096), (b"ady".to_vec(), 5));
    assert_eq!(
        buffer.total_bytes(),
        0,
        "unfinished output is not committed"
    );
}
