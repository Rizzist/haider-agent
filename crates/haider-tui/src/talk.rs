//! T2 — the `/talk` dictation experience: the toggle-to-talk state machine,
//! the right-to-left ASCII wave, the partial-transcript ghost row, and the
//! setup card. Everything in this module is DETERMINISTIC — no IO, no
//! clocks, no cpal, no sockets. The runtime side (capture worker, engines,
//! downloads, config IO) lives in [`crate::stt_runtime`] and talks back
//! exclusively through [`TalkEvent`]s; the reducer glue lives on
//! [`crate::app::AppModel`].
//!
//! Contract (T-wave brief, T2):
//! - press/⏎ on the TalkChip or `/talk` starts listening (live mode only);
//! - Esc CANCELS (discards — nothing lands anywhere);
//! - Enter COMMITS the transcript into the composer and SUBMITS;
//! - typing a character COMMITS the partial transcript into the composer
//!   and keeps editing (the engine's unseen tail is discarded — what you
//!   saw is what you keep);
//! - the ghost row is CHROME, never content: no partial text ever enters
//!   the transcript projection or the markdown machinery.

use std::collections::VecDeque;

use haider_rpc::SecretWire;
use haider_stt::capture::CaptureHealth;
use haider_stt::config::{TranscriptionConfig, TranscriptionEngine};
use haider_stt::{SttError, TranscriptFrame, TranscriptionResult};

// ---------------------------------------------------------------------------
// The right-to-left wave (the centerpiece)
// ---------------------------------------------------------------------------

/// Wave width in terminal cells — also the ring's fixed capacity. One cell
/// per envelope sample (~60 ms cadence), so the visible window spans
/// roughly the last 1.4 s of signal.
pub const WAVE_WIDTH: usize = 24;
/// Asymmetric smoothing: how fast the wave RISES toward a louder sample
/// (the ADE voice-ring recipe, TerminalView.jsx).
pub const WAVE_ATTACK: f32 = 0.5;
/// How slowly it FALLS after the voice stops — the decay tail is what makes
/// the wave read as motion instead of flicker.
pub const WAVE_DECAY: f32 = 0.13;
/// A stored slot at or above this level wears the gold (speaking) ink;
/// below it the column is faint (quiet history).
pub const WAVE_HOT_LEVEL: f32 = 0.12;
/// Minimum denominator for the noise-floor rescale — a floor calibrated
/// almost to 1.0 must not blow the division up.
pub const WAVE_FLOOR_HEADROOM: f32 = 0.05;

/// The 8-level partial blocks, quietest to loudest.
pub const WAVE_GLYPHS_BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Plain-ASCII fallback ramp for fonts without partial blocks (`/talk wave`
/// toggles). Same 8 indices, same mapping law.
pub const WAVE_GLYPHS_PLAIN: [char; 8] = ['_', '.', ':', '-', '=', '+', '#', '@'];

/// Perceptual glyph index for a stored level: `sqrt` mapping (small signals
/// get visible motion), floored into 8 steps. Total on `[0, 1]`, monotone,
/// `0.0 → 0`, `1.0 → 7`.
#[must_use]
pub fn wave_glyph_index(level: f32) -> usize {
    let display = level.clamp(0.0, 1.0).sqrt();
    ((display * 8.0).floor() as usize).min(7)
}

/// One renderable wave column: the glyph index (into either glyph table)
/// and whether the column is HOT (gold) or quiet (faint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveCell {
    pub glyph: usize,
    pub hot: bool,
}

/// A real ring whose peak sits below this reads as "no audio signal" (the
/// capture path never fed a level, or true silence) — the render then shows
/// the synthesized listening sweep so the state never looks dead.
pub const LISTENING_SIGNAL_MIN: f32 = 0.03;

