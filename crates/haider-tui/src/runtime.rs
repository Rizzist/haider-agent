//! The interactive runtime: one task owns the terminal and the [`AppModel`];
//! input, envelopes and frame deadlines multiplex through `tokio::select!`
//! (research rec 3/6). Alternate screen for v0.1 (rec 1); native-scrollback
//! insertion is explicitly deferred (rec 19).

use crate::app::{AppEvent, AppModel, AppRequest};
use crate::mock::demo_script;
use crate::render::render;
use crate::script::{
    AUTO_TITLE_MS, Beat, COMPACT_IDLE_GAP_MS, DemoEvent, TALK_HOLD_MS, compaction_beats,
    respond_preamble, title_note,
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
/// Returns when the user quits (⌃C from the launcher or boot — elsewhere
/// ⌃C is navigation back to the launcher, owner item 10) or input closes.
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
    // NB: no launcher auto-play. The sim has none — an untouched launcher
    // simply waits (owner item 1: opening/idling must not start a sequence).

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
            match request {
                // Runtime-owned: copying reads a RENDERED frame. NB — the
                // live terminal's `current_buffer_mut()` is the SWAPPED,
                // reset next-frame buffer after a draw, so the text is
                // re-rendered into a scratch buffer at the live size
                // through the same pure `render` the screen and the tests
                // use; the selection RANGE and model are current.
                AppRequest::CopySelection => {
                    if let Some(selection) = model.selection {
                        let size = terminal.size()?;
                        let text = rendered_selection_text(&model, size, &selection);
                        copy_selection_effects(&mut model, &text);
                    }
                }
                request => driver.handle_request(&mut model, request),
            }
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
        while let Some(pending) = model.outbox.first().cloned() {
            match answer_echo.try_send((
                driver.control_tag(),
                DemoEvent::Answer {
                    origin: pending.origin,
                    answer: pending.answer,
                },
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
                // Owner item 9: Down is only a POTENTIAL anchor — the click
                // dispatches on Up, because only Up knows whether the press
                // was a click or a drag-selection. A previous selection's
                // highlight clears here (the clearing law's click half).
                MouseEventKind::Down(MouseButton::Left) => {
                    if model.selection.take().is_some() {
                        model.dirty = true;
                    }
                    model.mouse_down = Some((mouse.column, mouse.row));
                }
                // Movement with the button held: meaningful movement (a
                // different cell than the anchor) enters selection mode
                // with a live linear highlight; same-cell jitter is not a
                // drag. Once selecting, every head change redraws.
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(anchor) = model.mouse_down {
                        let head = (mouse.column, mouse.row);
                        match &mut model.selection {
                            Some(selection) if selection.head != head => {
                                selection.head = head;
                                model.dirty = true;
                            }
                            None if head != anchor => {
                                model.selection = Some(crate::select::Selection {
                                    anchor,
                                    head,
                                    dragging: true,
                                });
                                model.dirty = true;
                            }
                            _ => {}
                        }
                    }
                }
                // Up resolves the press: a selection auto-copies (the
                // runtime extracts from its last frame on the request) and
                // SUPPRESSES the click-hit; a plain click dispatches from
                // the Down coordinates exactly as before.
                MouseEventKind::Up(MouseButton::Left) => {
                    let down = model.mouse_down.take();
                    if let Some(selection) = &mut model.selection {
                        selection.dragging = false;
                        model.requests.push(crate::app::AppRequest::CopySelection);
                        model.dirty = true;
                    } else if let Some((column, row)) = down
                        && let Some(action) = hit_at(column, row)
                    {
                        model.handle_hit(action);
                    }
                }
                // Hover (owner ask, TUI3a item 6): motion events flood —
                // handle_hover only dirties when the target CHANGES. While
                // a button is held the terminal reports Drag, never Moved,
                // so hover is naturally unchanged during a selection.
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
/// TUI3.1 (review P1-1/2/3) replaced three per-domain generation counters
/// with a live-ARM REGISTRY. Every spawned script and timer holds an id
/// allocated ONCE — atomically, against a still-live parent — and
/// consumption drops any event whose id is no longer registered.
/// Cancellation is OWNER-scoped, so closing one chip stops exactly that
/// chip's beats while its siblings keep running, and no arm can adopt a
/// newer generation at fire time (the teardown race the old `ChipScript`
/// beat had).
///
/// The always-live control tag: model→driver echoes (menu answers) and the
/// boot script ride it. It is never cancelled — a user's answer is not
/// stale work, and boot belongs to the harness, not to any turn.
pub const CONTROL_ARM: u64 = 0;

/// Who owns a spawned arm — the surface whose teardown cancels it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ArmOwner {
    /// One session's turn engine, keyed by SESSION ID (TUI4c: the sim's
    /// per-session `runTokensRef`, tui.js:1551-1567 — an interrupt cancels
    /// only ITS session's turn; other sessions' turns keep running in the
    /// background). Id 0 is the surface/scratch lineage (boot, pre-map
    /// tests) and routes to the model's live fields.
    Session(u64),
    /// One subagent chip's script. Its own close/removal cancels it, and a
    /// fresh session cancels every chip — but a session INTERRUPT does
    /// NOT: the sim's `interrupt` touches only the run token, the queue and
    /// the note (tui.js:1551-1567), so children outlive their parent's
    /// cancelled turn. Because the chip's PARKED arms survive with them,
    /// answering such a child's card still resolves cleanly — that is what
    /// closes the review's "permanently blocked chip" hole. Carries the
    /// owning SESSION's id so background chip events route to their
    /// session's tree.
    Chip { session: u64, agent: String },
    /// An aura orchestrate run or its talk timer. The next submit cancels
    /// the previous run (sim `++auraRunRef`, tui.js:2060) and `/reset`
    /// cancels it outright; `/clear`, a session interrupt, and a fresh
    /// session deliberately do NOT cancel it (sim tui.js:1913/1950 — a
    /// background orchestration finishes; review r2 P2-5).
    Aura,
}

impl ArmOwner {
    /// The session this arm belongs to (`None` for aura arms).
    const fn session_id(&self) -> Option<u64> {
        match self {
            Self::Session(sid) | Self::Chip { session: sid, .. } => Some(*sid),
            Self::Aura => None,
        }
    }
}

type Counter = std::sync::Arc<std::sync::atomic::AtomicU64>;

/// Per-session demo token meters, keyed by session id (TUI4c item 13a:
/// the sim's `branch.tokens` is per-session; one global pair would bleed
/// one session's turn into another's meter).
type SessionMeters = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u64, (u64, u64)>>>;

/// The live-arm table. A poisoned lock degrades to "nothing is live" — it
/// can never resurrect cancelled work.
#[derive(Clone)]
struct ArmTable {
    inner: std::sync::Arc<std::sync::Mutex<(u64, std::collections::HashMap<u64, ArmOwner>)>>,
}

impl ArmTable {
    fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new((
                CONTROL_ARM,
                std::collections::HashMap::new(),
            ))),
        }
    }

    /// Register a new arm. Returns `CONTROL_ARM` only if the lock is
    /// poisoned, in which case the caller's work rides the never-cancelled
    /// tag rather than being silently dropped.
    ///
    /// Arms are deliberately NOT deregistered when their script ends: a
    /// finished arm is still the parent of any in-flight continuation (the
    /// `Dispatch` hand-off, a parked menu, an auto-resume timer), and
    /// events it already sent may still be buffered in the channel. The
    /// table only ever shrinks on an explicit teardown, and each entry is
    /// one u64 plus a small enum.
    fn alloc(&self, owner: ArmOwner) -> u64 {
        let Ok(mut table) = self.inner.lock() else {
            return CONTROL_ARM;
        };
        table.0 += 1;
        let id = table.0;
        table.1.insert(id, owner);
        id
    }

    /// Register a child arm ONLY while `parent` is still live — one lock
    /// covers the check and the insert, so a teardown between them is
    /// impossible (the `ChipScript` adoption race, review P1-1).
    fn alloc_child(&self, parent: u64, owner: ArmOwner) -> Option<u64> {
        let mut table = self.inner.lock().ok()?;
        if parent != CONTROL_ARM && !table.1.contains_key(&parent) {
            return None;
        }
        table.0 += 1;
        let id = table.0;
        table.1.insert(id, owner);
        Some(id)
    }

    fn is_live(&self, id: u64) -> bool {
        id == CONTROL_ARM
            || self
                .inner
                .lock()
                .is_ok_and(|table| table.1.contains_key(&id))
    }

    fn owner(&self, id: u64) -> Option<ArmOwner> {
        self.inner.lock().ok()?.1.get(&id).cloned()
    }

    /// Cancel every arm whose owner matches, and report which ids died so
    /// parked continuations can be dropped with them.
    fn cancel(&self, matches: &dyn Fn(&ArmOwner) -> bool) {
        if let Ok(mut table) = self.inner.lock() {
            table.1.retain(|_, owner| !matches(owner));
        }
    }
}

