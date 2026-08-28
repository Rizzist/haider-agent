//! LocalWhisperEngine laws: exact argv, stub-CLI spawns, typed
//! model-missing, cancel, and the pseudo-streaming partial session.

#![allow(clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{StubBehavior, write_stub_command};
use haider_stt::local::{
    CancelToken, LocalWhisperEngine, MAX_AUDIO_BYTES, TRANSCRIBE_TIMEOUT_SECS, build_args,
    partial_thread_count, start_partial_session, thread_count,
};
use haider_stt::policy::CaptureStats;
use haider_stt::{EngineKind, SttError, TranscriptFrame};

fn healthy_stats() -> CaptureStats {
    CaptureStats {
        audio_ms: Some(5_000),
        rms: Some(0.2),
        peak: Some(0.6),
    }
}

fn engine_with(dir: &Path, cli: PathBuf) -> LocalWhisperEngine {
    let model = dir.join("ggml-base.en.bin");
    if !model.exists() {
        std::fs::write(&model, b"stub-model-bytes").expect("stub model");
    }
    LocalWhisperEngine::new(cli, model, "base.en".to_owned()).with_temp_dir(dir.join("scratch"))
}

/// The argv is the ADE's exact latency-tuned invocation, in order.
///
/// MUTATION CHECK: drop `-nf`, reorder `-bo`/`-bs`, or add timestamps.
/// Expected runtime failure: the literal argv pin below.
#[test]
fn build_args_pins_the_exact_ade_argv() {
    let args = build_args(
        Path::new("/models/ggml-base.en.bin"),
        Path::new("/tmp/take.wav"),
        "en",
        6,
        None,
    );
    assert_eq!(
        args,
        vec![
            "-m",
            "/models/ggml-base.en.bin",
            "-f",
            "/tmp/take.wav",
            "-l",
            "en",
            "-t",
            "6",
            "-nt",
            "-np",
            "-bo",
            "1",
            "-bs",
            "1",
            "-nf",
        ]
    );
    let with_prompt = build_args(
        Path::new("/m.bin"),
        Path::new("/a.wav"),
        "en",
        4,
        Some("Haider Deepgram"),
    );
    assert_eq!(
        &with_prompt[with_prompt.len() - 2..],
        &["--prompt", "Haider Deepgram"]
    );
}

/// Thread clamps: full runs 4..=8, partials ≤4 (ADE clamp law).
#[test]
fn thread_counts_clamp_to_ade_ranges() {
    let threads = thread_count();
    assert!((4..=8).contains(&threads));
    let partial = partial_thread_count();
    assert!((1..=4).contains(&partial));
    assert_eq!(TRANSCRIBE_TIMEOUT_SECS, 180);
    assert_eq!(MAX_AUDIO_BYTES, 32 * 1024 * 1024);
}

/// A successful spawn: stdout becomes the transcript, warm-up stderr is
/// ignored, and the CLI received the pinned argv shape.
///
/// MUTATION CHECK: read the transcript from stderr, or stop passing `-nf`.
/// Expected runtime failure: the transcript below carries warm-up noise,
/// or the recorded argv loses the flag.
#[tokio::test]
async fn stub_cli_roundtrip_returns_stdout_and_receives_pinned_args() {
    let dir = tempfile::tempdir().expect("dir");
    let args_file = dir.path().join("seen-args.txt");
    let cli = write_stub_command(
        dir.path(),
        "whisper-cli",
        StubBehavior::RecordArgs {
            path: args_file.clone(),
            stdout: "  hello from stub  ".to_owned(),
            stderr: vec![
                "whisper_init_from_file: loading model".to_owned(),
                "ggml_metal_init: found device".to_owned(),
            ],
        },
    );
    let engine = engine_with(dir.path(), cli);
    let text = engine
        .transcribe_wav_bytes(
            b"RIFF-fake-wav".to_vec(),
            healthy_stats(),
            4,
            &CancelToken::new(),
        )
        .await
        .expect("stub spawn succeeds")
        .expect("policy keeps healthy transcript");
    assert_eq!(text, "hello from stub");
    let seen = std::fs::read_to_string(&args_file).expect("recorded argv");
    let seen_args: Vec<&str> = seen.lines().collect();
    assert_eq!(seen_args[0], "-m");
    assert!(seen_args[1].ends_with("ggml-base.en.bin"));
    assert_eq!(seen_args[2], "-f");
    assert!(seen_args[3].ends_with(".wav"));
    assert_eq!(
        &seen_args[4..],
        &[
            "-l", "en", "-t", "4", "-nt", "-np", "-bo", "1", "-bs", "1", "-nf"
        ]
    );
}

