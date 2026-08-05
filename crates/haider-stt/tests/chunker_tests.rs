//! Pseudo-streaming chunker laws (ADE cadence: 750 ms / 10 s / 35 s).

#![allow(clippy::expect_used)]

use haider_stt::chunker::{
    ChunkReason, Chunker, PARTIAL_MAX_CHUNK_MS, PARTIAL_MIN_CHUNK_MS, PARTIAL_MIN_TAIL_MS,
    PARTIAL_SILENCE_MS,
};

const RATE: u32 = 1_000;

fn loud() -> Vec<f32> {
    vec![0.5f32; 100] // 100 ms of clear speech-level signal.
}

fn quiet() -> Vec<f32> {
    vec![0.0f32; 100] // 100 ms of silence.
}

/// The ADE thresholds are pinned.
#[test]
fn chunker_thresholds_are_ade_parity() {
    assert_eq!(PARTIAL_MIN_CHUNK_MS, 10_000);
    assert_eq!(PARTIAL_MAX_CHUNK_MS, 35_000);
    assert_eq!(PARTIAL_SILENCE_MS, 750);
    assert_eq!(PARTIAL_MIN_TAIL_MS, 1_200);
}

/// NO CUT before 10 s of buffered audio, no matter how long the silence.
///
/// MUTATION CHECK: cut on silence alone (drop the min-chunk gate).
/// Expected runtime failure: a chunk appears during the 5 s + 2 s schedule.
#[test]
fn silence_alone_never_cuts_before_ten_seconds() {
    let mut chunker = Chunker::new(RATE);
    for _ in 0..50 {
        assert!(chunker.ingest(&loud()).is_none());
    }
    for _ in 0..20 {
        assert!(
            chunker.ingest(&quiet()).is_none(),
            "5 s buffered + 2 s silence must NOT cut (min chunk is 10 s)"
        );
    }
}

/// The quiet-gap cut: ≥750 ms of silence once ≥10 s are buffered.
///
/// MUTATION CHECK: shrink the silence threshold to one batch or raise the
/// min-chunk gate. Expected runtime failure: the cut fires at the wrong
/// batch index or not at all.
#[test]
fn quiet_gap_cuts_after_750ms_silence_past_ten_seconds() {
    let mut chunker = Chunker::new(RATE);
    for _ in 0..100 {
        assert!(chunker.ingest(&loud()).is_none(), "10 s of speech buffers");
    }
    // Silence batches: cut fires at the 8th (800 ms ≥ 750 ms).
    let mut cut = None;
    for index in 0..10 {
        if let Some(chunk) = chunker.ingest(&quiet()) {
            cut = Some((index, chunk));
            break;
        }
    }
    let (index, chunk) = cut.expect("quiet gap must cut");
    assert_eq!(index, 7, "the 8th silence batch crosses 750 ms");
    assert_eq!(chunk.reason, ChunkReason::QuietGap);
    assert_eq!(chunk.index, 0);
    assert_eq!(chunk.audio_ms, 10_800);
    assert_eq!(chunk.samples.len(), 10_800);
    assert_eq!(chunk.sample_rate, RATE);
    assert!(chunk.rms > 0.0 && chunk.peak > 0.0);
}

/// The force cut at 35 s of continuous speech.
///
/// MUTATION CHECK: remove the max-length cut. Expected runtime failure: no
/// chunk within 36 s of continuous speech.
#[test]
fn continuous_speech_force_cuts_at_35_seconds() {
    let mut chunker = Chunker::new(RATE);
    let mut cut = None;
    for index in 0..360 {
        if let Some(chunk) = chunker.ingest(&loud()) {
            cut = Some((index, chunk));
            break;
        }
    }
    let (index, chunk) = cut.expect("force cut must fire");
    assert_eq!(index, 349, "the 350th batch crosses 35 s");
    assert_eq!(chunk.reason, ChunkReason::MaxLength);
    assert_eq!(chunk.audio_ms, 35_000);
}

/// Speechless audio NEVER yields a chunk — even past the force ceiling
/// (a silent chunk would only hallucinate in whisper).
///
/// MUTATION CHECK: drop the `has_speech` gate in the chunk taker. Expected
/// runtime failure: a silent chunk appears below.
#[test]
fn speechless_audio_yields_no_chunk_even_at_the_force_ceiling() {
    let mut chunker = Chunker::new(RATE);
    for _ in 0..400 {
        assert!(
            chunker.ingest(&quiet()).is_none(),
            "silence must never produce a chunk"
        );
    }
    assert!(
        chunker.finish().is_none(),
        "a speechless tail flushes nothing"
    );
}

/// The final flush allows short tails (forced) and restarts indexing state
/// for the next chunk.
#[test]
fn finish_flushes_a_short_speech_tail() {
    let mut chunker = Chunker::new(RATE);
    for _ in 0..8 {
        assert!(chunker.ingest(&loud()).is_none());
    }
    let tail = chunker.finish().expect("short speech tail flushes");
    assert_eq!(tail.reason, ChunkReason::FinalTail);
    assert_eq!(tail.audio_ms, 800);
    assert!(
        tail.audio_ms < PARTIAL_MIN_TAIL_MS,
        "forced flush admits short tails"
    );
    assert!(
        chunker.finish().is_none(),
        "an empty buffer flushes nothing"
    );
}

/// Consecutive cuts carry increasing indices and contiguous time ranges.
#[test]
fn consecutive_chunks_carry_increasing_indices_and_contiguous_ranges() {
    let mut chunker = Chunker::new(RATE);
    let mut chunks = Vec::new();
    for _ in 0..800 {
        if let Some(chunk) = chunker.ingest(&loud()) {
            chunks.push(chunk);
        }
    }
    assert!(chunks.len() >= 2, "80 s of speech cuts at least twice");
    assert_eq!(chunks[0].index, 0);
    assert_eq!(chunks[1].index, 1);
    assert_eq!(
        chunks[0].end_ms, chunks[1].start_ms,
        "chunk ranges are contiguous"
    );
}
