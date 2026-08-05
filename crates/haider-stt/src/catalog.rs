//! ADE-parity whisper model catalog and the selected-model HINT.
//!
//! The three-model table below is byte-identical to the ADE's
//! `WHISPER_MODEL_OPTIONS` (rust-diffforge `src-tauri/src/lib.rs:301-337`):
//! same ids, same `ggml-<id>.bin` filenames, same Hugging Face URLs, same
//! sha256 digests. Sharing depends on this literal equality — a renamed file
//! or a drifted digest would re-download or reject the model the ADE already
//! installed.
//!
//! `selected-model.txt` law (locked decision 3): Haider READS the ADE's
//! selection sidecar as a default hint and NEVER writes it. Nothing in this
//! crate may flip the ADE's selection; grep-proof: this module contains the
//! only `SELECTED_MODEL_FILE` consumers and none of them write.

use std::path::{Path, PathBuf};

/// The ADE's default model id.
pub const DEFAULT_MODEL_ID: &str = "base.en";
/// The ADE's selection sidecar filename inside the whisper dir.
pub const SELECTED_MODEL_FILE: &str = "selected-model.txt";

/// One installable whisper.cpp model (ADE `WhisperModelDefinition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WhisperModel {
    pub id: &'static str,
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub approximate_disk_mb: u64,
    pub approximate_memory_mb: u64,
    pub tier: &'static str,
    pub description: &'static str,
}

/// The shared catalog — byte-identical to the ADE's table.
pub const WHISPER_MODELS: &[WhisperModel] = &[
    WhisperModel {
        id: "tiny.en",
        name: "Whisper tiny.en",
        file: "ggml-tiny.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
        approximate_disk_mb: 74,
        approximate_memory_mb: 260,
        tier: "Fastest",
        description: "Lowest footprint",
    },
    WhisperModel {
        id: "base.en",
        name: "Whisper base.en",
        file: "ggml-base.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        approximate_disk_mb: 142,
        approximate_memory_mb: 500,
        tier: "Balanced",
        description: "Current default",
    },
    WhisperModel {
        id: "small.en",
        name: "Whisper small.en",
        file: "ggml-small.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin",
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        approximate_disk_mb: 465,
        approximate_memory_mb: 1100,
        tier: "Higher accuracy",
        description: "Larger local model",
    },
];

/// Looks up a model by id — trimmed, ASCII-case-insensitive (ADE parity).
#[must_use]
pub fn model_by_id(model_id: &str) -> Option<&'static WhisperModel> {
    WHISPER_MODELS
        .iter()
        .find(|model| model.id.eq_ignore_ascii_case(model_id.trim()))
}

/// The default model definition (`base.en`).
#[must_use]
pub fn default_model() -> &'static WhisperModel {
    model_by_id(DEFAULT_MODEL_ID).unwrap_or(&WHISPER_MODELS[0])
}

/// The on-disk path of one model inside the shared whisper dir.
#[must_use]
pub fn model_path(whisper_dir: &Path, model: &WhisperModel) -> PathBuf {
    whisper_dir.join(model.file)
}

/// Whether one model's file is installed right now.
///
/// Answered per call, never cached: the ADE's uninstall can evict the whole
/// directory at any moment.
#[must_use]
pub fn model_installed(whisper_dir: &Path, model: &WhisperModel) -> bool {
    model_path(whisper_dir, model).is_file()
}

/// Ids of every catalog model currently installed in the shared dir.
#[must_use]
pub fn installed_model_ids(whisper_dir: &Path) -> Vec<&'static str> {
    WHISPER_MODELS
        .iter()
        .filter(|model| model_installed(whisper_dir, model))
        .map(|model| model.id)
        .collect()
}

/// Reads the ADE's `selected-model.txt` as a HINT.
///
/// `None` when the sidecar is absent, unreadable, empty, or names a model
/// outside the catalog — the caller falls back to its own default. This
/// function (and this crate) never writes the sidecar.
#[must_use]
pub fn selected_model_hint(whisper_dir: &Path) -> Option<&'static WhisperModel> {
    let raw = std::fs::read_to_string(whisper_dir.join(SELECTED_MODEL_FILE)).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    model_by_id(trimmed)
}

/// Resolves the model a session should use: Haider's OWN configured
/// selection first, then the ADE hint, then the packaged default.
#[must_use]
pub fn effective_model(whisper_dir: &Path, own_selection: Option<&str>) -> &'static WhisperModel {
    own_selection
        .and_then(model_by_id)
        .or_else(|| selected_model_hint(whisper_dir))
        .unwrap_or_else(default_model)
}