/// A non-zero exit reports FILTERED stderr (warm-up noise removed, real
/// diagnostics kept).
#[tokio::test]
async fn failing_cli_reports_filtered_stderr() {
    let dir = tempfile::tempdir().expect("dir");
    let cli = write_stub_command(
        dir.path(),
        "whisper-cli",
        StubBehavior::Failure {
            stderr: vec![
                "whisper_init_from_file: loading model".to_owned(),
                "error: failed to load model".to_owned(),
            ],
            exit_code: 3,
        },
    );
    let engine = engine_with(dir.path(), cli);
    let error = engine
        .transcribe_wav_bytes(b"wav".to_vec(), healthy_stats(), 4, &CancelToken::new())
        .await
        .expect_err("non-zero exit fails");
    match error {
        SttError::Endpoint(message) => {
            assert!(message.contains("failed to load model"), "{message}");
            assert!(!message.contains("whisper_init"), "{message}");
        }
        other => panic!("expected Endpoint, got {other:?}"),
    }
}

/// THE EVICTION LAW: a missing model file is the typed
/// [`SttError::ModelMissing`] state at spawn time — never a crash, never a
/// stale cached handle.
///
/// MUTATION CHECK: cache the model open across chunks (skip the per-spawn
/// existence check). Expected runtime failure: the eviction below stops
/// producing `ModelMissing`.
#[tokio::test]
async fn evicted_model_is_typed_model_missing_per_spawn() {
    let dir = tempfile::tempdir().expect("dir");
    let cli = write_stub_command(
        dir.path(),
        "whisper-cli",
        StubBehavior::Output {
            stdout: "transcribed text".to_owned(),
        },
    );
    let engine = engine_with(dir.path(), cli);
    // First spawn succeeds (model present, cache warmed).
    engine
        .transcribe_wav_bytes(b"wav".to_vec(), healthy_stats(), 4, &CancelToken::new())
        .await
        .expect("first spawn succeeds");
    // The ADE evicts the whole dir; the NEXT spawn must observe it.
    std::fs::remove_file(dir.path().join("ggml-base.en.bin")).expect("evict model");
    let error = engine
        .transcribe_wav_bytes(b"wav".to_vec(), healthy_stats(), 4, &CancelToken::new())
        .await
        .expect_err("evicted model fails");
    assert_eq!(
        error,
        SttError::ModelMissing {
            model_id: "base.en".to_owned()
        }
    );
}

/// Oversized audio is refused BEFORE any spawn (ADE 32 MiB cap).
#[tokio::test]
async fn oversized_wav_is_refused_before_spawn() {
    let dir = tempfile::tempdir().expect("dir");
    let cli = write_stub_command(
        dir.path(),
        "whisper-cli",
        StubBehavior::Output {
            stdout: "x".to_owned(),
        },
    );
    let engine = engine_with(dir.path(), cli);
    let error = engine
        .transcribe_wav_bytes(
            vec![0u8; MAX_AUDIO_BYTES + 1],
            healthy_stats(),
            4,
            &CancelToken::new(),
        )
        .await
        .expect_err("oversized refused");
    assert!(matches!(error, SttError::InvalidArgument(_)));
}

