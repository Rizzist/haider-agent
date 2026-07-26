//! The app model + reducer: one owner of all TUI state, driven by a single
//! event stream (research rec 3/6). Rendering reads this model; nothing else
//! mutates it. The reducer is pure enough to test headlessly.

use crate::commands::{CommandSpec, palette_matches};
use crate::mock::{SampleSession, sample_sessions};
use crate::projection::SessionProjection;
use crate::sanctum::SanctumTier;
use crate::theme::ThemeKey;
use haider_protocol::EventPayload;
use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_protocol::state::HarnessStatus;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which screen is showing (sim: boot | main | session).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Boot,
    Launcher,
    Session,
}

/// Side effects the reducer requests from the runtime (the reducer itself
/// never performs IO).
#[derive(Debug, Clone, PartialEq)]
pub enum AppRequest {
    /// Play the canned response turn for user-typed text.
    SubmitText(String),
    /// Attach a sample session: replay the classic scripted demo turn.
    AttachSample(usize),
    /// Stop any playing script (fresh session / reset).
    StopScripts,
    /// Quit the app.
    Quit,
}

/// A clickable region's action (hit-testing: render reports regions, the
/// runtime maps clicks back through [`AppModel::handle_hit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    AttachSample(usize),
    /// Aura / Accounts / Peers launcher rows (UI stubs).
    ExtraRow(u8),
    PaletteRow(usize),
    MenuOption(usize),
    BackChip,
    TalkChip,
    HelpHint,
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
    /// The launcher sat untouched long enough — play the classic demo.
    AutoPlay,
}

/// Identity shown in the status bar and launcher info line. Real values come
/// from config/accounts in later waves; the demo pins sim-parity defaults.
#[derive(Debug, Clone)]
pub struct IdentityLine {
    pub provider: String,
    pub model_short: String,
    pub account: String,
    pub device: String,
    /// Working directory shown in the session header (~-abbreviated).
    pub dir: String,
    /// Voice pipeline label (sim: voice ships on by default).
    pub voice: String,
    pub context_window: u64,
}

impl Default for IdentityLine {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_owned(),
            model_short: "fable-5".to_owned(),
            account: "none · /login".to_owned(),
            device: "this-mac".to_owned(),
            dir: "~".to_owned(),
            voice: "whisper→openai".to_owned(),
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
    /// Session title shown in the header — derived from the first user
    /// message until real session naming arrives (daemon wave).
    pub session_title: Option<String>,
    /// Head callsign for the live demo session (sim: claimed from roster).
    pub session_head: (&'static str, &'static str),
    /// Launcher sample rows (sim seeds; display-only until the daemon).
    pub samples: Vec<SampleSession>,
    /// Selected option index while a blocking menu replaces the composer.
    pub menu_selection: usize,
    /// Selected row in the slash palette (open while composer starts with /).
    pub palette_selection: usize,
    /// The /help overlay (esc closes).
    pub help_open: bool,
    /// One-line transient notice shown in the status bar until the next
    /// keystroke (honest stubs: "/tree lands with the daemon").
    pub flash: Option<String>,
    /// Answers the user produced; the runtime drains these to the client
    /// (side effects never happen inside the reducer).
    pub outbox: Vec<MenuAnswer>,
    /// Reducer-requested side effects; the runtime drains these.
    pub requests: Vec<AppRequest>,
    /// True while a demo turn is playing (submits are ignored, honestly).
    pub turn_active: bool,
    /// The launcher auto-play fired or was cancelled by interaction.
    pub auto_play_spent: bool,
    /// Wheel scroll-back offset in the session transcript (0 = follow
    /// bottom; wheel up increases, wheel down decreases).
    pub scroll_back: u16,
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
            session_title: None,
            session_head: ("Hasan", "(a)"),
            samples: sample_sessions(),
            menu_selection: 0,
            palette_selection: 0,
            help_open: false,
            flash: None,
            outbox: Vec::new(),
            requests: Vec::new(),
            turn_active: false,
            auto_play_spent: false,
            scroll_back: 0,
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

