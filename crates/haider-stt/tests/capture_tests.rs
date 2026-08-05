//! Deterministic capture laws: DSP pins, ring/preroll, envelope cadence,
//! frames privacy, capture cap, and the mic watchdog.

#![allow(clippy::expect_used)]

use std::time::{Duration, Instant};

use haider_stt::capture::{
    BUFFER_MAX_SECONDS, CAPTURE_PREROLL_MS, CaptureConfig, CaptureEvent, CaptureHealth,
    CaptureState, ENVELOPE_WINDOW_SAMPLES, MIC_GRANT_HINT, STATS_INTERVAL_MS,
    STATS_STANDBY_INTERVAL_MS, TARGET_SAMPLE_RATE, audio_stats, encode_linear16, encode_wav,
    envelope_level, is_digital_zero, mono_mix, resample, resample_to_16khz, rms_dbfs,
};

/// The ADE capture constants are pinned (T2's wave and the engines depend
/// on these exact values).
#[test]
fn capture_constants_are_ade_parity() {
    assert_eq!(TARGET_SAMPLE_RATE, 16_000);
    assert!((BUFFER_MAX_SECONDS - 3.0).abs() < f64::EPSILON);
    assert_eq!(CAPTURE_PREROLL_MS, 500);
    assert_eq!(STATS_INTERVAL_MS, 60);
    assert_eq!(STATS_STANDBY_INTERVAL_MS, 1_000);
    assert_eq!(ENVELOPE_WINDOW_SAMPLES, 768);
}

/// The envelope blend is the ADE recipe: mean-removed rms·0.78 + peak·0.22,
/// clamped 0..1.
///
/// MUTATION CHECK: change either blend weight, drop the mean removal, or
/// drop the clamp. Expected runtime failure: one of the three literal pins
/// (0.5 window → 0.5, DC window → 0.0, full-scale window → 1.0).
#[test]
fn envelope_level_pins_the_ade_blend() {
    // Square wave ±0.5: mean 0, rms 0.5, peak 0.5 → 0.5·0.78 + 0.5·0.22 = 0.5.
    let square: Vec<f32> = (0..64)
        .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
        .collect();
    assert!((envelope_level(&square) - 0.5).abs() < 1e-6);
    // Pure DC offset carries NO signal after mean removal.
    let dc = vec![0.5f32; 64];
    assert!(envelope_level(&dc).abs() < 1e-6);
    // Full-scale square: rms 1, peak 1 → clamped exactly 1.
    let full: Vec<f32> = (0..64)
        .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    assert!((envelope_level(&full) - 1.0).abs() < 1e-6);
    assert_eq!(envelope_level(&[]), 0.0);
}

/// Mono mix averages frames and clamps (ADE `native_audio_mono_samples`).
#[test]
fn mono_mix_averages_channels_and_clamps() {
    assert_eq!(mono_mix(&[0.2, 0.4, -1.5, 0.5], 2), vec![0.3, -0.5]);
    assert_eq!(mono_mix(&[2.0], 1), vec![1.0]);
}

/// Stats and dBFS pins (ADE `native_audio_stats` / `audio_rms_dbfs`).
#[test]
fn audio_stats_and_dbfs_pin_known_values() {
    let (rms, peak) = audio_stats(&[0.6, -0.8]);
    assert!((peak - 0.8).abs() < 1e-6);
    // rms = sqrt((0.36 + 0.64) / 2) = sqrt(0.5).
    assert!((rms - 0.5f32.sqrt()).abs() < 1e-6);
    let (rms_nan, _) = audio_stats(&[f32::NAN, 0.0]);
    assert!(rms_nan.abs() < 1e-6, "non-finite samples count as zero");
    assert!((rms_dbfs(1.0) - 0.0).abs() < 1e-6);
    assert!((rms_dbfs(0.1) + 20.0).abs() < 1e-4);
    assert!((rms_dbfs(0.0) + 120.0).abs() < 1e-6);
}

/// Digital-zero detection matches the ADE's exact-zero definition.
#[test]
fn digital_zero_is_exact_zero_only() {
    assert!(is_digital_zero(&[0.0, 0.0, -0.0]));
    assert!(!is_digital_zero(&[0.0, 1e-4]));
    assert!(!is_digital_zero(&[]));
}

/// Linear resample pins: identity at equal rates, exact interpolation and
/// ADE length rounding across rates.
#[test]
fn resample_pins_identity_and_interpolation() {
    let samples = vec![0.0f32, 1.0, 2.0, 3.0];
    assert_eq!(resample(&samples, 16_000, 16_000), samples);
    // 32 kHz → 16 kHz: length (4·16000 + 16000) / 32000 = 2, step 2.0.
    assert_eq!(resample_to_16khz(&samples, 32_000), vec![0.0, 2.0]);
    // 8 kHz → 16 kHz upsample: length (4·16000 + 4000) / 8000 = 8.
    let up = resample(&[0.0f32, 1.0], 8_000, 16_000);
    assert_eq!(up.len(), 4);
    assert!((up[1] - 0.5).abs() < 1e-6, "midpoint interpolates linearly");
}

