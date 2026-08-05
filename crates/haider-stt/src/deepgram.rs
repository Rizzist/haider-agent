//! Deepgram cloud engine: key validation, streaming model catalog, and the
//! `/v1/listen` WebSocket session.
//!
//! Doc-verified surface (locked decision 5):
//! - Auth: `Authorization: Token <key>` for REST and the WS handshake.
//! - Paste-time validation: `GET /v1/auth/token` (zero transcription cost).
//! - Model catalog: `GET /v1/models`, filtered `streaming: true` (drops the
//!   batch-only `whisper-*` family) and EXCLUDING Flux (Flux lives on
//!   `/v2/listen` and would fail on `/v1/listen`).
//! - Streaming: `wss://api.deepgram.com/v1/listen?model=…&language=…&
//!   encoding=linear16&sample_rate=<native>&channels=1&interim_results=true&
//!   smart_format=true`, binary i16 LE PCM frames, `{"type":"KeepAlive"}`
//!   every 3–5 s, `{"type":"CloseStream"}` + bounded ≤8 s drain to end, and
//!   a hard 900 s session cap (a stuck-open socket bills per minute).
//!
//! Message semantics are the ADE port (`handle_deepgram_realtime_text`):
//! `type == "Error"` fails with `err_msg|message|error`; finals accumulate
//! into segments, interims overwrite; the session result joins finals with
//! spaces, falling back to the last interim.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::{EngineKind, SttError, TranscriptFrame, TranscriptionResult};

/// Production REST origin.
pub const DEEPGRAM_API_ORIGIN: &str = "https://api.deepgram.com";
/// Production streaming endpoint (ADE `DEEPGRAM_LISTEN_WS_URL`).
pub const DEEPGRAM_LISTEN_WS_URL: &str = "wss://api.deepgram.com/v1/listen";
/// Default model when the user has not picked one (ADE `DEEPGRAM_MODEL`).
pub const DEFAULT_MODEL: &str = "nova-3";
/// Default language (ADE `DEEPGRAM_DEFAULT_LANGUAGE`).
pub const DEFAULT_LANGUAGE: &str = "en";
/// WS connect budget (ADE `DEEPGRAM_CONNECT_TIMEOUT_SECS`).
pub const CONNECT_TIMEOUT_SECS: u64 = 10;
/// CloseStream drain budget (ADE `DEEPGRAM_CLOSE_TIMEOUT_SECS`).
pub const CLOSE_TIMEOUT_SECS: u64 = 8;
/// Key ceiling (ADE `DEEPGRAM_MAX_API_KEY_LENGTH`).
pub const MAX_API_KEY_LENGTH: usize = 512;
/// Language ceiling (ADE `DEEPGRAM_MAX_LANGUAGE_LENGTH`).
pub const MAX_LANGUAGE_LENGTH: usize = 24;
/// KeepAlive cadence — inside Deepgram's documented 3–5 s window.
pub const KEEPALIVE_INTERVAL_SECS: u64 = 4;
/// Hard session cap (ADE capture parity: 900 s).
pub const MAX_SESSION_SECS: u64 = 900;

/// Validates and trims a pasted API key (ADE `clean_deepgram_api_key`):
/// non-empty, ≤512 chars, no control bytes. The key VALUE never enters an
/// error message.
pub fn clean_api_key(value: &str) -> Result<String, SttError> {
    let api_key = value.trim();
    if api_key.is_empty() {
        return Err(SttError::InvalidArgument(
            "add a Deepgram API key before recording in cloud mode".into(),
        ));
    }
    if api_key.len() > MAX_API_KEY_LENGTH || api_key.chars().any(char::is_control) {
        return Err(SttError::InvalidArgument(
            "Deepgram API key is not valid".into(),
        ));
    }
    Ok(api_key.to_owned())
}

/// Validates a language tag (ADE `clean_deepgram_language`):
/// `[A-Za-z0-9-]{1,24}`, empty → `en`.
pub fn clean_language(value: Option<String>) -> Result<String, SttError> {
    let language = value
        .unwrap_or_else(|| DEFAULT_LANGUAGE.to_owned())
        .trim()
        .to_owned();
    if language.is_empty() {
        return Ok(DEFAULT_LANGUAGE.to_owned());
    }
    if language.len() > MAX_LANGUAGE_LENGTH
        || !language
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(SttError::InvalidArgument(
            "Deepgram language must be a supported language code".into(),
        ));
    }
    Ok(language)
}

