//! Hallucination-policy and transcript-normalization laws (ADE defaults).

#![allow(clippy::expect_used)]

use haider_stt::policy::{
    CaptureStats, DropReason, MAX_TRANSCRIPT_INSERT_CHARS, TranscriptPolicy, drop_reason,
    is_runtime_warning_line, join_partial_text, normalize_transcript_text,
};

fn healthy_stats() -> CaptureStats {
    CaptureStats {
        audio_ms: Some(5_000),
        rms: Some(0.2),
        peak: Some(0.6),
    }
}

/// The compiled-in defaults are the ADE's exact policy values.
#[test]
fn policy_defaults_are_ade_parity() {
    let policy = TranscriptPolicy::default();
    assert_eq!(policy.audio_ms_min_for_speech_ms, Some(900));
    assert_eq!(policy.capture_rms_min_for_speech, Some(0.01));
    assert_eq!(policy.capture_peak_min_for_speech, Some(0.02));
    assert!(policy.suppress_bracketed_markers);
    assert_eq!(policy.suppress_bracketed_markers_max_chars, 24);
    assert_eq!(
        policy.no_speech_markers,
        vec![
            "[BLANK_AUDIO]",
            "[BLANK]",
            "[SILENCE]",
            "[NOISE]",
            "[MUSIC]"
        ]
    );
    assert_eq!(policy.low_energy_suppressed_tokens, vec!["you"]);
    assert_eq!(policy.low_energy_max_chars, 4);
    assert_eq!(policy.low_energy_max_words, 1);
    assert_eq!(MAX_TRANSCRIPT_INSERT_CHARS, 8_000);
}

/// The drop table: every ADE drop rule fires with its stable reason, and a
/// healthy transcript passes.
///
/// MUTATION CHECK: invert the low-energy gate (drop "you" on HEALTHY
/// captures) or lower the 900 ms floor. Expected runtime failure: the kept
/// healthy "you" below is dropped, or the short-audio row stops firing.
#[test]
fn drop_table_matches_the_ade_rules() {
    let policy = TranscriptPolicy::default();
    // Healthy speech passes.
    assert_eq!(drop_reason(&policy, healthy_stats(), "hello world"), None);
    // Empty transcript.
    assert_eq!(
        drop_reason(&policy, healthy_stats(), "   "),
        Some(DropReason::EmptyTranscript)
    );
    // No-speech markers (case-insensitive).
    assert_eq!(
        drop_reason(&policy, healthy_stats(), "[blank_audio]"),
        Some(DropReason::NoSpeechMarker)
    );
    // Bracketed marker ≤ 24 chars.
    assert_eq!(
        drop_reason(&policy, healthy_stats(), "[HUM]"),
        Some(DropReason::BracketedMarker)
    );
    // A LONG bracketed string is real content, not a marker.
    assert_eq!(
        drop_reason(
            &policy,
            healthy_stats(),
            "[this is a long bracketed sentence of real speech]"
        ),
        None
    );
    // Sub-900 ms audio + short token: low-energy suppression.
    let short_audio = CaptureStats {
        audio_ms: Some(400),
        rms: Some(0.2),
        peak: Some(0.6),
    };
    assert_eq!(
        drop_reason(&policy, short_audio, "you"),
        Some(DropReason::LowEnergyShortToken)
    );
    // Low RMS capture + "you".
    let low_rms = CaptureStats {
        audio_ms: Some(5_000),
        rms: Some(0.005),
        peak: Some(0.6),
    };
    assert_eq!(
        drop_reason(&policy, low_rms, "You"),
        Some(DropReason::LowEnergyShortToken)
    );
    // Low peak capture + "you".
    let low_peak = CaptureStats {
        audio_ms: Some(5_000),
        rms: Some(0.2),
        peak: Some(0.01),
    };
    assert_eq!(
        drop_reason(&policy, low_peak, "you"),
        Some(DropReason::LowEnergyShortToken)
    );
    // HEALTHY capture keeps "you" — the suppression is low-energy-gated.
    assert_eq!(drop_reason(&policy, healthy_stats(), "you"), None);
    // Low-energy but MULTI-WORD content is kept.
    assert_eq!(drop_reason(&policy, low_rms, "you there"), None);
    // Unknown stats pass the low-energy gate (unknown is not low).
    assert_eq!(drop_reason(&policy, CaptureStats::default(), "you"), None);
}

/// Stable reason labels (ADE strings).
#[test]
fn drop_reasons_carry_stable_labels() {
    assert_eq!(DropReason::EmptyTranscript.as_str(), "empty_transcript");
    assert_eq!(DropReason::NoSpeechMarker.as_str(), "no_speech_marker");
    assert_eq!(DropReason::BracketedMarker.as_str(), "bracketed_marker");
    assert_eq!(
        DropReason::LowEnergyShortToken.as_str(),
        "low_energy_short_token"
    );
}

/// Warm-up noise is filtered by the ADE prefix family; real lines survive.
///
/// MUTATION CHECK: filter stderr by a blanket "drop everything" rule.
/// Expected runtime failure: the real transcript line disappears below.
#[test]
fn normalization_filters_warmup_noise_and_collapses_whitespace() {
    assert!(is_runtime_warning_line(
        "whisper_init_from_file: loading model"
    ));
    assert!(is_runtime_warning_line("ggml_metal_init: found device"));
    assert!(is_runtime_warning_line(
        "load_backend: loaded Metal backend"
    ));
    assert!(is_runtime_warning_line(
        "warning: the binary 'main' is deprecated"
    ));
    assert!(!is_runtime_warning_line("hello world"));
    let raw =
        "whisper_init_from_file: loading\n  hello   world \nggml_metal_init: done\nsecond line";
    assert_eq!(normalize_transcript_text(raw), "hello world second line");
}

/// The cumulative-join law: up to 12 words of case/punctuation-insensitive
/// overlap are deduplicated; punctuation-leading suffixes join without a
/// space (ADE `local_whisper_partial_join_text`).
#[test]
fn partial_join_removes_word_overlap() {
    assert_eq!(
        join_partial_text(
            "The first paragraph ends with a careful transition.",
            "careful transition. Then the next idea starts."
        ),
        "The first paragraph ends with a careful transition. Then the next idea starts."
    );
    assert_eq!(join_partial_text("", "hello"), "hello");
    assert_eq!(join_partial_text("hello", ""), "hello");
    assert_eq!(
        join_partial_text("one two", "three four"),
        "one two three four"
    );
    assert_eq!(
        join_partial_text("ends here", ", and continues"),
        "ends here, and continues"
    );
}
