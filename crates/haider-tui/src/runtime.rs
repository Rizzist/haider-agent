//! The interactive runtime: one task owns the terminal and the [`AppModel`];
//! input, envelopes and frame deadlines multiplex through `tokio::select!`
//! (research rec 3/6). Alternate screen for v0.1 (rec 1); native-scrollback
//! insertion is explicitly deferred (rec 19).

use crate::app::{AppEvent, AppModel};
use crate::mock::demo_script;
use crate::render::render;
use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{event, execute};
use std::io::{Stdout, Write, stdout};
use std::time::Duration;
use tokio::sync::mpsc;

/// RAII terminal state: raw mode + alternate screen + bracketed paste on
/// construction, restored on drop AND on panic (the panic hook restores
/// first so the report is readable — rec 18's restoration guarantee).
pub struct TerminalGuard;

impl TerminalGuard {
    /// Enter raw mode + alt screen. Registers a chaining panic hook that
    /// restores the terminal before the default report prints.
    pub fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen);
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

    // Demo driver: the script plays on its own clock.
    let (envelope_tx, mut envelope_rx) = mpsc::channel::<EventPayload>(64);
    tokio::spawn(async move {
        for payload in demo_script() {
            tokio::time::sleep(demo_pace(&payload)).await;
            if envelope_tx.send(payload).await.is_err() {
                return;
            }
        }
    });

    let mut frame_tick = tokio::time::interval(Duration::from_millis(33));
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    while !model.should_quit {
        tokio::select! {
            input = input_rx.recv() => match input {
                Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    model.handle(AppEvent::Key(key));
                }
                Some(Event::Paste(text)) => model.handle(AppEvent::Paste(text)),
                Some(Event::Resize(..)) => model.dirty = true,
                Some(_) => {}
                None => break,
            },
            payload = envelope_rx.recv() => match payload {
                Some(payload) => model.handle(AppEvent::Envelope(Box::new(payload))),
                None => model.handle(AppEvent::StreamEnded),
            },
            // Guarded tick: while the model is clean this branch is disabled,
            // so the idle loop takes NO periodic wakeups (efficiency rider
            // #10 — ~109k/hour otherwise). The first dirtying event re-arms
            // it and the overdue tick fires immediately, keeping the 30fps
            // coalescing behavior.
            _ = frame_tick.tick(), if model.dirty => {
                draw(&mut terminal, &model)?;
                model.dirty = false;
            }
        }
    }
    Ok(())
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    model: &AppModel,
) -> std::io::Result<()> {
    terminal.draw(|frame| render(model, frame))?;
    Ok(())
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
