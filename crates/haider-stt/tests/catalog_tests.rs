//! Catalog and selected-model-HINT laws.

#![allow(clippy::expect_used)]

use haider_stt::catalog::{
    DEFAULT_MODEL_ID, SELECTED_MODEL_FILE, WHISPER_MODELS, default_model, effective_model,
    installed_model_ids, model_by_id, model_installed, selected_model_hint,
};

/// The shared catalog is byte-identical to the ADE's table: same ids, same
/// filenames, same URLs, same sha256 digests. Sharing depends on literal
/// equality.
///
/// MUTATION CHECK: bump a model URL to a mirror, rename a file, or touch
/// one hex digit of a digest. Expected runtime failure: the corresponding
/// literal assertion.
#[test]
fn catalog_is_byte_identical_to_the_ade_table() {
    assert_eq!(WHISPER_MODELS.len(), 3);
    let tiny = &WHISPER_MODELS[0];
    assert_eq!(tiny.id, "tiny.en");
    assert_eq!(tiny.file, "ggml-tiny.en.bin");
    assert_eq!(
        tiny.url,
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin"
    );
    assert_eq!(
        tiny.sha256,
        "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f"
    );
    assert_eq!(
        (tiny.approximate_disk_mb, tiny.approximate_memory_mb),
        (74, 260)
    );
    let base = &WHISPER_MODELS[1];
    assert_eq!(base.id, "base.en");
    assert_eq!(base.file, "ggml-base.en.bin");
    assert_eq!(
        base.url,
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
    );
    assert_eq!(
        base.sha256,
        "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002"
    );
    assert_eq!(
        (base.approximate_disk_mb, base.approximate_memory_mb),
        (142, 500)
    );
    let small = &WHISPER_MODELS[2];
    assert_eq!(small.id, "small.en");
    assert_eq!(small.file, "ggml-small.en.bin");
    assert_eq!(
        small.url,
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin"
    );
    assert_eq!(
        small.sha256,
        "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d"
    );
    assert_eq!(
        (small.approximate_disk_mb, small.approximate_memory_mb),
        (465, 1100)
    );
    assert_eq!(DEFAULT_MODEL_ID, "base.en");
    assert_eq!(default_model().id, "base.en");
}

/// Lookup is trimmed and case-insensitive (ADE `whisper_model_definition`).
#[test]
fn model_lookup_trims_and_ignores_ascii_case() {
    assert_eq!(model_by_id(" BASE.EN ").expect("hit").id, "base.en");
    assert!(model_by_id("large-v3").is_none());
}

/// The HINT law: `selected-model.txt` is read (trimmed), unknown/absent/
/// empty content yields no hint, and NOTHING in the flow writes the file.
///
/// MUTATION CHECK: make the hint reader "helpfully" rewrite a normalized id
/// back to the sidecar, or default an unknown id to `base.en` at the HINT
/// level. Expected runtime failure: the sidecar bytes change below, or the
/// unknown-id case returns a hint.
#[test]
fn selected_model_is_a_read_only_hint() {
    let dir = tempfile::tempdir().expect("temp whisper dir");
    // Absent sidecar: no hint.
    assert_eq!(selected_model_hint(dir.path()), None);
    // Trimmed known id: hint.
    let sidecar = dir.path().join(SELECTED_MODEL_FILE);
    std::fs::write(&sidecar, "  tiny.en\n").expect("write sidecar");
    assert_eq!(selected_model_hint(dir.path()).expect("hint").id, "tiny.en");
    // Unknown id: no hint (the ADE may know models Haider does not).
    std::fs::write(&sidecar, "quantum.en").expect("write sidecar");
    assert_eq!(selected_model_hint(dir.path()), None);
    // Empty: no hint.
    std::fs::write(&sidecar, "  \n").expect("write sidecar");
    assert_eq!(selected_model_hint(dir.path()), None);
    // NEVER WRITTEN: exercise the whole read surface, then compare bytes.
    // The sidecar content is deliberately NON-normalized (case + padding):
    // a "helpful" normalization write-back would produce different bytes,
    // so a byte-identical rewrite cannot slip past this law.
    std::fs::write(&sidecar, "  SMALL.EN \n").expect("write sidecar");
    let before = std::fs::read(&sidecar).expect("sidecar bytes");
    assert_eq!(
        selected_model_hint(dir.path()).expect("hint").id,
        "small.en"
    );
    let _ = effective_model(dir.path(), Some("tiny.en"));
    let _ = effective_model(dir.path(), None);
    let _ = installed_model_ids(dir.path());
    let after = std::fs::read(&sidecar).expect("sidecar bytes");
    assert_eq!(before, after, "selected-model.txt must never be written");
}

/// Effective-model precedence: OWN selection → ADE hint → default.
///
/// MUTATION CHECK: prefer the ADE hint over Haider's own selection (or
/// skip the hint tier). Expected runtime failure: one of the three
/// precedence assertions.
#[test]
fn effective_model_prefers_own_selection_then_hint_then_default() {
    let dir = tempfile::tempdir().expect("temp whisper dir");
    std::fs::write(dir.path().join(SELECTED_MODEL_FILE), "small.en").expect("write sidecar");
    assert_eq!(effective_model(dir.path(), Some("tiny.en")).id, "tiny.en");
    assert_eq!(effective_model(dir.path(), None).id, "small.en");
    assert_eq!(effective_model(dir.path(), Some("unknown")).id, "small.en");
    std::fs::remove_file(dir.path().join(SELECTED_MODEL_FILE)).expect("remove sidecar");
    assert_eq!(effective_model(dir.path(), None).id, "base.en");
}

/// Installed-state answers are per-call filesystem truth (evictable dir).
#[test]
fn installed_models_reflect_live_filesystem_truth() {
    let dir = tempfile::tempdir().expect("temp whisper dir");
    assert!(installed_model_ids(dir.path()).is_empty());
    std::fs::write(dir.path().join("ggml-base.en.bin"), b"stub").expect("fake model");
    assert_eq!(installed_model_ids(dir.path()), vec!["base.en"]);
    assert!(model_installed(dir.path(), &WHISPER_MODELS[1]));
    std::fs::remove_file(dir.path().join("ggml-base.en.bin")).expect("evict model");
    assert!(!model_installed(dir.path(), &WHISPER_MODELS[1]));
}
