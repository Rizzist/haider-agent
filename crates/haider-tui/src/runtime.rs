//! The interactive runtime: one task owns the terminal and the [`AppModel`];
//! input, envelopes and frame deadlines multiplex through `tokio::select!`
//! (research rec 3/6). Alternate screen for v0.1 (rec 1); native-scrollback
//! insertion is explicitly deferred (rec 19).

use crate::app::{AppEvent, AppModel, AppRequest};
use crate::mock::{demo_script, response_script};
use crate::render::render;
use crate::theme::{Rgb, ThemeKey};
use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
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

    // Demo drivers: the boot beats play immediately; turns play on demand
    // (typed submits or sample attaches). `answer_echo` is the demo's client
    // seam — user menu answers loop back as MenuAnswered envelopes, exactly
    // the shape the daemon will publish later.
    let (envelope_tx, mut envelope_rx) = mpsc::channel::<EventPayload>(64);
    let answer_echo = envelope_tx.clone();
    let script_tx = envelope_tx.clone();
    tokio::spawn(async move {
        for payload in crate::mock::boot_script() {
            tokio::time::sleep(demo_pace(&payload)).await;
            if envelope_tx.send(payload).await.is_err() {
                return;
            }
        }
    });
    // Script generation: bumping it silences any in-flight script (fresh
    // session / reset), so two scripts can never interleave into one
    // projection (review r1 P1).
    let script_gen = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let play = |payloads: Vec<EventPayload>, generation: u64| {
        let tx = script_tx.clone();
        let gen_ref = std::sync::Arc::clone(&script_gen);
        tokio::spawn(async move {
            for payload in payloads {
                tokio::time::sleep(demo_pace(&payload)).await;
                if gen_ref.load(std::sync::atomic::Ordering::SeqCst) != generation {
                    return;
                }
                if tx.send(payload).await.is_err() {
                    return;
                }
            }
        });
    };
    let mut turn_counter: u64 = 0;
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
                Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    model.handle(AppEvent::Key(key));
                }
                Some(Event::Paste(text)) => model.handle(AppEvent::Paste(text)),
                Some(Event::Resize(..)) => model.dirty = true,
                Some(Event::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let hit = hit_map.iter().find(|(rect, _)| {
                            mouse.column >= rect.x
                                && mouse.column < rect.x + rect.width
                                && mouse.row >= rect.y
                                && mouse.row < rect.y + rect.height
                        });
                        if let Some((_, action)) = hit {
                            model.handle_hit(*action);
                        }
                    }
                    MouseEventKind::ScrollUp => model.handle_wheel(true),
                    MouseEventKind::ScrollDown => model.handle_wheel(false),
                    _ => {}
                },
                Some(_) => {}
                None => break,
            },
            () = &mut autoplay, if !model.auto_play_spent => {
                model.handle(AppEvent::AutoPlay);
            }
            payload = envelope_rx.recv(), if stream_open => match payload {
                Some(payload) => model.handle(AppEvent::Envelope(Box::new(payload))),
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
            }
        }
        // Reducer-requested side effects.
        for request in model.requests.drain(..) {
            match request {
                AppRequest::SubmitText(text) => {
                    turn_counter += 1;
                    let generation = script_gen.load(std::sync::atomic::Ordering::SeqCst);
                    play(response_script(&text, turn_counter), generation);
                }
                AppRequest::AttachSample(_) => {
                    turn_counter += 1;
                    let generation = script_gen.load(std::sync::atomic::Ordering::SeqCst);
                    play(crate::mock::turn_script(turn_counter), generation);
                }
                AppRequest::StopScripts => {
                    script_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                AppRequest::Quit => model.should_quit = true,
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
        while let Some(answer) = model.outbox.first().cloned() {
            match answer_echo.try_send(EventPayload::MenuAnswered(answer)) {
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
