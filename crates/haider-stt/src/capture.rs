//! Mic capture: deterministic DSP core + a thin cpal worker thread.
//!
//! Every observable behavior lives in [`CaptureState`], a pure state machine
//! driven by explicit `Instant`s so laws can pin it without a microphone:
//! mono mix, the 3 s standby ring, 500 ms preroll, the ~60 ms envelope
//! cadence (`rms·0.78 + peak·0.22`, mean-removed, clamped 0..1 — the ADE
//! recipe from `native_audio_envelope_samples`), the 900 s capture cap, and
//! the digital-zero/stall watchdog with its honest "grant mic to your
//! terminal" hint. The cpal glue ([`CaptureWorker`]) only moves samples from
//! the audio callback into that state machine on a dedicated thread.
//!
//! Privacy law: audio FRAMES leave this module only while a recording is
//! active. Standby keeps the preroll ring warm and emits envelope levels,
//! but never emits samples.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::SttError;

/// Whisper's required input rate (ADE `AUDIO_TARGET_SAMPLE_RATE`).
pub const TARGET_SAMPLE_RATE: u32 = 16_000;
/// Rolling standby buffer depth (ADE `AUDIO_BUFFER_MAX_SECONDS`).
pub const BUFFER_MAX_SECONDS: f64 = 3.0;
/// Hard capture cap (ADE `AUDIO_CAPTURE_MAX_SECONDS`).
pub const CAPTURE_MAX_SECONDS: f64 = 900.0;
/// Preroll copied from the standby ring on record start
/// (ADE `AUDIO_CAPTURE_PREROLL_MS`).
pub const CAPTURE_PREROLL_MS: u64 = 500;
/// Envelope cadence while recording (ADE `AUDIO_STATS_INTERVAL_MS`).
pub const STATS_INTERVAL_MS: u64 = 60;
/// Envelope cadence on standby (ADE `AUDIO_STATS_STANDBY_INTERVAL_MS`).
pub const STATS_STANDBY_INTERVAL_MS: u64 = 1_000;
/// Envelope analysis window (ADE `AUDIO_INPUT_WAVEFORM_WINDOW_SAMPLES`).
pub const ENVELOPE_WINDOW_SAMPLES: usize = 768;
/// Continuous exact-zero input before the watchdog reports a dead signal.
pub const DIGITAL_ZERO_HINT_AFTER_MS: u64 = 1_500;
/// Callback silence before the watchdog reports a stalled stream.
pub const STALL_HINT_AFTER_MS: u64 = 2_000;

/// The honest macOS-terminal TCC hint (the mic grant belongs to the
/// TERMINAL app for a CLI-spawned process, and a denied grant looks like
/// digital zero, not an error).
pub const MIC_GRANT_HINT: &str = "no microphone signal — grant microphone access to your terminal app (System Settings → Privacy & Security → Microphone), then retry";

/// Mixes interleaved multi-channel f32 frames to clamped mono
/// (ADE `native_audio_mono_samples`).
#[must_use]
pub fn mono_mix(data: &[f32], channels: usize) -> Vec<f32> {
    let channel_count = channels.max(1);
    let mut samples = Vec::with_capacity(data.len() / channel_count);
    for frame in data.chunks(channel_count) {
        let mixed = frame.iter().sum::<f32>() / frame.len().max(1) as f32;
        samples.push(mixed.clamp(-1.0, 1.0));
    }
    samples
}

/// `(rms, peak)` over raw samples (ADE `native_audio_stats`); non-finite
/// samples count as zero.
#[must_use]
pub fn audio_stats(samples: &[f32]) -> (f32, f32) {
    let mut sum_squares = 0.0f32;
    let mut peak = 0.0f32;
    for sample in samples {
        let value = if sample.is_finite() { *sample } else { 0.0 };
        sum_squares += value * value;
        peak = peak.max(value.abs());
    }
    ((sum_squares / samples.len().max(1) as f32).sqrt(), peak)
}

/// RMS in dBFS, floored at -120 (ADE `audio_rms_dbfs`).
#[must_use]
pub fn rms_dbfs(rms: f32) -> f32 {
    if rms <= 0.000_001 {
        -120.0
    } else {
        (20.0 * rms.max(0.000_001).log10()).clamp(-120.0, 0.0)
    }
}