/// The synthesized "I'm listening" sweep: a gold crest travelling left→right
/// over a faint baseline, keyed to the shared wall clock so it animates
/// without any real audio. Deterministic (a pure function of `clock_ms`) —
/// it is a liveness indicator, NOT a claim about your voice amplitude, so it
/// only shows when the real ring carries no signal.
#[must_use]
pub fn listening_pulse_cells(clock_ms: u64) -> Vec<WaveCell> {
    const PERIOD_MS: u64 = 1_600;
    let phase = (clock_ms % PERIOD_MS) as f32 / PERIOD_MS as f32;
    let center = phase * (WAVE_WIDTH as f32 - 1.0);
    (0..WAVE_WIDTH)
        .map(|i| {
            let distance = (i as f32 - center).abs();
            let bump = (1.0 - distance / 5.0).max(0.0);
            let level = 0.06 + 0.7 * bump * bump;
            WaveCell {
                glyph: wave_glyph_index(level),
                hot: level >= WAVE_HOT_LEVEL,
            }
        })
        .collect()
}

/// Fixed ring of smoothed envelope levels. Newest sample enters at the
/// RIGHT edge; history flows LEFT over time (`levels()[0]` is the oldest
/// visible column, `levels()[WAVE_WIDTH-1]` the newest).
#[derive(Debug, Clone, PartialEq)]
pub struct WaveRing {
    slots: VecDeque<f32>,
    smoothed: f32,
    /// Session noise floor: the quietest raw envelope seen since
    /// activation (starts at 1.0 = uncalibrated). Every sample is
    /// re-expressed as headroom above this floor, so ambient hum reads as
    /// a flat wave and speech stands out.
    floor: f32,
}

impl Default for WaveRing {
    fn default() -> Self {
        Self::new()
    }
}

impl WaveRing {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: std::iter::repeat_n(0.0, WAVE_WIDTH).collect(),
            smoothed: 0.0,
            floor: 1.0,
        }
    }

    /// Ingest one raw envelope sample (0..1): calibrate the floor, rescale,
    /// smooth asymmetrically, then shift the ring left and place the new
    /// value at the right edge.
    pub fn push(&mut self, raw: f32) {
        let raw = if raw.is_finite() {
            raw.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.floor = self.floor.min(raw);
        let headroom = (1.0 - self.floor).max(WAVE_FLOOR_HEADROOM);
        let calibrated = ((raw - self.floor) / headroom).clamp(0.0, 1.0);
        let rate = if calibrated > self.smoothed {
            WAVE_ATTACK
        } else {
            WAVE_DECAY
        };
        self.smoothed += (calibrated - self.smoothed) * rate;
        self.slots.pop_front();
        self.slots.push_back(self.smoothed);
    }

    /// The stored levels, oldest (left) to newest (right). Always exactly
    /// [`WAVE_WIDTH`] entries.
    #[must_use]
    pub fn levels(&self) -> Vec<f32> {
        self.slots.iter().copied().collect()
    }

    /// The current smoothed level (what the newest column shows).
    #[must_use]
    pub fn current(&self) -> f32 {
        self.smoothed
    }

    /// The calibrated noise floor (1.0 until the first sample lands).
    #[must_use]
    pub fn floor(&self) -> f32 {
        self.floor
    }

    /// Renderable cells, left to right.
    #[must_use]
    pub fn cells(&self) -> Vec<WaveCell> {
        self.slots
            .iter()
            .map(|&level| WaveCell {
                glyph: wave_glyph_index(level),
                hot: level >= WAVE_HOT_LEVEL,
            })
            .collect()
    }
}

/// The glyph for one cell in the chosen style.
#[must_use]
pub fn wave_glyph(cell: WaveCell, plain: bool) -> char {
    let table = if plain {
        &WAVE_GLYPHS_PLAIN
    } else {
        &WAVE_GLYPHS_BLOCKS
    };
    table[cell.glyph.min(7)]
}

// ---------------------------------------------------------------------------
// The talk state machine
// ---------------------------------------------------------------------------

/// Where the live talk session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TalkPhase {
    #[default]
    Idle,
    /// Toggle fired; the runtime is opening the mic/engine (or the key is
    /// in flight from the daemon vault). No audio captured yet.
    Starting,
    /// Capture live: wave + ghost row active.
    Listening,
    /// Enter (or the capture cap) ended input; the engine is assembling
    /// the final transcript.
    Finishing,
}

/// What a commit does with the final transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitIntent {
    /// Enter: realize into the composer and submit.
    #[default]
    Submit,
    /// The 900 s capture cap (or another forced stop): realize into the
    /// composer, but let the user press ⏎ themselves.
    Insert,
}