#[allow(clippy::type_complexity)]
type PendingArms = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, (u64, ArmOwner, Vec<Vec<Beat>>)>>,
>;

pub struct DemoDriver {
    tx: mpsc::Sender<(u64, DemoEvent)>,
    /// Every live script/timer arm (review P1-1/2/3).
    table: ArmTable,
    turn_counter: u64,
    /// Unique suffix for compaction item ids (review P2-13: `/compact`
    /// twice without token growth reused `compact-{before}` and the
    /// projection dropped the second row).
    compact_counter: u64,
    /// Sim `genRef` (tui.js:1488): generic-branch post-increment rotation.
    /// Read at DISPATCH time, not while building beats (review P2-11).
    generic_counter: Counter,
    /// Sim `rosterRef` (tui.js:681): starts at 3 (seed heads claim 0-2).
    roster_counter: Counter,
    /// Demo token meters, PER SESSION (input bucket: user text 9/unit +
    /// tools 2400; output bucket: streamed words 9/unit).
    meters: SessionMeters,
    /// Menus a script parked on: menu id → (arm, owner, continuation arms).
    /// `MenuAnswered` at consume selects `arms[option_index]` (clamped) and
    /// plays it under a FRESH arm owned by the same surface; a cancelled
    /// owner drops the continuation (sim `alive()` killing a turn parked on
    /// askMenu).
    pending_arms: PendingArms,
}

