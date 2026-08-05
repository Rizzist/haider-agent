//! The profile `transcription` config section (locked decision 7).
//!
//! Engine selection is EXPLICIT configuration — there is no silent fallback
//! between local and cloud (ADE parity). The section lives inside the
//! profile's `<store_dir>/config.json` next to `default_model`; readers of
//! that file tolerate unknown keys, so this section is additive. Save is a
//! read-modify-write over the raw JSON value: every key this module does not
//! own is preserved byte-meaning-identically, and the write is
//! temp-file + atomic rename.
//!
//! The Deepgram API key is NOT here — it lives in the daemon's FileVault
//! behind `transcription.secret_get/set` (UDS-only).

use std::path::Path;

use crate::SttError;

/// The profile config filename (matches `haider-client`).
pub const PROFILE_CONFIG_FILE: &str = "config.json";
/// The JSON key this module owns inside the profile config.
pub const TRANSCRIPTION_CONFIG_KEY: &str = "transcription";

/// Which engine `/talk` uses. Explicit, never inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionEngine {
    #[default]
    Local,
    Deepgram,
}

/// The durable transcription preferences.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptionConfig {
    #[serde(default)]
    pub engine: TranscriptionEngine,
    /// Haider's OWN whisper model choice; `None` defers to the ADE's
    /// `selected-model.txt` hint, then `base.en`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whisper_model_id: Option<String>,
    /// The selected Deepgram model; `None` defers to `nova-3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepgram_model: Option<String>,
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_language() -> String {
    "en".to_owned()
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            engine: TranscriptionEngine::default(),
            whisper_model_id: None,
            deepgram_model: None,
            language: default_language(),
        }
    }
}

/// Loads the section from `<store_dir>/config.json`.
///
/// Absent file or absent section → defaults. A PRESENT section that fails
/// to decode is a typed error — never silently replaced with defaults (that
/// would flip a user's explicit engine choice).
pub fn load(store_dir: &Path) -> Result<TranscriptionConfig, SttError> {
    let path = store_dir.join(PROFILE_CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TranscriptionConfig::default());
        }
        Err(error) => {
            return Err(SttError::Io(format!(
                "could not read profile config: {error}"
            )));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| SttError::Io(format!("profile config is invalid JSON: {error}")))?;
    match value.get(TRANSCRIPTION_CONFIG_KEY) {
        None | Some(serde_json::Value::Null) => Ok(TranscriptionConfig::default()),
        Some(section) => serde_json::from_value(section.clone()).map_err(|error| {
            SttError::InvalidArgument(format!("transcription config section is invalid: {error}"))
        }),
    }
}

/// Saves the section, preserving every other key in `config.json`.
pub fn save(store_dir: &Path, config: &TranscriptionConfig) -> Result<(), SttError> {
    let path = store_dir.join(PROFILE_CONFIG_FILE);
    let mut root = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|error| SttError::Io(format!("profile config is invalid JSON: {error}")))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(error) => {
            return Err(SttError::Io(format!(
                "could not read profile config: {error}"
            )));
        }
    };
    if !root.is_object() {
        return Err(SttError::Io(
            "profile config root is not a JSON object".into(),
        ));
    }
    let section = serde_json::to_value(config)
        .map_err(|error| SttError::Io(format!("could not encode transcription config: {error}")))?;
    if let Some(object) = root.as_object_mut() {
        object.insert(TRANSCRIPTION_CONFIG_KEY.to_owned(), section);
    }
    let serialized = serde_json::to_string_pretty(&root)
        .map_err(|error| SttError::Io(format!("could not encode profile config: {error}")))?;
    std::fs::create_dir_all(store_dir)
        .map_err(|error| SttError::Io(format!("could not create profile dir: {error}")))?;
    let temp = tempfile::Builder::new()
        .prefix(".config-")
        .tempfile_in(store_dir)
        .map_err(|error| SttError::Io(format!("could not stage profile config: {error}")))?;
    std::fs::write(temp.path(), serialized)
        .map_err(|error| SttError::Io(format!("could not write profile config: {error}")))?;
    temp.persist(&path)
        .map_err(|error| SttError::Io(format!("could not commit profile config: {error}")))?;
    Ok(())
}
