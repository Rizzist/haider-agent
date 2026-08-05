//! CHARTER — Haider's speech-to-text engine crate (T1).
//!
//! What lives here: the Diff Forge ADE-byte-compatible whisper model-dir
//! resolver and model manager, the cpal capture worker with its deterministic
//! DSP core, the local whisper.cpp CLI-spawn engine (no FFI, no unsafe, no
//! linking), and the Deepgram streaming engine — both emitting one
//! provider-agnostic [`TranscriptFrame`] stream. What may NOT live here: UI
//! (the TUI wave/talk UX is T2), wire frames (the Deepgram key rides
//! `haider-rpc`'s `transcription.secret_get/set`, defined there and served by
//! the daemon), and any write to the ADE's `selected-model.txt` (the shared
//! selection is read as a HINT, never flipped from Haider).
//!
//! Shared-install contract (locked decision 3): models live in the exact ADE
//! directory (`model_dir`), with the exact filenames, URLs, and sha256 table
//! (`catalog`), installed with the exact `.download` → verify → atomic-rename
//! discipline (`download`). Models are evictable at any moment — the ADE's
//! uninstall deletes the whole dir — so "model missing" is a first-class
//! typed state ([`SttError::ModelMissing`]), never a crash.

pub mod capture;
pub mod catalog;
pub mod chunker;
pub mod config;
pub mod deepgram;
pub mod download;
pub mod local;
pub mod model_dir;
pub mod policy;
pub mod runtime;

/// Which engine produced a transcript frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// Local whisper.cpp CLI (pseudo-streaming chunker).
    WhisperLocal,
    /// Deepgram `/v1/listen` streaming WebSocket.
    Deepgram,
}

impl EngineKind {
    /// The stable provider label carried on transcript frames (ADE parity:
    /// `whisper-local` / `deepgram`).
    #[must_use]
    pub const fn provider_label(self) -> &'static str {
        match self {
            Self::WhisperLocal => "whisper-local",
            Self::Deepgram => "deepgram",
        }
    }
}

/// One provider-agnostic partial/final transcript frame.
///
/// Local whisper emits CUMULATIVE assembled text per chunk with
/// `is_final: false`, plus one final cumulative frame when the session ends
/// with text; Deepgram emits interim frames (`is_final: false`, each
/// replacing the previous interim) and final segments (`is_final: true`).
/// The definitive assembled session text is the engine's `finish()` result,
/// not a frame.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptFrame {
    pub provider: EngineKind,
    pub text: String,
    pub is_final: bool,
    pub speech_final: bool,
}

/// Final result of one transcription session (ADE
/// `WhisperTranscriptionResult` shape, deliberately provider-agnostic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptionResult {
    pub text: String,
    pub segments: usize,
    pub duration_ms: u64,
}

/// Typed error surface for every engine seam.
///
/// Honest first-class states, never a panic: a missing model or runtime is a
/// recoverable "reinstall?" state, a denied mic is a hint-carrying state, and
/// no variant ever carries secret bytes (API keys are named only as "the
/// Deepgram API key", transcript text never enters an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttError {
    /// The whisper model file is not installed (or was evicted by the ADE).
    ModelMissing { model_id: String },
    /// No whisper-cli runtime could be discovered; carries the per-OS
    /// install hint.
    RuntimeMissing { hint: String },
    /// Audio capture could not start or produced no signal; carries an
    /// honest hint (macOS TCC: grant the TERMINAL app mic access).
    MicUnavailable { hint: String },
    /// Caller-supplied value refused before any I/O (empty key, bad
    /// language, oversized audio).
    InvalidArgument(String),
    /// Download/transfer failure (network, HTTP status, interrupted body).
    Download(String),
    /// Downloaded bytes did not match the pinned sha256; the temp file was
    /// removed and nothing was installed.
    ChecksumMismatch { expected: String, actual: String },
    /// The credential was rejected by the remote endpoint (HTTP 401/403).
    Unauthorized(String),
    /// A remote endpoint answered with a non-auth failure.
    Endpoint(String),
    /// Transport-level failure (socket, TLS, WebSocket).
    Transport(String),
    /// The operation was canceled by its cancel token.
    Canceled,
    /// A bounded wait elapsed.
    Timeout(String),
    /// Local filesystem/process failure.
    Io(String),
}

impl std::fmt::Display for SttError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelMissing { model_id } => write!(
                formatter,
                "whisper model `{model_id}` is not installed — download it to continue"
            ),
            Self::RuntimeMissing { hint } => {
                write!(formatter, "whisper.cpp CLI is not installed: {hint}")
            }
            Self::MicUnavailable { hint } => write!(formatter, "microphone unavailable: {hint}"),
            Self::InvalidArgument(message) => formatter.write_str(message),
            Self::Download(message) => write!(formatter, "download failed: {message}"),
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "downloaded file failed checksum verification (expected {expected}, got {actual})"
            ),
            Self::Unauthorized(message) => formatter.write_str(message),
            Self::Endpoint(message) => formatter.write_str(message),
            Self::Transport(message) => formatter.write_str(message),
            Self::Canceled => formatter.write_str("transcription canceled"),
            Self::Timeout(message) => write!(formatter, "timed out: {message}"),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SttError {}