impl DemoDriver {
    /// A driver plus the receiving end of its demo-event channel.
    #[must_use]
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<(u64, DemoEvent)>) {
        let (tx, rx) = mpsc::channel(capacity);
        let counter = || std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        (
            Self {
                tx,
                table: ArmTable::new(),
                turn_counter: 0,
                compact_counter: 0,
                generic_counter: counter(),
                roster_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                    crate::script::ROSTER_FIRST_CLAIM,
                )),
                meters: std::sync::Arc::new(
                    std::sync::Mutex::new(std::collections::HashMap::new()),
                ),
                pending_arms: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
            },
            rx,
        )
    }

    /// The never-cancelled tag for model→driver echoes (menu answers).
    #[must_use]
    pub const fn control_tag(&self) -> u64 {
        CONTROL_ARM
    }

    /// True iff this arm is still registered (tests assert cancellation
    /// through this, not through counter arithmetic).
    #[must_use]
    pub fn is_arm_live(&self, arm: u64) -> bool {
        self.table.is_live(arm)
    }

    /// Cancel every arm of the matching owners, dropping their parked
    /// continuations with them.
    fn cancel_arms(&self, matches: &dyn Fn(&ArmOwner) -> bool) {
        self.table.cancel(matches);
        if let Ok(mut pending) = self.pending_arms.lock() {
            pending.retain(|_, (_, owner, _)| !matches(owner));
        }
    }

    fn player(&self) -> Player {
        Player {
            tx: self.tx.clone(),
            meters: std::sync::Arc::clone(&self.meters),
            parked: std::sync::Arc::clone(&self.pending_arms),
            table: self.table.clone(),
        }
    }

    /// One session's demo token meter total (both buckets).
    #[must_use]
    pub fn tokens_total(&self, session: u64) -> u64 {
        self.meters
            .lock()
            .ok()
            .and_then(|meters| meters.get(&session).copied())
            .map_or(0, |(input, output)| input.saturating_add(output))
    }

    /// A clone of the tagged-event sender (the menu-answer echo seam).
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<(u64, DemoEvent)> {
        self.tx.clone()
    }

    /// Spawn the boot beats on the control tag: they belong to the harness,
    /// not to any turn, so no teardown may silence them.
    pub fn spawn_boot(&self) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            for payload in crate::mock::boot_script() {
                tokio::time::sleep(demo_pace(&payload)).await;
                if tx
                    .send((CONTROL_ARM, DemoEvent::Envelope(payload)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Play a script on a FRESH arm owned by `owner`, returning its id.
    fn play_owned(&self, beats: Vec<Beat>, owner: ArmOwner) -> u64 {
        let arm = self.table.alloc(owner);
        self.player().spawn(beats, arm);
        arm
    }

    /// Play a session script (the turn engine's own arm) for `session`.
    pub fn play_beats(&self, beats: Vec<Beat>, session: u64) -> u64 {
        self.play_owned(beats, ArmOwner::Session(session))
    }

    /// A timer that must land no matter what happens to the session (the
    /// sim's bare `setTimeout`s). Delivery is unconditional; relevance is
    /// decided at consumption by the event's own origin identity.
    fn spawn_control_timer(&self, delay: Duration, event: DemoEvent) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send((CONTROL_ARM, event)).await;
        });
    }

    /// A parked script's menu was answered: the option index selects the
    /// continuation arm (clamped to the last), which plays on a FRESH arm
    /// owned by the same surface. `alloc_child` re-checks the parked arm
    /// under one lock, so a continuation can never outlive its owner.
    fn resume_parked(&self, answer: &haider_protocol::menu::MenuAnswer) {
        let parked = self
            .pending_arms
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(answer.menu.as_str()));
        if let Some((parked_arm, owner, mut arms)) = parked
            && !arms.is_empty()
            && let Some(arm) = self.table.alloc_child(parked_arm, owner)
        {
            let index = (answer.option_index as usize).min(arms.len() - 1);
            let beats = arms.swap_remove(index);
            self.player().spawn(beats, arm);
        }
    }

    /// Spawn a guarded timer: it fires `event` after `delay` only while
    /// `parent` is still live, on a fresh arm of its own so a later
    /// teardown drops the event even once it is buffered in the channel.
    fn spawn_timer(&self, parent: u64, owner: ArmOwner, delay: Duration, event: DemoEvent) {
        let Some(arm) = self.table.alloc_child(parent, owner) else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send((arm, event)).await;
        });
    }

    /// Drain one reducer request — scripts play under an ArmTable arm id
    /// captured at spawn; stop/interrupt CANCEL the owning arms (Session,
    /// and Chip only for StopScripts — see `ArmOwner`) so buffered
    /// envelopes AND pending timers of a cancelled arm drop at
    /// consumption, and an interrupt schedules the sim's 30s idle(i)
    /// decay (tui.js:1561-1564).
    pub fn handle_request(&mut self, model: &mut AppModel, request: AppRequest) {
        // Requests are pushed by the reducer while its session is attached
        // (or from the no-session scratch surface = 0).
        let active = model.active_session.unwrap_or(0);
        match request {
            AppRequest::SubmitText { text, voice, title } => {
                self.turn_counter += 1;
                // The branch is chosen — and the generic/roster counters
                // advance — at DISPATCH, after the 750 ms think window, so
                // an interrupt inside it skips no intro and burns no
                // callsign (review P2-11, sim tui.js:1259).
                self.play_beats(
                    respond_preamble(
                        &text,
                        voice,
                        haider_protocol::DeliveryMode::Steer,
                        self.turn_counter,
                    ),
                    active,
                );
                // Auto-title (sim tui.js:1219-1227): the micro-call names
                // the session INSIDE the 1.5 s callback — the title and the
                // note land together. It SURVIVES an interrupt (the sim's
                // timeout is bare) and is voided only by a session
                // replacement via the origin epoch (review r2 P2-6).
                if title {
                    // The sim's micro-call is a bare setTimeout: it lands
                    // even if the turn is interrupted (review r2 P2-6), so
                    // it rides the control tag and is gated on identity
                    // alone.
                    self.spawn_control_timer(
                        Duration::from_millis(AUTO_TITLE_MS),
                        DemoEvent::AutoTitle {
                            // TUI4c: origin is the SESSION ID — the
                            // callback looks the session up wherever it
                            // now lives (attached or background) and does
                            // nothing if it is gone, exactly the sim's
                            // by-id lookup (tui.js:1219-1227).
                            origin: active,
                            text,
                        },
                    );
                }
            }
            AppRequest::StopScripts => {
                // Session teardown (`/clear`, `/reset`, a fresh session):
                // the session's arms and every chip's die with it. AURA
                // DOES NOT — the sim's `/clear` leaves `auraRunRef` alone
                // (tui.js:1950-1955); only `/reset` and the next
                // orchestrate advance it, so a background orchestration
                // finishes where the sim finishes it (review r2 P2-5).
                self.cancel_arms(&|owner| {
                    matches!(owner, ArmOwner::Session(_) | ArmOwner::Chip { .. })
                });
                if let Ok(mut meters) = self.meters.lock() {
                    meters.clear();
                }
            }
            AppRequest::Interrupt => {
                // Esc mid-turn cancels THIS session's turn (sim
                // tui.js:1551-1567 touches only the run token, the queue and
                // the note). Chip arms — and their PARKED continuations —
                // deliberately survive, exactly as the sim's children do, so
                // a chip card answered after an interrupt still resolves
                // instead of blocking forever. AURA survives too: the sim's
                // interrupt never advances `auraRunRef`, so a background
                // orchestration finishes (review r2 P2-5 corrected r1 here).
                // TUI4c: only THIS session's turn dies — a background
                // session's running turn is untouchable from here (sim:
                // `runTokensRef.current[sid]` bumps one key).
                self.cancel_arms(
                    &|owner| matches!(owner, ArmOwner::Session(sid) if *sid == active),
                );
                self.spawn_timer(
                    CONTROL_ARM,
                    ArmOwner::Session(active),
                    IDLE_DECAY,
                    DemoEvent::Envelope(EventPayload::IdleDecayed),
                );
            }
            AppRequest::Compact => {
                // Manual /compact (sim tui.js:1791-1806): before = current
                // meter, after = 6% of the window — 1200 ms, then IDLE.
                let before = self.tokens_total(active);
                let after = model.identity.context_window * 6 / 100;
                self.compact_counter += 1;
                self.play_beats(
                    compaction_beats(before, after, true, self.compact_counter),
                    active,
                );
            }
            AppRequest::Talk => {
                // ◉ talk hold (sim tui.js:2044-2054): 1300 ms of
                // `◉ listening…`, then the canned phrase fires through the
                // voice path. Owned by the SESSION, so a fresh session kills
                // it; Esc additionally clears `listening`, and `talk_fire`
                // refuses a hold nobody is holding (review P1-3 — the timer
                // used to fire from the Launcher and yank the user into a
                // brand-new canned session).
                self.spawn_timer(
                    CONTROL_ARM,
                    ArmOwner::Session(active),
                    Duration::from_millis(TALK_HOLD_MS),
                    DemoEvent::TalkFire,
                );
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
                            agent: agent.clone(),
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
                        &self.roster_counter,
                    )
                };
                self.play_owned(
                    beats,
                    ArmOwner::Chip {
                        session: active,
                        agent,
                    },
                );
            }
            AppRequest::ChipClose { agent } => self.close_chip(model, active, &agent),
            AppRequest::AuraSubmit { text, voice: _ } => {
                // Run REPLACEMENT is the other cancel point (sim `++auraRunRef`
                // at the head of `orchestrate`, tui.js:2060).
                // Sim: spoken = !muted at turn start; `voice` only shaped
                // the user row's ◉ sigil (already pushed by the reducer).
                // Sim `orchestrate` opens with `++auraRunRef` (tui.js:2060):
                // a new run CANCELS the previous one — two rapid submits can
                // never interleave (review P1-2).
                self.cancel_arms(&|owner| matches!(owner, ArmOwner::Aura));
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
                self.play_owned(beats, ArmOwner::Aura);
            }
            AppRequest::AuraTalk => {
                // auraTalk (tui.js:2128-2132): 1100 ms listening, then the
                // canned phrase orchestrates as a voice run.
                self.spawn_timer(
                    CONTROL_ARM,
                    ArmOwner::Aura,
                    Duration::from_millis(crate::script::AURA_TALK_MS),
                    DemoEvent::AuraTalkFire,
                );
            }
            AppRequest::ResetAura => {
                // `/reset` is the ONE navigation that stops an aura run
                // (sim tui.js:1930 `auraRunRef.current++`).
                self.cancel_arms(&|owner| matches!(owner, ArmOwner::Aura));
                self.reset_aura_state(model);
            }
            AppRequest::Quit => model.should_quit = true,
            // Runtime-owned (the event loop intercepts it before the
            // driver): copying reads the rendered frame, which the driver
            // never has. Reaching here means a headless harness drained it
            // through the driver — a no-op, never a panic.
            AppRequest::CopySelection => {}
        }
    }

    /// A cancelled aura run leaves the orb wherever it stopped; the model
    /// must return to a state the user can act on (the `idle` submit gate)
    /// and stop tagging later rows as spoken (review P1-2 + P2-10).
    fn reset_aura_state(&self, model: &mut AppModel) {
        if model.aura.state != crate::script::AuraState::Idle {
            model.aura.state = crate::script::AuraState::Idle;
            model.dirty = true;
        }
        model.aura.transcript.set_voice_live(false);
    }

    /// The chip close lifecycle (§2.5): reducer flags + parent note, then
    /// the driver's 5 s removal timer; closing the last LIVE child arms
    /// the auto-resume check (closing a done chip discharges nothing).
    ///
    /// Closing KILLS that chip's arms first (review P1-1: a closed chip
    /// used to keep streaming into its own transcript), subtree included —
    /// a closed parent's children leave the tree with it. The removal timer
    /// is allocated AFTER the cancellation so it is not swept up by it.
    fn close_chip(&mut self, model: &mut AppModel, session: u64, agent: &str) {
        let attached = model.active_session == Some(session)
            || (session == 0 && model.active_session.is_none());
        let doomed_of = |chips: &[crate::app::ChipModel]| {
            crate::app::find_chip(chips, agent)
                .map(|chip| {
                    let mut ids = vec![chip.agent.clone()];
                    ids.extend(
                        crate::app::flatten_chips(&chip.children)
                            .into_iter()
                            .map(|(_, child)| child.agent.clone()),
                    );
                    ids
                })
                .unwrap_or_default()
        };
        let (doomed, was_live) = if attached {
            let doomed = doomed_of(&model.chips);
            let Some(was_live) = model.close_chip_state(agent) else {
                return;
            };
            (doomed, was_live)
        } else {
            // Background close: same law against the session's slot — the
            // attached surface's screen/view-path concerns do not apply.
            let (doomed, closed) = match model.session_entry_mut(session) {
                Some(entry) => {
                    let doomed = doomed_of(&entry.chips);
                    let closed =
                        crate::app::close_chip_core(&mut entry.chips, &mut entry.projection, agent);
                    (doomed, closed)
                }
                None => return,
            };
            let Some(was_live) = closed else {
                return;
            };
            model.dirty = true;
            (doomed, was_live)
        };
        self.cancel_arms(&|owner| match owner {
            ArmOwner::Chip { agent: id, .. } => doomed.iter().any(|dead| dead == id),
            _ => false,
        });
        self.spawn_timer(
            CONTROL_ARM,
            ArmOwner::Chip {
                session,
                agent: agent.to_owned(),
            },
            Duration::from_millis(crate::script::CHIP_REMOVE_MS),
            DemoEvent::ChipRemove {
                agent: agent.to_owned(),
            },
        );
        if was_live {
            self.arm_auto_resume(CONTROL_ARM, session);
        }
    }

    /// The §2.7 auto-resume guards + kick-off for `session` — attached or
    /// background, ONE law: every child settled, the session idle (an
    /// interrupted `⏸ IDLE (i)` is NOT overwritten), no resume in flight.
    fn auto_resume_check(&mut self, model: &mut AppModel, session: u64) {
        let attached = model.active_session == Some(session)
            || (session == 0 && model.active_session.is_none());
        let reports = if attached {
            if crate::app::tree_live_count(&model.chips) != 0
                || model.turn_active
                || model.auto_resuming
                || model.projection.badge() != "IDLE"
            {
                return;
            }
            model.auto_resuming = true;
            model.turn_active = true;
            model.dirty = true;
            count_done_reports(&model.chips)
        } else {
            let Some(entry) = model.session_entry_mut(session) else {
                return;
            };
            if entry.live() != 0
                || entry.turn_active
                || entry.auto_resuming
                || entry.projection.badge() != "IDLE"
            {
                return;
            }
            entry.auto_resuming = true;
            entry.turn_active = true;
            let reports = count_done_reports(&entry.chips);
            model.dirty = true;
            reports
        };
        self.turn_counter += 1;
        self.play_beats(
            crate::script::auto_resume_beats(reports, self.turn_counter),
            session,
        );
    }

    /// Background event application (TUI4c): the state-mutating events go
    /// through [`crate::session::SessionState::absorb`]; the driver-owned
    /// ones (dispatch, turn end, close lifecycle, auto-resume) run their
    /// usual logic against the owning session's slot.
    fn consume_background(
        &mut self,
        model: &mut AppModel,
        session: u64,
        generation: u64,
        event: DemoEvent,
    ) {
        match event {
            DemoEvent::Envelope(payload) => {
                // Script self-answers resume their parked continuations in
                // the background exactly as they would attached.
                if let EventPayload::MenuAnswered(answer) = &payload {
                    self.resume_parked(answer);
                }
                if let Some(entry) = model.session_entry_mut(session) {
                    entry.absorb(DemoEvent::Envelope(payload));
                    model.dirty = true;
                }
            }
            DemoEvent::Dispatch { text, voice, turn } => {
                let beats = crate::script::respond_branch(
                    &text,
                    voice,
                    turn,
                    &self.generic_counter,
                    &self.roster_counter,
                );
                if let Some(arm) = self
                    .table
                    .alloc_child(generation, ArmOwner::Session(session))
                {
                    self.player().spawn(beats, arm);
                }
            }
            DemoEvent::TurnEnd => self.finish_turn(model, session),
            DemoEvent::ChipCloseReq { agent } => self.close_chip(model, session, &agent),
            DemoEvent::AutoResume => self.auto_resume_check(model, session),
            // TalkFire's hold cannot survive a detach (`listening` clears
            // on leave), and aura events never carry a session — both fall
            // through absorb's no-op arm if they ever land here.
            other => {
                if let Some(entry) = model.session_entry_mut(session) {
                    entry.absorb(other);
                    model.dirty = true;
                }
            }
        }
    }

    /// Spawn the 120 ms autoResumeParent defer (§2.7). It resumes the
    /// SESSION's parked turn, so the arm is session-owned: an interrupt
    /// drops it, and the §2.7 guards re-check the world at consumption.
    fn arm_auto_resume(&self, parent: u64, session: u64) {
        self.spawn_timer(
            parent,
            ArmOwner::Session(session),
            Duration::from_millis(crate::script::AUTO_RESUME_DEFER_MS),
            DemoEvent::AutoResume,
        );
    }

    /// Consume one arm-tagged demo event. Cancelled arms are dropped whole
    /// — the P1-1 law: a teardown invalidates events already buffered in
    /// the channel, not only future sends.
    pub fn consume(&mut self, model: &mut AppModel, generation: u64, event: DemoEvent) {
        if !self.table.is_live(generation) {
            return;
        }
        // TUI4c background routing: an arm owned by a session that is NOT
        // attached applies to that session's slot — never to the visible
        // surface (item 12's law from the driver's side). Control-tagged
        // and aura arms have no session and take the surface path; the
        // by-id events (AutoTitle, Answer) do their own lookup there.
        if let Some(sid) = self
            .table
            .owner(generation)
            .and_then(|owner| owner.session_id())
        {
            if sid == 0 {
                // Scratch-lineage arms (no session id) belong to the live
                // fields ONLY while the surface is still the scratch — a
                // later attached session must never receive their events.
                if model.active_session.is_some() {
                    return;
                }
            } else if model.active_session != Some(sid) {
                self.consume_background(model, sid, generation, event);
                return;
            }
        }
        match event {
            DemoEvent::Envelope(payload) => {
                if let EventPayload::MenuAnswered(answer) = &payload {
                    self.resume_parked(answer);
                }
                model.handle(AppEvent::Envelope(Box::new(payload)));
            }
            DemoEvent::Answer { origin, answer } => {
                // IDENTITY GATE (review r2 P1-1): the control tag guarantees
                // delivery, never relevance. An answer to a card rendered by
                // a session the user has since replaced is dropped whole —
                // it must not reconfigure the session that took its place,
                // nor start that card's parked continuation.
                if origin != model.session_epoch {
                    return;
                }
                self.resume_parked(&answer);
                model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuAnswered(
                    answer,
                ))));
            }
            DemoEvent::Dispatch { text, voice, turn } => {
                // The sim picks its branch AFTER the think window
                // (tui.js:1259) — build it now, on this same live arm, so
                // the generic/roster counters advance only for turns that
                // actually reach dispatch (review P2-11).
                let beats = crate::script::respond_branch(
                    &text,
                    voice,
                    turn,
                    &self.generic_counter,
                    &self.roster_counter,
                );
                let session = self
                    .table
                    .owner(generation)
                    .and_then(|owner| owner.session_id())
                    .unwrap_or(0);
                if let Some(arm) = self
                    .table
                    .alloc_child(generation, ArmOwner::Session(session))
                {
                    self.player().spawn(beats, arm);
                }
            }
            DemoEvent::AutoTitle { origin, text } => {
                // Sim tui.js:1219-1227: the callback looks the session up
                // BY ID wherever it now lives — attached or background —
                // and does nothing if it is gone (cleared/reset) or
                // already titled. It is NOT cancelled by an interrupt
                // (review r2 P2-6).
                let blurb = crate::app::auto_blurb(&text);
                if origin == model.session_epoch {
                    if model.session_title.is_none() {
                        model.projection.push_note(title_note(&blurb));
                        model.session_title = Some(blurb);
                        model.dirty = true;
                    }
                } else if let Some(entry) = model.session_entry_mut(origin)
                    && entry.title.is_none()
                {
                    entry.projection.push_note(title_note(&blurb));
                    entry.title = Some(blurb);
                    model.dirty = true;
                }
            }
            DemoEvent::Note(text) => {
                model.projection.push_note(text);
                model.dirty = true;
            }
            DemoEvent::Voice(on) => {
                model.projection.set_voice_live(on);
                model.dirty = true;
            }
            DemoEvent::TurnEnd => {
                let active = model.active_session.unwrap_or(0);
                self.finish_turn(model, active);
            }
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
            DemoEvent::ChipCloseReq { agent } => {
                let active = model.active_session.unwrap_or(0);
                self.close_chip(model, active, &agent);
            }
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
                let active = model.active_session.unwrap_or(0);
                self.auto_resume_check(model, active);
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
    /// idle; else IDLE, then (review P2-13) the sim's own transient
    /// `IDLE → 30 ms → COMPACTING` transition when the window is hot.
    pub fn finish_turn(&mut self, model: &mut AppModel, session: u64) {
        // The end-of-turn law runs for the OWNING session — attached, or a
        // background slot whose queue consumes just the same (the sim's
        // finishTurn is per-session, tui.js:1507-1543).
        let attached = model.active_session == Some(session)
            || (session == 0 && model.active_session.is_none());
        let queued = if attached {
            if model.msg_queue.is_empty() {
                None
            } else {
                model.dirty = true;
                Some(model.msg_queue.remove(0))
            }
        } else {
            model.session_entry_mut(session).and_then(|entry| {
                if entry.msg_queue.is_empty() {
                    None
                } else {
                    Some(entry.msg_queue.remove(0))
                }
            })
        };
        if let Some(text) = queued {
            self.turn_counter += 1;
            let mut beats = vec![Beat::Note(
                "· turn ended with queued input — consuming it directly, no idle".to_owned(),
            )];
            beats.extend(respond_preamble(
                &text,
                false,
                haider_protocol::DeliveryMode::Queue,
                self.turn_counter,
            ));
            self.play_beats(beats, session);
            return;
        }
        // `hot` is sampled BEFORE the 30 ms window, exactly as the sim
        // samples `b.tokens` before its `await sleep(30)` (tui.js:1510-1518).
        let window = model.identity.context_window;
        let total = self.tokens_total(session);
        let mut beats = vec![Beat::Emit(EventPayload::RunState(RunState::Done))];
        if window > 0 && total.saturating_mul(100) >= window.saturating_mul(85) {
            beats.push(Beat::Sleep(COMPACT_IDLE_GAP_MS));
            self.compact_counter += 1;
            beats.extend(compaction_beats(
                total,
                window * 6 / 100,
                false,
                self.compact_counter,
            ));
        }
        self.play_beats(beats, session);
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

/// The beat player: one spawned task per script, ARM-captured. Cloned into
/// child tasks so `ChipScript` beats can spawn concurrently (the future is
/// boxed at each spawn to break the recursive type).
#[derive(Clone)]
struct Player {
    tx: mpsc::Sender<(u64, DemoEvent)>,
    meters: SessionMeters,
    parked: PendingArms,
    table: ArmTable,
}

impl Player {
    fn spawn(&self, beats: Vec<Beat>, arm: u64) {
        let player = self.clone();
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(async move {
                player.run(beats, arm).await;
            });
        tokio::spawn(future);
    }

    /// Mutate one session's token meter and return its new totals.
    fn meter(&self, session: u64, apply: impl FnOnce(&mut (u64, u64))) -> (u64, u64) {
        let Ok(mut meters) = self.meters.lock() else {
            return (0, 0);
        };
        let entry = meters.entry(session).or_insert((0, 0));
        apply(entry);
        *entry
    }

    #[allow(clippy::too_many_lines)]
    async fn run(&self, beats: Vec<Beat>, arm: u64) {
        // Every token beat lands in the OWNING session's meter (TUI4c —
        // one global pair would bleed concurrent sessions together).
        let session = self
            .table
            .owner(arm)
            .and_then(|owner| owner.session_id())
            .unwrap_or(0);
        let usage_event = |(input, output): (u64, u64)| {
            DemoEvent::Envelope(EventPayload::Usage(Usage {
                input,
                output,
                reasoning: 0,
                cached: 0,
                source: UsageSource::Estimated,
                account: None,
            }))
        };
        for beat in beats {
            if !self.table.is_live(arm) {
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
                    let totals = self.meter(session, |(input_b, output_b)| {
                        if output {
                            *output_b = output_b.saturating_add(n);
                        } else {
                            *input_b = input_b.saturating_add(n);
                        }
                    });
                    usage_event(totals)
                }
                Beat::TokensReset(n) => {
                    let totals = self.meter(session, |entry| *entry = (n, 0));
                    usage_event(totals)
                }
                Beat::Dispatch { text, voice, turn } => {
                    // Park the branch choice for the driver: the counters
                    // must advance at dispatch, not while building beats.
                    DemoEvent::Dispatch { text, voice, turn }
                }
                Beat::AwaitMenu { menu, arms } => {
                    let owner = self.table.owner(arm).unwrap_or(ArmOwner::Session(session));
                    if let Ok(mut pending) = self.parked.lock() {
                        pending.insert(menu.as_str().to_owned(), (arm, owner, arms));
                    }
                    return;
                }
                Beat::TurnEnd => DemoEvent::TurnEnd,
                // Chip/aura beats map 1:1 onto their DemoEvents, tagged
                // with THIS script's arm.
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
                Beat::ChipScript { agent, beats } => {
                    // A concurrent child script on its OWN chip-owned arm,
                    // allocated against THIS arm under one lock — a
                    // teardown between the check and the spawn can no
                    // longer be adopted (review P1-1).
                    if let Some(child) = self
                        .table
                        .alloc_child(arm, ArmOwner::Chip { session, agent })
                    {
                        self.spawn(beats, child);
                    }
                    continue;
                }
                Beat::AutoResume => {
                    // The 120 ms defer resumes the SESSION's parked turn,
                    // so its arm is session-owned; the §2.7 state guards
                    // are re-checked at consumption.
                    if let Some(timer) = self.table.alloc_child(arm, ArmOwner::Session(session)) {
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(
                                crate::script::AUTO_RESUME_DEFER_MS,
                            ))
                            .await;
                            let _ = tx.send((timer, DemoEvent::AutoResume)).await;
                        });
                    }
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
            if !self.table.is_live(arm) {
                return;
            }
            if self.tx.send((arm, event)).await.is_err() {
                return;
            }
        }
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

/// Render the model into a scratch buffer at the live terminal size and
/// extract the selection's text — the copy path's ground truth. Uses the
/// same pure [`render`] as the screen, so what copies is exactly what a
/// redraw of THIS model state shows.
#[must_use]
pub fn rendered_selection_text(
    model: &AppModel,
    size: ratatui::layout::Size,
    selection: &crate::select::Selection,
) -> String {
    let backend = ratatui::backend::TestBackend::new(size.width, size.height);
    // TestBackend construction is infallible (uninhabited error type).
    let Ok(mut scratch) = Terminal::new(backend);
    if scratch.draw(|frame| drop(render(model, frame))).is_err() {
        return String::new();
    }
    crate::select::selection_text(scratch.backend().buffer(), selection)
}

/// Auto-copy side effects for a finished selection (owner item 9), in the
/// documented order: pbcopy (authoritative local clipboard), then OSC 52
/// (best-effort mirror for remote/embedded terminals — always emitted, but
/// unverifiable, so it never upgrades the flash: the flash reports the
/// channel we can actually observe).
fn copy_selection_effects(model: &mut AppModel, text: &str) {
    let ok = crate::clipboard::copy_local(text);
    let mut out = stdout();
    let _ = out.write_all(crate::clipboard::osc52(text).as_bytes());
    let _ = out.flush();
    model.flash = Some(if ok { "· copied" } else { "· copy failed" }.to_owned());
    model.dirty = true;
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
