//! LocalWhisperEngine: whisper.cpp CLI spawn per chunk (no FFI, no unsafe).
//!
//! ADE-parity mechanics (rust-diffforge `src-tauri/src/audio.rs:4489-4931`,
//! `5099-5189`): exact latency-tuned argv
//! (`-m … -f … -l en -t <4..8> -nt -np -bo 1 -bs 1 -nf [--prompt …]`),
//! transcript read from STDOUT, 180 s timeout, warm page-cache pre-read once
//! per (path, size), per-chunk fresh spawn, hallucination policy applied per
//! chunk, cumulative assembled text emitted as partial frames. The model
//! file's existence is re-checked at every spawn: the ADE's uninstall can
//! evict the shared dir at any moment, and eviction must surface as the
//! typed [`SttError::ModelMissing`] — never a crash.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::capture::{encode_wav, resample_to_16khz};
use crate::chunker::{Chunker, PartialChunk};
use crate::policy::{
    CaptureStats, TranscriptPolicy, drop_reason, join_partial_text, normalize_transcript_text,
};
use crate::{EngineKind, SttError, TranscriptFrame, TranscriptionResult};

/// Per-invocation CLI budget (ADE `WHISPER_TRANSCRIBE_TIMEOUT_SECS`).
pub const TRANSCRIBE_TIMEOUT_SECS: u64 = 180;
/// Largest WAV handed to the CLI (ADE `WHISPER_MAX_AUDIO_BYTES`).
pub const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;
/// Hard partial-session audio cap, ADE capture parity (900 s).
pub const MAX_SESSION_AUDIO_MS: u64 = 900_000;

/// CLI thread count: available parallelism clamped 4..=8 (ADE
/// `whisper_cli_thread_count`).
#[must_use]
pub fn thread_count() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(4, 8)
}

/// Partial chunks clamp threads to ≤4 (ADE partial variant).
#[must_use]
pub fn partial_thread_count() -> usize {
    thread_count().min(4)
}

/// Builds the exact ADE argv for one invocation. Order is a pinned contract:
/// `-m <model> -f <wav> -l <language> -t <threads> -nt -np -bo 1 -bs 1 -nf`
/// plus optional trailing `--prompt <bias>`.
#[must_use]
pub fn build_args(
    model_path: &Path,
    wav_path: &Path,
    language: &str,
    threads: usize,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "-m".to_owned(),
        model_path.display().to_string(),
        "-f".to_owned(),
        wav_path.display().to_string(),
        "-l".to_owned(),
        language.to_owned(),
        "-t".to_owned(),
        threads.to_string(),
        "-nt".to_owned(),
        "-np".to_owned(),
        "-bo".to_owned(),
        "1".to_owned(),
        "-bs".to_owned(),
        "1".to_owned(),
        "-nf".to_owned(),
    ];
    if let Some(prompt) = prompt {
        args.push("--prompt".to_owned());
        args.push(prompt.to_owned());
    }
    args
}

/// Cooperative cancel token: cancels in-flight CLI spawns and stops a
/// partial session's loop.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct WarmCacheState {
    model_path: Option<PathBuf>,
    model_bytes: u64,
}

/// Page-cache warmer: reads the model file once per (path, size) so the
/// first CLI spawn does not pay cold-file latency (ADE
/// `WhisperCliWarmCache`). Never holds an open handle — the model stays
/// evictable.
#[derive(Clone, Default)]
pub struct WarmCache {
    state: Arc<Mutex<WarmCacheState>>,
}

impl WarmCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Warms `model_path` if it changed since the last warm. Returns the
    /// bytes read (0 on a cache hit). A missing file is the typed
    /// model-missing state.
    pub fn prepare(&self, model_path: &Path, model_id: &str) -> Result<u64, SttError> {
        use std::io::Read as _;
        let metadata = std::fs::metadata(model_path).map_err(|_| SttError::ModelMissing {
            model_id: model_id.to_owned(),
        })?;
        let model_bytes = metadata.len();
        {
            let state = self
                .state
                .lock()
                .map_err(|_| SttError::Io("warm cache lock poisoned".into()))?;
            if state.model_path.as_deref() == Some(model_path) && state.model_bytes == model_bytes {
                return Ok(0);
            }
        }
        let mut file = std::fs::File::open(model_path).map_err(|_| SttError::ModelMissing {
            model_id: model_id.to_owned(),
        })?;
        let mut buffer = [0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| SttError::Io(format!("could not warm model cache: {error}")))?;
            if read == 0 {
                break;
            }
            total += read as u64;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| SttError::Io("warm cache lock poisoned".into()))?;
        state.model_path = Some(model_path.to_owned());
        state.model_bytes = model_bytes;
        Ok(total)
    }
}

/// The local engine: one discovered runtime + one selected model.
pub struct LocalWhisperEngine {
    runtime_path: PathBuf,
    model_path: PathBuf,
    model_id: String,
    language: String,
    policy: TranscriptPolicy,
    warm: WarmCache,
    temp_dir: PathBuf,
}