/// Why the reducer asked the daemon for the vaulted secret — routes the
/// [`crate::live::LiveReply::TranscriptionSecret`] answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretIntent {
    /// A Deepgram talk start is waiting on the key.
    Start,
    /// The setup card wants to know whether a key is vaulted (the secret
    /// itself is dropped).
    SetupPresence,
    /// The setup card's "reuse the vaulted key" path: validate + fetch the
    /// model list with it.
    SetupProbe,
}

/// The live talk state machine — pure data; every transition is a reducer
/// method on [`crate::app::AppModel`].
#[derive(Debug, Default)]
pub struct TalkState {
    pub phase: TalkPhase,
    /// Monotonic session mint: every start AND every cancel bumps it, so a
    /// stale runtime event (late `Finished` after Esc, an envelope from a
    /// torn-down session) correlates to a dead generation and is dropped
    /// whole.
    pub generation: u64,
    /// The engine the RUNNING session was started with.
    pub engine: TranscriptionEngine,
    pub intent: CommitIntent,
    /// The one in-flight secret read's purpose (`None` = no read pending).
    pub secret_intent: Option<SecretIntent>,
    /// Deepgram final segments (joined for display).
    finals: Vec<String>,
    /// Deepgram latest interim (replaced per frame).
    interim: String,
    /// The assembled display text the ghost row shows — for local whisper
    /// the engine's CUMULATIVE text verbatim; for Deepgram the joined
    /// finals plus the latest interim.
    pub ghost: String,
    pub wave: WaveRing,
    /// Plain-ASCII wave glyphs (`/talk wave` toggles; for fonts without
    /// partial blocks).
    pub wave_plain: bool,
}

impl TalkState {
    /// True from toggle until the session fully settles — the phases where
    /// talk owns Esc/Enter/typing and `model.listening` is held true.
    #[must_use]
    pub fn engaged(&self) -> bool {
        self.phase != TalkPhase::Idle
    }

    /// The wave renders while audio is flowing (not while starting or
    /// assembling the final text).
    #[must_use]
    pub fn wave_active(&self) -> bool {
        self.phase == TalkPhase::Listening
    }

    /// Begin a session: mint a fresh generation, reset assembly + wave.
    pub fn begin(&mut self, engine: TranscriptionEngine) -> u64 {
        self.generation += 1;
        self.phase = TalkPhase::Starting;
        self.engine = engine;
        self.intent = CommitIntent::Submit;
        self.finals.clear();
        self.interim.clear();
        self.ghost.clear();
        self.wave = WaveRing::new();
        self.generation
    }

    /// Settle back to idle. Bumps the generation so anything the old
    /// session still emits is dead on arrival.
    pub fn settle(&mut self) {
        self.generation += 1;
        self.phase = TalkPhase::Idle;
        self.secret_intent = None;
        self.finals.clear();
        self.interim.clear();
        self.ghost.clear();
    }