/// Whether a callback batch is exact digital zero — the shape a denied or
/// disconnected mic produces (ADE `native_audio_samples_are_digital_zero`).
#[must_use]
pub fn is_digital_zero(samples: &[f32]) -> bool {
    !samples.is_empty()
        && samples
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= f32::MIN_POSITIVE)
}

/// One envelope level over a trailing window: mean-removed
/// `rms·0.78 + peak·0.22`, clamped 0..1 (the ADE waveform blend).
#[must_use]
pub fn envelope_level(window: &[f32]) -> f32 {
    if window.is_empty() {
        return 0.0;
    }
    let mean = window.iter().sum::<f32>() / window.len() as f32;
    let mut sum_squares = 0.0f32;
    let mut peak = 0.0f32;
    for sample in window {
        let value = (*sample - mean).clamp(-1.0, 1.0);
        sum_squares += value * value;
        peak = peak.max(value.abs());
    }
    let rms = (sum_squares / window.len() as f32).sqrt();
    ((rms * 0.78) + (peak * 0.22)).clamp(0.0, 1.0)
}

/// Linear-interpolation resample (ADE `resample_whisper_audio_to_16khz`
/// generalized to any target rate, same rounding).
#[must_use]
pub fn resample(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if samples.is_empty() || source_rate == 0 || target_rate == 0 || source_rate == target_rate {
        return samples.to_vec();
    }
    let output_len = ((samples.len() as u64 * u64::from(target_rate) + u64::from(source_rate) / 2)
        / u64::from(source_rate))
    .max(1) as usize;
    let step = f64::from(source_rate) / f64::from(target_rate);
    let mut output = Vec::with_capacity(output_len);
    for index in 0..output_len {
        let position = index as f64 * step;
        let left_index = (position.floor() as usize).min(samples.len() - 1);
        let right_index = (left_index + 1).min(samples.len() - 1);
        let blend = (position - left_index as f64) as f32;
        let left = samples[left_index];
        let right = samples[right_index];
        output.push(left + ((right - left) * blend));
    }
    output
}

/// Resamples to whisper's 16 kHz input rate.
#[must_use]
pub fn resample_to_16khz(samples: &[f32], source_rate: u32) -> Vec<f32> {
    resample(samples, source_rate, TARGET_SAMPLE_RATE)
}

/// f32 → i16 LE PCM bytes with the ADE's asymmetric scaling
/// (`encode_linear16_audio`) — the Deepgram binary-frame format.
#[must_use]
pub fn encode_linear16(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let clipped = sample.clamp(-1.0, 1.0);
        let value = if clipped < 0.0 {
            (clipped * 32768.0) as i16
        } else {
            (clipped * 32767.0) as i16
        };
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Minimal 16-bit mono PCM WAV encoder — byte-identical to the ADE's
/// `encode_native_wav` (44-byte canonical header + LE samples).
#[must_use]
pub fn encode_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let bytes_per_sample = 2u16;
    let data_len = samples.len() as u32 * u32::from(bytes_per_sample);
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * u32::from(bytes_per_sample)).to_le_bytes());
    bytes.extend_from_slice(&bytes_per_sample.to_le_bytes());
    bytes.extend_from_slice(&(bytes_per_sample * 8).to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        let clipped = sample.clamp(-1.0, 1.0);
        let value = if clipped < 0.0 {
            (clipped * 32768.0) as i16
        } else {
            (clipped * 32767.0) as i16
        };
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Capture health reported by the watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureHealth {
    /// The stream delivers exact zeros — a denied/parked mic. Carries
    /// [`MIC_GRANT_HINT`].
    DigitalZero { hint: String },
    /// No callback arrived within the stall window.
    Stalled { hint: String },
    /// Signal returned after a reported episode.
    Recovered,
    /// CPAL reported a post-start stream failure (device vanished,
    /// permission revoked, backend reset). This is terminal for the worker.
    Failed { error: String },
}

/// One observable capture emission.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    /// ~60 ms cadence while recording, 1 s on standby.
    Envelope {
        level: f32,
        recording: bool,
    },
    /// Mono native-rate samples — emitted ONLY while recording.
    Frames {
        samples: Vec<f32>,
        sample_rate: u32,
    },
    Health(CaptureHealth),
    /// The 900 s cap was reached; the recording stopped accumulating.
    CaptureCapReached,
}