    /// The terminal window title for the current screen (OSC 2).
    #[must_use]
    pub fn window_title(&self) -> String {
        match self.screen {
            Screen::Boot => "haider — starting".to_owned(),
            Screen::Launcher => "haider — launcher".to_owned(),
            Screen::Session => {
                // Strip control characters: user text must never smuggle
                // escape sequences into OSC 2 (review r1 P1).
                let title: String = self
                    .session_title
                    .as_deref()
                    .unwrap_or("session")
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                format!("haider — {title} · {}", self.identity.device)
            }
        }
    }

    /// The palette is open while the composer is a slash query and no
    /// blocking menu owns the input.
    #[must_use]
    pub fn palette_open(&self) -> bool {
        self.composer.starts_with('/')
            && !self.help_open
            && !(self.screen == Screen::Session && self.projection.open_menu().is_some())
    }

    /// Current palette matches for rendering and completion.
    #[must_use]
    pub fn palette_items(&self) -> Vec<&'static CommandSpec> {
        palette_matches(
            self.composer.trim_start_matches('/'),
            self.screen == Screen::Session,
        )
    }

    /// Reduce one event into the model. Returns nothing; render reads state,
    /// the runtime drains [`Self::outbox`] and [`Self::requests`].
    /// `StreamEnded` is a no-op and must NOT dirty the frame (r1 P1).
    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => {
                self.dirty = true;
                self.flash = None;
                self.auto_play_spent = true;
                self.handle_key(key);
            }
            AppEvent::Paste(text) => {
                self.dirty = true;
                self.auto_play_spent = true;
                // While a blocking menu replaces the composer, paste has no
                // target (r2 P2).
                if self.projection.open_menu().is_some() && self.screen == Screen::Session {
                    return;
                }
                // Paste is atomic text; pasted newlines become spaces so they
                // can never trigger submit (rec 14; multi-line lands later).
                let normalized = text.replace("\r\n", "\n").replace(['\r', '\n'], " ");
                self.composer.push_str(&normalized);
            }
            AppEvent::Envelope(payload) => {
                self.dirty = true;
                self.handle_envelope(&payload);
            }
            AppEvent::AutoPlay => {
                if !self.auto_play_spent && self.screen == Screen::Launcher && !self.turn_active {
                    self.auto_play_spent = true;
                    self.dirty = true;
                    self.attach_sample(0);
                }
            }
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
        // Boot renders no composer — hidden input must not accumulate or
        // start turns (review r1 P2).
        if self.screen == Screen::Boot {
            return;
        }
        if self.help_open {
            // esc/enter/q close help; everything else is swallowed.
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.help_open = false;
            }
            return;
        }
        // A blocking menu REPLACES the composer (sim §3 law).
        if self.projection.open_menu().is_some() && self.screen == Screen::Session {
            self.handle_menu_key(key.code);
            return;
        }
        if self.palette_open() {
            match key.code {
                KeyCode::Up => {
                    self.palette_selection = self.palette_selection.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    // Only 8 rows render; selection stays visible (r1 P2).
                    let count = self.palette_items().len().min(8);
                    if count > 0 {
                        self.palette_selection = (self.palette_selection + 1).min(count - 1);
                    }
                    return;
                }
                KeyCode::Tab => {
                    if let Some(spec) = self.palette_items().get(self.palette_selection) {
                        self.composer = format!("/{} ", spec.name);
                    }
                    return;
                }
                KeyCode::Esc => {
                    self.composer.clear();
                    self.palette_selection = 0;
                    return;
                }
                KeyCode::Enter => {
                    // Enter runs the HIGHLIGHTED command with any typed args
                    // (r1 P2: /t + Down + Enter must run the selection).
                    if let Some(spec) = self.palette_items().get(self.palette_selection) {
                        let args: String = self
                            .composer
                            .trim_start_matches('/')
                            .split_whitespace()
                            .skip(1)
                            .collect::<Vec<_>>()
                            .join(" ");
                        self.composer = if args.is_empty() {
                            format!("/{}", spec.name)
                        } else {
                            format!("/{} {args}", spec.name)
                        };
                    }
                    self.execute_slash();
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc if self.screen == Screen::Session => {
                self.screen = Screen::Launcher;
            }
            KeyCode::Enter => self.submit_composer(),
            KeyCode::Backspace => {
                self.composer.pop();
                self.palette_selection = 0;
            }
            KeyCode::Char(c @ '1'..='3')
                if self.screen == Screen::Launcher && self.composer.is_empty() =>
            {
                let index = (c as usize) - ('1' as usize);
                self.attach_sample(index);
            }
            KeyCode::Char(c) => {
                self.composer.push(c);
                self.palette_selection = 0;
            }
            _ => {}
        }
    }

    fn submit_composer(&mut self) {
        let text = self.composer.trim().to_owned();
        self.composer.clear();
        self.palette_selection = 0;
        if text.is_empty() {
            if self.screen == Screen::Launcher && !self.projection.entries().is_empty() {
                self.screen = Screen::Session;
            }
            return;
        }
        if text.starts_with('/') {
            self.composer = text;
            self.execute_slash();
            return;
        }
        if self.turn_active {
            self.flash =
                Some("· a turn is already running — steer lands with the daemon".to_owned());
            return;
        }
        // Typing on the LAUNCHER starts a FRESH session (sim promise);
        // typing inside a session continues it.
        if self.screen == Screen::Launcher {
            self.fresh_session();
        }
        if self.session_title.is_none() {
            let mut title: String = text.chars().take(38).collect();
            if text.chars().count() > 38 {
                title.push('…');
            }
            self.session_title = Some(title);
        }
        self.screen = Screen::Session;
        self.turn_active = true;
        self.scroll_back = 0;
        self.requests.push(AppRequest::SubmitText(text));
    }

    fn execute_slash(&mut self) {
        let raw = self.composer.trim_start_matches('/').trim().to_owned();
        self.composer.clear();
        self.palette_selection = 0;
        let mut words = raw.split_whitespace();
        let name = words.next().unwrap_or("").to_ascii_lowercase();
        let arg = words.next().map(str::to_ascii_lowercase);
        match name.as_str() {
            "help" => self.help_open = true,
            "theme" => match arg.as_deref() {
                Some(name) => match ThemeKey::parse(name) {
                    Some(key) => {
                        self.theme = key;
                        self.flash = Some(format!("· theme → {}", key.theme().label));
                    }
                    None => {
                        self.flash =
                            Some(format!("· unknown theme “{name}” — dawn · ivory · dark"));
                    }
                },
                None => self.cycle_theme(),
            },
            "clear" | "back" => {
                // /clear promises a fresh start (review r1 P2).
                self.fresh_session();
                self.screen = Screen::Launcher;
                self.requests.push(AppRequest::StopScripts);
            }
            "reset" => {
                self.fresh_session();
                self.samples = sample_sessions();
                self.screen = Screen::Launcher;
                self.flash = Some("· demo reset".to_owned());
                self.requests.push(AppRequest::StopScripts);
            }
            "quit" | "exit" => self.requests.push(AppRequest::Quit),
            "" => {}
            other => {
                // Known stubs name their wave; typos say so (review r1 P2).
                let wave = match other {
                    "model" | "provider" | "login" | "account" | "accounts" => {
                        Some("the account switchboard (W3)")
                    }
                    "sessions" | "tree" | "fork" | "rename" | "compact" | "tokens" | "queue" => {
                        Some("the daemon wave (W3)")
                    }
                    "voice" | "say" | "aura" => Some("the voice wave (post-v0.1)"),
                    "peers" => Some("the mesh wave (post-v0.1)"),
                    "hooks" | "update" | "tools" => Some("the gates wave (W4)"),
                    _ => None,
                };
                self.flash = Some(match wave {
                    Some(wave) => format!("· /{other} — UI ready; lands with {wave}"),
                    None => format!("· unknown command /{other} — /help lists commands"),
                });
            }
        }
    }

    fn handle_menu_key(&mut self, code: KeyCode) {
        let Some(menu) = self.projection.open_menu() else {
            return;
        };
        let option_count = menu.options.len();
        match code {
            KeyCode::Up => {
                self.menu_selection = self.menu_selection.saturating_sub(1);
            }
            KeyCode::Down if option_count > 0 => {
                self.menu_selection = (self.menu_selection + 1).min(option_count - 1);
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < option_count {
                    self.menu_selection = index;
                    self.submit_menu_answer();
                }
            }
            KeyCode::Enter => self.submit_menu_answer(),
            _ => {}
        }
    }

    fn submit_menu_answer(&mut self) {
        let Some(menu) = self.projection.open_menu() else {
            return;
        };
        let Some(option) = menu.options.get(self.menu_selection) else {
            return;
        };
        let answer = MenuAnswer {
            menu: menu.id.clone(),
            option_key: Some(option.key.clone()),
            option_index: u32::try_from(self.menu_selection).unwrap_or(u32::MAX),
            value: None,
            via: AnswerVia::Tui,
        };
        self.outbox.push(answer);
        self.menu_selection = 0;
    }

    fn handle_envelope(&mut self, payload: &EventPayload) {
        // Screen auto-transitions (sim: boot → launcher when startup
        // completes; the first user message attaches the session view).
        if matches!(payload, EventPayload::HarnessStatus(HarnessStatus::Ready))
            && self.screen == Screen::Boot
        {
            self.screen = Screen::Launcher;
        }
        if let EventPayload::UserMessage { text, .. } = payload {
            self.screen = Screen::Session;
            self.turn_active = true;
            if self.session_title.is_none() {
                let mut title: String = text.chars().take(38).collect();
                if text.chars().count() > 38 {
                    title.push('…');
                }
                self.session_title = Some(title);
            }
        }
        if let EventPayload::RunState(state) = payload
            && state.is_terminal()
        {
            self.turn_active = false;
        }
        if matches!(payload, EventPayload::MenuOpened(_)) {
            self.menu_selection = 0;
        }
        self.projection.apply(payload);
    }

    /// Start-fresh semantics (review r1 P2): a new session begins from an
    /// empty projection; the previous demo transcript does not leak in.
    fn fresh_session(&mut self) {
        self.projection = SessionProjection::new();
        self.session_title = None;
        self.turn_active = false;
        self.scroll_back = 0;
    }

    /// Attach a sample session (digit or click) — one turn at a time.
    fn attach_sample(&mut self, index: usize) {
        if self.turn_active {
            self.flash = Some("· a turn is already running".to_owned());
            return;
        }
        if let Some(sample) = self.samples.get(index) {
            let (blurb, head, honorific, name) =
                (sample.blurb, sample.head, sample.honorific, sample.name);
            self.fresh_session();
            self.session_title = Some(blurb.to_owned());
            self.session_head = (head, honorific);
            self.flash = Some(format!("· attached {name} — demo replay"));
            self.turn_active = true;
            self.requests.push(AppRequest::AttachSample(index));
        }
    }

    /// A left-click resolved through the frame's hit map.
    pub fn handle_hit(&mut self, hit: Hit) {
        self.dirty = true;
        self.flash = None;
        self.auto_play_spent = true;
        match hit {
            Hit::AttachSample(index) => self.attach_sample(index),
            Hit::ExtraRow(which) => {
                let (name, wave) = match which {
                    0 => ("aura", "the voice wave (post-v0.1)"),
                    1 => ("accounts", "the account switchboard (W3)"),
                    _ => ("peers", "the mesh wave (post-v0.1)"),
                };
                self.flash = Some(format!("· /{name} — UI ready; lands with {wave}"));
            }
            Hit::PaletteRow(index) => {
                if index < self.palette_items().len() {
                    self.palette_selection = index;
                    self.execute_slash();
                }
            }
            Hit::MenuOption(index) => {
                let count = self.projection.open_menu().map_or(0, |m| m.options.len());
                if index < count {
                    self.menu_selection = index;
                    self.submit_menu_answer();
                }
            }
            Hit::BackChip => {
                if self.screen == Screen::Session {
                    self.screen = Screen::Launcher;
                }
            }
            Hit::TalkChip => {
                self.flash =
                    Some("· ◉ talk — voice lands post-v0.1; the chip is the promise".to_owned());
            }
            Hit::HelpHint => self.help_open = true,
        }
    }

    /// Wheel scroll in the session transcript (⇧-drag selects text natively).
    pub fn handle_wheel(&mut self, up: bool) {
        if self.screen != Screen::Session {
            return;
        }
        self.dirty = true;
        if up {
            self.scroll_back = self.scroll_back.saturating_add(3);
        } else {
            self.scroll_back = self.scroll_back.saturating_sub(3);
        }
    }

    fn cycle_theme(&mut self) {
        let keys = ThemeKey::ALL;
        let index = keys.iter().position(|k| *k == self.theme).unwrap_or(0);
        self.theme = keys[(index + 1) % keys.len()];
        self.flash = Some(format!("· theme → {}", self.theme.theme().label));
    }
}
