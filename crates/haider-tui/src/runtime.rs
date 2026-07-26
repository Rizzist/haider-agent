//! The interactive runtime: one task owns the terminal and the [`AppModel`];
//! input, envelopes and frame deadlines multiplex through `tokio::select!`
//! (research rec 3/6). Alternate screen for v0.1 (rec 1); native-scrollback
//! insertion is explicitly deferred (rec 19).

use crate::app::{AppEvent, AppModel, AppRequest};
use crate::mock::demo_script;
use crate::render::render;
use crate::script::{
    AUTO_TITLE_MS, Beat, DemoEvent, TALK_HOLD_MS, compaction_beats, from_legacy, respond_beats,
    title_note,
};
use crate::theme::{Rgb, ThemeKey};
use haider_protocol::EventPayload;
use haider_protocol::provider::{Usage, UsageSource};
use haider_protocol::state::{HarnessStatus, RunState};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{event, execute};
use std::io::{Stdout, Write, stdout};
use std::time::Duration;
use tokio::sync::mpsc;

/// OSC 11 sequence setting the terminal's own background — the emulator's
/// window padding around the cell grid then matches the theme ground, so the
/// app reaches the window edge. Restored by [`osc_reset_background`].
#[must_use]
pub fn osc_set_background(rgb: Rgb) -> String {
    format!("\u{1b}]11;#{:02x}{:02x}{:02x}\u{7}", rgb.r, rgb.g, rgb.b)
}

/// OSC 111: reset the terminal background to the user's default.
#[must_use]
pub const fn osc_reset_background() -> &'static str {
    "\u{1b}]111\u{7}"
}

/// OSC 2: set the terminal window title (owner ask — the window should say
/// what haider is doing). Restored via [`osc_reset_title`] on exit.
#[must_use]
pub fn osc_set_title(title: &str) -> String {
    format!("\u{1b}]2;{title}\u{7}")
}

/// Best effort title restore: a single XTWINOPS pop matching the single
/// push (terminals without the stack simply ignore it).
#[must_use]
pub const fn osc_reset_title() -> &'static str {
    "\u{1b}[23;2t"
}

static TITLE_PUSHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn sync_window_title(title: &str) {
    let mut out = stdout();
    // Push the user's title exactly ONCE; later syncs only set. The single
    // matching pop runs in restore_terminal (review r1 P2: balanced stack).
    if !TITLE_PUSHED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[22;2t");
    }
    let _ = out.write_all(osc_set_title(title).as_bytes());
    let _ = out.flush();
}

/// Best-effort terminal-background sync to the active theme.
fn sync_terminal_bg(theme: ThemeKey) {
    let mut out = stdout();
    let _ = out.write_all(osc_set_background(theme.theme().bg).as_bytes());
    let _ = out.flush();
}

/// Detect the system/terminal appearance (OSC 11 background luminance) and
/// map it to a theme: dark ground → Dark, light ground (or undetectable) →
/// Desert Dawn. Call BEFORE entering raw mode/alt screen.
///
/// Known residual (review TUI1 P2): the probe owns the tty for its bounded
/// window, so a keystroke typed in that pre-UI instant is consumed, not
/// forwarded. The window is kept tiny (80ms) and runs before any UI invites
/// input. The loss-free design — parsing the OSC reply inside the sole input
/// reader — lands with the daemon-era input stack (see OPTIMIZATIONS.md).
#[must_use]
pub fn detect_system_theme() -> ThemeKey {
    match termbg::theme(Duration::from_millis(80)) {
        Ok(termbg::Theme::Dark) => ThemeKey::Dark,
        Ok(termbg::Theme::Light) | Err(_) => ThemeKey::Dawn,
    }
}

/// RAII terminal state: raw mode + alternate screen + bracketed paste on
/// construction, restored on drop AND on panic (the panic hook restores
/// first so the report is readable — rec 18's restoration guarantee).
///
/// One-shot per process (review r1 P3): a second `enter` while one guard
/// lives fails instead of stacking panic hooks and fighting over the screen.
/// The panic hook stays installed after drop — `restore_terminal` is
/// idempotent and harmless once the terminal is already restored.
pub struct TerminalGuard {
    /// Construction only via [`TerminalGuard::enter`] — an unentered guard
    /// must be unrepresentable (its Drop restores state it never owned).
    _private: (),
}

static GUARD_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PANIC_HOOK: std::sync::Once = std::sync::Once::new();