/// Capture tuning; defaults are the ADE constants.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureConfig {
    pub sample_rate: u32,
    pub standby_seconds: f64,
    pub preroll_ms: u64,
    pub stats_interval_ms: u64,
    pub standby_stats_interval_ms: u64,
    pub max_capture_seconds: f64,
}

impl CaptureConfig {
    /// ADE-parity defaults at `sample_rate`.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            standby_seconds: BUFFER_MAX_SECONDS,
            preroll_ms: CAPTURE_PREROLL_MS,
            stats_interval_ms: STATS_INTERVAL_MS,
            standby_stats_interval_ms: STATS_STANDBY_INTERVAL_MS,
            max_capture_seconds: CAPTURE_MAX_SECONDS,
        }
    }
}

/// Deterministic capture core: every transition takes an explicit `now`.
#[derive(Debug)]
pub struct CaptureState {
    config: CaptureConfig,
    standby_ring: VecDeque<f32>,
    recording: Option<Vec<f32>>,
    cap_reported: bool,
    last_emit_at: Option<Instant>,
    last_callback_at: Option<Instant>,
    digital_zero_started_at: Option<Instant>,
    zero_reported: bool,
    stall_reported: bool,
}

impl CaptureState {
    #[must_use]
    pub fn new(config: CaptureConfig) -> Self {
        Self {
            config,
            standby_ring: VecDeque::new(),
            recording: None,
            cap_reported: false,
            last_emit_at: None,
            last_callback_at: None,
            digital_zero_started_at: None,
            zero_reported: false,
            stall_reported: false,
        }
    }

    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    fn standby_capacity(&self) -> usize {
        (self.config.standby_seconds * f64::from(self.config.sample_rate)).round() as usize
    }

    fn capture_capacity(&self) -> usize {
        (self.config.max_capture_seconds * f64::from(self.config.sample_rate)).round() as usize
    }

    /// Starts a recording, seeding it with the last `preroll_ms` of standby
    /// audio (ADE preroll law). Idempotent while already recording.
    pub fn start_recording(&mut self) {
        if self.recording.is_some() {
            return;
        }
        let preroll_samples = ((u128::from(self.config.preroll_ms)
            * u128::from(self.config.sample_rate))
            / 1_000) as usize;
        let available = self.standby_ring.len();
        let skip = available.saturating_sub(preroll_samples);
        let preroll: Vec<f32> = self.standby_ring.iter().skip(skip).copied().collect();
        self.recording = Some(preroll);
        self.cap_reported = false;
    }

    /// Stops the recording and returns the accumulated mono samples
    /// (preroll included), or an empty vec when idle.
    pub fn stop_recording(&mut self) -> Vec<f32> {
        self.cap_reported = false;
        self.recording.take().unwrap_or_default()
    }