/// A fired cancel token kills the in-flight CLI promptly.
///
/// MUTATION CHECK: stop polling the token in the spawn loop. Expected
/// runtime failure: this law rides the 5 s stub sleep into its own timeout.
#[tokio::test]
async fn cancel_kills_the_inflight_spawn() {
    let dir = tempfile::tempdir().expect("dir");
    let cli = write_stub_command(
        dir.path(),
        "whisper-cli",
        StubBehavior::DelayedOutput {
            delay_ms: 5_000,
            stdout: "late".to_owned(),
        },
    );
    let engine = engine_with(dir.path(), cli);
    let cancel = CancelToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        canceller.cancel();
    });
    let started = std::time::Instant::now();
    let error = engine
        .transcribe_wav_bytes(b"wav".to_vec(), healthy_stats(), 4, &cancel)
        .await
        .expect_err("cancel fails the call");
    assert_eq!(error, SttError::Canceled);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "cancel must not wait out the stub"
    );
}

/// The partial session end-to-end over a stub CLI: chunk cadence produces
/// cumulative partial frames, `finish` returns the assembled result, and
/// every emitted frame wears the `whisper-local` provider.
///
/// MUTATION CHECK: emit per-chunk text instead of CUMULATIVE assembled
/// text, or skip the final-cumulative frame. Expected runtime failure: the
/// frame sequence pin below.
#[tokio::test]
async fn partial_session_emits_cumulative_frames_and_assembles() {
    let dir = tempfile::tempdir().expect("dir");
    let count_file = dir.path().join("count");
    let cli = write_stub_command(
        dir.path(),
        "whisper-cli",
        StubBehavior::CounterOutput {
            path: count_file,
            prefix: "chunk ".to_owned(),
        },
    );
    let engine = Arc::new(engine_with(dir.path(), cli));
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<TranscriptFrame>();
    let session = start_partial_session(engine, 1_000, events_tx);
    // Two full chunk cycles: 10 s speech + 0.8 s silence each.
    for _ in 0..2 {
        for _ in 0..100 {
            session.push_samples(vec![0.5f32; 100]);
        }
        for _ in 0..8 {
            session.push_samples(vec![0.0f32; 100]);
        }
    }
    let result = session.finish().await.expect("session finishes");
    assert_eq!(result.text, "chunk 1 chunk 2");
    assert_eq!(result.segments, 2);
    let mut frames = Vec::new();
    while let Ok(frame) = events_rx.try_recv() {
        frames.push(frame);
    }
    assert!(
        frames
            .iter()
            .all(|frame| frame.provider == EngineKind::WhisperLocal)
    );
    let texts: Vec<(&str, bool)> = frames
        .iter()
        .map(|frame| (frame.text.as_str(), frame.is_final))
        .collect();
    assert_eq!(
        texts,
        vec![
            ("chunk 1", false),
            ("chunk 1 chunk 2", false),
            ("chunk 1 chunk 2", true),
        ]
    );
}

/// Mid-session eviction: an already-assembled session still settles with
/// its text (ADE parity — errors surface only when NOTHING was assembled),
/// and a session with zero assembled text surfaces the typed error.
#[tokio::test]
async fn partial_session_error_surfaces_only_without_assembled_text() {
    // Session A: no model at all → the only chunk errors → finish is the
    // typed ModelMissing.
    let dir = tempfile::tempdir().expect("dir");
    let cli = write_stub_command(
        dir.path(),
        "whisper-cli",
        StubBehavior::Output {
            stdout: "text".to_owned(),
        },
    );
    let engine = engine_with(dir.path(), cli);
    std::fs::remove_file(dir.path().join("ggml-base.en.bin")).expect("evict model");
    let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel::<TranscriptFrame>();
    let session = start_partial_session(Arc::new(engine), 1_000, events_tx);
    for _ in 0..30 {
        session.push_samples(vec![0.5f32; 100]);
    }
    let error = session.finish().await.expect_err("nothing assembled");
    assert_eq!(
        error,
        SttError::ModelMissing {
            model_id: "base.en".to_owned()
        }
    );
}