impl LocalWhisperEngine {
    /// An engine over an already-discovered runtime and model. `language`
    /// is `en` for the catalog's `.en` models (ADE hardcodes `-l en`).
    #[must_use]
    pub fn new(runtime_path: PathBuf, model_path: PathBuf, model_id: String) -> Self {
        Self {
            runtime_path,
            model_path,
            model_id,
            language: "en".to_owned(),
            policy: TranscriptPolicy::default(),
            warm: WarmCache::new(),
            temp_dir: std::env::temp_dir().join("haider-stt"),
        }
    }

    /// Overrides the chunk-WAV scratch directory (tests).
    #[must_use]
    pub fn with_temp_dir(mut self, temp_dir: PathBuf) -> Self {
        self.temp_dir = temp_dir;
        self
    }

    /// Transcribes one 16 kHz mono WAV byte buffer through a fresh CLI
    /// spawn. Returns `Ok(None)` when the hallucination policy drops the
    /// transcript.
    ///
    /// Laws: the model file is re-verified at THIS call (eviction →
    /// [`SttError::ModelMissing`]); WAVs above [`MAX_AUDIO_BYTES`] are
    /// refused before any spawn; a non-zero exit reports filtered stderr; a
    /// fired cancel token kills the child and returns
    /// [`SttError::Canceled`]; the scratch WAV is removed on every path.
    pub async fn transcribe_wav_bytes(
        &self,
        wav_bytes: Vec<u8>,
        stats: CaptureStats,
        threads: usize,
        cancel: &CancelToken,
    ) -> Result<Option<String>, SttError> {
        if wav_bytes.len() > MAX_AUDIO_BYTES {
            return Err(SttError::InvalidArgument(format!(
                "audio exceeds the {MAX_AUDIO_BYTES}-byte whisper input limit"
            )));
        }
        if cancel.is_canceled() {
            return Err(SttError::Canceled);
        }
        self.warm.prepare(&self.model_path, &self.model_id)?;
        std::fs::create_dir_all(&self.temp_dir)
            .map_err(|error| SttError::Io(format!("could not create scratch dir: {error}")))?;
        let wav_file = tempfile::Builder::new()
            .prefix("chunk-")
            .suffix(".wav")
            .tempfile_in(&self.temp_dir)
            .map_err(|error| SttError::Io(format!("could not create scratch WAV: {error}")))?;
        std::fs::write(wav_file.path(), &wav_bytes)
            .map_err(|error| SttError::Io(format!("could not write scratch WAV: {error}")))?;
        let args = build_args(
            &self.model_path,
            wav_file.path(),
            &self.language,
            threads,
            None,
        );
        let mut child = tokio::process::Command::new(&self.runtime_path)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| SttError::Io(format!("could not spawn whisper-cli: {error}")))?;
        async fn drain(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
            use tokio::io::AsyncReadExt as _;
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes).await;
            }
            bytes
        }
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_task = tokio::spawn(drain(stdout_pipe));
        let stderr_task = tokio::spawn(drain(stderr_pipe));
        let deadline = tokio::time::sleep(Duration::from_secs(TRANSCRIBE_TIMEOUT_SECS));
        tokio::pin!(deadline);
        let status = loop {
            if cancel.is_canceled() {
                let _ = child.kill().await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(SttError::Canceled);
            }
            tokio::select! {
                status = child.wait() => {
                    break status.map_err(|error| {
                        SttError::Io(format!("could not wait for whisper-cli: {error}"))
                    })?;
                }
                () = &mut deadline => {
                    let _ = child.kill().await;
                    stdout_task.abort();
                    stderr_task.abort();
                    return Err(SttError::Timeout(
                        "whisper-cli did not finish within its budget".into(),
                    ));
                }
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        };
        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stderr_bytes = stderr_task.await.unwrap_or_default();
        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            let detail = stderr
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !crate::policy::is_runtime_warning_line(line))
                .collect::<Vec<_>>()
                .join(" ");
            let stdout = String::from_utf8_lossy(&stdout_bytes);
            let fallback = stdout.trim();
            let message = if detail.is_empty() {
                if fallback.is_empty() {
                    format!("whisper-cli exited with {status}")
                } else {
                    fallback.to_owned()
                }
            } else {
                detail
            };
            return Err(SttError::Endpoint(format!("whisper-cli failed: {message}")));
        }
        let text = normalize_transcript_text(&String::from_utf8_lossy(&stdout_bytes));
        if drop_reason(&self.policy, stats, &text).is_some() {
            return Ok(None);
        }
        Ok(Some(text))
    }
}

/// Handle to one running partial session.
pub struct LocalPartialSession {
    frames_tx: tokio::sync::mpsc::UnboundedSender<Vec<f32>>,
    cancel: CancelToken,
    done: tokio::task::JoinHandle<Result<TranscriptionResult, SttError>>,
}

impl LocalPartialSession {
    /// Feeds native-rate mono samples into the session's chunker.
    pub fn push_samples(&self, samples: Vec<f32>) {
        let _ = self.frames_tx.send(samples);
    }

