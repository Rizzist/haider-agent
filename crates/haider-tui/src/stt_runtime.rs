//! T2 — the TUI-process STT runtime: the ONE place that touches cpal, the
//! whisper CLI, the Deepgram socket, model downloads and profile-config IO.
//!
//! Shape: a single long-lived supervisor task (the `Link` pattern) that
//! owns the live capture session. `run_live` hands it
//! [`TalkShellCommand`]s (fire-and-forget) and every outcome flows back as
//! a [`TalkEvent`] on one channel the UI loop selects on. The capture
//! worker itself is `haider-stt`'s thin cpal seam (T1); the bridge from
//! its std-mpsc events into the async world is a dedicated thread, exactly
//! like the stdin reader.
//!
//! Determinism boundary: NOTHING in this module is law-testable with a
//! real microphone, so it stays logic-free — every decision (state
//! machine, assembly, glyphs, setup flow) lives in [`crate::talk`] and the
//! reducer, and this module only executes. The live-mic path is the
//! T-wave ship-gate probe.

use std::path::PathBuf;
use std::sync::Arc;

use haider_stt::capture::{CaptureEvent, CaptureWorker, encode_linear16};
use haider_stt::deepgram::{DEEPGRAM_API_ORIGIN, DeepgramSession, DeepgramSessionConfig};
use haider_stt::local::{LocalPartialSession, LocalWhisperEngine, start_partial_session};
use haider_stt::{SttError, TranscriptFrame};
use tokio::sync::mpsc;

use crate::talk::{
    DeepgramModelRow, TalkEngineSpec, TalkEvent, TalkSetupSnapshot, TalkShellCommand,
};

/// A talk-events sender — unbounded: emissions are UI-paced (~16 Hz
/// envelopes plus rare one-shots) and the UI loop is the sole consumer.
pub type TalkEvents = mpsc::UnboundedSender<TalkEvent>;

/// The friendly terminal-app name for the mic-permission hint, from
/// `TERM_PROGRAM`. Pure so the mapping is law-testable; `None`/unknown
/// values fall back to the generic phrase.
#[must_use]
pub fn terminal_display_name(term_program: Option<&str>) -> String {
    match term_program {
        Some("Apple_Terminal") => "Terminal".to_owned(),
        Some("iTerm.app") => "iTerm2".to_owned(),
        Some("ghostty") => "Ghostty".to_owned(),
        Some("WezTerm") => "WezTerm".to_owned(),
        Some("vscode") => "VS Code".to_owned(),
        Some(other) if !other.trim().is_empty() => other.trim().to_owned(),
        _ => "your terminal app".to_owned(),
    }
}

/// Enrich a mic hint with the ACTUAL terminal app the TCC grant belongs to
/// (macOS attributes the grant to the responsible app — the terminal, not
/// haider).
#[must_use]
pub fn enrich_mic_hint(hint: &str, term_program: Option<&str>) -> String {
    let name = terminal_display_name(term_program);
    if name == "your terminal app" {
        hint.to_owned()
    } else {
        hint.replace("your terminal app", &format!("{name} (your terminal app)"))
    }
}

enum EngineSession {
    Local(LocalPartialSession),
    Deepgram(DeepgramSession),
}

enum EngineDirective {
    Finish,
    Cancel,
}

struct ActiveTalk {
    generation: u64,
    capture: Option<CaptureWorker>,
    directive: mpsc::Sender<EngineDirective>,
}

/// Handle to the supervisor; `run_live` owns one.
pub struct TalkRuntime {
    commands: mpsc::UnboundedSender<TalkShellCommand>,
}

impl TalkRuntime {
    /// Spawn the supervisor. `store_dir` is the resolved profile store dir
    /// (config home); `events` feeds the UI loop.
    #[must_use]
    pub fn spawn(events: TalkEvents, store_dir: PathBuf) -> Self {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let commands = Arc::new(tokio::sync::Mutex::new(commands_rx));
        tokio::spawn(supervise_with_restart(commands, events, store_dir));
        Self {
            commands: commands_tx,
        }
    }

    /// Hand one command to the supervisor (non-blocking; a dead supervisor
    /// drops it — the UI's generation gates make that safe).
    pub fn execute(&self, command: TalkShellCommand) {
        let _ = self.commands.send(command);
    }
}

async fn supervise_with_restart(
    commands: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<TalkShellCommand>>>,
    events: TalkEvents,
    store_dir: PathBuf,
) {
    const MAX_STARTS: u8 = 2;
    for attempt in 1..=MAX_STARTS {
        let child = tokio::spawn(supervise(
            Arc::clone(&commands),
            events.clone(),
            store_dir.clone(),
        ));
        let outcome = child.await;
        if commands.lock().await.is_closed() {
            return;
        }
        if attempt < MAX_STARTS {
            let _ = events.send(TalkEvent::SupervisorRestarting {
                attempt: attempt + 1,
                max: MAX_STARTS,
            });
            continue;
        }
        let reason = outcome.map_or_else(
            |error| format!("talk supervisor task failed: {error}"),
            |()| "talk supervisor exited unexpectedly".into(),
        );
        let _ = events.send(TalkEvent::SupervisorFailed { reason });
        return;
    }
}