fn percent_encode_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Builds the exact `/v1/listen` URL (ADE `deepgram_realtime_url` with the
/// selected model replacing the hardcoded `nova-3`). Parameter set and
/// order are a pinned contract.
#[must_use]
pub fn realtime_url(base_ws_url: &str, model: &str, language: &str, sample_rate: u32) -> String {
    format!(
        "{base_ws_url}?model={}&language={}&encoding=linear16&sample_rate={sample_rate}&channels=1&interim_results=true&smart_format=true",
        percent_encode_query_component(model),
        percent_encode_query_component(language),
    )
}

fn auth_error(status: reqwest::StatusCode) -> SttError {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        SttError::Unauthorized("Deepgram rejected this API key".into())
    } else {
        SttError::Endpoint(format!("Deepgram answered HTTP {status}"))
    }
}

/// Paste-time key validation: `GET <origin>/v1/auth/token`.
pub async fn validate_key(
    client: &reqwest::Client,
    origin: &str,
    api_key: &str,
) -> Result<(), SttError> {
    let api_key = clean_api_key(api_key)?;
    let response = client
        .get(format!("{origin}/v1/auth/token"))
        .header("Authorization", format!("Token {api_key}"))
        .timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|error| SttError::Transport(format!("could not reach Deepgram: {error}")))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(auth_error(response.status()))
    }
}

/// One Deepgram STT model row (tolerant decode of `GET /v1/models`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct DeepgramModel {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub canonical_name: String,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub batch: bool,
    #[serde(default)]
    pub streaming: bool,
}

#[derive(serde::Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    stt: Vec<DeepgramModel>,
}

/// Whether a model row is Flux (excluded: Flux only speaks `/v2/listen`).
#[must_use]
pub fn is_flux_model(model: &DeepgramModel) -> bool {
    model.architecture.eq_ignore_ascii_case("flux")
        || model.name.to_ascii_lowercase().starts_with("flux")
        || model
            .canonical_name
            .to_ascii_lowercase()
            .starts_with("flux")
}

/// Fetches the dictation-eligible model catalog: `GET <origin>/v1/models`,
/// keeping only `streaming: true` rows that are not Flux.
pub async fn fetch_streaming_models(
    client: &reqwest::Client,
    origin: &str,
    api_key: &str,
) -> Result<Vec<DeepgramModel>, SttError> {
    let api_key = clean_api_key(api_key)?;
    let response = client
        .get(format!("{origin}/v1/models"))
        .header("Authorization", format!("Token {api_key}"))
        .timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|error| SttError::Transport(format!("could not reach Deepgram: {error}")))?;
    if !response.status().is_success() {
        return Err(auth_error(response.status()));
    }
    let models: ModelsResponse = response
        .json()
        .await
        .map_err(|error| SttError::Endpoint(format!("invalid Deepgram model list: {error}")))?;
    Ok(models
        .stt
        .into_iter()
        .filter(|model| model.streaming && !is_flux_model(model))
        .collect())
}

