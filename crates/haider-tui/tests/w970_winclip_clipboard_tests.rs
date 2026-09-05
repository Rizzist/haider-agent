//! Platform policy and honest copy/read results without touching a desktop.
#![allow(clippy::expect_used)]

use haider_tui::clipboard::{
    ClipboardContent, ClipboardSource, FakeClipboard, LocalClipboardWriter, copy_confirmation,
    local_writer_for_os, osc52,
};

#[test]
fn local_writer_selects_windows_arboard_and_preserves_unix_behavior() {
    assert_eq!(
        local_writer_for_os("windows"),
        LocalClipboardWriter::WindowsArboard
    );
    for os in ["macos", "linux", "freebsd"] {
        assert_eq!(local_writer_for_os(os), LocalClipboardWriter::Pbcopy);
    }
}

#[test]
fn copy_flash_distinguishes_confirmed_local_osc_only_and_total_failure() {
    assert_eq!(copy_confirmation(true, true), "· copied");
    assert_eq!(copy_confirmation(true, false), "· copied");
    assert_eq!(
        copy_confirmation(false, true),
        "· copy unconfirmed — sent via OSC 52 only"
    );
    assert_eq!(
        copy_confirmation(false, false),
        "· copy failed — local clipboard and OSC 52 unavailable"
    );
}

#[test]
fn read_text_is_zeroizing_and_redacted_before_reaching_the_reducer() {
    let content = FakeClipboard::text_with("winclip-secret-sentinel")
        .read()
        .expect("fake clipboard readable");
    assert!(!format!("{content:?}").contains("winclip-secret-sentinel"));
    let ClipboardContent::Text(text) = content else {
        panic!("expected text");
    };
    let protected: &zeroize::Zeroizing<String> = text.zeroizing_inner();
    assert_eq!(protected.as_str(), "winclip-secret-sentinel");
}

#[test]
fn osc52_remains_an_exact_utf8_remote_mirror() {
    use base64::Engine as _;
    let text = "hello\r\n🪟 日本語";
    assert_eq!(
        osc52(text),
        format!(
            "\x1b]52;c;{}\x07",
            base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
        )
    );
}