impl TerminalGuard {
    /// Enter raw mode + alt screen TRANSACTIONALLY (review r1 P1): if any
    /// later step fails, everything already entered is rolled back before
    /// the error returns. The chaining panic hook is installed ONCE per
    /// process (review r2 P3 — enter/drop cycles must not stack hooks) and
    /// only restores while a guard is actually active.
    pub fn enter() -> std::io::Result<Self> {
        use std::sync::atomic::Ordering;
        if GUARD_ACTIVE.swap(true, Ordering::SeqCst) {
            return Err(std::io::Error::other("terminal guard already active"));
        }
        if let Err(error) = enable_raw_mode() {
            GUARD_ACTIVE.store(false, Ordering::SeqCst);
            return Err(error);
        }
        if let Err(error) = execute!(
            stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        ) {
            // Roll back the partial setup: raw mode is on, and the alternate
            // screen may or may not have been entered before the failure —
            // restore_terminal unwinds both (leaving a screen we never
            // entered is a no-op escape sequence).
            restore_terminal();
            GUARD_ACTIVE.store(false, Ordering::SeqCst);
            return Err(error);
        }
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if GUARD_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) {
                    restore_terminal();
                }
                previous(info);
            }));
        });
        Ok(Self { _private: () })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        GUARD_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = stdout().write_all(osc_reset_background().as_bytes());
    let _ = stdout().write_all(osc_reset_title().as_bytes());
    let _ = stdout().flush();
}

/// Demo pacing per payload — boot beats slower, stream deltas fast.
#[must_use]
pub fn demo_pace(payload: &EventPayload) -> Duration {
    match payload {
        EventPayload::HarnessStatus(HarnessStatus::Starting { .. }) => Duration::from_millis(420),
        EventPayload::HarnessStatus(_) => Duration::from_millis(650),
        EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. }) => {
            Duration::from_millis(130)
        }
        EventPayload::RunState(_) => Duration::from_millis(240),
        _ => Duration::from_millis(300),
    }
}

/// Run `haider tui --demo`: the scripted stream drives every surface.
/// Returns when the user quits (Ctrl+C) or input closes.
pub async fn run_demo(mut model: AppModel) -> std::io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    // Sync the emulator's own background (window padding) to the theme
    // ground. (No Terminal::clear() here: ratatui's clear paths can issue a
    // cursor-position query that hangs non-answering PTYs; the first full
    // draw repaints every cell anyway.)
    sync_terminal_bg(model.theme);
    sync_window_title(&model.window_title());
    let mut active_theme = model.theme;
    let mut active_title = model.window_title();

    // Input pump: crossterm's blocking read on a dedicated thread, forwarded
    // into the async loop (no event-stream feature needed).
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(item) => {
                    if input_tx.blocking_send(item).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    // The demo driver owns the generation-tagged envelope channel and the
    // script/decay timers — the SAME production seams the tests drive
    // (review r3 P3-7).
    let (mut driver, mut envelope_rx) = DemoDriver::new(64);
    driver.spawn_boot();
    let answer_echo = driver.sender();
    // Launcher auto-play: if untouched, the classic demo plays once.
    let autoplay = tokio::time::sleep(Duration::from_secs(6));
    tokio::pin!(autoplay);

    let mut frame_tick = tokio::time::interval(Duration::from_millis(33));
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Fused: after the stream closes this branch is disabled — a closed
    // receiver must never spin the loop (review r1 P1).
    let mut stream_open = true;
    // The last frame's clickable regions (render reports, mouse consumes).
    let mut hit_map: Vec<(ratatui::layout::Rect, crate::app::Hit)> = Vec::new();

    while !model.should_quit {
        tokio::select! {
            input = input_rx.recv() => match input {
                Some(event) => dispatch_input(&mut model, &hit_map, event),
                None => break,
            },
            () = &mut autoplay, if !model.auto_play_spent => {
                model.handle(AppEvent::AutoPlay);
            }
            tagged = envelope_rx.recv(), if stream_open => match tagged {
                Some((generation, event)) => {
                    driver.consume(&mut model, generation, event);
                }
                None => {
                    stream_open = false;
                    model.handle(AppEvent::StreamEnded);
                }
            },
            // Guarded tick: while the model is clean this branch is disabled,
            // so the idle loop takes NO periodic wakeups (efficiency rider
            // #10 — ~109k/hour otherwise). The first dirtying event re-arms
            // it and the overdue tick fires immediately, keeping the 30fps
            // coalescing behavior.
            _ = frame_tick.tick(), if model.dirty => {
                hit_map = draw(&mut terminal, &model)?;
                model.dirty = false;
                // The hover target may have vanished with the new frame
                // (screen switch, shed region): drop it WITHOUT dirtying —
                // the frame just painted reality (TUI3a item 6).
                if model
                    .hovered
                    .as_ref()
                    .is_some_and(|hovered| !hit_map.iter().any(|(_, hit)| hit == hovered))
                {
                    model.hovered = None;
                }
            }
        }
        // Reducer-requested side effects.
        let requests: Vec<AppRequest> = model.requests.drain(..).collect();
        for request in requests {
            driver.handle_request(&mut model, request);
        }
        // Theme cycled: re-sync the emulator background.
        if model.theme != active_theme {
            active_theme = model.theme;
            sync_terminal_bg(active_theme);
        }
        // Screen/session changed: re-sync the window title (owner ask).
        let title = model.window_title();
        if title != active_title {
            active_title = title;
            sync_window_title(&active_title);
        }
        // Reliable outbox drain (review r2 P2): a Full channel keeps the
        // answer queued for the next loop turn — a full channel guarantees
        // pending envelopes, so the loop WILL wake and retry. Awaiting the
        // send here instead would deadlock: this loop is the only consumer.
        while let Some(answer) = model.outbox.first().cloned() {
            let generation = driver.generation();
            match answer_echo.try_send((
                generation,
                DemoEvent::Envelope(EventPayload::MenuAnswered(answer)),
            )) {
                Ok(()) => {
                    model.outbox.remove(0);
                }
                Err(mpsc::error::TrySendError::Full(_)) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    model.outbox.clear();
                    break;
                }
            }
        }
    }
    Ok(())
}

