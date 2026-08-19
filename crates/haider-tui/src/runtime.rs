//! The interactive runtime: one task owns the terminal and the [`AppModel`];
//! input, envelopes and frame deadlines multiplex through `tokio::select!`
//! (research rec 3/6). Alternate screen for v0.1 (rec 1); native-scrollback
//! insertion is explicitly deferred (rec 19).

use crate::app::{AppEvent, AppModel, AppRequest, DemoRequest};
use crate::identity::UiGeneration;
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
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
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

/// ADE correlation seam (W-OSC): a namespaced OSC naming the session this
/// PTY's TUI is attached to. Real terminals discard unknown OSC numbers;
/// an embedding ADE parses it to re-home this terminal into the right
/// session surface. An empty payload announces "back at the launcher".
/// The id is control-stripped so a hostile name can never smuggle a second
/// escape sequence into the stream.
#[must_use]
pub fn osc_session_announce(session_id: Option<&str>) -> String {
    let clean: String = session_id
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() && *c != ';')
        .collect();
    format!("\u{1b}]7791;haider;attached={clean}\u{1b}\\")
}

fn sync_session_announce(session_id: Option<&str>) {
    let mut out = stdout();
    let _ = out.write_all(osc_session_announce(session_id).as_bytes());
    let _ = out.flush();
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

/// Detect the system/terminal appearance and map it to a theme family:
/// light ground → Light, dark ground → Dark, undetectable → Dark (owner
/// spec §3's fallback law). Two best-effort probes, authority first:
/// the OSC 11 background query under a short deadline (ground truth when
/// the emulator answers), then the `COLORFGBG` env convention for
/// terminals that set it but stay silent on OSC (e.g. rxvt lineage,
/// some tmux setups). Call BEFORE entering raw mode/alt screen.
///
/// Known residual (review TUI1 P2): the probe owns the tty for its bounded
/// window, so a keystroke typed in that pre-UI instant is consumed, not
/// forwarded. The window is kept tiny (80ms) and runs before any UI invites
/// input. The loss-free design — parsing the OSC reply inside the sole input
/// reader — lands with the daemon-era input stack (see OPTIMIZATIONS.md).
#[must_use]
pub fn detect_system_theme() -> ThemeKey {
    let osc = termbg::theme(Duration::from_millis(80)).ok();
    resolve_system_theme(osc, std::env::var("COLORFGBG").ok().as_deref())
}

/// The OSC probe's answer type, re-exported so tests can drive
/// [`resolve_system_theme`] without depending on termbg themselves.
pub use termbg::Theme as TerminalAppearance;

/// The pure resolution behind [`detect_system_theme`], testable without a
/// terminal: OSC answer beats `COLORFGBG`; nothing detectable → Dark.
#[must_use]
pub fn resolve_system_theme(osc: Option<termbg::Theme>, colorfgbg: Option<&str>) -> ThemeKey {
    match osc {
        Some(termbg::Theme::Light) => ThemeKey::Light,
        Some(termbg::Theme::Dark) => ThemeKey::Dark,
        None => colorfgbg
            .and_then(theme_from_colorfgbg)
            .unwrap_or(ThemeKey::Dark),
    }
}

/// Whether the terminal renders 24-bit color, over an env lookup (pure so
/// tests drive it without touching the process environment, mirroring
/// [`crate::wordmark::graphics_terminal_likely`]). The app emits truecolor
/// everywhere, so the ANSWER DEFAULTS TO TRUE and downgrades only on
/// POSITIVE low-color evidence: a `COLORTERM` that names `truecolor`/`24bit`
/// pins true; otherwise a `TERM` that tops out at 16 colors (`linux`,
/// `dumb`, an explicit `-16color`) is the only thing that degrades the
/// Thinking-verb shimmer to its two-tone wave (W-E decision 6 / LE6).
#[must_use]
pub fn truecolor_capable(env: &dyn Fn(&str) -> Option<String>) -> bool {
    if env("COLORTERM").is_some_and(|c| c.contains("truecolor") || c.contains("24bit")) {
        return true;
    }
    !env("TERM").is_some_and(|term| {
        term == "linux" || term == "dumb" || term == "vt100" || term.ends_with("-16color")
    })
}

/// The ONE persistence authority for the theme choice (ui-themes-fix):
/// both runtime loops call this every beat. It keys on the model's COMMIT
/// counter — not on a choice diff — so a commit that re-affirms the boot
/// default still writes the settings file (the live probe found no file
/// after exactly that flow). Previews and boot resolution bump nothing
/// and therefore never write.
pub fn sync_theme_persistence(
    model: &crate::app::AppModel,
    seen_commits: &mut u64,
    settings: &mut Option<crate::settings::SettingsStore>,
) {
    if model.theme_commits == *seen_commits {
        return;
    }
    *seen_commits = model.theme_commits;
    if let Some(store) = settings.as_mut() {
        store.save_if_changed(model.theme_choice);
    }
}

/// W-C M2: persist a notification-toggle change (mirrors the theme sync).
pub fn sync_notification_persistence(
    model: &crate::app::AppModel,
    seen_commits: &mut u64,
    settings: &mut Option<crate::settings::SettingsStore>,
) {
    if model.notification_commits == *seen_commits {
        return;
    }
    *seen_commits = model.notification_commits;
    if let Some(store) = settings.as_mut() {
        store.save_notifications_if_changed(model.theme_choice, model.notifications_enabled);
    }
}

/// Model retention (owner 2026-08-15): persist a committed model pick so the
/// next boot opens the harness on it (mirrors the theme/notification sync).
pub fn sync_model_persistence(
    model: &crate::app::AppModel,
    seen_commits: &mut u64,
    settings: &mut Option<crate::settings::SettingsStore>,
) {
    if model.model_commits == *seen_commits {
        return;
    }
    *seen_commits = model.model_commits;
    if let Some(store) = settings.as_mut() {
        store.save_last_model_if_changed(
            model.theme_choice,
            &model.identity.provider,
            &model.identity.model_short,
        );
    }
}

/// W-C M2: emit each queued desktop notification as an OSC 9 sequence — but
/// ONLY to a real terminal. A piped/redirected stdout receives NO escape
/// bytes (the non-tty suppression law), so a captured run stays clean.
fn emit_notifications(model: &mut crate::app::AppModel) {
    let pending = model.take_notifications();
    if pending.is_empty() {
        return;
    }
    let is_tty = std::io::IsTerminal::is_terminal(&stdout());
    let mut out = stdout();
    for line in pending {
        // Non-tty stdout yields no bytes (the suppression law's single home).
        let _ = out.write_all(&crate::notify::osc9_for_tty(&line, is_tty));
    }
    let _ = out.flush();
}

/// Parse the `COLORFGBG` convention (`"<fg>;<bg>"`, sometimes
/// `"<fg>;default;<bg>"`): the LAST field is the background's 16-color
/// index — 0-6 and 8 are dark grounds, 7 and 15 light. Anything else
/// (unset, `default`, malformed) is honestly undetectable → `None`.
#[must_use]
pub fn theme_from_colorfgbg(value: &str) -> Option<ThemeKey> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    match bg {
        0..=6 | 8 => Some(ThemeKey::Dark),
        7 | 15 => Some(ThemeKey::Light),
        _ => None,
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
            EnableMouseCapture,
            // W-C M2: report focus so the notification gate can suppress a
            // ping while the terminal is focused.
            EnableFocusChange
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
        DisableFocusChange,
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
///
/// `store` is the DEMO persistence file (TUI4c-13b — see
/// [`crate::demo_store`]): the loop saves after every drawn frame (the
/// coalesced form of the sim's save-on-every-change), once more on quit,
/// and intercepts `/reset`'s purge request. `None` disables persistence
/// (the headless/plain paths and CI stay deterministic).
pub async fn run_demo(
    mut model: AppModel,
    mut store: Option<crate::demo_store::DemoStore>,
) -> std::io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    // Sync the emulator's own background (window padding) to the theme
    // ground. (No Terminal::clear() here: ratatui's clear paths can issue a
    // cursor-position query that hangs non-answering PTYs; the first full
    // draw repaints every cell anyway.)
    sync_terminal_bg(model.theme);
    sync_window_title(&model.window_title());
    let mut active_theme = model.theme;
    // Owner spec §3: the theme CHOICE is TUI-local display state — persist
    // every user COMMIT to the profile-dir settings file. Previews inside
    // the open picker move only the resolved theme, so they never write.
    let mut settings = crate::settings::SettingsStore::open_default();
    let mut seen_theme_commits = model.theme_commits;
    let mut active_title = model.window_title();

    // Query the terminal for a graphics protocol and build the wordmark image
    // NOW — after raw mode, before the input pump claims stdin — so the
    // capability response is not eaten by the pump. Degrades to None (the
    // half-block art) on a non-graphics or non-answering terminal; never hangs.
    *model.wordmark.borrow_mut() = crate::wordmark::Wordmark::detect();
    // Pin the truecolor capability once (read by render for the Thinking
    // shimmer's fidelity); the default is true, so this only downgrades a
    // proven 16-color terminal.
    model.truecolor = truecolor_capable(&|name| std::env::var(name).ok());

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
    // ONE honour roll (sim `rosterRef`, tui.js:681): the driver's chip
    // claims post-increment the same counter the reducer's head claims do —
    // and the persistence load's guard-3 restore covers both through it.
    let (mut driver, mut envelope_rx) = DemoDriver::new(64, std::sync::Arc::clone(&model.roster));
    // Meter continuity (sim: `branch.tokens` is persisted state the next
    // turn adds to): every session's demo meter resumes from its restored
    // (or seeded) usage total instead of resetting to zero on first beat.
    for entry in &model.sessions {
        if let Some(usage) = entry.projection.usage() {
            driver.prime_meter(
                entry.ui_gen,
                usage.input.saturating_add(usage.cached),
                usage.output.saturating_add(usage.reasoning),
            );
        }
    }
    driver.spawn_boot();
    let answer_echo = driver.sender();
    // NB: no launcher auto-play. The sim has none — an untouched launcher
    // simply waits (owner item 1: opening/idling must not start a sequence).

    let mut frame_tick = tokio::time::interval(Duration::from_millis(33));
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The shared animation clock (TUI4d item 14): guarded exactly like the
    // frame tick — while nothing on screen pulses the branch is disabled
    // and the idle loop takes ZERO periodic wakeups (the efficiency law
    // this port was once deferred over). When the last animated state
    // ends the gate closes on the next loop pass — the clock stops within
    // one phase — and no animator outlives any teardown because nothing
    // is registered anywhere: no timer arm, no per-element state, just
    // this guarded branch reading `AppModel::animated`.
    let mut anim_tick = tokio::time::interval(Duration::from_millis(ANIM_PHASE_MS));
    anim_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Fused: after the stream closes this branch is disabled — a closed
    // receiver must never spin the loop (review r1 P1).
    let mut stream_open = true;
    // The last frame's clickable regions (render reports, mouse consumes).
    let mut hit_map: Vec<(ratatui::layout::Rect, crate::app::Hit)> = Vec::new();
    // The last pointer cell, for post-draw hover settling (W5g-7).
    let mut pointer: Option<(u16, u16)> = None;

    while !model.should_quit {
        tokio::select! {
            input = input_rx.recv() => match input {
                Some(event) => {
                    match &event {
                        // The pre-resize map describes a frame that no
                        // longer exists — a queued Moved must not re-arm a
                        // hover from dead geometry (W5g-7).
                        Event::Resize(..) => hit_map.clear(),
                        Event::Mouse(mouse) => pointer = Some((mouse.column, mouse.row)),
                        _ => {}
                    }
                    dispatch_input(&mut model, &hit_map, event);
                }
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
            // The phase clock: a pure render-phase toggle — no projection
            // mutation, no arms, no persistence (the store's snapshot
            // never carries the phase, so the save-on-frame hash-skip
            // stays quiet across ticks).
            _ = anim_tick.tick(), if model.animated() => {
                model.anim_phase = model.anim_phase.wrapping_add(1);
                // S4: the same tick is the live chips' elapsed clock — no
                // second timer, and a closed gate costs nothing (terminal
                // chips read frozen journal time, not this).
                model.clock_ms = model.clock_ms.max(now_epoch_ms());
                model.dirty = true;
            }
            // Guarded tick: while the model is clean this branch is disabled,
            // so the idle loop takes NO periodic wakeups (efficiency rider
            // #10 — ~109k/hour otherwise). The first dirtying event re-arms
            // it and the overdue tick fires immediately, keeping the 30fps
            // coalescing behavior.
            _ = frame_tick.tick(), if model.dirty => {
                hit_map = draw(&mut terminal, &model)?;
                model.dirty = false;
                // W5g-7: hover survives a redraw only while the pointer
                // still resolves to it (subsumes the old identity-vanish
                // cleanup, and also kills a highlight whose target MOVED
                // under a stationary pointer).
                settle_hover_after_draw(&mut model, &hit_map, pointer);
                // Demo persistence (TUI4c-13b): a drawn frame means state
                // changed — save here, coalesced to the frame cadence
                // (hash-skipped when nothing persisted moved). This is the
                // sim's save-on-every-change without a timer or an arm.
                if let Some(store) = store.as_mut() {
                    store.save(&model);
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
                // TUI5 items 4+5: model-known text (the composer
                // selection) — same pbcopy + OSC 52 + honest-flash path,
                // no frame extraction needed.
                AppRequest::CopyText(text) => copy_selection_effects(&mut model, &text),
                request => driver.handle_request(&mut model, request),
            }
        }
        // Demo-only side effects (W3c3, report R11 cut 3) — their own
        // queue, drained ONLY here. `run_live` never sees this vocabulary,
        // so a live reset can never delete demo persistence.
        let demo_requests: Vec<DemoRequest> = model.demo_requests.drain(..).collect();
        for demo_request in demo_requests {
            match demo_request {
                // Runtime-owned like CopySelection: only this loop knows
                // the store path. /reset deletes the demo state file (sim
                // tui.js:1918); the reducer already reseeded, and the next
                // frame's save rewrites the seeds exactly as the sim's save
                // effect refills localStorage after removeItem.
                DemoRequest::PurgeStore => {
                    if let Some(store) = store.as_mut() {
                        store.purge();
                    }
                }
            }
        }
        // Theme cycled: re-sync the emulator background.
        sync_theme_persistence(&model, &mut seen_theme_commits, &mut settings);
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
    // The quit-path write (13b brief): mutations between the last drawn
    // frame and the exit still land on disk.
    if let Some(store) = store.as_mut() {
        store.save(&model);
    }
    Ok(())
}

/// The sim's idle(i) decay window (tui.js:1562: 30s of nothing).
pub const IDLE_DECAY: Duration = Duration::from_secs(30);

/// TUI4d item 14 — one phase of the shared animation clock. The sim runs
/// per-element CSS periods (1.1-1.5 s pulses, a 1.8 s rail shimmer); the
/// port folds them onto ONE clock: 600 ms per phase gives a 1.2 s pulse
/// (two phases) and, via `% 3`, the shimmer's 1.8 s exactly.
pub const ANIM_PHASE_MS: u64 = 600;

/// Wall clock in epoch ms — the S4 chip clocks' runtime time base: the
/// anim tick advances [`AppModel::clock_ms`] with it, and the demo driver
/// stamps chip events with it (the demo fabricates locally, so its journal
/// time IS the wall clock). Saturating: a pre-epoch clock renders 0.
#[must_use]
pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

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
        // TUI6.3 fix 2: the paste is wrapped at RECEIPT — the zeroizing
        // buffer takes the same allocation, so our one owned copy wipes
        // on drop and Debug-prints redacted.
        Event::Paste(text) => model.handle(AppEvent::Paste(crate::app::Pasted::new(text))),
        Event::Resize(cols, _) => {
            // TUI6.1 fix 1 (reflow-before-input): the next frame's wrap
            // budget is a pure function of the NEW width, so apply it
            // here — before any queued key can walk the previous width's
            // visual rows (review r1 finding 1: a Down racing the redraw
            // landed on the old wrap geometry). The reducer stays
            // wrap-ignorant: this is the dispatch seam, the same layer
            // that owns the hit map. handle_resize bumps the geometry
            // epoch, which retires every composer hit the old frames
            // stamped.
            model
                .composer
                .set_wrap_budget(crate::render::composer_text_budget(cols));
            model.handle_resize();
        }
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
                // TUI5 item 5: a Down on a COMPOSER text row is different —
                // native inputs place the caret ON the press, and a drag
                // from there is a composer selection (region
                // disambiguation by drag START). The composer press never
                // arms `mouse_down`, so the transcript path stays silent.
                MouseEventKind::Down(MouseButton::Left) => {
                    if model.selection.take().is_some() {
                        model.dirty = true;
                    }
                    if let Some((
                        rect,
                        crate::app::Hit::ComposerText {
                            start,
                            content,
                            surface,
                            revision,
                            epoch,
                        },
                    )) = hit_rect_at(hit_map, mouse.column, mouse.row)
                    {
                        let col = usize::from(mouse.column.saturating_sub(rect.x));
                        model.composer_press(start, &content, col, surface, revision, epoch);
                        return;
                    }
                    model.mouse_down = Some((mouse.column, mouse.row));
                }
                // Movement with the button held: meaningful movement (a
                // different cell than the anchor) enters selection mode
                // with a live linear highlight; same-cell jitter is not a
                // drag. Once selecting, every head change redraws.
                MouseEventKind::Drag(MouseButton::Left) => {
                    // TUI5 item 5: a drag that STARTED in the composer
                    // extends the composer selection — the pointer maps
                    // through the frame's composer windows; off the band
                    // it clamps to the text's start/end (native
                    // drag-past-the-edge law).
                    if model.composer_drag {
                        model.composer_drag_to(composer_byte_at(
                            hit_map,
                            model,
                            mouse.column,
                            mouse.row,
                        ));
                        return;
                    }
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
                // the Down coordinates exactly as before. A composer press
                // resolves first (item 5): its selection auto-copies with
                // the same flash, a plain composer click already placed
                // the caret on Down.
                MouseEventKind::Up(MouseButton::Left) => {
                    if model.composer_drag {
                        model.composer_release();
                        return;
                    }
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
        // W-C M2: crossterm focus reporting drives the notification focus gate
        // (a notification fires only when the terminal is UNFOCUSED). Seeing
        // ANY focus event also flips the "focus is reported" latch, so the
        // fire-anyway fallback only holds on emulators that never report it.
        Event::FocusGained => model.set_focus(true),
        Event::FocusLost => model.set_focus(false),
        _ => {}
    }
}

/// The first hit-map entry containing the cell, WITH its rect (TUI5
/// Post-draw hover consistency (W5g-7): a redraw can move every target
/// under a STATIONARY pointer (session inserts, shell output, screen
/// switches), and identity-only cleanup let the highlight follow the OLD
/// target to its new row. If the freshly installed map no longer resolves
/// the pointer to the hovered identity, the painted highlight is a lie —
/// drop it and repaint. Never ADOPT the newly resolved target here:
/// imposing pointer selection on every keyboard-driven redraw would steal
/// palette/menu navigation from the keys. Real motion re-arms hover.
pub fn settle_hover_after_draw(
    model: &mut AppModel,
    hit_map: &[(ratatui::layout::Rect, crate::app::Hit)],
    pointer: Option<(u16, u16)>,
) {
    if model.hovered.is_none() {
        return;
    }
    let resolved =
        pointer.and_then(|(column, row)| hit_rect_at(hit_map, column, row).map(|(_, hit)| hit));
    if resolved != model.hovered {
        model.hovered = None;
        model.dirty = true;
    }
}

/// item 5: the composer press maps the click column against the rect's
/// origin — the same first-match rule the `hit_at` closure applies).
fn hit_rect_at(
    hit_map: &[(ratatui::layout::Rect, crate::app::Hit)],
    column: u16,
    row: u16,
) -> Option<(ratatui::layout::Rect, crate::app::Hit)> {
    hit_map
        .iter()
        .find(|(rect, _)| {
            column >= rect.x
                && column < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
        })
        .cloned()
}

/// Map a dragging pointer to a composer byte (TUI5 item 5): on a composer
/// row, through that row's render-time window; above the band, the text
/// start; below it, the text end — the native "drag past the edge selects
/// to the boundary" law. With no composer rows in the frame the caret
/// stays put.
fn composer_byte_at(
    hit_map: &[(ratatui::layout::Rect, crate::app::Hit)],
    model: &AppModel,
    column: u16,
    row: u16,
) -> usize {
    let mut band = None::<(u16, u16)>;
    for (rect, hit) in hit_map {
        let crate::app::Hit::ComposerText {
            start,
            content,
            surface,
            revision,
            epoch,
        } = hit
        else {
            continue;
        };
        // TUI5.1 fix 2: drag rows bind to surface + revision exactly as
        // the press does — a stale row is no row. TUI6.1 fix 1: and to
        // the geometry epoch, so a drag armed before a resize maps
        // through NOTHING (the caret stays put) instead of the previous
        // frame's rows.
        if *surface != model.surface_key()
            || *revision != model.composer.revision()
            || *epoch != model.geometry_epoch.get()
        {
            continue;
        }
        let (top, bottom) = band.get_or_insert((rect.y, rect.y));
        *top = (*top).min(rect.y);
        *bottom = (*bottom).max(rect.y);
        if rect.y == row {
            let col = usize::from(column.saturating_sub(rect.x));
            return start + crate::composer::byte_at_col(content, col);
        }
    }
    match band {
        Some((top, _)) if row < top => 0,
        Some((_, bottom)) if row > bottom => model.composer.text().len(),
        _ => model.composer.cursor(),
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
    /// One session's turn engine, keyed by its LOCAL GENERATION (TUI4c:
    /// the sim's per-session `runTokensRef`, tui.js:1551-1567 — an
    /// interrupt cancels only ITS session's turn; other sessions' turns
    /// keep running in the background). [`UiGeneration::SCRATCH`] is the
    /// surface/scratch lineage (boot, pre-map tests) and routes to the
    /// model's live fields.
    ///
    /// W3c3: a generation, never the protocol `SessionId` (report R11
    /// cut 1) — arms are demo-local stale-work tags, and the scratch
    /// sentinel has no opaque-string analogue.
    Session(UiGeneration),
    /// One subagent chip's script. Its own close/removal cancels it, and a
    /// fresh session cancels every chip — but a session INTERRUPT does
    /// NOT: the sim's `interrupt` touches only the run token, the queue and
    /// the note (tui.js:1551-1567), so children outlive their parent's
    /// cancelled turn. Because the chip's PARKED arms survive with them,
    /// answering such a child's card still resolves cleanly — that is what
    /// closes the review's "permanently blocked chip" hole. Carries the
    /// owning SESSION's id so background chip events route to their
    /// session's tree.
    Chip {
        session: UiGeneration,
        agent: String,
    },
    /// An aura orchestrate run or its talk timer. The next submit cancels
    /// the previous run (sim `++auraRunRef`, tui.js:2060) and `/reset`
    /// cancels it outright; `/clear`, a session interrupt, and a fresh
    /// session deliberately do NOT cancel it (sim tui.js:1913/1950 — a
    /// background orchestration finishes; review r2 P2-5).
    Aura,
}

impl ArmOwner {
    /// The generation this arm belongs to (`None` for aura arms).
    const fn generation(&self) -> Option<UiGeneration> {
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
type SessionMeters =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<UiGeneration, (u64, u64)>>>;

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

    /// Any live arm belonging to `session` (its turn engine OR its chips).
    /// The persistence port's stale-card gate: a session restored from disk
    /// has NO arms until something runs in it, so an answer to one of its
    /// hydrated cards has no live run to land on (the sim's missing menu
    /// RESOLVER after reload, tui.js:870-876).
    fn has_session_arms(&self, session: UiGeneration) -> bool {
        self.inner.lock().is_ok_and(|table| {
            table
                .1
                .values()
                .any(|owner| owner.generation() == Some(session))
        })
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
    /// A driver plus the receiving end of its demo-event channel. `roster`
    /// is the MODEL's claim counter (sim `rosterRef`, tui.js:681) — a
    /// constructor argument on purpose (review TUI4.1, Fable D2-2): heads
    /// (reducer claims) and chips (driver claims) draw from ONE honour
    /// roll, and taking the counter here makes the split-brain
    /// unrepresentable — there is no second counter to forget to replace.
    #[must_use]
    pub fn new(capacity: usize, roster: Counter) -> (Self, mpsc::Receiver<(u64, DemoEvent)>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                table: ArmTable::new(),
                turn_counter: 0,
                compact_counter: 0,
                generic_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                roster_counter: roster,
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

    /// Prime one session's demo token meter (TUI4c-13b: the sim's
    /// `branch.tokens` is persisted state the next turn ADDS to — without
    /// priming, the first post-reload beat would reset the meter to its own
    /// small delta).
    pub fn prime_meter(&self, session: UiGeneration, input: u64, output: u64) {
        if let Ok(mut meters) = self.meters.lock() {
            meters.insert(session, (input, output));
        }
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
    pub fn tokens_total(&self, session: UiGeneration) -> u64 {
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
    pub fn play_beats(&self, beats: Vec<Beat>, session: UiGeneration) -> u64 {
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
    /// and Chip only for ResetAllSessions — see `ArmOwner`) so buffered
    /// envelopes AND pending timers of a cancelled arm drop at
    /// consumption, and an interrupt schedules the sim's 30s idle(i)
    /// decay (tui.js:1561-1564).
    pub fn handle_request(&mut self, model: &mut AppModel, request: AppRequest) {
        // Requests are pushed by the reducer while its session is attached
        // (or from the no-session scratch surface = 0).
        let active = model.ui_generation();
        match request {
            // The captured branch is live-wire vocabulary: the demo world
            // is single-branch, so the capture is always `None` here.
            AppRequest::SubmitText {
                text, voice, title, ..
            } => {
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
                // replacement via the origin identity (review r2 P2-6).
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
            AppRequest::ResetAllSessions => {
                // GLOBAL session teardown (pushed only by fresh_session:
                // the `/reset` arm and the scratch surface's fresh start —
                // `/clear` no longer reaches here, it detaches instead):
                // every session's arms and every chip's die, and all
                // meters clear. AURA DOES NOT — the sim's `/clear` leaves
                // `auraRunRef` alone (tui.js:1950-1955); only `/reset`
                // and the next orchestrate advance it, so a background
                // orchestration finishes where the sim finishes it
                // (review r2 P2-5).
                self.cancel_arms(&|owner| {
                    matches!(owner, ArmOwner::Session(_) | ArmOwner::Chip { .. })
                });
                if let Ok(mut meters) = self.meters.lock() {
                    meters.clear();
                }
            }
            AppRequest::Interrupt { .. } => {
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
            AppRequest::Compact { .. } => {
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
            // Runtime-owned (the event loop intercepts them before the
            // driver): copying reads the rendered frame, and the store
            // purge needs the state-file path — the driver has neither.
            // Reaching here means a headless harness drained them through
            // the driver — no-ops, never a panic.
            // Runtime/driver split: copy requests are intercepted by the
            // event loop above; `Reattach` and `CreateSession` belong to
            // `LiveDriver` — the demo stream has no cursor, never gaps, and
            // mints its own sessions locally. Reaching here means a
            // headless harness drained them through the demo driver:
            // no-ops, never a panic.
            AppRequest::CopySelection
            | AppRequest::CopyText(_)
            // OTA is production-shell vocabulary. The demo reducer keeps
            // `/update` as a stub and never emits either request.
            | AppRequest::CheckForUpdate
            | AppRequest::RunUpdate
            | AppRequest::Reattach { .. }
            | AppRequest::CreateSession { .. }
            // W8b live-only vocabulary: the reducer's demo gates flash
            // instead of pushing these, so the demo driver never sees one.
            | AppRequest::ShellExec { .. }
            | AppRequest::ToolsRefresh
            // H4 live-only vocabulary: `/hooks` in demo opens its
            // sim-honest empty state and refuses trust locally — neither
            // request is ever pushed.
            | AppRequest::HooksRefresh { .. }
            | AppRequest::HooksTrust { .. }
            // U2 live-only vocabulary: `/usage` in demo opens its honest
            // empty state (usage is daemon truth, never fabricated) and
            // the reducer's demo gate never pushes the read.
            | AppRequest::UsageRefresh
            // Fleet live-only vocabulary: demo synthesizes its snapshot
            // from the local chips at open and never pushes the read.
            | AppRequest::FleetRefresh
            // CG-M1 live-only vocabulary: graph reduction is daemon truth,
            // never fabricated in demo; the feature gate drops the read,
            // and the mutations refuse honestly upstream.
            | AppRequest::GraphRefresh
            | AppRequest::GraphInspectRefresh
            | AppRequest::RunRetry { .. }
            // Computer OS-permission actions are daemon/OS truth — the demo
            // world has no parked TCC grant to open Settings for.
            | AppRequest::OpenPermissionSettings { .. }
            | AppRequest::FleetMemberGraph { .. }
            | AppRequest::GraphPin
            | AppRequest::GraphAbandon { .. }
            // B2b live-only vocabulary: `/branch new` in demo mode flashes
            // its honest stub upstream — branches are daemon truth.
            | AppRequest::BranchCreate { .. }
            // B4b live-only vocabulary: `/attach` and the real paste pill
            // refuse honestly upstream in demo mode — attachments are
            // daemon-CAS truth, and the read itself is shell-owned.
            | AppRequest::AttachRead { .. }
            | AppRequest::AttachUpload { .. }
            // W10b live-only mutations: the demo reducer removes locally.
            | AppRequest::AccountRemove { .. }
            | AppRequest::ProviderRemove { .. }
            // Device auto-adoption refresh is live-only; demo has no host
            // credential stores and the reducer gate never pushes it.
            | AppRequest::DeviceCandidatesRefresh
            // T2 live-only vocabulary: `/talk` refuses honestly upstream
            // in demo mode (the chip keeps the canned hold), so neither
            // the secret RPCs nor the stt-shell effects can be pushed.
            | AppRequest::TranscriptionSecretRead
            | AppRequest::TranscriptionSecretStore { .. }
            | AppRequest::TalkShell(_) => {}
            // `/accounts` (W5d): the demo world answers from the sim's seed
            // list, synchronously — through the SAME reducer seams as live
            // (apply_snapshot / apply_account_selected), so the
            // forbidden-optimism gates run in both modes.
            AppRequest::AccountsRefresh => {
                if model.accounts.rows.is_empty() {
                    model
                        .accounts
                        .apply_snapshot(crate::mock::seed_account_rows(), None);
                }
                model.dirty = true;
            }
            AppRequest::ProvidersRefresh => {
                if model.providers.providers.is_empty() {
                    model
                        .providers
                        .apply_snapshot(crate::mock::seed_provider_summaries(), 1);
                }
                model.dirty = true;
            }
            // Unreachable in demo BY DESIGN: the demo card's `[1]`
            // fabricates locally (sim confirmAuth) and never pushes this
            // request — only the live card's ⏎ does.
            AppRequest::ProviderConfigure { .. } => {}
            // Live-only read (G4a): demo fabricates no discovery.
            AppRequest::ProviderModelsRefresh { .. } => {}
            AppRequest::SetDefaultModel {
                provider,
                model: default,
                ..
            } => {
                let Some(mut summary) = model
                    .providers
                    .providers
                    .iter()
                    .find(|summary| summary.provider == provider)
                    .cloned()
                else {
                    model.default_model_failed(&provider, "unknown provider", false);
                    return;
                };
                summary.default_model = Some(default);
                let revision = model.providers.revision.map_or(1, |current| current + 1);
                model.apply_default_model_set(summary, revision);
            }
            // F2a: the reducer fabricates demo selections locally and
            // never pushes this request in demo mode — but a defensive
            // fabricated commit keeps the twin honest if one arrives.
            AppRequest::SelectModel {
                model: model_name,
                provider,
                ..
            } => {
                model.apply_model_selected(&provider, &model_name);
            }
            // G2: same twin-honesty shape — demo renames locally in the
            // reducer and never pushes this request; a stray one commits
            // the fabricated truth instead of vanishing.
            AppRequest::Rename { session, title } => {
                model.apply_renamed(&session, Some(title));
            }
            // G3: same defensive fabricated commit for the tuning twins.
            AppRequest::SelectEffort { effort, .. } => {
                model.apply_effort_selected(effort.as_deref());
            }
            AppRequest::SelectFast { enabled, .. } => {
                model.apply_fast_selected(enabled);
            }
            AppRequest::OAuthAddStart {
                provider, attempt, ..
            } => {
                // The demo card goes straight to the sim's authorize step
                // (tui.js:3633): a canned loopback URL, `[1]` simulates.
                let origin = match provider.as_str() {
                    "openai-oauth" => "auth.openai.com",
                    // B6b: the kimi demo authorize points at the real
                    // device-flow host (the daemon's sanctioned issuer).
                    "kimi-oauth" => "auth.kimi.com",
                    "grok-oauth" => "auth.x.ai",
                    _ => "claude.ai",
                };
                model.oauth_add_phase(
                    attempt,
                    crate::app::OAuthAddPhase::WaitingBrowser {
                        url: "http://localhost:1455/callback (demo)".to_owned(),
                        origin: origin.to_owned(),
                    },
                );
            }
            AppRequest::OAuthAddCancel { .. } => {}
            AppRequest::OpenUrl { url } => {
                model.flash = Some(format!("· browser (demo): {url}"));
                model.dirty = true;
            }
            AppRequest::RevealPath { path } => {
                model.flash = Some(format!("· reveal (demo): {path}"));
                model.dirty = true;
            }
            AppRequest::AccountSetActive { alias, .. } => {
                let Some(row) = model
                    .accounts
                    .rows
                    .iter()
                    .find(|row| row.alias == alias)
                    .cloned()
                else {
                    model.account_select_failed(&alias, "unknown account");
                    return;
                };
                let descriptor = haider_protocol::credential::CredentialDescriptor {
                    alias: haider_protocol::ids::CredentialAlias::new(row.alias),
                    provider: row.provider,
                    base_url: row.base_url,
                    auth_method: row.method,
                    identity: row.identity,
                    status: row.status,
                    active: true,
                };
                let revision = model.accounts.revision.map_or(1, |current| current + 1);
                model.apply_account_selected(&descriptor, revision);
            }
            // `/login … api` needs a daemon. The demo answers honestly
            // rather than pretending to store a key (and the secret drops —
            // zeroized — right here).
            // The demo's login declines synchronously below, so there is
            // never an in-flight attempt to retire (TUI6.3 fix 1).
            AppRequest::LoginRetired { .. } => {}
            AppRequest::LoginApi { .. } => {
                // TUI6.2c finding 5: through the model's one close method
                // — a bare `login = None` here stranded the parked draft
                // and its history ring (restore_draft is private to the
                // model by design).
                model.close_login_card();
                model.flash =
                    Some("· /login — needs the daemon; run `haider` (not --demo)".to_owned());
                model.dirty = true;
            }
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
    fn close_chip(&mut self, model: &mut AppModel, session: UiGeneration, agent: &str) {
        // W3c3: ONE expression covers both arms of the old disjunction —
        // `ui_generation()` is SCRATCH exactly when nothing is attached.
        let attached = model.ui_generation() == session;
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
            let (doomed, closed) = match model.session_entry_by_generation(session) {
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
    fn auto_resume_check(&mut self, model: &mut AppModel, session: UiGeneration) {
        let attached = model.ui_generation() == session;
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
            let Some(entry) = model.session_entry_by_generation(session) else {
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
    /// through [`absorb_demo_event`]; the driver-owned ones (dispatch, turn
    /// end, close lifecycle, auto-resume) run their usual logic against the
    /// owning session's slot.
    fn consume_background(
        &mut self,
        model: &mut AppModel,
        session: UiGeneration,
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
                if let Some(entry) = model.session_entry_by_generation(session) {
                    absorb_demo_event(entry, DemoEvent::Envelope(payload));
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
                if let Some(entry) = model.session_entry_by_generation(session) {
                    absorb_demo_event(entry, other);
                    model.dirty = true;
                }
            }
        }
    }

    /// Spawn the 120 ms autoResumeParent defer (§2.7). It resumes the
    /// SESSION's parked turn, so the arm is session-owned: an interrupt
    /// drops it, and the §2.7 guards re-check the world at consumption.
    fn arm_auto_resume(&self, parent: u64, session: UiGeneration) {
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
            .and_then(|owner| owner.generation())
        {
            if sid.is_scratch() {
                // Scratch-lineage arms (no session id) belong to the live
                // fields ONLY while the surface is still the scratch — a
                // later attached session must never receive their events.
                if model.active_session.is_some() {
                    return;
                }
            } else if model.ui_generation() != sid {
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
                if origin != model.ui_generation() {
                    return;
                }
                // TUI4c-13b: a card restored from disk has no parked
                // continuation AND its session has no live arms (nothing
                // has run in it since boot) — the port of the sim's missing
                // menu RESOLVER after reload. Answering it closes the card
                // and lands the sim's note verbatim (tui.js:874-876);
                // sampled BEFORE resume_parked, though a parked entry
                // implies a live arm anyway.
                let stale = !self.table.has_session_arms(origin);
                self.resume_parked(&answer);
                model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuAnswered(
                    answer,
                ))));
                if stale {
                    model.projection.push_note(
                        "· stale menu dismissed — no live run attached (answered after reload)"
                            .to_owned(),
                    );
                    model.dirty = true;
                }
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
                    .and_then(|owner| owner.generation())
                    .unwrap_or(UiGeneration::SCRATCH);
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
                if origin == model.ui_generation() {
                    if model.session_title.is_none() {
                        model.projection.push_note(title_note(&blurb));
                        model.session_title = Some(blurb);
                        model.dirty = true;
                    }
                } else if let Some(entry) = model.session_entry_by_generation(origin)
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
                let active = model.ui_generation();
                self.finish_turn(model, active);
            }
            DemoEvent::TalkFire => model.talk_fire(),
            // ---- Chip events (§2) ----
            DemoEvent::ChipAdd(seed) => {
                let parent = seed.parent.clone();
                let mut chip = crate::app::ChipModel::from_seed(*seed);
                // S4: the demo fabricates locally — its spawn instant IS
                // the wall clock (the live path reads `AgentSpawned`'s
                // `committed_at_ms` instead).
                let now = now_epoch_ms();
                chip.spawned_at_ms = Some(now);
                chip.note_event_at(now);
                model.clock_ms = model.clock_ms.max(now);
                match parent.and_then(|agent| crate::app::find_chip_mut(&mut model.chips, &agent)) {
                    Some(parent_chip) => parent_chip.children.push(chip),
                    None => model.chips.push(chip),
                }
                model.dirty = true;
            }
            DemoEvent::ChipState { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.set_state_at(state, now_epoch_ms());
                    model.dirty = true;
                }
            }
            DemoEvent::ChipEmit { agent, payload } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.note_event_at(now_epoch_ms());
                    chip.transcript.apply(&payload);
                    model.dirty = true;
                }
            }
            DemoEvent::ChipNote { agent, text } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.note_event_at(now_epoch_ms());
                    chip.transcript.push_note(text);
                    model.dirty = true;
                }
            }
            DemoEvent::ChipTokens { agent, n } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.note_event_at(now_epoch_ms());
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
                    chip.set_state_at(
                        if recovery {
                            crate::script::ChipDisplayState::Error
                        } else {
                            crate::script::ChipDisplayState::InputRequired
                        },
                        now_epoch_ms(),
                    );
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
                    chip.set_state_at(state, now_epoch_ms());
                    model.dirty = true;
                }
            }
            DemoEvent::ChipQuestionClear { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut model.chips, &agent) {
                    chip.question = None;
                    chip.set_state_at(state, now_epoch_ms());
                    model.dirty = true;
                }
            }
            DemoEvent::ChipCloseReq { agent } => {
                let active = model.ui_generation();
                self.close_chip(model, active, &agent);
            }
            DemoEvent::ChipRemove { agent } => {
                if crate::app::remove_chip(&mut model.chips, &agent) {
                    if model.view_path.contains(&agent) {
                        model.view_path.clear();
                        if model.screen == crate::app::Screen::Subagent {
                            // TUI6.2c finding 6: through the switch
                            // authority — the driver held the fifth
                            // unenumerated direct screen write
                            // (same-key-safe today, the exact recurrence
                            // shape fix 3 exists to prevent).
                            model.switch_surface(crate::app::Screen::Session);
                        }
                    }
                    model.dirty = true;
                }
            }
            DemoEvent::AutoResume => {
                let active = model.ui_generation();
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
    pub fn finish_turn(&mut self, model: &mut AppModel, session: UiGeneration) {
        // The end-of-turn law runs for the OWNING session — attached, or a
        // background slot whose queue consumes just the same (the sim's
        // finishTurn is per-session, tui.js:1507-1543).
        let attached = model.ui_generation() == session;
        let queued = if attached {
            if model.msg_queue.is_empty() {
                None
            } else {
                model.dirty = true;
                Some(model.msg_queue.remove(0))
            }
        } else {
            model
                .session_entry_by_generation(session)
                .and_then(|entry| {
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

/// Apply one DEMO event to a NON-attached session's slot — the demo half
/// of background routing (W3c3, report R11 cut 3).
///
/// This lived on `SessionState` until W3c3. It moved here because
/// `DemoEvent`'s chip variants are DEMO VOCABULARY: live subagent state
/// arrives as `AgentSpawned`/`AgentChipState`/`AgentReport` envelopes
/// through `SessionState::absorb_raw`, and a common session type that still
/// spoke `DemoEvent` would have kept the demo inside the layer the swap
/// must outlive. `SessionState` now knows only envelopes; this function is
/// reachable only from `DemoDriver`.
///
/// It is the state-mutating half of the driver's active-session `consume`
/// arms, against THIS session's fields. The active path's screen flips,
/// menu-selection resets and view-path edits are attached-surface concerns
/// and deliberately have no counterpart here; everything that is SESSION
/// state stays law-identical with the active arms.
pub fn absorb_demo_event(state: &mut crate::session::SessionState, event: DemoEvent) {
    use crate::app::{ChipModel, ChipQuestion, find_chip_mut, remove_chip};
    match event {
        DemoEvent::Envelope(payload) => state.absorb_envelope(&payload),
        DemoEvent::Note(text) => state.projection.push_note(text),
        DemoEvent::Voice(on) => state.projection.set_voice_live(on),
        DemoEvent::ChipAdd(seed) => {
            let parent = seed.parent.clone();
            let mut chip = ChipModel::from_seed(*seed);
            // S4: law-identical with the active arm — the demo's spawn
            // instant is the wall clock it fabricates with.
            let now = now_epoch_ms();
            chip.spawned_at_ms = Some(now);
            chip.note_event_at(now);
            match parent.and_then(|agent| find_chip_mut(&mut state.chips, &agent)) {
                Some(parent_chip) => parent_chip.children.push(chip),
                None => state.chips.push(chip),
            }
        }
        DemoEvent::ChipState { agent, state: next } => {
            if let Some(chip) = find_chip_mut(&mut state.chips, &agent) {
                chip.set_state_at(next, now_epoch_ms());
            }
        }
        DemoEvent::ChipEmit { agent, payload } => {
            if let Some(chip) = find_chip_mut(&mut state.chips, &agent) {
                chip.note_event_at(now_epoch_ms());
                chip.transcript.apply(&payload);
            }
        }
        DemoEvent::ChipNote { agent, text } => {
            if let Some(chip) = find_chip_mut(&mut state.chips, &agent) {
                chip.note_event_at(now_epoch_ms());
                chip.transcript.push_note(text);
            }
        }
        DemoEvent::ChipTokens { agent, n } => {
            if let Some(chip) = find_chip_mut(&mut state.chips, &agent) {
                chip.note_event_at(now_epoch_ms());
                chip.tokens = chip.tokens.saturating_add(n);
            }
        }
        DemoEvent::ChipQuestion {
            agent,
            recovery,
            text,
            options,
        } => {
            if let Some(chip) = find_chip_mut(&mut state.chips, &agent) {
                // Atomic with the state, exactly as the active arm.
                chip.set_state_at(
                    if recovery {
                        crate::script::ChipDisplayState::Error
                    } else {
                        crate::script::ChipDisplayState::InputRequired
                    },
                    now_epoch_ms(),
                );
                chip.question = Some(ChipQuestion {
                    recovery,
                    text,
                    options,
                    resolved: false,
                });
            }
        }
        DemoEvent::ChipResolve { agent, state: next } => {
            if let Some(chip) = find_chip_mut(&mut state.chips, &agent) {
                if let Some(question) = &mut chip.question {
                    question.resolved = true;
                }
                chip.set_state_at(next, now_epoch_ms());
            }
        }
        DemoEvent::ChipQuestionClear { agent, state: next } => {
            if let Some(chip) = find_chip_mut(&mut state.chips, &agent) {
                chip.question = None;
                chip.set_state_at(next, now_epoch_ms());
            }
        }
        DemoEvent::ChipRemove { agent } => {
            let _ = remove_chip(&mut state.chips, &agent);
        }
        // Driver-owned events (Dispatch, TurnEnd, AutoResume, Answer,
        // AutoTitle, chip close lifecycle, aura, talk) are routed by
        // `consume` itself — they spawn scripts or touch surfaces this
        // function does not own.
        _ => {}
    }
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
    fn meter(&self, session: UiGeneration, apply: impl FnOnce(&mut (u64, u64))) -> (u64, u64) {
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
            .and_then(|owner| owner.generation())
            .unwrap_or(UiGeneration::SCRATCH);
        let usage_event = |(input, output): (u64, u64)| {
            DemoEvent::Envelope(EventPayload::Usage(Usage {
                input,
                output,
                reasoning: 0,
                cached: 0,
                source: UsageSource::Estimated,
                account: None,
                accounts: Vec::new(),
                normalized: None,
                scope: None,
                cache_cost: None,
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
///
/// KNOWN, DOCUMENTED TRANSIENT (TUI6.2 fix 7, review r2 finding 7): this
/// scratch render bumps the model's geometry epoch like any render, so a
/// click already queued against the LIVE frame's hit map can arrive
/// wearing the pre-bump stamp and be dropped by the epoch gate until the
/// pending redraw re-stamps the map. That is the gate failing CLOSED —
/// the click is discarded, never mapped through stale geometry — and the
/// copy path only runs on selection release, so the window is one
/// frame's worth of queued clicks at worst. Accepted; no mechanism
/// change (weakening the bump would reopen the r1 stale-consumption
/// class this gate exists to kill).
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
/// The browser hop's effects, opener-injected so tests can pin BOTH
/// outcomes without spawning anything.
///
/// Success stays quiet — the OAuth card already narrates ("your browser
/// opened …"). Failure must not strand the user behind copy that claims a
/// browser opened: the URL lands on the clipboard and the flash says so.
/// The `/attach` read effect (B4b + G2) — the ONE filesystem touch in the
/// attach pipeline, shell-owned like every other IO. The shared client
/// loader handles images, `.pdf` page-tree admission, then UTF-8 files in
/// the same order as `haider run --attach`. Failure is a typed card plus flash
/// and NOTHING uploads; success chips the draft and issues the
/// receipt-free upload through the model's one issuance seam.
pub fn attach_read_effects(model: &mut AppModel, path: &str) {
    let loaded = haider_client::load_attachment(std::path::Path::new(path));
    match loaded {
        Ok(haider_client::HeadlessAttachment::Image(image)) => {
            let name = std::path::Path::new(path).file_name().map_or_else(
                || path.to_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            let kib = image.bytes.len().div_ceil(1024);
            model.begin_attachment_upload(
                image.bytes,
                crate::composer::PendingKind::Image { mime: image.mime },
                format!("{name} · {kib} KB"),
            );
        }
        Ok(haider_client::HeadlessAttachment::File(file)) => {
            let label = format!("{} · {} lines", file.name, file.lines);
            model.begin_attachment_upload(
                file.bytes,
                crate::composer::PendingKind::File {
                    name: file.name,
                    lines: file.lines,
                },
                label,
            );
        }
        Ok(haider_client::HeadlessAttachment::Pdf(pdf)) => {
            let label = format!("{} · {}p", pdf.name, pdf.pages);
            model.begin_attachment_upload(
                pdf.bytes,
                crate::composer::PendingKind::Pdf {
                    name: pdf.name,
                    pages: pdf.pages,
                },
                label,
            );
        }
        Err(error) => {
            let (note, presentation) = match error {
                haider_client::HeadlessRunError::Attachment {
                    message,
                    presentation,
                    ..
                } => (message, Some(presentation)),
                other => (other.to_string(), None),
            };
            if let Some(presentation) = presentation {
                if let Some(session) = model.active_session.clone() {
                    model.record_session_error_card(&session, presentation.clone());
                }
                model.command_diagnostic = Some(presentation);
            }
            model.flash = Some(format!("· /attach — {note}"));
            model.dirty = true;
        }
    }
}

pub fn open_url_effects(
    model: &mut AppModel,
    url: &str,
    opener: &dyn Fn(&str) -> std::io::Result<()>,
) {
    if opener(url).is_ok() {
        return;
    }
    copy_selection_effects(model, url);
    model.flash = Some(
        "· couldn't open a browser — the sign-in link is on your clipboard ([1] retries)"
            .to_owned(),
    );
    model.dirty = true;
}

fn copy_selection_effects(model: &mut AppModel, text: &str) {
    let confirmed = crate::clipboard::copy_local(text);
    let mut out = stdout();
    let _ = out.write_all(crate::clipboard::osc52(text).as_bytes());
    let _ = out.flush();
    // Honest wording (review TUI4.1 P3-5): `· copied` only on a CONFIRMED
    // local copy (pbcopy exit 0). Otherwise the OSC 52 mirror already
    // went out — best-effort remains, and the flash says exactly that
    // instead of claiming a copy nobody verified.
    model.flash = Some(
        if confirmed {
            "· copied"
        } else {
            "· copy unconfirmed — sent via OSC 52 only"
        }
        .to_owned(),
    );
    model.dirty = true;
}

/// Run the demo headlessly: play the whole script through the model and
/// return the final plain rendering (the `--plain` path and the CI oracle).
#[must_use]
pub fn run_demo_plain(mut model: AppModel) -> String {
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let mut output = crate::plain::render_plain_with_cache(
        &model.projection,
        model.identity.context_window,
        // W-G: the plain surface mirrors the always-visible pill (the last
        // measured rate persists at rest), not the old streaming-only row.
        model.throughput_pill().as_ref(),
        &model.cache_usage,
    );
    output.push_str(&crate::plain::agent_metrics_plain(&model));
    output
}

/// Sleep until `deadline`, or forever when there is none.
///
/// A `select!` arm needs a future either way; `pending()` is the honest
/// "this driver has no deadline right now" (W3c3.1 r2, P2-B).
async fn wait_until(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// What one [`live_pass`] produced.
#[derive(Debug, Default)]
pub struct LivePass {
    /// RPCs the IO shell must issue, in order.
    pub commands: Vec<crate::live::LiveCommand>,
    /// The requests only the SHELL can perform — they need the terminal
    /// (a rendered selection) or the process (quit). Handed BACK rather
    /// than swallowed, so the pass stays free of IO and every other
    /// request is provably translated into a command.
    pub shell: Vec<ShellRequest>,
}

/// The CLOSED vocabulary of shell-owned effects. `run_live` must match
/// every variant — no `_` arm can exist over a type this small, which is
/// the structural fix for the browser-never-opened bug: `OpenUrl` reached
/// the executor as a full `AppRequest` and died in its catch-all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRequest {
    /// Run an immediate (user-requested) release check, bypassing the quiet
    /// startup rate limit. Its outcome re-enters as an update [`AppEvent`].
    CheckForUpdate,
    /// Run the existing atomic update transaction, then restart this TUI.
    /// The host executable owns the implementation because `haider-tui`
    /// deliberately has no dependency on the CLI update pipeline.
    RunUpdate,
    /// Copy the rendered selection (needs the terminal's frame).
    CopySelection,
    /// Copy model-known text (composer selection).
    CopyText(String),
    /// Open a URL in the user's browser (the OAuth authorize hop).
    OpenUrl(String),
    /// Reveal a durable image-created payload in the OS file explorer.
    RevealPath(String),
    /// Read + magic-sniff an `/attach` file (B4b — needs the filesystem).
    /// The outcome re-enters through [`attach_read_effects`]: an honest
    /// flash, or a chip + upload request on the model.
    AttachRead(String),
    /// T2: one talk effect for the stt supervisor (mic capture, engines,
    /// model downloads, config IO — all TUI-process, none wire). Outcomes
    /// re-enter as [`crate::talk::TalkEvent`]s on the talk channel.
    Talk(crate::talk::TalkShellCommand),
    /// End the process.
    Quit,
}

/// A fact produced by the executable that embeds the live TUI. Availability
/// and failures reduce as ordinary data; `Installed` is process control and
/// is acted on only by the runtime after the transaction has fully finished.
#[derive(Debug)]
pub enum LiveUpdateEvent {
    /// A CHECK outcome (startup or /update check). Never touches the
    /// install latch — an unrelated check must not cancel an installer
    /// (rev933b finding 4).
    App(AppEvent),
    /// An INSTALL transaction outcome other than success. Clears the
    /// latch and owns the dead-link exit semantics.
    Install(AppEvent),
    Installed,
}

/// Host-owned OTA effects injected into the live runtime. The callbacks only
/// start background work; their results return on `events`, so neither
/// discovery nor the update transaction can stall input/rendering.
pub struct LiveUpdateBridge {
    events: mpsc::UnboundedReceiver<LiveUpdateEvent>,
    check_now: Box<dyn Fn() + Send>,
    run_update: Box<dyn Fn() + Send>,
}

impl LiveUpdateBridge {
    #[must_use]
    pub fn new(
        events: mpsc::UnboundedReceiver<LiveUpdateEvent>,
        check_now: impl Fn() + Send + 'static,
        run_update: impl Fn() + Send + 'static,
    ) -> Self {
        Self {
            events,
            check_now: Box::new(check_now),
            run_update: Box::new(run_update),
        }
    }
}

/// Why the live terminal loop returned successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveExit {
    Quit,
    /// The CLI+daemon pair is committed and healthy; restart the process.
    UpdateInstalled,
}

/// ONE PASS of the live loop's tail — the ordering that makes live mode
/// correct, in one function that `run_live` and the tests both call.
///
/// This exists because of what its absence cost. The loop tail used to be
/// inline in [`run_live`], so nothing executed it: the driver tests
/// re-typed the tail by hand, the re-typed copy omitted `sync_selection`,
/// and a double attach on every gap / `Lagged` / reconnect stayed green
/// through an adversarial review (W3c3 review r1, P1-3 == D1-1). A test
/// that copies the loop cannot pin the loop.
///
/// The order is the law:
///
/// 1. **stamp the clock**, so every deadline in this pass is a pure
///    function of the value handed in (tests move time, never sleep);
/// 2. **expire elapsed deadlines** — BEFORE the reply applies (TUI6.5,
///    review r5): a stage at its deadline is dead before it can mint;
///    expiry wins the tie by construction;
/// 3. **reduce the inbound reply** — model mutations before the request
///    drain, because the requests they raise are drained in step 4;
/// 4. **drain the reducer's requests** into commands, handing back the
///    shell-owned ones;
/// 5. **`sync_selection`** — attach-on-selection (R11 cut 4): the launcher
///    lists cold sessions, and opening one is when its history is wanted;
/// 6. **`drain_answers`** — menu answers at their committed coordinates.
pub fn live_pass(
    driver: &mut crate::live::LiveDriver,
    model: &mut AppModel,
    reply: Option<crate::live::LiveReply>,
    now: std::time::Instant,
) -> LivePass {
    driver.set_now(now);
    // TUI6.5 (review r5's same-pass boundary): deadlines expire BEFORE
    // the inbound reply applies — a stage at or past its deadline must be
    // DEAD before its reply can mint. The old order let a late Staged
    // mint LoginApi and only then expired the internal state, leaving the
    // already-returned command untouched. Expiry-wins-the-tie is the law:
    // a reply racing its own deadline in one pass mints nothing.
    driver.expire_login(model);
    let mut commands = Vec::new();
    commands.extend(driver.busy_retries_due());
    // The OAuth poll sweep (W5e-1): same clock as the login deadline.
    commands.extend(driver.oauth_poll());
    if let Some(reply) = reply {
        commands.extend(driver.apply(model, reply));
    }
    let mut shell = Vec::new();
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    for request in requests {
        match request {
            AppRequest::CheckForUpdate => shell.push(ShellRequest::CheckForUpdate),
            AppRequest::RunUpdate => shell.push(ShellRequest::RunUpdate),
            AppRequest::CopySelection => shell.push(ShellRequest::CopySelection),
            AppRequest::CopyText(text) => shell.push(ShellRequest::CopyText(text)),
            AppRequest::OpenUrl { url } => shell.push(ShellRequest::OpenUrl(url)),
            AppRequest::RevealPath { path } => shell.push(ShellRequest::RevealPath(path)),
            AppRequest::AttachRead { path } => shell.push(ShellRequest::AttachRead(path)),
            AppRequest::TalkShell(command) => shell.push(ShellRequest::Talk(command)),
            AppRequest::Quit => shell.push(ShellRequest::Quit),
            request => commands.extend(driver.handle_request(model, request)),
        }
    }
    // Demo requests are structurally unreachable here (report R11 cut 3):
    // `/reset` takes its live branch and emits none. Clearing is
    // belt-and-braces, never an execution.
    model.demo_requests.clear();
    commands.extend(driver.sync_selection(model));
    commands.extend(driver.drain_answers(model));
    LivePass { commands, shell }
}

/// Run the LIVE TUI against a real daemon (W3c3 M2 — the swap).
///
/// Structurally the twin of [`run_demo`]: same terminal guard, same input
/// pump, same frame/animation ticks, same request drain, same draw. The
/// ONLY differences are the event source (a [`crate::link::Link`] instead
/// of the demo channel), the driver ([`LiveDriver`] instead of
/// [`DemoDriver`]) and the absence of demo persistence — the daemon's store
/// is the real one, and `run_live` never touches the demo state file.
///
/// The model enters in [`crate::app::RuntimeMode::Live`], so every reducer
/// branch that would otherwise FABRICATE local session state takes its live
/// side instead — [`crate::app::RuntimeMode`] enumerates them exhaustively,
/// and they all read the one predicate `fabricates_locally`.
///
/// The loop TAIL — the ordering that makes live mode correct — lives in
/// [`live_pass`], which this function and the tests both call. See its
/// charter for why.
pub async fn run_live(
    mut model: AppModel,
    client: haider_client::RpcClient,
    profile: haider_client::ResolvedProfile,
    config: haider_client::ClientConfig,
    mut updates: LiveUpdateBridge,
) -> std::io::Result<LiveExit> {
    use crate::live::LiveDriver;

    model.mode = crate::app::RuntimeMode::Live;
    let instance = if config.client_instance_id.is_empty() {
        format!("haider-tui-{}", std::process::id())
    } else {
        config.client_instance_id.clone()
    };
    let mut driver = LiveDriver::new(instance);
    // T2: the talk supervisor needs the profile store dir (config home)
    // before the profile moves into the link.
    let store_dir = profile.store_dir.clone();
    let mut link = crate::link::Link::start(client, profile, config);
    // W5e-1b: what this daemon actually serves gates the UI's affordances,
    // so a stale daemon shows an honest note instead of a failed request.
    model.daemon_features = link.daemon_features.clone();
    model.daemon_version = Some(link.daemon_version.clone());
    // T2: load the profile's transcription section ONCE at boot (shell-
    // owned IO — the reducer only ever sees the data). A present-but-
    // corrupt section is a typed error `/talk` surfaces honestly.
    match haider_stt::config::load(&store_dir) {
        Ok(talk_config) => model.talk_config = talk_config,
        Err(error) => model.talk_config_error = Some(error.to_string()),
    }
    // The stt supervisor + its event channel: outcomes re-enter the loop
    // below and reduce through `handle_talk` — the same seam the tests
    // drive.
    let (talk_tx, mut talk_rx) = mpsc::unbounded_channel::<crate::talk::TalkEvent>();
    let talk_runtime = crate::stt_runtime::TalkRuntime::spawn(talk_tx, store_dir);

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    sync_terminal_bg(model.theme);
    sync_window_title(&model.window_title());
    let mut active_theme = model.theme;
    // Owner spec §3: the theme CHOICE is TUI-local display state — persist
    // every user COMMIT to the profile-dir settings file. Previews inside
    // the open picker move only the resolved theme, so they never write.
    let mut settings = crate::settings::SettingsStore::open_default();
    // W-C M2: seed the desktop-notification toggle from the persisted setting
    // (default on) so a prior `/notifications off` survives a restart, and
    // mirror it into the store so a later theme save never drops it.
    let notifications_on = settings
        .as_ref()
        .is_none_or(crate::settings::SettingsStore::load_notifications);
    model.set_notifications_enabled(notifications_on);
    if let Some(store) = settings.as_mut() {
        store.set_notifications(notifications_on);
    }
    // Model retention (owner 2026-08-15): the harness OPENS on the model the
    // user last selected — seed the identity pair from the persisted pick.
    // The next CreateSession mints from this pair (M9), and live daemon
    // replies stay the running authority afterwards.
    if let Some((provider, model_short)) = settings
        .as_ref()
        .and_then(crate::settings::SettingsStore::load_last_model)
    {
        model.identity.provider = provider.clone();
        model.identity.model_short = model_short.clone();
        model.identity_pinned = true;
        model.refresh_context_window();
        if let Some(store) = settings.as_mut() {
            store.set_last_model(Some((provider, model_short)));
        }
    }
    let mut seen_theme_commits = model.theme_commits;
    let mut seen_notification_commits = model.notification_commits;
    let mut seen_model_commits = model.model_commits;
    let mut active_title = model.window_title();

    // Graphics wordmark query — after raw mode, before the input pump (see the
    // run_demo note); None on non-graphics terminals falls back to `crate::mark`.
    *model.wordmark.borrow_mut() = crate::wordmark::Wordmark::detect();
    // Truecolor capability (see run_demo) — pinned once for render's shimmer.
    model.truecolor = truecolor_capable(&|name| std::env::var(name).ok());

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

    // Entering live mode boots on the ready Welcome (already negotiated by
    // the caller) and LISTS sessions. It attaches only on selection — a
    // launcher that eagerly attached to everything it can see would burn
    // the working set before the user chose anything.
    let mut pending: std::collections::VecDeque<crate::live::LiveCommand> =
        driver.boot().into_iter().collect();
    // Boot is over the moment the daemon is reachable: there is no harness
    // startup script to watch in live mode.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));

    let mut frame_tick = tokio::time::interval(Duration::from_millis(33));
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut anim_tick = tokio::time::interval(Duration::from_millis(ANIM_PHASE_MS));
    anim_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut hit_map: Vec<(ratatui::layout::Rect, crate::app::Hit)> = Vec::new();
    // The last pointer cell, for post-draw hover settling (W5g-7).
    let mut pointer: Option<(u16, u16)> = None;
    let mut update_events_open = true;
    let mut update_in_progress = false;
    // Finding 5 (rev933): the update transaction RESTARTS the daemon, and
    // the link's bounded recovery (~3s) can lose the race against the
    // updater's 30s health window. A link death while a transaction is in
    // flight is therefore expected — the transaction owns the outcome. The
    // grace deadline bounds the wait so a dead updater can never wedge the
    // terminal.
    let mut link_replies_open = true;
    let mut update_grace: Option<tokio::time::Instant> = None;
    // W-OSC: outer None = nothing announced yet, so the first tail pass
    // always announces (launcher or the --session target alike).
    let mut announced_session: Option<Option<String>> = None;
    // W-INP: the composer publishes itself as the session's volatile input
    // surface — one publish per actual change, revisions monotonic for the
    // life of this process.
    let mut published_input: Option<(haider_protocol::ids::SessionId, String)> = None;
    let mut input_revision: u64 = 0;

    while !model.should_quit {
        // Issue whatever the driver asked for. `try_send` keeps the UI loop
        // non-blocking: a full command channel means the link is saturated,
        // and the command stays queued for the next pass.
        while let Some(command) = pending.front().cloned() {
            match link.commands.try_send(command) {
                Ok(()) => {
                    pending.pop_front();
                }
                Err(mpsc::error::TrySendError::Full(_)) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    pending.clear();
                    break;
                }
            }
        }
        let mut inbound: Option<crate::live::LiveReply> = None;
        // The driver's own wakeup. Without it a deadline fires only when
        // something ELSE wakes the loop, which a quiet terminal facing a
        // wedged daemon never does (W3c3.1 r2, P2-B).
        let deadline = driver.next_deadline();
        tokio::select! {
            input = input_rx.recv() => match input {
                Some(event) => {
                    match &event {
                        // The pre-resize map describes a frame that no
                        // longer exists — a queued Moved must not re-arm a
                        // hover from dead geometry (W5g-7).
                        Event::Resize(..) => hit_map.clear(),
                        Event::Mouse(mouse) => pointer = Some((mouse.column, mouse.row)),
                        _ => {}
                    }
                    // rev933b finding 3: while the update transaction holds
                    // a dead link, a keystroke would type into a void and a
                    // submit would be silently lost across the restart.
                    // Hold key input honestly; ⌃C still reaches dispatch so
                    // the user can always leave.
                    let quit_key = matches!(
                        &event,
                        Event::Key(key)
                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                    );
                    if link_replies_open || quit_key || !matches!(event, Event::Key(_)) {
                        dispatch_input(&mut model, &hit_map, event);
                    } else {
                        model.flash =
                            Some("· updating — input held until the restart".to_owned());
                        model.dirty = true;
                    }
                }
                None => break,
            },
            reply = link.replies.recv(), if link_replies_open => match reply {
                Some(reply) => inbound = Some(reply),
                None if update_in_progress => {
                    link_replies_open = false;
                    update_grace = Some(
                        tokio::time::Instant::now() + std::time::Duration::from_secs(90),
                    );
                    model.flash =
                        Some("· updating — daemon restarting, holding on…".to_owned());
                    model.dirty = true;
                }
                None => return Err(std::io::Error::other(
                    "link supervisor stopped unexpectedly after bounded recovery",
                )),
            },
            () = async {
                match update_grace {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            }, if update_grace.is_some() => {
                return Err(std::io::Error::other(
                    "daemon connection lost during an update and no outcome arrived in time",
                ));
            }
            // T2: talk-runtime outcomes (envelopes mark dirty and ride the
            // guarded 30 fps frame tick below — the wave adds NO timer).
            // The supervisor outlives this loop (its handle is owned
            // here), so `None` is unreachable; guarded anyway.
            talk_event = talk_rx.recv() => {
                let Some(event) = talk_event else {
                    return Err(std::io::Error::other(
                        "talk supervisor stopped unexpectedly after bounded recovery",
                    ));
                };
                model.handle_talk(event);
            }
            update_event = updates.events.recv(), if update_events_open => {
                match update_event {
                    // Check outcomes are plain data — they never clear the
                    // install latch and never own the dead-link exit
                    // (rev933b finding 4).
                    Some(LiveUpdateEvent::App(event)) => model.handle(event),
                    Some(LiveUpdateEvent::Install(event)) => {
                        update_in_progress = false;
                        if !link_replies_open {
                            // The daemon link is already gone; a failed
                            // transaction leaves nothing to render from.
                            let detail = match &event {
                                AppEvent::UpdateFailed { message } => message.clone(),
                                _ => "update did not install".to_owned(),
                            };
                            return Err(std::io::Error::other(format!(
                                "daemon connection lost during a failed update: {detail}"
                            )));
                        }
                        model.handle(event);
                    }
                    Some(LiveUpdateEvent::Installed) => return Ok(LiveExit::UpdateInstalled),
                    None if !link_replies_open => {
                        return Err(std::io::Error::other(
                            "update worker stopped while the daemon link was down",
                        ));
                    }
                    None => update_events_open = false,
                }
            }
            _ = anim_tick.tick(), if model.animated() => {
                model.anim_phase = model.anim_phase.wrapping_add(1);
                // S4: the same tick is the live chips' elapsed clock — no
                // second timer, and a closed gate costs nothing (terminal
                // chips read frozen journal time, not this).
                model.clock_ms = model.clock_ms.max(now_epoch_ms());
                // W-G: the same clock samples throughput — so the rate keeps
                // breathing (and honestly dips) while a tool runs mid-turn or
                // deltas pause, with no timer of its own.
                model.note_throughput();
                model.dirty = true;
            }
            () = wait_until(deadline) => {}
            _ = frame_tick.tick(), if model.dirty => {
                hit_map = draw(&mut terminal, &model)?;
                model.dirty = false;
                // W5g-7: hover survives a redraw only while the pointer
                // still resolves to it (subsumes identity-vanish cleanup,
                // and kills a highlight whose target MOVED under a
                // stationary pointer).
                settle_hover_after_draw(&mut model, &hit_map, pointer);
            }
        }
        let pass = live_pass(&mut driver, &mut model, inbound, std::time::Instant::now());
        pending.extend(pass.commands);
        // Exhaustive over the CLOSED shell vocabulary — adding a variant
        // without an arm here is a compile error, never a silent drop.
        for request in pass.shell {
            match request {
                ShellRequest::CheckForUpdate => (updates.check_now)(),
                ShellRequest::RunUpdate if !update_in_progress => {
                    update_in_progress = true;
                    (updates.run_update)();
                }
                ShellRequest::RunUpdate => {
                    model.flash = Some("· update already in progress".to_owned());
                    model.dirty = true;
                }
                ShellRequest::CopySelection => {
                    if let Some(selection) = model.selection {
                        let size = terminal.size()?;
                        let text = rendered_selection_text(&model, size, &selection);
                        copy_selection_effects(&mut model, &text);
                    }
                }
                ShellRequest::CopyText(text) => copy_selection_effects(&mut model, &text),
                ShellRequest::OpenUrl(url) => {
                    open_url_effects(&mut model, &url, &crate::browser::open_url);
                }
                ShellRequest::RevealPath(path) => {
                    if crate::browser::reveal_path(&path).is_err() {
                        model.flash = Some(format!("· couldn't reveal image — {path}"));
                        model.dirty = true;
                    }
                }
                ShellRequest::AttachRead(path) => attach_read_effects(&mut model, &path),
                ShellRequest::Talk(command) => talk_runtime.execute(command),
                ShellRequest::Quit => model.should_quit = true,
            }
        }
        sync_theme_persistence(&model, &mut seen_theme_commits, &mut settings);
        // W-C M2: persist a toggle change, then flush any queued desktop
        // notifications as OSC 9 to the terminal (tty-gated inside).
        sync_notification_persistence(&model, &mut seen_notification_commits, &mut settings);
        sync_model_persistence(&model, &mut seen_model_commits, &mut settings);
        emit_notifications(&mut model);
        if model.theme != active_theme {
            active_theme = model.theme;
            sync_terminal_bg(active_theme);
        }
        let title = model.window_title();
        if title != active_title {
            active_title = title;
            sync_window_title(&active_title);
        }
        // W-OSC: announce the attached session to the embedding terminal
        // whenever the binding changes (attach, hop, back-to-launcher).
        let attached = match model.screen {
            crate::app::Screen::Boot | crate::app::Screen::Launcher => None,
            _ => model
                .active_session
                .as_ref()
                .map(|id| id.as_str().to_owned()),
        };
        if announced_session.as_ref() != Some(&attached) {
            sync_session_announce(attached.as_deref());
            announced_session = Some(attached);
        }
        // W-INP: mirror the composer into the daemon's volatile surface —
        // never journaled, never in prompts, keystroke-latency for an
        // embedding ADE. Publish only real changes; the daemon drops stale
        // revisions and reassigns ownership to the latest publisher.
        if model.daemon_serves(haider_rpc::FEATURE_INPUT_MIRROR_V1)
            && matches!(
                model.screen,
                crate::app::Screen::Session | crate::app::Screen::Subagent
            )
            && let Some(session) = model.active_session.clone()
        {
            let text = model.composer.text().to_owned();
            let current = (session.clone(), text.clone());
            if published_input.as_ref() != Some(&current) {
                input_revision = input_revision.saturating_add(1);
                pending.push_back(crate::live::LiveCommand::SurfacePublish {
                    session,
                    input: Some((text, input_revision)),
                    status: None,
                });
                published_input = Some(current);
            }
        }
    }
    Ok(LiveExit::Quit)
}