    /// Apply one engine transcript frame to the display assembly.
    ///
    /// Local whisper frames carry CUMULATIVE assembled text — each replaces
    /// the ghost whole. Deepgram interim frames replace the previous
    /// interim; final frames append a segment and clear the interim.
    pub fn apply_frame(&mut self, frame: &TranscriptFrame) {
        match self.engine {
            TranscriptionEngine::Local => {
                self.ghost = frame.text.clone();
            }
            TranscriptionEngine::Deepgram => {
                if frame.is_final {
                    if !frame.text.trim().is_empty() {
                        self.finals.push(frame.text.trim().to_owned());
                    }
                    self.interim.clear();
                } else {
                    self.interim = frame.text.trim().to_owned();
                }
                let mut assembled = self.finals.join(" ");
                if !self.interim.is_empty() {
                    if !assembled.is_empty() {
                        assembled.push(' ');
                    }
                    assembled.push_str(&self.interim);
                }
                self.ghost = assembled;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime command / event vocabulary
// ---------------------------------------------------------------------------

/// Which engine a talk session starts with, fully resolved by the reducer
/// (the runtime fills catalog defaults but performs no policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TalkEngineSpec {
    /// Local whisper.cpp: `None` model defers to the ADE
    /// `selected-model.txt` hint, then `base.en` (T1 `effective_model`).
    Local { model_id: Option<String> },
    /// Deepgram streaming: the vaulted key rides as [`SecretWire`]
    /// (redacted Debug, zeroize-on-drop), never a plain string.
    Deepgram {
        secret: SecretWire,
        model: Option<String>,
        language: String,
    },
}

/// Runtime-owned talk effects — the reducer's vocabulary toward
/// [`crate::stt_runtime::TalkRuntime`]. Rides
/// `AppRequest::TalkShell` → `ShellRequest::Talk`; the demo loop can never
/// reach it (the reducer refuses `/talk` in demo mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TalkShellCommand {
    /// Open the mic + engine for `generation`.
    Start {
        generation: u64,
        engine: TalkEngineSpec,
    },
    /// Commit: stop capture, flush the engine, report
    /// [`TalkEvent::Finished`].
    Finish { generation: u64 },
    /// Discard: stop capture, cancel the engine, report nothing.
    Cancel { generation: u64 },
    /// Gather the setup snapshot (installed models, runtime discovery,
    /// profile config) → [`TalkEvent::SetupSnapshot`].
    LoadSetup,
    /// Validate a Deepgram key (`GET /v1/auth/token`) and fetch its
    /// streaming model list → `KeyAccepted` / `KeyRejected`.
    ProbeKey { secret: SecretWire },
    /// Download one whisper model into the shared ADE dir (sha256-verified,
    /// `.download` → rename) with progress events.
    InstallModel { model_id: String },
    /// Drive the per-OS whisper-cli install (brew / pinned zip / hint).
    InstallRuntime,
    /// Persist the `transcription` section of the profile config.
    StoreConfig { config: TranscriptionConfig },
}

/// One Deepgram model row for the picker (name + language summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepgramModelRow {
    pub name: String,
    pub languages: String,
}

/// The setup card's world snapshot, gathered by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkSetupSnapshot {
    /// The profile `transcription` section — `Err` carries the TYPED load
    /// error (a present-but-corrupt section is never silently defaulted).
    pub config: Result<TranscriptionConfig, String>,
    /// The resolved shared model dir, for display. `None` = unresolvable.
    pub whisper_dir: Option<String>,
    /// Installed whisper model ids (filesystem truth at snapshot time).
    pub installed: Vec<String>,
    /// The ADE `selected-model.txt` hint, when readable.
    pub selected_hint: Option<String>,
    /// The discovered whisper-cli, for display. `None` = not found.
    pub runtime: Option<String>,
    /// The per-OS install hint shown when the runtime is missing.
    pub runtime_hint: String,
}

/// Everything the stt runtime reports back to the reducer. Session-scoped
/// variants carry the generation they belong to; the reducer drops stale
/// ones whole.
#[derive(Debug, Clone, PartialEq)]
pub enum TalkEvent {
    /// Unexpected supervisor death is restarted once, with both edges visible.
    SupervisorRestarting {
        attempt: u8,
        max: u8,
    },
    SupervisorFailed {
        reason: String,
    },
    /// Mic + engine are live; audio is flowing.
    Started {
        generation: u64,
        sample_rate: u32,
    },
    /// One ~60 ms envelope sample (recording only).
    Envelope {
        generation: u64,
        level: f32,
    },
    /// One engine transcript frame (cumulative for local, interim/final
    /// for Deepgram).
    Partial {
        generation: u64,
        frame: TranscriptFrame,
    },
    /// Capture watchdog news (digital zero / stall / recovered).
    Health {
        generation: u64,
        health: CaptureHealth,
    },
    /// The 900 s capture cap: recording stopped accumulating.
    CapReached {
        generation: u64,
    },
    /// The engine's assembled session result (after `Finish`).
    Finished {
        generation: u64,
        result: Result<TranscriptionResult, SttError>,
    },
    /// The session could not start (mic denied, model missing, runtime
    /// missing, bad key, endpoint down, …).
    StartFailed {
        generation: u64,
        error: SttError,
    },
    /// Setup snapshot answer.
    SetupSnapshot {
        snapshot: TalkSetupSnapshot,
    },
    /// The probed key validated; the streaming model list rode along. The
    /// SAME secret travels back so the reducer can vault it without ever
    /// holding two live copies.
    KeyAccepted {
        secret: SecretWire,
        models: Vec<DeepgramModelRow>,
    },
    /// The probed key was refused (401/403) or the endpoint failed.
    KeyRejected {
        error: SttError,
    },
    /// Whisper model download progress (`None` percent = unknown total).
    DownloadProgress {
        model_id: String,
        percent: Option<u8>,
    },
    /// Download settled: `None` error = installed and verified.
    DownloadFinished {
        model_id: String,
        error: Option<String>,
    },
    /// Runtime install settled: `Ok(Some(path))` = installed and
    /// re-discovered, `Ok(None)` = driver unavailable (hint in `hint`),
    /// `Err` = the driver failed.
    RuntimeInstalled {
        outcome: Result<Option<String>, String>,
        hint: Option<String>,
    },
    /// Config save settled.
    ConfigStored {
        config: TranscriptionConfig,
        error: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// The setup card
// ---------------------------------------------------------------------------

/// Which pane of the setup flow is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStage {
    /// Engine picker: local whisper | deepgram.
    Engine,
    /// Whisper model rows (+ runtime row) with download progress.
    Local,
    /// Deepgram key paste (masked) / reuse.
    DeepgramKey,
    /// Fetched streaming model list.
    DeepgramModels,
    /// Language field (Deepgram only; local models are `.en`).
    Language,
}

/// One whisper model row's install state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhisperRowState {
    Installed,
    Absent,
    Downloading { percent: Option<u8> },
    Failed(String),
}