/// The sim's idle(i) decay window (tui.js:1562: 30s of nothing).
pub const IDLE_DECAY: Duration = Duration::from_secs(30);

/// One terminal input event through the production dispatch — key/paste
/// into the reducer, resize into [`AppModel::handle_resize`], mouse through
/// the last frame's hit map. Extracted from the event loop so tests drive
/// the SAME wiring (review r3 P3-7).
pub fn dispatch_input(
    model: &mut AppModel,
    hit_map: &[(ratatui::layout::Rect, crate::app::Hit)],
    event: Event,
) {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            model.handle(AppEvent::Key(key));
        }
        Event::Paste(text) => model.handle(AppEvent::Paste(text)),
        Event::Resize(..) => model.handle_resize(),
        Event::Mouse(mouse) => {
            let hit_at = |column: u16, row: u16| {
                hit_map
                    .iter()
                    .find(|(rect, _)| {
                        column >= rect.x
                            && column < rect.x + rect.width
                            && row >= rect.y
                            && row < rect.y + rect.height
                    })
                    .map(|(_, action)| action.clone())
            };
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(action) = hit_at(mouse.column, mouse.row) {
                        model.handle_hit(action);
                    }
                }
                // Hover (owner ask, TUI3a item 6): motion events flood —
                // handle_hover only dirties when the target CHANGES.
                MouseEventKind::Moved => {
                    model.handle_hover(hit_at(mouse.column, mouse.row));
                }
                MouseEventKind::ScrollUp => model.handle_wheel(true),
                MouseEventKind::ScrollDown => model.handle_wheel(false),
                _ => {}
            }
        }
        _ => {}
    }
}

/// The demo's script engine — the production seams of [`run_demo`]'s event
/// loop, extracted whole so tests drive the SAME wiring (review r3 P3-7):
/// the same generation-tagged channel, the same spawn/bump/decay behavior,
/// the same consumption guard. TUI3b: plays [`Beat`] scripts (the sim's
/// respond() port), owns the token meter (cumulative `Usage` frames), the
/// GENERIC/roster rotation counters, parked `AwaitMenu` arms, and the
/// turn-end law (`finish_turn`).
/// Generation number spaces: three guard domains share one tagged channel
/// — session turns (bumped by interrupt + fresh session), chip scripts
/// (bumped by fresh session ONLY — the sim's children survive a parent
/// interrupt), and aura runs (bumped by /reset only). Disjoint bases keep
/// the spaces collision-free, so consumption accepts a tag iff it equals
/// its own domain's live value.
pub const CHIP_GEN_BASE: u64 = 1 << 40;
pub const AURA_GEN_BASE: u64 = 1 << 41;

type Guard = std::sync::Arc<std::sync::atomic::AtomicU64>;
#[allow(clippy::type_complexity)]
type PendingArms =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (u64, Vec<Vec<Beat>>)>>>;

pub struct DemoDriver {
    tx: mpsc::Sender<(u64, DemoEvent)>,
    script_gen: Guard,
    /// The chip guard (chip scripts + removal/resume timers).
    chip_gen: Guard,
    /// The aura guard (orchestrate runs + the talk timer).
    aura_gen: Guard,
    turn_counter: u64,
    /// Sim `genRef` (tui.js:1488): generic-branch post-increment rotation.
    generic_counter: u64,
    /// Sim `rosterRef` (tui.js:681): starts at 3 (seed heads claim 0-2).
    roster_counter: u64,
    /// Demo token meter, input bucket (user text 9/char + tools 2400).
    tokens_input: Guard,
    /// Demo token meter, output bucket (streamed words 9/char).
    tokens_output: Guard,
    /// Menus a script parked on: menu id → (generation, continuation arms).
    /// `MenuAnswered` at consume selects `arms[option_index]` (clamped) and
    /// plays it under the SAME generation; a bump while parked cancels the
    /// continuation (sim `alive()` guard killing a turn parked on askMenu).
    pending_arms: PendingArms,
}

