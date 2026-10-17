#![allow(clippy::unwrap_used)]

use haider_protocol::activity::ActivityDisplay as D;
use haider_tui::app::AppModel;
use haider_tui::runtime::sync_activity_icons_persistence;
use haider_tui::settings::SettingsStore;
use haider_tui::theme::{ThemeChoice, ThemeKey};

#[test]
fn explicit_equal_display_is_persisted_for_each_mode() {
    for mode in [D::Emoji, D::Text, D::Off] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut store = Some(SettingsStore::at(path.clone()));
        let mut model = AppModel::default();
        model.activity_display = mode;
        let mut saved = 0;
        model.set_activity_display(Some(mode));
        sync_activity_icons_persistence(&mut model, &mut saved, &mut store);
        assert_eq!(model.activity_display_choice, Some(mode));
        assert_eq!(saved, model.activity_display_commits);
        assert!(model.flash.as_ref().unwrap().ends_with(" saved"));
        assert_eq!(SettingsStore::at(path).load_activity_icons(), Some(mode));
    }
}

#[test]
fn failed_rename_write_and_parent_creation_are_visible_and_literal_retry_succeeds() {
    for failure in ["rename", "write", "parent"] {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("profile");
        let path = parent.join("tui-settings.json");
        if failure == "parent" {
            std::fs::write(&parent, "file").unwrap();
        } else {
            std::fs::create_dir(&parent).unwrap();
            std::fs::create_dir(if failure == "rename" {
                path.clone()
            } else {
                path.with_extension("json.tmp")
            })
            .unwrap();
        }
        let mut store = Some(SettingsStore::at(path.clone()));
        let mut model = AppModel::default();
        let mut saved = 0;
        model.set_activity_display(Some(D::Text));
        sync_activity_icons_persistence(&mut model, &mut saved, &mut store);
        assert_eq!(saved, 0, "{failure}");
        assert!(model.flash.as_ref().unwrap().contains("Could not save"));
        let flash = model.flash.clone();
        sync_activity_icons_persistence(&mut model, &mut saved, &mut store);
        assert_eq!(model.flash, flash); // no IO retry on every animation frame
        std::fs::remove_dir_all(&parent)
            .or_else(|_| std::fs::remove_file(&parent))
            .unwrap();
        model.set_activity_display(Some(D::Text));
        sync_activity_icons_persistence(&mut model, &mut saved, &mut store);
        assert_eq!(saved, 2);
        assert!(model.flash.as_ref().unwrap().ends_with(" saved"));
        assert_eq!(SettingsStore::at(path).load_activity_icons(), Some(D::Text));
    }
}

#[test]
fn direct_store_retry_keeps_dirty_intent_and_other_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let theme = ThemeChoice::Fixed(ThemeKey::Dark);
    let mut store = SettingsStore::at(path.clone());
    store.set_notifications(false);
    store.set_last_model(Some(("fake".into(), "fixture".into())));
    store.set_verbosity(haider_tui::toolfold::Verbosity::Verbose);
    std::fs::create_dir(&path).unwrap();
    assert!(!store.save_activity_icons_if_changed(theme, D::Text));
    assert!(store.activity_icons_dirty());
    std::fs::remove_dir(&path).unwrap();
    assert!(store.save_activity_icons_if_changed(theme, D::Text));
    assert!(!store.activity_icons_dirty());
    assert!(store.save_activity_icons_if_changed(theme, D::Off));
    assert!(store.save_activity_icons_if_changed(theme, D::Text));
    let reread = SettingsStore::at(path);
    assert_eq!(reread.load(), Some(theme));
    assert!(!reread.load_notifications());
    assert_eq!(
        reread.load_verbosity(),
        haider_tui::toolfold::Verbosity::Verbose
    );
    assert_eq!(reread.load_activity_icons(), Some(D::Text));
    assert_eq!(
        reread.load_last_model(),
        Some(("fake".into(), "fixture".into()))
    );
}

#[test]
fn no_store_reports_failure_and_retry_after_store_available_saves() {
    let mut model = AppModel::default();
    let mut settings = None;
    let mut saved = 0;
    model.set_activity_display(Some(D::Emoji));
    sync_activity_icons_persistence(&mut model, &mut saved, &mut settings);
    assert_eq!(saved, 0);
    assert!(model.flash.as_ref().unwrap().contains("Could not save"));
    let dir = tempfile::tempdir().unwrap();
    settings = Some(SettingsStore::at(dir.path().join("settings.json")));
    model.set_activity_display(Some(D::Emoji));
    sync_activity_icons_persistence(&mut model, &mut saved, &mut settings);
    assert_eq!(saved, 2);
}

#[test]
fn already_saved_equal_choice_rechecks_the_actual_write_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let theme = ThemeChoice::Fixed(ThemeKey::Dark);
    let mut store = SettingsStore::at(path.clone());
    assert!(store.save_activity_icons_if_changed(theme, D::Text));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(!store.save_activity_icons_if_changed(theme, D::Text));
    assert!(store.activity_icons_dirty());
    std::fs::remove_dir(&path).unwrap();
    assert!(store.save_activity_icons_if_changed(theme, D::Text));
    assert!(!store.activity_icons_dirty());
    assert_eq!(SettingsStore::at(path).load_activity_icons(), Some(D::Text));
}

#[cfg(unix)]
#[test]
fn unwritable_parent_keeps_setting_dirty_until_identical_retry() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("profile");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("settings.json");
    let mut store = SettingsStore::at(path.clone());
    let theme = ThemeChoice::Fixed(ThemeKey::Dark);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let saved = store.save_activity_icons_if_changed(theme, D::Text);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!saved);
    assert!(store.activity_icons_dirty());
    assert!(store.save_activity_icons_if_changed(theme, D::Text));
    assert!(!store.activity_icons_dirty());
    assert_eq!(SettingsStore::at(path).load_activity_icons(), Some(D::Text));
}
