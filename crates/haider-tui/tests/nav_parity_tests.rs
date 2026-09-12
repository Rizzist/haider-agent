//! Navigation parity contracts for transcript search and workspace mentions.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;

use haider_tui::app::{AppEvent, AppModel, Screen};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;

#[test]
fn workspace_mentions_index_files_and_tab_accepts_the_selected_path() {
    let root = tempfile::tempdir().expect("temporary workspace");
    fs::create_dir(root.path().join("src")).expect("source directory");
    fs::write(root.path().join("src/main.rs"), "fn main() {}").expect("source file");
    fs::write(root.path().join("README.md"), "# readme").expect("readme");

    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.session_workspace_cwd = Some(root.path().to_string_lossy().into_owned());
    model.handle(common::key(KeyCode::Char('@')));
    for character in "src/".chars() {
        model.handle(common::key(KeyCode::Char(character)));
    }

    let completion = model
        .mention_completion
        .as_ref()
        .expect("@ opens file completion");
    assert!(
        completion
            .candidates
            .iter()
            .any(|path| path == "src/main.rs")
    );
    assert!(!completion.candidates.iter().any(|path| path == "README.md"));

    model.handle(common::key(KeyCode::Tab));
    assert_eq!(model.composer.text(), "@src/main.rs");
    assert!(model.mention_completion.is_none());
}

#[test]
fn ctrl_f_opens_a_transcript_search_without_mutating_the_composer() {
    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.composer.insert_str("draft");
    model.handle(common::ctrl(KeyCode::Char('f')));
    assert!(model.transcript_search.is_some());
    assert_eq!(model.composer.text(), "draft");

    model.handle(common::key(KeyCode::Char('x')));
    assert_eq!(model.transcript_search.as_ref().unwrap().query, "x");
    model.handle(common::key(KeyCode::Esc));
    assert!(model.transcript_search.is_none());

    // The control chord is explicit; a bare `f` remains normal composer input.
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::NONE,
    )));
    assert_eq!(model.composer.text(), "draftf");
}