impl DemoDriver {
    /// A driver plus the receiving end of its demo-event channel.
    #[must_use]
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<(u64, DemoEvent)>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                script_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                chip_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(CHIP_GEN_BASE)),
                aura_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(AURA_GEN_BASE)),
                turn_counter: 0,
                generic_counter: 0,
                roster_counter: crate::script::ROSTER_FIRST_CLAIM,
                tokens_input: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                tokens_output: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                pending_arms: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
            },
            rx,
        )
    }

    /// The current script generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.script_gen.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The current chip-domain generation.
    #[must_use]
    pub fn chip_generation(&self) -> u64 {
        self.chip_gen.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The current aura-domain generation.
    #[must_use]
    pub fn aura_generation(&self) -> u64 {
        self.aura_gen.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// True iff `generation` is the LIVE value of its own domain.
    fn is_live(&self, generation: u64) -> bool {
        generation == self.generation()
            || generation == self.chip_generation()
            || generation == self.aura_generation()
    }

    /// The guard a generation value belongs to (disjoint number spaces).
    fn guard_for(&self, generation: u64) -> Guard {
        if generation >= AURA_GEN_BASE {
            std::sync::Arc::clone(&self.aura_gen)
        } else if generation >= CHIP_GEN_BASE {
            std::sync::Arc::clone(&self.chip_gen)
        } else {
            std::sync::Arc::clone(&self.script_gen)
        }
    }

    fn player(&self) -> Player {
        Player {
            tx: self.tx.clone(),
            input: std::sync::Arc::clone(&self.tokens_input),
            output: std::sync::Arc::clone(&self.tokens_output),
            arms: std::sync::Arc::clone(&self.pending_arms),
            chip_gen: std::sync::Arc::clone(&self.chip_gen),
        }
    }

    /// The demo token meter's current total (both buckets).
    #[must_use]
    pub fn tokens_total(&self) -> u64 {
        self.tokens_input
            .load(std::sync::atomic::Ordering::SeqCst)
            .saturating_add(self.tokens_output.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// A clone of the tagged-event sender (the menu-answer echo seam).
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<(u64, DemoEvent)> {
        self.tx.clone()
    }

    /// Spawn the boot beats. They tag with the CURRENT generation at each
    /// send: they belong to the harness, not any turn, and survive bumps.
    pub fn spawn_boot(&self) {
        let tx = self.tx.clone();
        let gen_ref = std::sync::Arc::clone(&self.script_gen);
        tokio::spawn(async move {
            for payload in crate::mock::boot_script() {
                tokio::time::sleep(demo_pace(&payload)).await;
                let generation = gen_ref.load(std::sync::atomic::Ordering::SeqCst);
                if tx
                    .send((generation, DemoEvent::Envelope(payload)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Play one beat script under the SESSION guard, generation-captured:
    /// bumps at EITHER end of the channel silence it (send-side check +
    /// consumption guard).
    pub fn play_beats(&self, beats: Vec<Beat>, generation: u64) {
        self.player()
            .spawn(beats, std::sync::Arc::clone(&self.script_gen), generation);
    }

    /// Play a script under an explicit guard domain (chip/aura scripts).
    fn play_guarded(&self, beats: Vec<Beat>, guard: Guard, generation: u64) {
        self.player().spawn(beats, guard, generation);
    }

    /// Drain one reducer request — scripts play generation-captured,
    /// stop/interrupt bump the generation (so buffered envelopes AND
    /// pending timers of the old context drop at consumption — review r2
    /// P1-1, r3 P3-6), and an interrupt schedules the sim's 30s idle(i)
    /// decay (tui.js:1561-1564).
    pub fn handle_request(&mut self, model: &mut AppModel, request: AppRequest) {
        match request {
            AppRequest::SubmitText { text, voice, title } => {
                self.turn_counter += 1;
                let generation = self.generation();
                let beats = respond_beats(
                    &text,
                    voice,
                    haider_protocol::DeliveryMode::Steer,
                    self.turn_counter,
                    &mut self.generic_counter,
                    &mut self.roster_counter,
                );
                self.play_beats(beats, generation);
                // Auto-title (sim tui.js:1221-1227): scheduled at turn
                // start, the note lands 1.5 s later wherever the transcript
                // is; an interrupt inside the window drops it (gen guard).
                if let Some(blurb) = title {
                    self.play_beats(
                        vec![Beat::Sleep(AUTO_TITLE_MS), Beat::Note(title_note(&blurb))],
                        generation,
                    );
                }
            }
            AppRequest::AttachSample(_) => {
                self.turn_counter += 1;
                self.play_beats(
                    from_legacy(crate::mock::turn_script(self.turn_counter)),
                    self.generation(),
                );
            }
            AppRequest::StopScripts => {
                self.script_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // A fresh session kills its chips' scripts too (the chip
                // domain survives interrupts, not session teardown).
                self.chip_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // A fresh session starts from a zero meter and no parked
                // continuations (stale arms are also gen-guarded; clearing
                // is hygiene, not correctness).
                self.tokens_input
                    .store(0, std::sync::atomic::Ordering::SeqCst);
                self.tokens_output
                    .store(0, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut pending) = self.pending_arms.lock() {
                    pending.clear();
                }
            }
            AppRequest::Interrupt => {
                let decay_gen = self
                    .script_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                if let Ok(mut pending) = self.pending_arms.lock() {
                    pending.clear();
                }
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(IDLE_DECAY).await;
                    let _ = tx
                        .send((decay_gen, DemoEvent::Envelope(EventPayload::IdleDecayed)))
                        .await;
                });
            }
            AppRequest::Compact => {
                // Manual /compact (sim tui.js:1791-1806): before = current
                // meter, after = 6% of the window — 1200 ms, then IDLE.
                let before = self.tokens_total();
                let after = model.identity.context_window * 6 / 100;
                self.play_beats(compaction_beats(before, after, true), self.generation());
            }
            AppRequest::Talk => {
                // ◉ talk hold (sim tui.js:2044-2054): 1300 ms of
                // `◉ listening…`, then the canned phrase fires through the
                // voice path. Generation-captured like every timer.
                let generation = self.generation();
                let tx = self.tx.clone();
                let gen_ref = std::sync::Arc::clone(&self.script_gen);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(TALK_HOLD_MS)).await;
                    if gen_ref.load(std::sync::atomic::Ordering::SeqCst) != generation {
                        return;
                    }
                    let _ = tx.send((generation, DemoEvent::TalkFire)).await;
                });
            }
            AppRequest::ChipSubmit { agent, text } => {
                // respondChip (§2.4): a full turn on the CHIP's state
                // machine, played under the CHIP guard (children survive a
                // parent interrupt). An unresolved question steers-queues —
                // the sim never actually delivers it (ported as-is).
                // Sim respondChip's entry guard (tui.js:1099): a CLOSED chip
                // never runs another turn.
                let Some(chip) =
                    crate::app::find_chip(&model.chips, &agent).filter(|chip| !chip.closed)
                else {
                    return;
                };
                // Sim gate (tui.js:1105): `input_required` ALONE queues the
                // steer — the `error`/recovery card does not (a message
                // there starts a fresh chip turn). The state and its
                // question land atomically, so reading the state suffices.
                let blocked = chip.state == crate::script::ChipDisplayState::InputRequired;
                let beats = if blocked {
                    vec![
                        Beat::ChipEmit {
                            agent: agent.clone(),
                            payload: EventPayload::UserMessage {
                                text,
                                attachments: vec![],
                                mode: haider_protocol::DeliveryMode::Steer,
                            },
                        },
                        Beat::ChipNote {
                            agent,
                            text: "· steer queued — delivered when the pending question resolves"
                                .to_owned(),
                        },
                    ]
                } else {
                    self.turn_counter += 1;
                    crate::script::respond_chip_beats(
                        &agent,
                        &chip.callsign,
                        &chip.model,
                        &chip.device,
                        &text,
                        self.turn_counter,
                        &mut self.roster_counter,
                    )
                };
                self.play_guarded(
                    beats,
                    std::sync::Arc::clone(&self.chip_gen),
                    self.chip_generation(),
                );
            }
            AppRequest::ChipClose { agent } => self.close_chip(model, &agent),
            AppRequest::AuraSubmit { text, voice: _ } => {
                // Sim: spoken = !muted at turn start; `voice` only shaped
                // the user row's ◉ sigil (already pushed by the reducer).
                let low = text.to_lowercase();
                let spoken = !model.aura.muted;
                let runs = model.aura.runs;
                let beats = if crate::script::aura_is_status(&low) {
                    let summary = if model.aura.roster.is_empty() {
                        "nothing running yet".to_owned()
                    } else {
                        model
                            .aura
                            .roster
                            .iter()
                            .map(|row| format!("{} on {} — {}", row.name, row.device, row.activity))
                            .collect::<Vec<_>>()
                            .join("; ")
                    };
                    crate::script::aura_status_beats(spoken, &summary, runs)
                } else {
                    let (name, device) = crate::script::aura_target(&low);
                    crate::script::aura_spawn_beats(spoken, &name, &device, runs)
                };
                self.play_guarded(
                    beats,
                    std::sync::Arc::clone(&self.aura_gen),
                    self.aura_generation(),
                );
            }
            AppRequest::AuraTalk => {
                // auraTalk (tui.js:2128-2132): 1100 ms listening, then the
                // canned phrase orchestrates as a voice run.
                let generation = self.aura_generation();
                let tx = self.tx.clone();
                let gen_ref = std::sync::Arc::clone(&self.aura_gen);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(crate::script::AURA_TALK_MS)).await;
                    if gen_ref.load(std::sync::atomic::Ordering::SeqCst) != generation {
                        return;
                    }
                    let _ = tx.send((generation, DemoEvent::AuraTalkFire)).await;
                });
            }
            AppRequest::ResetAura => {
                self.aura_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            AppRequest::Quit => model.should_quit = true,
        }
    }

    /// The chip close lifecycle (§2.5): reducer flags + parent note, then
    /// the driver's 5 s removal timer; closing the last LIVE child arms
    /// the auto-resume check (closing a done chip discharges nothing).
    fn close_chip(&mut self, model: &mut AppModel, agent: &str) {
        let Some(was_live) = model.close_chip_state(agent) else {
            return;
        };
        let generation = self.chip_generation();
        let tx = self.tx.clone();
        let gen_ref = std::sync::Arc::clone(&self.chip_gen);
        let removed = agent.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(crate::script::CHIP_REMOVE_MS)).await;
            if gen_ref.load(std::sync::atomic::Ordering::SeqCst) != generation {
                return;
            }
            let _ = tx
                .send((generation, DemoEvent::ChipRemove { agent: removed }))
                .await;
        });
        if was_live {
            self.arm_auto_resume();
        }
    }

    /// Spawn the 120 ms autoResumeParent defer (§2.7) under the chip guard.
    fn arm_auto_resume(&self) {
        let generation = self.chip_generation();
        let tx = self.tx.clone();
        let gen_ref = std::sync::Arc::clone(&self.chip_gen);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(crate::script::AUTO_RESUME_DEFER_MS)).await;
            if gen_ref.load(std::sync::atomic::Ordering::SeqCst) != generation {
                return;
            }
            let _ = tx.send((generation, DemoEvent::AutoResume)).await;
        });
    }

    /// Consume one generation-tagged demo event. Stale generations are
    /// dropped whole — the P1-1 law: a bump invalidates events already
    /// buffered in the channel, not only future sends. A tag is live iff
    /// it equals its OWN domain's current value (disjoint number spaces).
    pub fn consume(&mut self, model: &mut AppModel, generation: u64, event: DemoEvent) {
        if !self.is_live(generation) {
            return;
        }
        match event {
            DemoEvent::Envelope(payload) => {
                // A parked script's menu was answered: the option index
                // selects the continuation arm (clamped to the last arm),
                // played under the SAME (still-live) generation.
                if let EventPayload::MenuAnswered(answer) = &payload {
                    let parked = self
                        .pending_arms
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.remove(answer.menu.as_str()));
                    if let Some((arm_gen, mut arms)) = parked
                        && self.is_live(arm_gen)
                        && !arms.is_empty()
                    {
                        let index = (answer.option_index as usize).min(arms.len() - 1);
                        let beats = arms.swap_remove(index);
                        let guard = self.guard_for(arm_gen);
                        self.play_guarded(beats, guard, arm_gen);
                    }
                }
                consume_scripted(model, generation, generation, payload);
            }
            DemoEvent::Note(text) => {
                model.projection.push_note(text);
                model.dirty = true;
            }
            DemoEvent::Voice(on) => {
                model.projection.set_voice_live(on);
                model.dirty = true;
            }
            DemoEvent::TurnEnd => self.finish_turn(model),
            DemoEvent::TalkFire => model.talk_fire(),
            // ---- Chip events (§2) ----
            DemoEvent::ChipAdd(seed) => {
                let parent = seed.parent.clone();
                let chip = crate::app::ChipModel::from_seed(*seed);
                match parent.and_then(|agent| crate::app::find_chip_mut(&mut model.chips, &agent)) {
                    Some(parent_chip) => parent_chip.children.push(chip),
                    None => model.chips.push(chip),
                }
                model.dirty = true;
            }
            DemoEvent::ChipState { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.state = state;
                    model.dirty = true;
                }
            }
            DemoEvent::ChipEmit { agent, payload } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.transcript.apply(&payload);
                    model.dirty = true;
                }
            }
            DemoEvent::ChipNote { agent, text } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.transcript.push_note(text);
                    model.dirty = true;
                }
            }
            DemoEvent::ChipTokens { agent, n } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.tokens = chip.tokens.saturating_add(n);
                    model.dirty = true;
                }
            }
            DemoEvent::ChipQuestion {
                agent,
                recovery,
                text,
                options,
            } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    // ATOMIC with the state (sim writes one patch): the pair
                    // `state == input_required && question` is what
                    // respondChip's steer-queue gate reads, so it must never
                    // be observable half-applied.
                    chip.state = if recovery {
                        crate::script::ChipDisplayState::Error
                    } else {
                        crate::script::ChipDisplayState::InputRequired
                    };
                    chip.question = Some(crate::app::ChipQuestion {
                        recovery,
                        text,
                        options,
                        resolved: false,
                    });
                    model.dirty = true;
                }
            }
            DemoEvent::ChipResolve { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    if let Some(question) = &mut chip.question {
                        question.resolved = true;
                    }
                    chip.state = state;
                    model.dirty = true;
                }
            }
            DemoEvent::ChipQuestionClear { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.question = None;
                    chip.state = state;
                    model.dirty = true;
                }
            }
            DemoEvent::ChipCloseReq { agent } => self.close_chip(model, &agent),
            DemoEvent::ChipRemove { agent } => {
                if crate::app::remove_chip(&mut model.chips, &agent) {
                    if model.view_path.contains(&agent) {
                        model.view_path.clear();
                        if model.screen == crate::app::Screen::Subagent {
                            model.screen = crate::app::Screen::Session;
                        }
                    }
                    model.dirty = true;
                }
            }
            DemoEvent::AutoResume => {
                // §2.7 guards: every child settled, the session idle (an
                // interrupted session is NOT overwritten — its badge is
                // `⏸ IDLE (i)`, not `IDLE`), no resume already in flight.
                let idle = model.projection.badge() == "IDLE";
                if crate::app::tree_live_count(&model.chips) == 0
                    && !model.turn_active
                    && !model.auto_resuming
                    && idle
                {
                    let reports = count_done_reports(&model.chips);
                    model.auto_resuming = true;
                    model.turn_active = true;
                    model.dirty = true;
                    self.turn_counter += 1;
                    let beats = crate::script::auto_resume_beats(reports, self.turn_counter);
                    let generation = self.generation();
                    self.play_beats(beats, generation);
                }
            }
            // ---- Aura events (§3) ----
            DemoEvent::AuraState(state) => {
                model.aura.state = state;
                model.dirty = true;
            }
            DemoEvent::AuraEmit(payload) => {
                model.aura.transcript.apply(&payload);
                model.dirty = true;
            }
            DemoEvent::AuraNote(text) => {
                model.aura.transcript.push_note(text);
                model.dirty = true;
            }
            DemoEvent::AuraVoice(on) => {
                model.aura.transcript.set_voice_live(on);
                model.dirty = true;
            }
            DemoEvent::AuraRosterPush { name, device } => {
                model.aura.roster.push(crate::app::AuraAgentRow {
                    name,
                    device,
                    state: crate::script::ChipDisplayState::Running,
                    activity: "starting…".to_owned(),
                });
                model.dirty = true;
            }
            DemoEvent::AuraRosterPatch {
                name,
                state,
                activity,
            } => {
                if let Some(row) = model
                    .aura
                    .roster
                    .iter_mut()
                    .rev()
                    .find(|row| row.name == name)
                {
                    if let Some(state) = state {
                        row.state = state;
                    }
                    row.activity = activity;
                    model.dirty = true;
                }
            }
            DemoEvent::AuraLog(text) => {
                model.aura.log.push(text);
                model.dirty = true;
            }
            DemoEvent::AuraTalkFire => model.aura_talk_fire(),
        }
    }

    /// The sim's `finishTurn` + auto-compaction law (tui.js:1507-1543):
    /// queued input consumes directly — the session never passes through
    /// idle; else the 85% auto-compaction check (checked BEFORE emitting
    /// Done — the sim's 30 ms transient-IDLE flicker is a known sim wart
    /// the spec says not to port); else IDLE.
    pub fn finish_turn(&mut self, model: &mut AppModel) {
        let generation = self.generation();
        if !model.msg_queue.is_empty() {
            let text = model.msg_queue.remove(0);
            model.dirty = true;
            self.turn_counter += 1;
            let mut beats = vec![Beat::Note(
                "· turn ended with queued input — consuming it directly, no idle".to_owned(),
            )];
            beats.extend(respond_beats(
                &text,
                false,
                haider_protocol::DeliveryMode::Queue,
                self.turn_counter,
                &mut self.generic_counter,
                &mut self.roster_counter,
            ));
            self.play_beats(beats, generation);
            return;
        }
        let window = model.identity.context_window;
        let total = self.tokens_total();
        if window > 0 && total.saturating_mul(100) >= window.saturating_mul(85) {
            self.play_beats(compaction_beats(total, window * 6 / 100, false), generation);
            return;
        }
        self.play_beats(
            vec![Beat::Emit(EventPayload::RunState(RunState::Done))],
            generation,
        );
    }
}