    /// The session's cancel token (cancel = drop everything unfinished).
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Ends audio input, flushes the tail chunk, and returns the assembled
    /// result.
    pub async fn finish(self) -> Result<TranscriptionResult, SttError> {
        drop(self.frames_tx);
        self.done
            .await
            .map_err(|error| SttError::Io(format!("partial session task failed: {error}")))?
    }
}

/// Starts the pseudo-streaming partial session.
///
/// Frames flow out through `events`: cumulative assembled text per kept
/// chunk (`is_final: false`), then exactly one final frame. Chunk cadence is
/// the ADE chunker's (≥750 ms quiet gap past 10 s, force at 35 s); every
/// chunk is resampled to 16 kHz, WAV-encoded, policy-screened, and
/// transcribed by a fresh CLI spawn with partial thread clamp. The session
/// stops accepting audio past [`MAX_SESSION_AUDIO_MS`].
#[must_use]
pub fn start_partial_session(
    engine: Arc<LocalWhisperEngine>,
    sample_rate: u32,
    events: tokio::sync::mpsc::UnboundedSender<TranscriptFrame>,
) -> LocalPartialSession {
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
    let cancel = CancelToken::new();
    let cancel_for_task = cancel.clone();
    let done = tokio::spawn(async move {
        let started_at = Instant::now();
        let mut chunker = Chunker::new(sample_rate);
        let mut assembled = String::new();
        let mut segments = 0usize;
        let mut ingested_ms = 0u64;
        let mut last_error: Option<SttError> = None;
        let threads = partial_thread_count();
        loop {
            let chunk = match frames_rx.recv().await {
                Some(samples) => {
                    if cancel_for_task.is_canceled() {
                        break;
                    }
                    if sample_rate > 0 {
                        ingested_ms = ingested_ms.saturating_add(
                            ((samples.len() as u64) * 1_000) / u64::from(sample_rate),
                        );
                    }
                    if ingested_ms > MAX_SESSION_AUDIO_MS {
                        // Cap: stop consuming; flush what we have.
                        if let Some(tail) = chunker.finish() {
                            transcribe_chunk(
                                &engine,
                                tail,
                                threads,
                                &cancel_for_task,
                                &mut assembled,
                                &mut segments,
                                &events,
                                &mut last_error,
                            )
                            .await;
                        }
                        break;
                    }
                    chunker.ingest(&samples)
                }
                None => {
                    if let Some(tail) = chunker.finish() {
                        transcribe_chunk(
                            &engine,
                            tail,
                            threads,
                            &cancel_for_task,
                            &mut assembled,
                            &mut segments,
                            &events,
                            &mut last_error,
                        )
                        .await;
                    }
                    break;
                }
            };
            if cancel_for_task.is_canceled() {
                break;
            }
            if let Some(chunk) = chunk {
                transcribe_chunk(
                    &engine,
                    chunk,
                    threads,
                    &cancel_for_task,
                    &mut assembled,
                    &mut segments,
                    &events,
                    &mut last_error,
                )
                .await;
            }
        }
        if cancel_for_task.is_canceled() {
            return Err(SttError::Canceled);
        }
        if assembled.trim().is_empty()
            && let Some(error) = last_error
        {
            return Err(error);
        }
        let result = TranscriptionResult {
            text: assembled.trim().to_owned(),
            segments,
            duration_ms: started_at.elapsed().as_millis() as u64,
        };
        // ADE parity: the final cumulative frame is emitted only when the
        // session produced text; the definitive end is `finish()`'s result.
        if !result.text.is_empty() {
            let _ = events.send(TranscriptFrame {
                provider: EngineKind::WhisperLocal,
                text: result.text.clone(),
                is_final: true,
                speech_final: true,
            });
        }
        Ok(result)
    });
    LocalPartialSession {
        frames_tx,
        cancel,
        done,
    }
}

#[allow(clippy::too_many_arguments)]
async fn transcribe_chunk(
    engine: &LocalWhisperEngine,
    chunk: PartialChunk,
    threads: usize,
    cancel: &CancelToken,
    assembled: &mut String,
    segments: &mut usize,
    events: &tokio::sync::mpsc::UnboundedSender<TranscriptFrame>,
    last_error: &mut Option<SttError>,
) {
    let resampled = resample_to_16khz(&chunk.samples, chunk.sample_rate);
    let wav_bytes = encode_wav(&resampled, crate::capture::TARGET_SAMPLE_RATE);
    let stats = CaptureStats {
        audio_ms: Some(chunk.audio_ms),
        rms: Some(chunk.rms),
        peak: Some(chunk.peak),
    };
    match engine
        .transcribe_wav_bytes(wav_bytes, stats, threads, cancel)
        .await
    {
        Ok(Some(text)) if !text.trim().is_empty() => {
            *assembled = join_partial_text(assembled, &text);
            *segments += 1;
            let _ = events.send(TranscriptFrame {
                provider: EngineKind::WhisperLocal,
                text: assembled.clone(),
                is_final: false,
                speech_final: true,
            });
        }
        Ok(_) => {}
        Err(SttError::Canceled) => {}
        Err(error) => *last_error = Some(error),
    }
}
