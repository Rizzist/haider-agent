//! Android has no native clipboard backend; terminal paste/copy remain usable.

use haider_tui::app::ImageNotice;
use haider_tui::clipboard::{
    ClipboardError, FakeClipboard, LocalClipboardWriter, local_writer_for_os,
};
use haider_tui::runtime::clipboard_paste_effects;

mod common;

#[test]
fn android_declines_the_local_clipboard_writer() {
    assert_eq!(
        local_writer_for_os("android"),
        LocalClipboardWriter::Unavailable
    );
}

#[test]
fn unavailable_clipboard_reports_a_notice_and_preserves_the_draft() {
    let mut model = common::launcher_model();
    model.composer.set_text("keep this draft".to_owned());
    model.requests.clear();
    clipboard_paste_effects(&mut model, &FakeClipboard(Err(ClipboardError::Unavailable)));
    assert_eq!(model.composer.text(), "keep this draft");
    assert!(model.composer.attachments().is_empty());
    assert!(model.requests.is_empty());
    assert_eq!(
        model.composer_notice,
        Some(ImageNotice::ClipboardUnreadable {
            note: "clipboard unavailable on this platform".to_owned(),
        })
    );
}

// --no-default-features executes the same fallback on a desktop test host.
#[cfg(any(not(feature = "desktop-clipboard"), target_os = "android"))]
#[test]
fn native_clipboard_without_a_backend_returns_typed_unavailable() {
    use haider_tui::clipboard::{ClipboardSource, OsClipboard};

    assert_eq!(OsClipboard.read(), Err(ClipboardError::Unavailable));
}
