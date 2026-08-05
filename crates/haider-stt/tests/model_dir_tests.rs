//! Literal-path laws for the ADE-byte-compatible model-dir resolver.
//!
//! Every expectation below is a LITERAL path: the resolver must reproduce
//! the Diff Forge ADE's `cloud_mcp_native_data_root() + "whisper"` byte for
//! byte on all three platforms, both env overrides, and the lowercase-Linux
//! trap. A "reasonable-looking" divergence here silently forks the shared
//! model store.

#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use haider_stt::model_dir::{
    DATA_DIR_ENV, HOME_ENV, Platform, WHISPER_DIR_NAME, resolve_data_root, resolve_whisper_dir,
};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), OsString::from(*value)))
        .collect();
    move |key: &str| map.get(key).cloned()
}

/// MUTATION CHECK: append `DiffForge` (or `whisper` twice) to the
/// `RUST_DIFFFORGE_DATA_DIR` override, or reorder it after
/// `RUST_DIFFFORGE_HOME`. Expected runtime failure: the literal override
/// paths below stop matching.
#[test]
fn data_dir_env_override_wins_verbatim_on_every_platform() {
    for platform in [Platform::MacOs, Platform::Windows, Platform::Linux] {
        let resolved = resolve_whisper_dir(
            platform,
            &env(&[
                (DATA_DIR_ENV, "/custom/forge-data"),
                (HOME_ENV, "/custom/forge-home"),
                ("HOME", "/Users/kim"),
                ("XDG_DATA_HOME", "/xdg"),
                ("APPDATA", "C:/Users/kim/AppData/Roaming"),
            ]),
        );
        assert_eq!(
            resolved,
            Some(PathBuf::from("/custom/forge-data/whisper")),
            "platform {platform:?}"
        );
    }
}

/// MUTATION CHECK: drop the `RUST_DIFFFORGE_HOME` tier or suffix it with
/// `DiffForge`. Expected runtime failure: the literal second-override path
/// stops matching.
#[test]
fn home_env_override_is_second_and_verbatim() {
    for platform in [Platform::MacOs, Platform::Windows, Platform::Linux] {
        let resolved = resolve_whisper_dir(
            platform,
            &env(&[
                (HOME_ENV, "/custom/forge-home"),
                ("HOME", "/Users/kim"),
                ("XDG_DATA_HOME", "/xdg"),
                ("APPDATA", "C:/Users/kim/AppData/Roaming"),
            ]),
        );
        assert_eq!(
            resolved,
            Some(PathBuf::from("/custom/forge-home/whisper")),
            "platform {platform:?}"
        );
    }
}

/// MUTATION CHECK: treat an empty env value as set. Expected runtime
/// failure: an empty `RUST_DIFFFORGE_DATA_DIR` masks the macOS default
/// below (ADE `cloud_mcp_env_path` filters empty values).
#[test]
fn empty_env_values_are_unset() {
    let resolved = resolve_whisper_dir(
        Platform::MacOs,
        &env(&[(DATA_DIR_ENV, ""), (HOME_ENV, ""), ("HOME", "/Users/kim")]),
    );
    assert_eq!(
        resolved,
        Some(PathBuf::from(
            "/Users/kim/Library/Application Support/DiffForge/whisper"
        ))
    );
}

/// The macOS literal: `~/Library/Application Support/DiffForge/whisper` —
/// capital-D capital-F `DiffForge`, space in `Application Support`.
///
/// MUTATION CHECK: lowercase `DiffForge` on macOS (symmetry with Linux) or
/// resolve `~/Library/ApplicationSupport`. Expected runtime failure: the
/// literal below.
#[test]
fn macos_default_is_the_literal_application_support_diffforge() {
    let resolved = resolve_whisper_dir(Platform::MacOs, &env(&[("HOME", "/Users/kim")]));
    assert_eq!(
        resolved,
        Some(PathBuf::from(
            "/Users/kim/Library/Application Support/DiffForge/whisper"
        ))
    );
}

/// macOS home falls back to `USERPROFILE` (ADE `cloud_mcp_home_dir`).
#[test]
fn home_lookup_falls_back_to_userprofile() {
    let resolved = resolve_whisper_dir(Platform::MacOs, &env(&[("USERPROFILE", "/Users/kim")]));
    assert_eq!(
        resolved,
        Some(PathBuf::from(
            "/Users/kim/Library/Application Support/DiffForge/whisper"
        ))
    );
}

/// The Windows literals: `%APPDATA%\DiffForge\whisper`, falling back
/// `%LOCALAPPDATA%\DiffForge\whisper`.
///
/// MUTATION CHECK: prefer LOCALAPPDATA over APPDATA, or drop the fallback.
/// Expected runtime failure: one of the two literals below.
#[test]
fn windows_default_prefers_appdata_then_localappdata() {
    let both = resolve_whisper_dir(
        Platform::Windows,
        &env(&[
            ("APPDATA", "C:/Users/kim/AppData/Roaming"),
            ("LOCALAPPDATA", "C:/Users/kim/AppData/Local"),
        ]),
    );
    assert_eq!(
        both,
        Some(PathBuf::from(
            "C:/Users/kim/AppData/Roaming/DiffForge/whisper"
        ))
    );
    let local_only = resolve_whisper_dir(
        Platform::Windows,
        &env(&[("LOCALAPPDATA", "C:/Users/kim/AppData/Local")]),
    );
    assert_eq!(
        local_only,
        Some(PathBuf::from(
            "C:/Users/kim/AppData/Local/DiffForge/whisper"
        ))
    );
}

/// THE LOWERCASE-LINUX TRAP: the Linux directory is `diffforge`, all
/// lowercase — NOT `DiffForge` like macOS/Windows.
///
/// MUTATION CHECK: reuse the `DiffForge` casing on Linux. Expected runtime
/// failure: both lowercase literals below.
#[test]
fn linux_default_is_lowercase_diffforge_via_xdg_then_local_share() {
    let xdg = resolve_whisper_dir(
        Platform::Linux,
        &env(&[("XDG_DATA_HOME", "/home/kim/.data"), ("HOME", "/home/kim")]),
    );
    assert_eq!(
        xdg,
        Some(PathBuf::from("/home/kim/.data/diffforge/whisper"))
    );
    let fallback = resolve_whisper_dir(Platform::Linux, &env(&[("HOME", "/home/kim")]));
    assert_eq!(
        fallback,
        Some(PathBuf::from("/home/kim/.local/share/diffforge/whisper"))
    );
}

/// No override and no home resolves to NOTHING — an honest absence, never a
/// guessed path.
#[test]
fn no_home_and_no_override_resolves_to_none() {
    for platform in [Platform::MacOs, Platform::Windows, Platform::Linux] {
        assert_eq!(resolve_data_root(platform, &env(&[])), None);
    }
}

/// The whisper subdirectory name is the ADE's literal `whisper`.
#[test]
fn whisper_subdir_is_literal() {
    assert_eq!(WHISPER_DIR_NAME, "whisper");
}