    /// Ingests one interleaved callback batch at `now`.
    pub fn ingest(&mut self, data: &[f32], channels: usize, now: Instant) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        self.last_callback_at = Some(now);
        self.stall_reported = false;
        let samples = mono_mix(data, channels);
        if samples.is_empty() {
            return events;
        }
        if is_digital_zero(&samples) {
            if self.digital_zero_started_at.is_none() {
                self.digital_zero_started_at = Some(now);
            }
            if !self.zero_reported
                && self.digital_zero_started_at.is_some_and(|started| {
                    now.duration_since(started) >= Duration::from_millis(DIGITAL_ZERO_HINT_AFTER_MS)
                })
            {
                self.zero_reported = true;
                events.push(CaptureEvent::Health(CaptureHealth::DigitalZero {
                    hint: MIC_GRANT_HINT.into(),
                }));
            }
        } else {
            self.digital_zero_started_at = None;
            if self.zero_reported {
                self.zero_reported = false;
                events.push(CaptureEvent::Health(CaptureHealth::Recovered));
            }
        }
        // Standby ring: always warm, capped at the standby depth.
        self.standby_ring.extend(samples.iter().copied());
        let capacity = self.standby_capacity();
        while self.standby_ring.len() > capacity {
            self.standby_ring.pop_front();
        }
        // Recording accumulation under the 900 s cap.
        let capture_capacity = self.capture_capacity();
        let mut frame_batch = None;
        if let Some(recording) = self.recording.as_mut() {
            let room = capture_capacity.saturating_sub(recording.len());
            let take = samples.len().min(room);
            recording.extend_from_slice(&samples[..take]);
            if take < samples.len() && !self.cap_reported {
                self.cap_reported = true;
                events.push(CaptureEvent::CaptureCapReached);
            }
            if take > 0 {
                frame_batch = Some(samples[..take].to_vec());
            }
        }
        if let Some(batch) = frame_batch {
            events.push(CaptureEvent::Frames {
                samples: batch,
                sample_rate: self.config.sample_rate,
            });
        }
        // Envelope cadence: 60 ms recording / 1000 ms standby.
        let interval = Duration::from_millis(if self.recording.is_some() {
            self.config.stats_interval_ms
        } else {
            self.config.standby_stats_interval_ms
        });
        let due = self
            .last_emit_at
            .is_none_or(|last| now.duration_since(last) >= interval);
        if due {
            self.last_emit_at = Some(now);
            let window_start = self
                .standby_ring
                .len()
                .saturating_sub(ENVELOPE_WINDOW_SAMPLES);
            let window: Vec<f32> = self
                .standby_ring
                .iter()
                .skip(window_start)
                .copied()
                .collect();
            events.push(CaptureEvent::Envelope {
                level: envelope_level(&window),
                recording: self.recording.is_some(),
            });
        }
        events
    }

    /// Periodic watchdog tick: reports a stalled stream once per episode.
    pub fn tick(&mut self, now: Instant) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        if let Some(last) = self.last_callback_at
            && !self.stall_reported
            && now.duration_since(last) >= Duration::from_millis(STALL_HINT_AFTER_MS)
        {
            self.stall_reported = true;
            events.push(CaptureEvent::Health(CaptureHealth::Stalled {
                hint: MIC_GRANT_HINT.into(),
            }));
        }
        events
    }
}

enum WorkerMessage {
    Audio { data: Vec<f32>, channels: usize },
    StartRecording,
    StopRecording(mpsc::Sender<Vec<f32>>),
    StreamError(String),
    Shutdown,
}