/// Non-closed chips holding a `done` report, recursively (§2.7 step 1).
fn count_done_reports(chips: &[crate::app::ChipModel]) -> usize {
    chips
        .iter()
        .map(|chip| {
            usize::from(!chip.closed && chip.state == crate::script::ChipDisplayState::Done)
                + count_done_reports(&chip.children)
        })
        .sum()
}

/// The beat player: one spawned task per script, guard-captured. Cloned
/// into child tasks so `ChipScript` beats can spawn concurrently (the
/// future is boxed at each spawn to break the recursive type).
#[derive(Clone)]
struct Player {
    tx: mpsc::Sender<(u64, DemoEvent)>,
    input: Guard,
    output: Guard,
    arms: PendingArms,
    chip_gen: Guard,
}

impl Player {
    fn spawn(&self, beats: Vec<Beat>, guard: Guard, generation: u64) {
        let player = self.clone();
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(async move {
                player.run(beats, guard, generation).await;
            });
        tokio::spawn(future);
    }

    #[allow(clippy::too_many_lines)]
    async fn run(&self, beats: Vec<Beat>, guard: Guard, generation: u64) {
        use std::sync::atomic::Ordering::SeqCst;
        let usage_event = |player: &Self| {
            DemoEvent::Envelope(EventPayload::Usage(Usage {
                input: player.input.load(SeqCst),
                output: player.output.load(SeqCst),
                reasoning: 0,
                cached: 0,
                source: UsageSource::Estimated,
                account: None,
            }))
        };
        for beat in beats {
            if guard.load(SeqCst) != generation {
                return;
            }
            let event = match beat {
                Beat::Sleep(ms) => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                Beat::Emit(payload) => DemoEvent::Envelope(payload),
                Beat::Note(text) => DemoEvent::Note(text),
                Beat::Voice(on) => DemoEvent::Voice(on),
                Beat::Tokens { n, output } => {
                    if output {
                        self.output.fetch_add(n, SeqCst);
                    } else {
                        self.input.fetch_add(n, SeqCst);
                    }
                    usage_event(self)
                }
                Beat::TokensReset(n) => {
                    self.input.store(n, SeqCst);
                    self.output.store(0, SeqCst);
                    usage_event(self)
                }
                Beat::AwaitMenu { menu, arms } => {
                    if let Ok(mut pending) = self.arms.lock() {
                        pending.insert(menu.as_str().to_owned(), (generation, arms));
                    }
                    return;
                }
                Beat::TurnEnd => DemoEvent::TurnEnd,
                // Chip/aura beats map 1:1 onto their DemoEvents, tagged
                // with THIS script's generation (a chip script tags chip-
                // domain; its parent-transcript beats ride the same tag
                // and stay live across a session interrupt).
                Beat::ChipAdd(seed) => DemoEvent::ChipAdd(seed),
                Beat::ChipState { agent, state } => DemoEvent::ChipState { agent, state },
                Beat::ChipEmit { agent, payload } => DemoEvent::ChipEmit { agent, payload },
                Beat::ChipNote { agent, text } => DemoEvent::ChipNote { agent, text },
                Beat::ChipTokens { agent, n } => DemoEvent::ChipTokens { agent, n },
                Beat::ChipQuestion {
                    agent,
                    recovery,
                    text,
                    options,
                } => DemoEvent::ChipQuestion {
                    agent,
                    recovery,
                    text,
                    options,
                },
                Beat::ChipResolve { agent, state } => DemoEvent::ChipResolve { agent, state },
                Beat::ChipQuestionClear { agent, state } => {
                    DemoEvent::ChipQuestionClear { agent, state }
                }
                Beat::ChipClose { agent } => DemoEvent::ChipCloseReq { agent },
                Beat::ChipScript(child) => {
                    // Concurrent child script under the CHIP guard.
                    let chip_now = self.chip_gen.load(SeqCst);
                    self.spawn(child, std::sync::Arc::clone(&self.chip_gen), chip_now);
                    continue;
                }
                Beat::AutoResume => {
                    // The 120 ms defer runs under the CHIP guard; the §2.7
                    // state guards are checked at consumption.
                    let chip_now = self.chip_gen.load(SeqCst);
                    let tx = self.tx.clone();
                    let gen_ref = std::sync::Arc::clone(&self.chip_gen);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(
                            crate::script::AUTO_RESUME_DEFER_MS,
                        ))
                        .await;
                        if gen_ref.load(SeqCst) != chip_now {
                            return;
                        }
                        let _ = tx.send((chip_now, DemoEvent::AutoResume)).await;
                    });
                    continue;
                }
                Beat::AuraState(state) => DemoEvent::AuraState(state),
                Beat::AuraEmit(payload) => DemoEvent::AuraEmit(payload),
                Beat::AuraNote(text) => DemoEvent::AuraNote(text),
                Beat::AuraVoice(on) => DemoEvent::AuraVoice(on),
                Beat::AuraRosterPush { name, device } => DemoEvent::AuraRosterPush { name, device },
                Beat::AuraRosterPatch {
                    name,
                    state,
                    activity,
                } => DemoEvent::AuraRosterPatch {
                    name,
                    state,
                    activity,
                },
                Beat::AuraLog(text) => DemoEvent::AuraLog(text),
            };
            if guard.load(SeqCst) != generation {
                return;
            }
            if self.tx.send((generation, event)).await.is_err() {
                return;
            }
        }
    }
}

/// Consume one generation-tagged script envelope against `current`. Kept
/// public as the driver's inner law (tests may exercise it directly, but
/// the production path is [`DemoDriver::consume`]).
pub fn consume_scripted(
    model: &mut AppModel,
    generation: u64,
    current: u64,
    payload: EventPayload,
) {
    if generation == current {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    model: &AppModel,
) -> std::io::Result<Vec<(ratatui::layout::Rect, crate::app::Hit)>> {
    let mut hits = Vec::new();
    terminal.draw(|frame| hits = render(model, frame))?;
    Ok(hits)
}

/// Run the demo headlessly: play the whole script through the model and
/// return the final plain rendering (the `--plain` path and the CI oracle).
#[must_use]
pub fn run_demo_plain(mut model: AppModel) -> String {
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    crate::plain::render_plain(&model.projection, model.identity.context_window)
}
