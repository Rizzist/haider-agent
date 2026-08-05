//! Whisper hallucination policy + transcript normalization (ADE parity).
//!
//! Ports the ADE's compiled-in `WhisperTranscriptPolicy` defaults
//! (rust-diffforge `src-tauri/src/audio.rs:4196-4312`): drop transcripts of
//! sub-900 ms / RMS < 0.01 / peak < 0.02 captures, suppress bracketed
//! no-speech markers ≤ 24 chars, and suppress the classic low-energy "you"
//! hallucination (≤ 4 chars, 1 word, only on low-energy captures). Warm-up
//! noise on stderr/stdout is filtered by the ADE's prefix family before any
//! policy decision.

/// Insert ceiling shared with the ADE (`MAX_AUDIO_TRANSCRIPT_INSERT_CHARS`).
pub const MAX_TRANSCRIPT_INSERT_CHARS: usize = 8_000;

/// Whisper runtime warm-up/deprecation noise detector (ADE
/// `is_whisper_runtime_warning_line`).
#[must_use]
pub fn is_runtime_warning_line(line: &str) -> bool {
    let lowercase = line.trim().to_lowercase();
    lowercase.contains("the binary 'main.exe' is deprecated")
        || lowercase.contains("the binary \"main.exe\" is deprecated")
        || lowercase.contains("the binary 'main' is deprecated")
        || lowercase.contains("the binary \"main\" is deprecated")
        || lowercase.contains("deprecation-warning")
        || lowercase.starts_with("load_backend:")
        || lowercase.starts_with("whisper_init")
        || lowercase.starts_with("ggml_")
}

/// Drops warning lines and collapses whitespace to single spaces (ADE
/// `normalize_transcript_text`).
#[must_use]
pub fn normalize_transcript_text(text: &str) -> String {
    text.lines()
        .filter(|line| !is_runtime_warning_line(line))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Capture stats accompanying one transcribed chunk. `None` means unknown,
/// which the policy treats as passing (ADE `unwrap_or(MAX)` semantics).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CaptureStats {
    pub audio_ms: Option<u64>,
    pub rms: Option<f32>,
    pub peak: Option<f32>,
}

/// The hallucination policy (ADE `WhisperTranscriptPolicy` defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptPolicy {
    pub audio_ms_min_for_speech_ms: Option<u64>,
    pub capture_rms_min_for_speech: Option<f32>,
    pub capture_peak_min_for_speech: Option<f32>,
    pub suppress_bracketed_markers: bool,
    pub suppress_bracketed_markers_max_chars: usize,
    pub no_speech_markers: Vec<String>,
    pub low_energy_suppressed_tokens: Vec<String>,
    pub low_energy_max_chars: usize,
    pub low_energy_max_words: usize,
}

impl Default for TranscriptPolicy {
    fn default() -> Self {
        Self {
            audio_ms_min_for_speech_ms: Some(900),
            capture_rms_min_for_speech: Some(0.01),
            capture_peak_min_for_speech: Some(0.02),
            suppress_bracketed_markers: true,
            suppress_bracketed_markers_max_chars: 24,
            no_speech_markers: vec![
                "[BLANK_AUDIO]".into(),
                "[BLANK]".into(),
                "[SILENCE]".into(),
                "[NOISE]".into(),
                "[MUSIC]".into(),
            ],
            low_energy_suppressed_tokens: vec!["you".into()],
            low_energy_max_chars: 4,
            low_energy_max_words: 1,
        }
    }
}

/// Why a transcript was dropped (stable reason labels, ADE parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    EmptyTranscript,
    NoSpeechMarker,
    BracketedMarker,
    LowEnergyShortToken,
}

impl DropReason {
    /// The ADE's stable reason string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyTranscript => "empty_transcript",
            Self::NoSpeechMarker => "no_speech_marker",
            Self::BracketedMarker => "bracketed_marker",
            Self::LowEnergyShortToken => "low_energy_short_token",
        }
    }
}

fn is_low_energy_capture(policy: &TranscriptPolicy, stats: CaptureStats) -> bool {
    let rms = stats.rms.map(|value| value.max(0.0)).unwrap_or(f32::MAX);
    let peak = stats.peak.map(|value| value.max(0.0)).unwrap_or(f32::MAX);
    let audio_ms = stats.audio_ms.unwrap_or(u64::MAX);
    policy
        .capture_rms_min_for_speech
        .is_some_and(|threshold| rms < threshold)
        || policy
            .capture_peak_min_for_speech
            .is_some_and(|threshold| peak < threshold)
        || policy
            .audio_ms_min_for_speech_ms
            .is_some_and(|threshold| audio_ms < threshold)
}

/// Returns the drop reason for a transcript, or `None` to keep it (ADE
/// `whisper_local_transcript_drop_reason`).
#[must_use]
pub fn drop_reason(
    policy: &TranscriptPolicy,
    stats: CaptureStats,
    text: &str,
) -> Option<DropReason> {
    let normalized = text.trim();
    if normalized.is_empty() {
        return Some(DropReason::EmptyTranscript);
    }
    let normalized_lower = normalized.to_lowercase();
    if policy
        .no_speech_markers
        .iter()
        .any(|marker| normalized_lower == marker.to_lowercase())
    {
        return Some(DropReason::NoSpeechMarker);
    }
    if policy.suppress_bracketed_markers
        && normalized.len() <= policy.suppress_bracketed_markers_max_chars
        && normalized.starts_with('[')
        && normalized.ends_with(']')
    {
        return Some(DropReason::BracketedMarker);
    }
    if is_low_energy_capture(policy, stats) {
        let word_count = normalized.split_whitespace().count();
        if word_count <= policy.low_energy_max_words
            && normalized.chars().count() <= policy.low_energy_max_chars
            && policy
                .low_energy_suppressed_tokens
                .iter()
                .any(|token| normalized.eq_ignore_ascii_case(token))
        {
            return Some(DropReason::LowEnergyShortToken);
        }
    }
    None
}

/// Joins a new chunk's text onto assembled text, dropping up to 12 words of
/// case/punctuation-insensitive overlap (ADE
/// `local_whisper_partial_join_text`).
#[must_use]
pub fn join_partial_text(existing: &str, next: &str) -> String {
    let existing = existing.trim();
    let next = next.trim();
    if existing.is_empty() {
        return next.to_owned();
    }
    if next.is_empty() {
        return existing.to_owned();
    }
    let existing_words = existing.split_whitespace().collect::<Vec<_>>();
    let next_words = next.split_whitespace().collect::<Vec<_>>();
    let max_overlap = existing_words.len().min(next_words.len()).min(12);
    let mut overlap = 0usize;
    let normalize = |word: &str| {
        word.trim_matches(|ch: char| !ch.is_alphanumeric())
            .to_lowercase()
    };
    for count in (1..=max_overlap).rev() {
        let left = existing_words[existing_words.len() - count..]
            .iter()
            .map(|word| normalize(word))
            .collect::<Vec<_>>();
        let right = next_words[..count]
            .iter()
            .map(|word| normalize(word))
            .collect::<Vec<_>>();
        if left == right && left.iter().any(|word| !word.is_empty()) {
            overlap = count;
            break;
        }
    }
    let suffix = next_words
        .into_iter()
        .skip(overlap)
        .collect::<Vec<_>>()
        .join(" ");
    if suffix.is_empty() {
        existing.to_owned()
    } else if matches!(
        suffix.chars().next(),
        Some('.' | ',' | ';' | ':' | '?' | '!')
    ) {
        format!("{existing}{suffix}")
    } else {
        format!("{existing} {suffix}")
    }
}
