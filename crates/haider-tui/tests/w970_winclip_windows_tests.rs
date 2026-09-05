//! Native Windows clipboard gate: no terminal or GUI input is required.
//! One test serializes all clipboard mutations within this binary. It runs
//! normally on Windows and explicitly in xplat's Windows test job; a missing
//! or inaccessible clipboard FAILS the gate rather than skipping evidence.
#![cfg(windows)]
#![allow(clippy::expect_used)]

use std::borrow::Cow;

use haider_protocol::ids::SessionId;
use haider_tui::app::{AppEvent, AppModel, AppRequest, RuntimeMode, Screen};
use haider_tui::clipboard::{ClipboardContent, ClipboardSource, OsClipboard, copy_local};
use haider_tui::composer::PendingKind;
use haider_tui::runtime::clipboard_paste_effects;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;

fn live_session() -> AppModel {
    let mut model = common::launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned()]
        .into_iter()
        .collect();
    model.daemon_version = Some("0.0.970".to_owned());
    let session = SessionId::new("winclip-native-test");
    model.upsert_live_session(&session);
    model.open_session(&session);
    model.screen = Screen::Session;
    model.requests.clear();
    model
}

fn paste_forwarded_ctrl_v(model: &mut AppModel) {
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ClipboardRead)),
        "a forwarded Ctrl+V must request the OS read"
    );
    model.requests.clear();
    clipboard_paste_effects(model, &OsClipboard);
}

#[test]
fn native_windows_clipboard_writer_and_text_image_paste_round_trip() {
    // arboard's Windows implementation writes CF_UNICODETEXT via
    // clipboard-win/SetClipboardData. Supplementary characters exercise
    // surrogate pairs; accents, Persian and Japanese reject ANSI-only paths.
    let original = "line one\r\n🪟 café فارسی 日本語\rline three";
    assert!(copy_local(original), "native Windows write must confirm");
    let mut clipboard = arboard::Clipboard::new().expect("Windows clipboard handle");
    assert_eq!(clipboard.get_text().expect("CF_UNICODETEXT read"), original);
    let ClipboardContent::Text(read) = OsClipboard.read().expect("production read") else {
        panic!("production read must retain Unicode text");
    };
    assert_eq!(read.as_str(), original);

    let mut model = live_session();
    paste_forwarded_ctrl_v(&mut model);
    assert_eq!(
        model.composer.text(),
        "line one\n🪟 café فارسی 日本語\nline three"
    );
    assert!(model.composer.attachments().is_empty());
    assert!(model.requests.is_empty(), "pasted newlines never submit");

    // Larger than commonly restricted OSC 52 payloads: success must come
    // from the native clipboard and the large-paste pill must expand intact.
    let large = "Unicode 🪟 日本語\r\n".repeat(8_192);
    assert!(copy_local(&large));
    assert_eq!(clipboard.get_text().expect("large native read"), large);
    model.composer.clear();
    paste_forwarded_ctrl_v(&mut model);
    let display = model.composer.text().to_owned();
    assert_eq!(
        model.composer.expand_pastes(&display),
        large.replace("\r\n", "\n")
    );
    assert!(model.requests.is_empty(), "large paste is draft-local");

    // Real OS image write -> production RGBA/PNG read -> normal attachment
    // request. Distinct colors and alpha detect channel/row corruption.
    let rgba = vec![
        255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 255, 255, 0, 64,
    ];
    clipboard
        .set_image(arboard::ImageData {
            width: 2,
            height: 2,
            bytes: Cow::Borrowed(&rgba),
        })
        .expect("native Windows image write");
    let ClipboardContent::Image(image) = OsClipboard.read().expect("production image read") else {
        panic!("native image must be decoded");
    };
    assert_eq!((image.width, image.height), (2, 2));
    assert_eq!(
        image::load_from_memory(&image.png)
            .expect("production PNG")
            .to_rgba8()
            .into_raw(),
        rgba
    );
    model.composer.clear();
    paste_forwarded_ctrl_v(&mut model);
    assert_eq!(model.composer.attachments().len(), 1);
    assert!(matches!(
        &model.composer.attachments()[0].kind,
        PendingKind::Image { mime } if mime == "image/png"
    ));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::AttachUpload { .. })),
        "image bytes reach the attachment upload path"
    );

    assert!(copy_local(""), "an empty native write still succeeds");
    assert_eq!(
        OsClipboard.read().expect("empty read"),
        ClipboardContent::Empty
    );
}