/// Error text extraction (ADE `deepgram_error_from_body`):
/// `err_msg` → `message` → `error`.
#[must_use]
pub fn error_from_body(body: &Value) -> Option<String> {
    body.get("err_msg")
        .or_else(|| body.get("message"))
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Applies one server text payload to the running accumulation state and
/// reports an emittable frame (ADE `handle_deepgram_realtime_text`).
pub fn handle_realtime_text(
    text: &str,
    final_segments: &mut Vec<String>,
    latest_interim: &mut String,
) -> Result<Option<TranscriptFrame>, SttError> {
    let body: Value = serde_json::from_str(text).map_err(|error| {
        SttError::Endpoint(format!(
            "Deepgram realtime stream returned invalid JSON: {error}"
        ))
    })?;
    let message_type = body.get("type").and_then(Value::as_str).unwrap_or("");
    if message_type == "Error" {
        return Err(SttError::Endpoint(error_from_body(&body).unwrap_or_else(
            || "Deepgram realtime transcription failed".into(),
        )));
    }
    let transcript = body
        .pointer("/channel/alternatives/0/transcript")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if transcript.is_empty() {
        return Ok(None);
    }
    let is_final = body
        .get("is_final")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let speech_final = body
        .get("speech_final")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_final {
        final_segments.push(transcript.to_owned());
        latest_interim.clear();
    } else {
        *latest_interim = transcript.to_owned();
    }
    Ok(Some(TranscriptFrame {
        provider: EngineKind::Deepgram,
        text: transcript.to_owned(),
        is_final,
        speech_final,
    }))
}

/// Session configuration; production values are the constants above, tests
/// inject a local fixture URL and short budgets.
#[derive(Debug, Clone)]
pub struct DeepgramSessionConfig {
    pub ws_url: String,
    pub api_key: Zeroizing<String>,
    pub model: String,
    pub language: String,
    pub sample_rate: u32,
    pub keepalive_interval: Duration,
    pub close_timeout: Duration,
    pub connect_timeout: Duration,
    pub max_session: Duration,
}

impl DeepgramSessionConfig {
    /// Production defaults for one dictation session.
    pub fn new(
        api_key: &str,
        model: &str,
        language: &str,
        sample_rate: u32,
    ) -> Result<Self, SttError> {
        Ok(Self {
            ws_url: DEEPGRAM_LISTEN_WS_URL.to_owned(),
            api_key: Zeroizing::new(clean_api_key(api_key)?),
            model: model.to_owned(),
            language: clean_language(Some(language.to_owned()))?,
            sample_rate,
            keepalive_interval: Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
            close_timeout: Duration::from_secs(CLOSE_TIMEOUT_SECS),
            connect_timeout: Duration::from_secs(CONNECT_TIMEOUT_SECS),
            max_session: Duration::from_secs(MAX_SESSION_SECS),
        })
    }
}

/// A live streaming session.
///
/// Feed i16 LE PCM binary frames through [`Self::send_audio`]; transcript
/// frames arrive on the events receiver handed to [`start_session`].
/// [`Self::finish`] ends input, performs CloseStream + bounded drain, and
/// returns the assembled result.
pub struct DeepgramSession {
    audio_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    done: tokio::task::JoinHandle<Result<TranscriptionResult, SttError>>,
}

impl DeepgramSession {
    /// Queues one binary PCM frame (silently dropped after finish/cap).
    pub fn send_audio(&self, pcm_le_bytes: Vec<u8>) {
        let _ = self.audio_tx.send(pcm_le_bytes);
    }

    /// Ends audio input and returns the session result after the drain.
    pub async fn finish(self) -> Result<TranscriptionResult, SttError> {
        drop(self.audio_tx);
        self.done
            .await
            .map_err(|error| SttError::Io(format!("deepgram session task failed: {error}")))?
    }
}

/// Opens the WebSocket (Token auth, bounded connect) and spawns the session
/// loop. Returns after the handshake so a bad key/endpoint fails HERE, not
/// on first audio.
pub async fn start_session(
    config: DeepgramSessionConfig,
    events: tokio::sync::mpsc::UnboundedSender<TranscriptFrame>,
) -> Result<DeepgramSession, SttError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::protocol::Message;

    let url = realtime_url(
        &config.ws_url,
        &config.model,
        &config.language,
        config.sample_rate,
    );
    let mut request = url
        .into_client_request()
        .map_err(|error| SttError::InvalidArgument(format!("invalid Deepgram URL: {error}")))?;
    let auth_value = format!("Token {}", config.api_key.as_str());
    let header = auth_value
        .parse()
        .map_err(|_| SttError::InvalidArgument("Deepgram API key could not be sent".into()))?;
    request.headers_mut().insert("Authorization", header);
    let (ws_stream, _) = tokio::time::timeout(
        config.connect_timeout,
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| SttError::Timeout("Deepgram WebSocket connect".into()))?
    .map_err(|error| {
        SttError::Transport(format!(
            "unable to open Deepgram realtime WebSocket: {error}"
        ))
    })?;

    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let done = tokio::spawn(async move {
        let started_at = std::time::Instant::now();
        let (mut write, mut read) = ws_stream.split();
        let mut final_segments: Vec<String> = Vec::new();
        let mut latest_interim = String::new();
        let mut stream_error: Option<SttError> = None;
        let mut keepalive = tokio::time::interval(config.keepalive_interval);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let cap = tokio::time::sleep(config.max_session);
        tokio::pin!(cap);
        loop {
            tokio::select! {
                maybe_audio = audio_rx.recv() => {
                    let Some(audio_bytes) = maybe_audio else {
                        break;
                    };
                    if !audio_bytes.is_empty()
                        && let Err(error) = write.send(Message::Binary(audio_bytes.into())).await
                    {
                        stream_error = Some(SttError::Transport(format!(
                            "unable to stream audio to Deepgram: {error}"
                        )));
                        break;
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(error) = write
                        .send(Message::Text("{\"type\":\"KeepAlive\"}".into()))
                        .await
                    {
                        stream_error = Some(SttError::Transport(format!(
                            "unable to keep Deepgram stream alive: {error}"
                        )));
                        break;
                    }
                }
                () = &mut cap => {
                    // Hard cap: stop input and settle what we have.
                    break;
                }
                maybe_message = read.next() => {
                    match maybe_message {
                        Some(Ok(message)) => {
                            match apply_message(
                                message,
                                &mut final_segments,
                                &mut latest_interim,
                                &events,
                            ) {
                                Ok(true) => break,
                                Ok(false) => {}
                                Err(error) => {
                                    stream_error = Some(error);
                                    break;
                                }
                            }
                        }
                        Some(Err(error)) => {
                            stream_error = Some(SttError::Transport(format!(
                                "Deepgram realtime stream failed: {error}"
                            )));
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        if stream_error.is_none() {
            let close_message = Message::Text("{\"type\":\"CloseStream\"}".into());
            if let Err(error) = write.send(close_message).await {
                stream_error = Some(SttError::Transport(format!(
                    "unable to close Deepgram realtime stream: {error}"
                )));
            }
        }
        if stream_error.is_none() {
            loop {
                match tokio::time::timeout(config.close_timeout, read.next()).await {
                    Ok(Some(Ok(message))) => {
                        match apply_message(
                            message,
                            &mut final_segments,
                            &mut latest_interim,
                            &events,
                        ) {
                            Ok(true) => break,
                            Ok(false) => {}
                            Err(error) => {
                                stream_error = Some(error);
                                break;
                            }
                        }
                    }
                    Ok(Some(Err(error))) => {
                        stream_error = Some(SttError::Transport(format!(
                            "Deepgram realtime stream failed: {error}"
                        )));
                        break;
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
        if let Some(error) = stream_error {
            return Err(error);
        }
        let transcript = if final_segments.is_empty() {
            latest_interim
        } else {
            final_segments.join(" ")
        };
        let segments = if final_segments.is_empty() {
            usize::from(!transcript.trim().is_empty())
        } else {
            final_segments.len()
        };
        // ADE parity: per-message frames were already emitted; the
        // assembled session text is `finish()`'s result, not an extra frame.
        Ok(TranscriptionResult {
            text: transcript.trim().to_owned(),
            segments,
            duration_ms: started_at.elapsed().as_millis() as u64,
        })
    });
    Ok(DeepgramSession { audio_tx, done })
}

fn apply_message(
    message: tokio_tungstenite::tungstenite::protocol::Message,
    final_segments: &mut Vec<String>,
    latest_interim: &mut String,
    events: &tokio::sync::mpsc::UnboundedSender<TranscriptFrame>,
) -> Result<bool, SttError> {
    use tokio_tungstenite::tungstenite::protocol::Message;
    match message {
        Message::Text(text) => {
            if let Some(frame) =
                handle_realtime_text(text.as_ref(), final_segments, latest_interim)?
            {
                let _ = events.send(frame);
            }
            Ok(false)
        }
        Message::Binary(bytes) => {
            if let Ok(text) = std::str::from_utf8(&bytes)
                && let Some(frame) = handle_realtime_text(text, final_segments, latest_interim)?
            {
                let _ = events.send(frame);
            }
            Ok(false)
        }
        Message::Close(_) => Ok(true),
        _ => Ok(false),
    }
}