/// Handle to the capture worker thread; dropping it stops the stream.
pub struct CaptureWorker {
    commands: mpsc::Sender<WorkerMessage>,
    sample_rate: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CaptureWorker {
    /// Opens the default cpal input device and spawns the worker thread.
    ///
    /// Every capture event flows through `events`. This is the ONLY
    /// cpal-touching seam in the crate; failures map to
    /// [`SttError::MicUnavailable`] with an honest hint.
    pub fn spawn(events: mpsc::Sender<CaptureEvent>) -> Result<Self, SttError> {
        use cpal::traits::{DeviceTrait as _, HostTrait as _};
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(SttError::MicUnavailable {
                hint: format!("no input device found; {MIC_GRANT_HINT}"),
            })?;
        let default_config =
            device
                .default_input_config()
                .map_err(|error| SttError::MicUnavailable {
                    hint: format!(
                        "input device rejected configuration ({error}); {MIC_GRANT_HINT}"
                    ),
                })?;
        let sample_rate = default_config.sample_rate().0;
        let channels = usize::from(default_config.channels());
        let (message_tx, message_rx) = mpsc::channel::<WorkerMessage>();
        let audio_tx = message_tx.clone();
        let stream_message_tx = message_tx.clone();
        let stream_config: cpal::StreamConfig = default_config.config();
        let sample_format = default_config.sample_format();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), SttError>>();
        let join = std::thread::Builder::new()
            .name("haider-stt-capture".into())
            .spawn(move || {
                use cpal::traits::StreamTrait as _;
                let error_hint = |error: &dyn std::fmt::Display| SttError::MicUnavailable {
                    hint: format!("could not open input stream ({error}); {MIC_GRANT_HINT}"),
                };
                let stream = match sample_format {
                    cpal::SampleFormat::F32 => {
                        let stream_error_tx = stream_message_tx.clone();
                        device.build_input_stream(
                        &stream_config,
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            let _ = audio_tx.send(WorkerMessage::Audio {
                                data: data.to_vec(),
                                channels,
                            });
                        },
                        move |error| {
                            let _ = stream_error_tx.send(WorkerMessage::StreamError(
                                format!("microphone stream failed after start: {error}; {MIC_GRANT_HINT}"),
                            ));
                        },
                        None,
                    )
                    }
                    cpal::SampleFormat::I16 => {
                        let stream_error_tx = stream_message_tx.clone();
                        device.build_input_stream(
                        &stream_config,
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            let converted = data
                                .iter()
                                .map(|value| f32::from(*value) / 32_768.0)
                                .collect();
                            let _ = audio_tx.send(WorkerMessage::Audio {
                                data: converted,
                                channels,
                            });
                        },
                        move |error| {
                            let _ = stream_error_tx.send(WorkerMessage::StreamError(
                                format!("microphone stream failed after start: {error}; {MIC_GRANT_HINT}"),
                            ));
                        },
                        None,
                    )
                    }
                    cpal::SampleFormat::U16 => {
                        let stream_error_tx = stream_message_tx.clone();
                        device.build_input_stream(
                        &stream_config,
                        move |data: &[u16], _: &cpal::InputCallbackInfo| {
                            let converted = data
                                .iter()
                                .map(|value| (f32::from(*value) - 32_768.0) / 32_768.0)
                                .collect();
                            let _ = audio_tx.send(WorkerMessage::Audio {
                                data: converted,
                                channels,
                            });
                        },
                        move |error| {
                            let _ = stream_error_tx.send(WorkerMessage::StreamError(
                                format!("microphone stream failed after start: {error}; {MIC_GRANT_HINT}"),
                            ));
                        },
                        None,
                    )
                    }
                    other => {
                        let _ = ready_tx.send(Err(SttError::MicUnavailable {
                            hint: format!("unsupported input sample format {other:?}"),
                        }));
                        return;
                    }
                };
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error_hint(&error)));
                        return;
                    }
                };
                if let Err(error) = stream.play() {
                    let _ = ready_tx.send(Err(error_hint(&error)));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                let mut state = CaptureState::new(CaptureConfig::new(sample_rate));
                loop {
                    match message_rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(WorkerMessage::Audio { data, channels }) => {
                            for event in state.ingest(&data, channels, Instant::now()) {
                                if events.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(WorkerMessage::StartRecording) => state.start_recording(),
                        Ok(WorkerMessage::StopRecording(reply)) => {
                            let _ = reply.send(state.stop_recording());
                        }
                        Ok(WorkerMessage::StreamError(error)) => {
                            let _ = events.send(CaptureEvent::Health(CaptureHealth::Failed {
                                error,
                            }));
                            return;
                        }
                        Ok(WorkerMessage::Shutdown) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            for event in state.tick(Instant::now()) {
                                if events.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .map_err(|error| SttError::Io(format!("could not spawn capture thread: {error}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                commands: message_tx,
                sample_rate,
                join: Some(join),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(SttError::Timeout("capture stream did not start".into())),
        }
    }

    /// The device's native sample rate (frames are emitted at this rate).
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Begins accumulating a recording (preroll included).
    pub fn start_recording(&self) -> Result<(), SttError> {
        self.commands
            .send(WorkerMessage::StartRecording)
            .map_err(|_| SttError::Io("capture worker is gone".into()))
    }

    /// Ends the recording and returns the full mono take.
    pub fn stop_recording(&self) -> Result<Vec<f32>, SttError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(WorkerMessage::StopRecording(reply_tx))
            .map_err(|_| SttError::Io("capture worker is gone".into()))?;
        reply_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| SttError::Timeout("capture worker did not answer stop".into()))
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerMessage::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