/// One whisper model row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperRow {
    pub id: &'static str,
    /// `tier · NNN MB on disk`.
    pub detail: String,
    pub state: WhisperRowState,
}

/// The whisper-cli runtime row's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRowState {
    /// Snapshot not yet loaded.
    Unknown,
    Found(String),
    /// Missing; carries the per-OS install hint.
    Missing(String),
    Installing,
    Failed(String),
}

/// Where the Deepgram key flow is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStage {
    /// Typing/pasting a key.
    Entry,
    /// A key is vaulted — ⏎ reuses it, `r` retypes.
    Reuse,
    /// `GET /v1/auth/token` + model fetch in flight.
    Validating,
    /// `transcription.secret_set` in flight.
    Storing,
}

/// The `/talk` setup card. Owns the input band while open (the login-card
/// modality): every keystroke lands here, the composer never sees a key
/// byte, and the renderer is handed a MASK LENGTH, never the secret.
pub struct TalkSetupCard {
    pub stage: SetupStage,
    /// Highlighted row on the current stage.
    pub selection: usize,
    /// Working copy of the profile section; saved via `StoreConfig` on
    /// completion, never optimistically.
    pub config: TranscriptionConfig,
    /// Snapshot arrived (whisper/runtime rows are real, not "loading…").
    pub loaded: bool,
    pub whisper: Vec<WhisperRow>,
    pub runtime: RuntimeRowState,
    /// The typed Deepgram key. Never rendered, never persisted, never
    /// logged; zeroized on drop.
    key: zeroize::Zeroizing<String>,
    /// Daemon truth: a key is vaulted (drives the Reuse path).
    pub key_present: bool,
    pub key_stage: KeyStage,
    /// The in-flight probe reuses the VAULTED key (skip the store on
    /// acceptance — it is already vaulted).
    pub key_reused: bool,
    /// Fetched streaming models (post-probe).
    pub models: Vec<DeepgramModelRow>,
    /// Editable language field (Deepgram).
    pub language: String,
    /// Honest inline error for the current stage.
    pub error: Option<String>,
    /// The daemon advertises `transcription_v1` (without it the Deepgram
    /// path is honestly refused — there is nowhere to vault the key).
    pub vault_supported: bool,
    /// A config save is in flight (`StoreConfig` issued; the card closes
    /// on the stored reply, never optimistically).
    pub saving: bool,
}

impl std::fmt::Debug for TalkSetupCard {
    /// Redacted by construction (the `LoginCard` precedent): the key —
    /// and its length — never reach a `{:?}`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TalkSetupCard")
            .field("stage", &self.stage)
            .field("selection", &self.selection)
            .field("key", &"<redacted>")
            .field("key_present", &self.key_present)
            .field("key_stage", &self.key_stage)
            .finish_non_exhaustive()
    }
}