async fn supervise(
    commands: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<TalkShellCommand>>>,
    events: TalkEvents,
    store_dir: PathBuf,
) {
    let mut active: Option<ActiveTalk> = None;
    loop {
        let command = commands.lock().await.recv().await;
        let Some(command) = command else {
            teardown(&mut active).await;
            return;
        };
        match command {
            TalkShellCommand::Start { generation, engine } => {
                teardown(&mut active).await;
                match start_session(generation, engine, events.clone()).await {
                    Ok(started) => {
                        let sample_rate = started
                            .capture
                            .as_ref()
                            .map_or(0, CaptureWorker::sample_rate);
                        active = Some(started);
                        let _ = events.send(TalkEvent::Started {
                            generation,
                            sample_rate,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(TalkEvent::StartFailed { generation, error });
                    }
                }
            }
            TalkShellCommand::Finish { generation } => {
                if active.as_ref().is_some_and(|a| a.generation == generation)
                    && let Some(mut session) = active.take()
                {
                    // Stop the mic FIRST (frames end), then ask the
                    // engine to flush; the forwarder task reports the
                    // result.
                    drop_capture(session.capture.take()).await;
                    let _ = session.directive.send(EngineDirective::Finish).await;
                }
            }
            TalkShellCommand::Cancel { generation } => {
                if active.as_ref().is_some_and(|a| a.generation == generation) {
                    teardown(&mut active).await;
                }
            }
            TalkShellCommand::LoadSetup => {
                let _ = events.send(TalkEvent::SetupSnapshot {
                    snapshot: gather_snapshot(&store_dir),
                });
            }
            TalkShellCommand::ProbeKey { secret } => {
                let events = events.clone();
                tokio::spawn(async move {
                    let event = probe_key(secret).await;
                    let _ = events.send(event);
                });
            }
            TalkShellCommand::InstallModel { model_id } => {
                let events = events.clone();
                tokio::spawn(async move {
                    let error = install_model(&model_id, &events).await.err().map(|error| {
                        // Honest text; SttError never carries secrets.
                        error.to_string()
                    });
                    let _ = events.send(TalkEvent::DownloadFinished { model_id, error });
                });
            }
            TalkShellCommand::InstallRuntime => {
                let events = events.clone();
                tokio::spawn(async move {
                    let event = install_runtime().await;
                    let _ = events.send(event);
                });
            }
            TalkShellCommand::StoreConfig { config } => {
                let error = haider_stt::config::save(&store_dir, &config)
                    .err()
                    .map(|error| error.to_string());
                let _ = events.send(TalkEvent::ConfigStored { config, error });
            }
        }
    }
}

/// Drop the capture worker off the async thread (its Drop joins the cpal
/// thread).
async fn drop_capture(capture: Option<CaptureWorker>) {
    if let Some(capture) = capture {
        let _ = tokio::task::spawn_blocking(move || drop(capture)).await;
    }
}

async fn teardown(active: &mut Option<ActiveTalk>) {
    if let Some(mut session) = active.take() {
        drop_capture(session.capture.take()).await;
        let _ = session.directive.send(EngineDirective::Cancel).await;
    }
}

/// Open mic + engine for one session. On error nothing is left running.
async fn start_session(
    generation: u64,
    engine: TalkEngineSpec,
    events: TalkEvents,
) -> Result<ActiveTalk, SttError> {
    // 1. The mic first: its sample rate parameterizes both engines. The
    //    spawn blocks up to 15 s on stream readiness, so it runs on the
    //    blocking pool.
    let (capture_tx, capture_rx) = std::sync::mpsc::channel::<CaptureEvent>();
    let capture = tokio::task::spawn_blocking(move || CaptureWorker::spawn(capture_tx))
        .await
        .map_err(|error| SttError::Io(format!("capture spawn task failed: {error}")))?
        .map_err(|error| match error {
            SttError::MicUnavailable { hint } => SttError::MicUnavailable {
                hint: enrich_mic_hint(&hint, std::env::var("TERM_PROGRAM").ok().as_deref()),
            },
            other => other,
        })?;
    let sample_rate = capture.sample_rate();

    // 2. The engine session, at the mic's native rate.
    let (frames_tx, frames_rx) = mpsc::unbounded_channel::<TranscriptFrame>();
    let session = match engine {
        TalkEngineSpec::Local { model_id } => {
            let whisper_dir = haider_stt::model_dir::whisper_dir().ok_or_else(|| {
                SttError::Io("could not resolve the shared DiffForge data dir".into())
            })?;
            let model = haider_stt::catalog::effective_model(&whisper_dir, model_id.as_deref());
            let model_path = haider_stt::catalog::model_path(&whisper_dir, model);
            if !haider_stt::catalog::model_installed(&whisper_dir, model) {
                return Err(SttError::ModelMissing {
                    model_id: model.id.to_owned(),
                });
            }
            let runtime = haider_stt::runtime::discover_runtime(&whisper_dir).ok_or_else(|| {
                SttError::RuntimeMissing {
                    hint: haider_stt::runtime::RUNTIME_INSTALL_HINT.to_owned(),
                }
            })?;
            let engine = LocalWhisperEngine::new(runtime, model_path, model.id.to_owned());
            EngineSession::Local(start_partial_session(
                Arc::new(engine),
                sample_rate,
                frames_tx,
            ))
        }
        TalkEngineSpec::Deepgram {
            secret,
            model,
            language,
        } => {
            let config = DeepgramSessionConfig::new(
                secret.expose_secret(),
                model
                    .as_deref()
                    .unwrap_or(haider_stt::deepgram::DEFAULT_MODEL),
                &language,
                sample_rate,
            )?;
            drop(secret);
            EngineSession::Deepgram(haider_stt::deepgram::start_session(config, frames_tx).await?)
        }
    };

    // 3. Transcript frames → TalkEvents.
    let frame_events = events.clone();
    tokio::spawn(async move {
        let mut frames_rx = frames_rx;
        while let Some(frame) = frames_rx.recv().await {
            if frame_events
                .send(TalkEvent::Partial { generation, frame })
                .is_err()
            {
                return;
            }
        }
    });

    // 4. The engine forwarder: owns the session, feeds it PCM, and on a
    //    directive consumes it (finish → result event; cancel → drop).
    let (feed_tx, mut feed_rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let (directive_tx, mut directive_rx) = mpsc::channel::<EngineDirective>(1);
    let finish_events = events.clone();
    tokio::spawn(async move {
        let session = session;
        let directive = loop {
            tokio::select! {
                samples = feed_rx.recv() => match samples {
                    Some(samples) => match &session {
                        EngineSession::Local(local) => local.push_samples(samples),
                        EngineSession::Deepgram(deepgram) => {
                            deepgram.send_audio(encode_linear16(&samples));
                        }
                    },
                    // Capture gone (mic stopped) — wait for the directive.
                    None => {
                        break directive_rx.recv().await;
                    }
                },
                directive = directive_rx.recv() => break directive,
            }
        };
        match directive {
            Some(EngineDirective::Finish) => {
                let result = match session {
                    EngineSession::Local(local) => local.finish().await,
                    EngineSession::Deepgram(deepgram) => deepgram.finish().await,
                };
                let _ = finish_events.send(TalkEvent::Finished { generation, result });
            }
            Some(EngineDirective::Cancel) | None => match session {
                EngineSession::Local(local) => {
                    local.cancel_token().cancel();
                    drop(local);
                }
                EngineSession::Deepgram(deepgram) => drop(deepgram),
            },
        }
    });

    // 5. The capture bridge thread (the stdin-reader pattern): std-mpsc
    //    events → talk events + engine feed. Ends when the worker thread
    //    drops its sender.
    let bridge_events = events;
    let _ = std::thread::Builder::new()
        .name("haider-talk-bridge".into())
        .spawn(move || {
            while let Ok(event) = capture_rx.recv() {
                match event {
                    CaptureEvent::Envelope { level, recording } => {
                        if recording
                            && bridge_events
                                .send(TalkEvent::Envelope { generation, level })
                                .is_err()
                        {
                            return;
                        }
                    }
                    CaptureEvent::Frames { samples, .. } => {
                        if feed_tx.send(samples).is_err() {
                            return;
                        }
                    }
                    CaptureEvent::Health(health) => {
                        if bridge_events
                            .send(TalkEvent::Health { generation, health })
                            .is_err()
                        {
                            return;
                        }
                    }
                    CaptureEvent::CaptureCapReached => {
                        if bridge_events
                            .send(TalkEvent::CapReached { generation })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        });

    // 6. Roll audio: preroll + frames start flowing.
    capture.start_recording()?;

    Ok(ActiveTalk {
        generation,
        capture: Some(capture),
        directive: directive_tx,
    })
}

/// Gather the setup card's world snapshot (filesystem truth at call time —
/// the ADE may evict models at any moment, so this is a view, not a cache).
fn gather_snapshot(store_dir: &std::path::Path) -> TalkSetupSnapshot {
    let config = haider_stt::config::load(store_dir).map_err(|error| error.to_string());
    let whisper_dir = haider_stt::model_dir::whisper_dir();
    let (installed, selected_hint, runtime) =
        whisper_dir
            .as_deref()
            .map_or((Vec::new(), None, None), |dir| {
                (
                    haider_stt::catalog::installed_model_ids(dir)
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    haider_stt::catalog::selected_model_hint(dir).map(|model| model.id.to_owned()),
                    haider_stt::runtime::discover_runtime(dir)
                        .map(|path| path.display().to_string()),
                )
            });
    TalkSetupSnapshot {
        config,
        whisper_dir: whisper_dir.map(|dir| dir.display().to_string()),
        installed,
        selected_hint,
        runtime,
        runtime_hint: haider_stt::runtime::RUNTIME_INSTALL_HINT.to_owned(),
    }
}

/// Validate a key (`GET /v1/auth/token`) and fetch its streaming model
/// list. The SAME secret travels back on success so the reducer can vault
/// it without a second live copy.
async fn probe_key(secret: haider_rpc::SecretWire) -> TalkEvent {
    // The validate/fetch calls carry their own 10 s per-request budgets;
    // the shared download client is only the transport.
    let client = match haider_stt::download::download_client() {
        Ok(client) => client,
        Err(error) => {
            return TalkEvent::KeyRejected { error };
        }
    };
    if let Err(error) =
        haider_stt::deepgram::validate_key(&client, DEEPGRAM_API_ORIGIN, secret.expose_secret())
            .await
    {
        return TalkEvent::KeyRejected { error };
    }
    match haider_stt::deepgram::fetch_streaming_models(
        &client,
        DEEPGRAM_API_ORIGIN,
        secret.expose_secret(),
    )
    .await
    {
        Ok(models) => TalkEvent::KeyAccepted {
            secret,
            models: models
                .into_iter()
                .map(|model| {
                    let languages = if model.languages.is_empty() {
                        "multi".to_owned()
                    } else if model.languages.len() > 4 {
                        format!("{} languages", model.languages.len())
                    } else {
                        model.languages.join(" · ")
                    };
                    DeepgramModelRow {
                        name: if model.name.is_empty() {
                            model.canonical_name
                        } else {
                            model.name
                        },
                        languages,
                    }
                })
                .collect(),
        },
        Err(error) => TalkEvent::KeyRejected { error },
    }
}

/// Download one whisper model into the shared dir with progress events.
async fn install_model(model_id: &str, events: &TalkEvents) -> Result<(), SttError> {
    let model = haider_stt::catalog::model_by_id(model_id)
        .ok_or_else(|| SttError::InvalidArgument(format!("unknown whisper model `{model_id}`")))?;
    let whisper_dir = haider_stt::model_dir::whisper_dir()
        .ok_or_else(|| SttError::Io("could not resolve the shared DiffForge data dir".into()))?;
    std::fs::create_dir_all(&whisper_dir)
        .map_err(|error| SttError::Io(format!("could not create the model dir: {error}")))?;
    let client = haider_stt::download::download_client()?;
    let progress_events = events.clone();
    let id_for_progress = model.id.to_owned();
    haider_stt::download::install(
        &client,
        &whisper_dir,
        model.download_spec(),
        move |progress| {
            let percent = progress
                .percent
                .map(|value| (value.clamp(0.0, 100.0)) as u8);
            let _ = progress_events.send(TalkEvent::DownloadProgress {
                model_id: id_for_progress.clone(),
                percent,
            });
        },
    )
    .await
    .map(|_| ())
}

/// Drive the per-OS runtime install, then re-discover.
async fn install_runtime() -> TalkEvent {
    let Some(whisper_dir) = haider_stt::model_dir::whisper_dir() else {
        return TalkEvent::RuntimeInstalled {
            outcome: Err("could not resolve the shared DiffForge data dir".into()),
            hint: None,
        };
    };
    let client = match haider_stt::download::download_client() {
        Ok(client) => client,
        Err(error) => {
            return TalkEvent::RuntimeInstalled {
                outcome: Err(error.to_string()),
                hint: None,
            };
        }
    };
    match haider_stt::runtime::install_runtime(&client, &whisper_dir, |_progress| {}).await {
        Ok(haider_stt::runtime::RuntimeInstallOutcome::Installed) => {
            let found = haider_stt::runtime::discover_runtime(&whisper_dir)
                .map(|path| path.display().to_string());
            TalkEvent::RuntimeInstalled {
                outcome: Ok(found),
                hint: None,
            }
        }
        Ok(haider_stt::runtime::RuntimeInstallOutcome::Unavailable { hint }) => {
            TalkEvent::RuntimeInstalled {
                outcome: Ok(None),
                hint: Some(hint),
            }
        }
        Err(error) => TalkEvent::RuntimeInstalled {
            outcome: Err(error.to_string()),
            hint: None,
        },
    }
}
