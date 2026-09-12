#![allow(clippy::expect_used)]

#[test]
fn pages_reconstruct_full_redacted_capture_without_utf8_loss() {
    let safe = haider_tools::redact_output_text(&format!(
        "{}\nsk-abcdefghijklmnopQRSTUV\ntail",
        "é".repeat(80_000)
    ));
    let mut cursor = 0;
    let mut output = String::new();
    loop {
        let result = super::capture_page("capture:fixture", &safe, cursor).expect("page");
        let value: serde_json::Value = serde_json::from_str(&result.preview).expect("JSON");
        output.push_str(value["chunk"].as_str().expect("chunk"));
        let next = value["next_cursor"].as_u64().expect("cursor");
        if value["exhausted"] == true {
            break;
        }
        assert!(next > cursor);
        cursor = next;
    }
    assert_eq!(output, safe);
    assert!(!output.contains("sk-"));
    assert!(super::capture_page("capture:fixture", &safe, 1).is_err());
    assert!(super::capture_page("capture:fixture", &safe, u64::MAX).is_err());
}