impl TalkSetupCard {
    #[must_use]
    pub fn new(config: TranscriptionConfig, vault_supported: bool) -> Self {
        let language = config.language.clone();
        Self {
            stage: SetupStage::Engine,
            selection: 0,
            config,
            loaded: false,
            whisper: Vec::new(),
            runtime: RuntimeRowState::Unknown,
            key: zeroize::Zeroizing::new(String::new()),
            key_present: false,
            key_stage: KeyStage::Entry,
            key_reused: false,
            models: Vec::new(),
            language,
            error: None,
            vault_supported,
            saving: false,
        }
    }

    /// Install the snapshot: build the three catalog rows with filesystem
    /// truth, the runtime row, and the config seed.
    pub fn apply_snapshot(&mut self, snapshot: TalkSetupSnapshot) {
        self.loaded = true;
        match snapshot.config {
            Ok(config) => {
                self.language = config.language.clone();
                self.config = config;
            }
            Err(error) => self.error = Some(error),
        }
        self.whisper = haider_stt::catalog::WHISPER_MODELS
            .iter()
            .map(|model| WhisperRow {
                id: model.id,
                detail: format!("{} · {} MB on disk", model.tier, model.approximate_disk_mb),
                state: if snapshot.installed.iter().any(|id| id == model.id) {
                    WhisperRowState::Installed
                } else {
                    WhisperRowState::Absent
                },
            })
            .collect();
        self.runtime = match snapshot.runtime {
            Some(path) => RuntimeRowState::Found(path),
            None => RuntimeRowState::Missing(snapshot.runtime_hint),
        };
    }

    /// How many mask glyphs to draw (capped by the renderer).
    #[must_use]
    pub fn masked_len(&self) -> usize {
        self.key.chars().count()
    }

    #[must_use]
    pub fn key_is_empty(&self) -> bool {
        self.key.is_empty()
    }

    /// One typed character into the key buffer.
    pub fn key_push(&mut self, c: char) {
        if self.key_stage == KeyStage::Entry && !c.is_control() {
            self.key.push(c);
        }
    }

    /// Bracketed paste into the key buffer (keys are pasted more often
    /// than typed).
    pub fn key_push_str(&mut self, text: &str) {
        if self.key_stage == KeyStage::Entry {
            self.key.push_str(text.trim());
        }
    }

    pub fn key_backspace(&mut self) {
        if self.key_stage == KeyStage::Entry {
            self.key.pop();
        }
    }

    /// TAKE the key for the probe — the card's copy empties in the same
    /// move, so exactly one live copy exists between here and the wire.
    pub fn take_key(&mut self) -> SecretWire {
        let taken = std::mem::replace(&mut self.key, zeroize::Zeroizing::new(String::new()));
        SecretWire::new(taken.as_str())
    }

    /// Rows selectable on the current stage (for ↑↓/digit navigation).
    #[must_use]
    pub fn row_count(&self) -> usize {
        match self.stage {
            SetupStage::Engine => 2,
            // 3 model rows + the runtime row.
            SetupStage::Local => self.whisper.len() + 1,
            SetupStage::DeepgramKey | SetupStage::Language => 1,
            SetupStage::DeepgramModels => self.models.len().max(1),
        }
    }

    /// Band rows this card claims (title + stage rows + hint), clamped so
    /// a tall model list cannot swallow the screen.
    #[must_use]
    pub fn height(&self) -> u16 {
        let body = match self.stage {
            SetupStage::Engine => 2,
            SetupStage::Local => {
                if self.loaded {
                    self.whisper.len() + 1
                } else {
                    1
                }
            }
            // key line + status line.
            SetupStage::DeepgramKey => 2,
            SetupStage::DeepgramModels => self.models.len().clamp(1, 5),
            SetupStage::Language => 1,
        };
        let error = usize::from(self.error.is_some());
        // title + body + error + hint row.
        u16::try_from(2 + body + error).unwrap_or(8)
    }
}

/// Cap on transcript characters realized into the composer per commit
/// (ADE insert cap — `haider-stt` policy).
pub const MAX_REALIZED_CHARS: usize = haider_stt::policy::MAX_TRANSCRIPT_INSERT_CHARS;

/// Truncate a transcript to the realize cap at a char boundary.
#[must_use]
pub fn clamp_realized(text: &str) -> &str {
    match text.char_indices().nth(MAX_REALIZED_CHARS) {
        Some((byte, _)) => &text[..byte],
        None => text,
    }
}