/// The WAV encoder is byte-identical to the ADE's `encode_native_wav`:
/// canonical 44-byte header + asymmetric i16 scaling.
///
/// MUTATION CHECK: scale positives by 32768 (symmetric encoder) or reorder
/// header fields. Expected runtime failure: the literal byte vector below.
#[test]
fn wav_encoder_is_byte_identical_to_the_ade() {
    let bytes = encode_wav(&[0.0, 0.5, -0.5, 1.0, -1.0], 16_000);
    let mut expected = Vec::new();
    expected.extend_from_slice(b"RIFF");
    expected.extend_from_slice(&46u32.to_le_bytes()); // 36 + 10 data bytes
    expected.extend_from_slice(b"WAVE");
    expected.extend_from_slice(b"fmt ");
    expected.extend_from_slice(&16u32.to_le_bytes());
    expected.extend_from_slice(&1u16.to_le_bytes()); // PCM
    expected.extend_from_slice(&1u16.to_le_bytes()); // mono
    expected.extend_from_slice(&16_000u32.to_le_bytes());
    expected.extend_from_slice(&32_000u32.to_le_bytes()); // byte rate
    expected.extend_from_slice(&2u16.to_le_bytes()); // block align
    expected.extend_from_slice(&16u16.to_le_bytes()); // bits
    expected.extend_from_slice(b"data");
    expected.extend_from_slice(&10u32.to_le_bytes());
    for value in [0i16, 16_383, -16_384, 32_767, -32_768] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(bytes, expected);
}

/// linear16 uses the same asymmetric scaling (Deepgram frame format).
#[test]
fn linear16_encoding_pins_asymmetric_scaling() {
    let bytes = encode_linear16(&[1.0, -1.0, 0.5]);
    assert_eq!(bytes.len(), 6);
    assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 32_767);
    assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -32_768);
    assert_eq!(i16::from_le_bytes([bytes[4], bytes[5]]), 16_383);
}

fn test_config(sample_rate: u32) -> CaptureConfig {
    CaptureConfig::new(sample_rate)
}

fn envelopes(events: &[CaptureEvent]) -> Vec<(f32, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            CaptureEvent::Envelope { level, recording } => Some((*level, *recording)),
            _ => None,
        })
        .collect()
}

fn frames(events: &[CaptureEvent]) -> Vec<Vec<f32>> {
    events
        .iter()
        .filter_map(|event| match event {
            CaptureEvent::Frames { samples, .. } => Some(samples.clone()),
            _ => None,
        })
        .collect()
}

/// PRIVACY LAW: standby ingestion emits envelopes but NEVER frames; frames
/// flow only while a recording is active.
///
/// MUTATION CHECK: emit `Frames` unconditionally in `ingest` (drop the
/// recording gate). Expected runtime failure: standby frames appear below.
#[test]
fn frames_are_emitted_only_while_recording() {
    let mut state = CaptureState::new(test_config(1_000));
    let t0 = Instant::now();
    let standby_events = state.ingest(&[0.4f32; 100], 1, t0);
    assert!(frames(&standby_events).is_empty(), "standby leaks no audio");
    assert!(
        !envelopes(&standby_events).is_empty(),
        "standby still meters"
    );
    state.start_recording();
    let recording_events = state.ingest(&[0.4f32; 100], 1, t0 + Duration::from_millis(100));
    assert_eq!(frames(&recording_events), vec![vec![0.4f32; 100]]);
}

/// The standby ring holds exactly 3 s and record-start seeds exactly the
/// last 500 ms of it (ADE preroll law).
///
/// MUTATION CHECK: seed the whole standby ring (or none of it) on record
/// start. Expected runtime failure: the preroll length/content pin below.
#[test]
fn record_start_seeds_exactly_the_preroll_tail() {
    let rate = 1_000u32;
    let mut state = CaptureState::new(test_config(rate));
    let t0 = Instant::now();
    // 4 s of a ramp: values 0..4000 scaled into -1..1.
    let ramp: Vec<f32> = (0..4_000).map(|i| (i as f32 / 4_000.0) - 0.5).collect();
    for (index, chunk) in ramp.chunks(100).enumerate() {
        state.ingest(chunk, 1, t0 + Duration::from_millis(index as u64 * 100));
    }
    state.start_recording();
    let take = state.stop_recording();
    // Preroll: exactly 500 samples (500 ms at 1 kHz), and they are the LAST
    // 500 of the ramp.
    assert_eq!(take.len(), 500);
    assert_eq!(take[0], ramp[3_500]);
    assert_eq!(take[499], ramp[3_999]);
}

