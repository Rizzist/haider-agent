//! The app model + reducer: one owner of all TUI state, driven by a single
//! event stream (research rec 3/6). Rendering reads this model; nothing else
//! mutates it. The reducer is pure enough to test headlessly.

use crate::projection::SessionProjection;
use crate::sanctum::SanctumTier;
use crate::theme::ThemeKey;
use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which screen is showing (sim: boot | main | session).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Boot,
    Launcher,
    Session,
}

/// Everything the reducer consumes.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    /// Bracketed paste arrives atomically; newlines never submit (rec 14).
    Paste(String),
    /// Boxed: `EventPayload` is much larger than the other variants.
    Envelope(Box<EventPayload>),
    /// The demo script (or stream) ended.
    StreamEnded,
}

/// Identity shown in the status bar and launcher info line. Real values come
/// from config/accounts in later waves; the demo pins sim-parity defaults.
#[derive(Debug, Clone)]
pub struct IdentityLine {
    pub provider: String,
    pub model_short: String,
    pub account: String,
    pub device: String,
    pub context_window: u64,
}

impl Default for IdentityLine {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_owned(),
            model_short: "claude-fable-5".to_owned(),
            account: "none · /login".to_owned(),
            device: "this-mac".to_owned(),
            context_window: 200_000,
        }
    }
}

/// The single mutable application state (research rec 3).
#[derive(Debug)]
pub struct AppModel {
    pub screen: Screen,
    pub theme: ThemeKey,
    pub sanctum_tier: SanctumTier,
    pub projection: SessionProjection,
    pub identity: IdentityLine,
    pub composer: String,
    pub should_quit: bool,
    /// Set by every state change; cleared when a frame is drawn (rec 6).
    pub dirty: bool,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            screen: Screen::Boot,
            theme: ThemeKey::Dawn,
            sanctum_tier: SanctumTier::default(),
            projection: SessionProjection::new(),
            identity: IdentityLine::default(),
            composer: String::new(),
            should_quit: false,
            dirty: true,
        }
    }
}

impl AppModel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reduce one event into the model. Returns nothing; render reads state.
    pub fn handle(&mut self, event: AppEvent) {
        self.dirty = true;
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Paste(text) => {
                // Paste is atomic text; pasted newlines become spaces so they
                // can never trigger submit (rec 14; multi-line lands later).
                let normalized = text.replace("\r\n", "\n").replace(['\r', '\n'], " ");
                self.composer.push_str(&normalized);
            }
            AppEvent::Envelope(payload) => self.handle_envelope(&payload),
            AppEvent::StreamEnded => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => self.should_quit = true,
                // Ctrl+T cycles the theme (demo stand-in for /theme).
                KeyCode::Char('t') => self.cycle_theme(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc if self.screen == Screen::Session => {
                self.screen = Screen::Launcher;
            }
            KeyCode::Enter => {
                if self.screen == Screen::Launcher && !self.projection.entries().is_empty() {
                    self.screen = Screen::Session;
                }
                // Submitting a real message is daemon-wave work; the demo's
                // turn is script-driven.
                self.composer.clear();
            }
            KeyCode::Backspace => {
                self.composer.pop();
            }
            KeyCode::Char(c) => self.composer.push(c),
            _ => {}
        }
    }

    fn handle_envelope(&mut self, payload: &EventPayload) {
        // Screen auto-transitions (sim: boot → launcher when startup
        // completes; the first user message attaches the session view).
        if matches!(payload, EventPayload::HarnessStatus(HarnessStatus::Ready))
            && self.screen == Screen::Boot
        {
            self.screen = Screen::Launcher;
        }
        if matches!(payload, EventPayload::UserMessage { .. }) {
            self.screen = Screen::Session;
        }
        self.projection.apply(payload);
    }

    fn cycle_theme(&mut self) {
        let keys = ThemeKey::ALL;
        let index = keys.iter().position(|k| *k == self.theme).unwrap_or(0);
        self.theme = keys[(index + 1) % keys.len()];
    }
}
