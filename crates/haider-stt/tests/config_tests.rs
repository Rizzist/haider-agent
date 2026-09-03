//! Profile `transcription` section laws: explicit engine selection,
//! tolerant absence, typed invalidity, and key-preserving saves.

#![allow(clippy::expect_used)]

use haider_stt::SttError;
use haider_stt::config::{
    PROFILE_CONFIG_FILE, TRANSCRIPTION_CONFIG_KEY, TranscriptionConfig, TranscriptionEngine, load,
    save,
};

/// Absent file and absent section load the defaults (local engine, `en`).
#[test]
fn absent_config_loads_defaults() {
    let dir = tempfile::tempdir().expect("dir");
    let config = load(dir.path()).expect("absent file defaults");
    assert_eq!(config.engine, TranscriptionEngine::Local);
    assert_eq!(config.language, "en");
    assert_eq!(config.whisper_model_id, None);
    assert_eq!(config.deepgram_model, None);
    assert!(!config.auto_send, "dictation auto-send ships OFF");
    std::fs::write(
        dir.path().join(PROFILE_CONFIG_FILE),
        r#"{"default_model": "claude-fable-5"}"#,
    )
    .expect("config without section");
    assert_eq!(
        load(dir.path()).expect("absent section defaults"),
        TranscriptionConfig::default()
    );
}

/// 970 owner requirement 1 — DICTATION NEVER AUTO-SENDS unless the
/// profile says so explicitly. A section that predates the key (every
/// existing user's config) must read as OFF, not as "unset means submit".
///
/// MUTATION CHECK: give `auto_send` a `default = "..."` of `true`, or drop
/// its `serde(default)`. Expected failure: the legacy section below either
/// loads as auto-sending or fails to decode at all.
#[test]
fn a_section_without_auto_send_reads_as_off() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join(PROFILE_CONFIG_FILE),
        r#"{"transcription": {"engine": "deepgram", "language": "en-US"}}"#,
    )
    .expect("legacy section");
    let config = load(dir.path()).expect("legacy section loads");
    assert_eq!(config.engine, TranscriptionEngine::Deepgram);
    assert!(
        !config.auto_send,
        "a config written before the key existed must not auto-send"
    );
}

/// The engine tag is the locked serde contract: `local` / `deepgram`.
#[test]
fn engine_serde_tags_are_locked() {
    assert_eq!(
        serde_json::to_value(TranscriptionEngine::Local).expect("encode"),
        serde_json::json!("local")
    );
    assert_eq!(
        serde_json::to_value(TranscriptionEngine::Deepgram).expect("encode"),
        serde_json::json!("deepgram")
    );
}

/// EXPLICIT-SELECTION LAW: a PRESENT but invalid section is a typed error —
/// silently replacing it with defaults would flip the user's engine choice.
///
/// MUTATION CHECK: map a corrupt section to `TranscriptionConfig::default()`.
/// Expected runtime failure: the load below succeeds with `Local` instead
/// of erroring.
#[test]
fn corrupt_section_is_a_typed_error_never_silent_defaults() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join(PROFILE_CONFIG_FILE),
        r#"{"transcription": {"engine": "telepathy"}}"#,
    )
    .expect("corrupt section");
    let error = load(dir.path()).expect_err("unknown engine must not default");
    assert!(matches!(error, SttError::InvalidArgument(_)));
}

/// Save preserves every foreign key in `config.json` and roundtrips the
/// section.
///
/// MUTATION CHECK: serialize a fresh `{transcription: …}` object instead of
/// read-modify-write. Expected runtime failure: `default_model` (and the
/// unknown future key) vanish below.
#[test]
fn save_preserves_foreign_keys_and_roundtrips() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join(PROFILE_CONFIG_FILE),
        r#"{"default_model": "claude-fable-5", "future_key": {"nested": true}}"#,
    )
    .expect("seed config");
    let config = TranscriptionConfig {
        engine: TranscriptionEngine::Deepgram,
        whisper_model_id: Some("tiny.en".into()),
        deepgram_model: Some("nova-3".into()),
        language: "en-US".into(),
        // 970: the explicit dictation auto-send opt-in round-trips
        // through save/load like every other key this section owns.
        auto_send: true,
    };
    save(dir.path(), &config).expect("save succeeds");
    let reloaded = load(dir.path()).expect("reload");
    assert_eq!(reloaded, config);
    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(PROFILE_CONFIG_FILE)).expect("raw"),
    )
    .expect("valid JSON");
    assert_eq!(raw["default_model"], "claude-fable-5");
    assert_eq!(raw["future_key"]["nested"], true);
    assert_eq!(raw[TRANSCRIPTION_CONFIG_KEY]["engine"], "deepgram");
    // Save into an empty dir creates the file from scratch.
    let fresh = tempfile::tempdir().expect("fresh dir");
    save(fresh.path(), &TranscriptionConfig::default()).expect("fresh save");
    assert_eq!(
        load(fresh.path()).expect("fresh load"),
        TranscriptionConfig::default()
    );
}