/// Envelope cadence: ~60 ms while recording, 1 s on standby.
///
/// MUTATION CHECK: use the recording cadence on standby (or vice versa).
/// Expected runtime failure: the standby emission count below jumps from 2
/// to ~17, or the recording count collapses.
#[test]
fn envelope_cadence_is_60ms_recording_and_1s_standby() {
    let rate = 1_000u32;
    // Standby: 1 s of 100 ms batches → first batch emits, the next due emit
    // is at ≥1 s.
    let mut standby = CaptureState::new(test_config(rate));
    let t0 = Instant::now();
    let mut standby_emits = 0usize;
    for index in 0..11u64 {
        let events = standby.ingest(&[0.1f32; 100], 1, t0 + Duration::from_millis(index * 100));
        standby_emits += envelopes(&events).len();
    }
    assert_eq!(standby_emits, 2, "standby meters at 1 Hz");
    // Recording: the same schedule emits on every 100 ms batch (≥60 ms gap).
    let mut recording = CaptureState::new(test_config(rate));
    recording.start_recording();
    let mut recording_emits = 0usize;
    for index in 0..11u64 {
        let events = recording.ingest(&[0.1f32; 100], 1, t0 + Duration::from_millis(index * 100));
        recording_emits += envelopes(&events).len();
    }
    assert_eq!(recording_emits, 11, "recording meters at the 60 ms cadence");
}

/// The capture cap: accumulation stops at the configured ceiling and
/// reports `CaptureCapReached` exactly once.
///
/// MUTATION CHECK: keep accumulating past the cap (unbounded memory on a
/// stuck session). Expected runtime failure: the recorded length exceeds
/// the cap, or the event repeats.
#[test]
fn capture_cap_stops_accumulation_and_reports_once() {
    let rate = 1_000u32;
    let mut config = test_config(rate);
    config.max_capture_seconds = 0.25; // 250 samples at 1 kHz.
    config.preroll_ms = 0;
    let mut state = CaptureState::new(config);
    state.start_recording();
    let t0 = Instant::now();
    let mut cap_events = 0usize;
    for index in 0..5u64 {
        let events = state.ingest(&[0.2f32; 100], 1, t0 + Duration::from_millis(index * 100));
        cap_events += events
            .iter()
            .filter(|event| matches!(event, CaptureEvent::CaptureCapReached))
            .count();
    }
    assert_eq!(cap_events, 1, "the cap reports exactly once");
    assert_eq!(
        state.stop_recording().len(),
        250,
        "nothing accumulates past the cap"
    );
}

/// The digital-zero watchdog: sustained exact-zero input produces ONE
/// honest mic-grant hint, and signal return produces `Recovered`.
///
/// MUTATION CHECK: drop the watchdog (or report every batch). Expected
/// runtime failure: no hint below, a repeated hint, or a missing recovery.
#[test]
fn digital_zero_watchdog_hints_once_and_recovers() {
    let rate = 1_000u32;
    let mut state = CaptureState::new(test_config(rate));
    let t0 = Instant::now();
    let early = state.ingest(&[0.0f32; 100], 1, t0);
    assert!(
        !early
            .iter()
            .any(|event| matches!(event, CaptureEvent::Health(_))),
        "a short zero burst is not an episode"
    );
    let fired = state.ingest(&[0.0f32; 100], 1, t0 + Duration::from_millis(1_600));
    let hints: Vec<&CaptureEvent> = fired
        .iter()
        .filter(|event| {
            matches!(
                event,
                CaptureEvent::Health(CaptureHealth::DigitalZero { .. })
            )
        })
        .collect();
    assert_eq!(hints.len(), 1, "one hint per episode");
    if let CaptureEvent::Health(CaptureHealth::DigitalZero { hint }) = hints[0] {
        assert_eq!(hint, MIC_GRANT_HINT);
        assert!(hint.contains("grant microphone access to your terminal"));
    }
    let again = state.ingest(&[0.0f32; 100], 1, t0 + Duration::from_millis(1_700));
    assert!(
        !again
            .iter()
            .any(|event| matches!(event, CaptureEvent::Health(_))),
        "the episode does not re-report"
    );
    let recovered = state.ingest(&[0.3f32; 100], 1, t0 + Duration::from_millis(1_800));
    assert!(
        recovered
            .iter()
            .any(|event| matches!(event, CaptureEvent::Health(CaptureHealth::Recovered))),
        "signal return reports recovery"
    );
}

/// The stall watchdog: a silent callback stream reports `Stalled` once via
/// `tick`.
#[test]
fn stall_watchdog_reports_a_dead_callback_stream_once() {
    let mut state = CaptureState::new(test_config(1_000));
    let t0 = Instant::now();
    state.ingest(&[0.1f32; 100], 1, t0);
    assert!(state.tick(t0 + Duration::from_millis(500)).is_empty());
    let stalled = state.tick(t0 + Duration::from_millis(2_500));
    assert!(matches!(
        stalled.as_slice(),
        [CaptureEvent::Health(CaptureHealth::Stalled { hint })] if hint == MIC_GRANT_HINT
    ));
    assert!(
        state.tick(t0 + Duration::from_millis(3_000)).is_empty(),
        "the stall episode does not re-report"
    );
}
