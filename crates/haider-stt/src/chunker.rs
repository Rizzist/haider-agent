//! Pseudo-streaming chunker (ADE `native_partial_ingest_samples` port).
//!
//! Local whisper has no streaming mode, so a partial session accumulates mic
//! audio and cuts batch chunks: at a quiet gap of ≥750 ms once ≥10 s are
//! buffered, force-cut at 35 s (rust-diffforge `WHISPER_PARTIAL_*`,
//! `src-tauri/src/audio.rs:1877-2002`). Speech/quiet detection adapts a
//! noise floor in dBFS; a chunk without detected speech, or a non-forced
//! chunk shorter than the 1.2 s minimum tail, is dropped rather than sent to
//! the CLI (it would only hallucinate).

use crate::capture::{audio_stats, rms_dbfs};

/// Minimum buffered audio before a quiet gap may cut (ADE
/// `WHISPER_PARTIAL_MIN_CHUNK_MS`).
pub const PARTIAL_MIN_CHUNK_MS: u64 = 10_000;
/// Force-cut ceiling (ADE `WHISPER_PARTIAL_MAX_CHUNK_MS`).
pub const PARTIAL_MAX_CHUNK_MS: u64 = 35_000;
/// Quiet-gap length that triggers a cut (ADE `WHISPER_PARTIAL_SILENCE_MS`).
pub const PARTIAL_SILENCE_MS: u64 = 750;
/// Shortest non-forced chunk worth transcribing (ADE
/// `WHISPER_PARTIAL_MIN_TAIL_MS`).
pub const PARTIAL_MIN_TAIL_MS: u64 = 1_200;

/// Why a chunk was cut (ADE reason strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkReason {
    /// `quiet-gap`: silence ≥750 ms after ≥10 s buffered.
    QuietGap,
    /// `max-length`: the 35 s force cut.
    MaxLength,
    /// The session-final flush (short tails allowed).
    FinalTail,
}

/// One cut chunk of native-rate mono audio plus its capture stats.
#[derive(Debug, Clone, PartialEq)]
pub struct PartialChunk {
    pub index: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub audio_ms: u64,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub peak: f32,
    pub rms: f32,
    pub reason: ChunkReason,
}

/// The chunker state machine.
#[derive(Debug)]
pub struct Chunker {
    sample_rate: u32,
    next_index: u64,
    buffered_samples: Vec<f32>,
    buffered_start_ms: f64,
    buffered_ms: f64,
    total_ms: f64,
    silence_ms: f64,
    noise_floor_db: f32,
    peak: f32,
    rms: f32,
    has_speech: bool,
    min_chunk_ms: u64,
    max_chunk_ms: u64,
    silence_cut_ms: u64,
}

impl Chunker {
    /// A chunker with the ADE default thresholds at `sample_rate`.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self::with_thresholds(
            sample_rate,
            PARTIAL_MIN_CHUNK_MS,
            PARTIAL_MAX_CHUNK_MS,
            PARTIAL_SILENCE_MS,
        )
    }

    /// Custom thresholds, clamped to the ADE's accepted ranges
    /// (min 3–35 s, max 10–35 s, silence 0.3–2 s).
    #[must_use]
    pub fn with_thresholds(
        sample_rate: u32,
        min_chunk_ms: u64,
        max_chunk_ms: u64,
        silence_ms: u64,
    ) -> Self {
        Self {
            sample_rate,
            next_index: 0,
            buffered_samples: Vec::new(),
            buffered_start_ms: 0.0,
            buffered_ms: 0.0,
            total_ms: 0.0,
            silence_ms: 0.0,
            noise_floor_db: -60.0,
            peak: 0.0,
            rms: 0.0,
            has_speech: false,
            min_chunk_ms: min_chunk_ms.clamp(3_000, PARTIAL_MAX_CHUNK_MS),
            max_chunk_ms: max_chunk_ms.clamp(PARTIAL_MIN_CHUNK_MS, PARTIAL_MAX_CHUNK_MS),
            silence_cut_ms: silence_ms.clamp(300, 2_000),
        }
    }

    fn reset_buffer(&mut self) {
        self.buffered_samples.clear();
        self.buffered_start_ms = self.total_ms;
        self.buffered_ms = 0.0;
        self.silence_ms = 0.0;
        self.peak = 0.0;
        self.rms = 0.0;
        self.has_speech = false;
    }

    fn take_chunk(&mut self, reason: ChunkReason, force_short_tail: bool) -> Option<PartialChunk> {
        if self.buffered_samples.is_empty() {
            self.reset_buffer();
            return None;
        }
        let audio_ms = self.buffered_ms.round().max(0.0) as u64;
        if !self.has_speech || (!force_short_tail && audio_ms < PARTIAL_MIN_TAIL_MS) {
            self.reset_buffer();
            return None;
        }
        let start_ms = self.buffered_start_ms.round().max(0.0) as u64;
        let end_ms = (self.buffered_start_ms + self.buffered_ms)
            .round()
            .max(start_ms as f64) as u64;
        let samples = std::mem::take(&mut self.buffered_samples);
        let chunk = PartialChunk {
            index: self.next_index,
            start_ms,
            end_ms,
            audio_ms,
            samples,
            sample_rate: self.sample_rate,
            peak: self.peak,
            rms: self.rms,
            reason,
        };
        self.next_index += 1;
        self.buffered_start_ms = self.total_ms;
        self.buffered_ms = 0.0;
        self.silence_ms = 0.0;
        self.peak = 0.0;
        self.rms = 0.0;
        self.has_speech = false;
        Some(chunk)
    }

    /// Ingests one batch of native-rate mono samples; returns a chunk when a
    /// cut fires.
    pub fn ingest(&mut self, samples: &[f32]) -> Option<PartialChunk> {
        if samples.is_empty() || self.sample_rate == 0 {
            return None;
        }
        let duration_ms = (samples.len() as f64 / f64::from(self.sample_rate)) * 1_000.0;
        let (rms, peak) = audio_stats(samples);
        let rms_db = rms_dbfs(rms);
        let speech_threshold = (self.noise_floor_db + 10.0).max(-45.0);
        let silence_threshold = (self.noise_floor_db + 6.0).max(-50.0);
        let speech = rms_db > speech_threshold || peak >= 0.035;
        let quiet = rms_db <= silence_threshold && peak < 0.025;
        if speech {
            self.has_speech = true;
            self.silence_ms = 0.0;
        } else if quiet || !self.has_speech {
            self.silence_ms += duration_ms;
            let blend = if self.has_speech { 0.01 } else { 0.05 };
            self.noise_floor_db = (self.noise_floor_db * (1.0 - blend)) + (rms_db * blend);
        } else {
            self.silence_ms = 0.0;
        }
        self.buffered_samples.extend_from_slice(samples);
        self.buffered_ms += duration_ms;
        self.total_ms += duration_ms;
        self.peak = self.peak.max(peak);
        self.rms = self.rms.max(rms);
        if self.buffered_ms >= self.min_chunk_ms as f64
            && self.silence_ms >= self.silence_cut_ms as f64
        {
            return self.take_chunk(ChunkReason::QuietGap, false);
        }
        if self.buffered_ms >= self.max_chunk_ms as f64 {
            return self.take_chunk(ChunkReason::MaxLength, false);
        }
        None
    }

    /// Flushes the remaining tail at session end (short tails allowed; a
    /// speechless tail still yields `None`).
    pub fn finish(&mut self) -> Option<PartialChunk> {
        self.take_chunk(ChunkReason::FinalTail, true)
    }
}
