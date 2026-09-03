//! The حيدر wordmark as a real graphics-protocol image.
//!
//! `crate::mark` renders حيدر as half-block pixel art — the universal fallback
//! that works in any emulator, needs no shaping, and never hangs. This module
//! is the UPGRADE: on a terminal that speaks a real graphics protocol
//! (Kitty / iTerm2 / Sixel) the same wordmark is drawn from a bundled PNG of
//! the Damascus calligraphy, at full pixel resolution instead of chunky blocks.
//!
//! The two paths share one footprint. The image is fit (aspect-preserved,
//! centered) into the SAME cell rectangle the half-block art would occupy
//! (banner 28×4, header 24×2), so the layout is identical — only the fidelity
//! of the mark changes.
//!
//! DIGNITY (sanctum rule 2, mirrored from `crate::mark`): whole or nothing.
//! When no real graphics protocol is present, [`Wordmark::detect`] returns
//! `None` and the caller draws the crafted half-block art, which reads cleaner
//! at header scale than a half-block DOWNSAMPLE of the image would.

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget as _;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

/// The bundled wordmark: حيدر in Damascus Naskh, Desert-Dawn gold, transparent
/// background so it composites over any terminal ground. Regenerate with
/// `assets/gen-wordmark.swift` (CoreText, which shapes Arabic natively).
const WORDMARK_PNG: &[u8] = include_bytes!("../assets/wordmark-haider.png");
const WORDMARK_IMAGE_SIZE: Size = Size::new(24, 2);

/// A terminal-graphics wordmark, ready to render into a reserved cell rect.
///
/// Held behind a `RefCell` in `AppModel` because the render pass takes
/// `&AppModel` while first-draw initialization and one-shot relief mutate it.
pub struct Wordmark {
    state: WordmarkState,
    kind: ProtocolType,
}

enum WordmarkState {
    /// Capability detection is complete, but the PNG has not been decoded and
    /// no terminal image buffer exists. Sessions which never actually draw
    /// the mark stay in this cheap state for their entire lifetime.
    Deferred(Picker),
    /// One persistent fixed-size protocol owns the encoded terminal payload.
    /// It contains no resizeable source, so constructing it once makes later
    /// renders allocation-free by type.
    Ready {
        protocol: Protocol,
        relief_pending: bool,
    },
    /// Decode is attempted once. A damaged embedded asset must not turn every
    /// frame into another failed allocation attempt.
    Failed,
}

impl std::fmt::Debug for Wordmark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never recurse into the protocol (it owns image bytes): AppModel's
        // derive(Debug) only needs a stable, cheap summary.
        f.debug_struct("Wordmark")
            .field("kind", &self.kind)
            .field("state", &self.state_label())
            .finish_non_exhaustive()
    }
}

impl Wordmark {
    /// Query the terminal for a graphics protocol and defer building the
    /// wordmark until a frame actually has a non-empty image rectangle, or
    /// return `None` to fall back to `crate::mark`.
    ///
    /// Returns `None` when the terminal offers only half-blocks (the crafted
    /// art is cleaner there) or on any decode/query failure.
    ///
    /// SAFE-BY-CONSTRUCTION against the non-answering-PTY hang this codebase
    /// guards elsewhere: `Picker::from_query_stdio` reads the capability
    /// response under an internal timeout and degrades to half-blocks on
    /// silence — it never blocks.
    ///
    /// TIMING: must run after raw mode is entered and BEFORE the input-pump
    /// thread starts, or the pump consumes the terminal's query response.
    ///
    /// v0.0.15 field bug (the swallowed-first-key probe): on a terminal that
    /// NEVER answers the query, the stdio reader's timeout path leaves the
    /// next stdin byte to be consumed and discarded — the user's first
    /// keypress after launch silently vanished (a leading `/` most visibly).
    /// The query is therefore gated on ENVIRONMENT EVIDENCE of a terminal
    /// that actually answers graphics-capability queries; everything else
    /// skips straight to the half-block art without ever touching stdin.
    #[must_use]
    pub fn detect() -> Option<Self> {
        if !graphics_terminal_likely(&|name| std::env::var(name).ok()) {
            return None;
        }
        Self::from_picker(Picker::from_query_stdio().ok()?)
    }

    /// Build deferred wordmark state from an already-configured picker.
    /// Useful for embedders that perform capability detection themselves.
    #[must_use]
    pub fn from_picker(picker: Picker) -> Option<Self> {
        let kind = picker.protocol_type();
        (!matches!(kind, ProtocolType::Halfblocks)).then_some(Self {
            state: WordmarkState::Deferred(picker),
            kind,
        })
    }

    /// The detected protocol (for diagnostics/tests).
    #[must_use]
    pub fn kind(&self) -> ProtocolType {
        self.kind
    }

    /// Whether the first real render has constructed the persistent protocol.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        matches!(&self.state, WordmarkState::Ready { .. })
    }

    /// Decode the embedded PNG and create the persistent protocol at most
    /// once. Render calls this itself; the outer renderer uses it before
    /// clearing the half-block fallback cells.
    pub(crate) fn prepare(&mut self) -> bool {
        if matches!(&self.state, WordmarkState::Deferred(_)) {
            let WordmarkState::Deferred(picker) =
                std::mem::replace(&mut self.state, WordmarkState::Failed)
            else {
                unreachable!("wordmark state was checked immediately before replacement");
            };
            let Ok(image) = image::load_from_memory(WORDMARK_PNG) else {
                return false;
            };
            let Ok(protocol) = picker.new_protocol(image, WORDMARK_IMAGE_SIZE, Resize::Fit(None))
            else {
                return false;
            };
            self.state = WordmarkState::Ready {
                protocol,
                relief_pending: true,
            };
        }
        matches!(&self.state, WordmarkState::Ready { .. })
    }

    /// Render the already encoded 24x2 wordmark into `area`. The caller passes
    /// the fixed geometry and must have reserved it (drawn nothing there).
    pub fn render_into(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if !self.prepare() {
            return;
        }
        let WordmarkState::Ready {
            protocol,
            relief_pending,
        } = &mut self.state
        else {
            unreachable!("prepare accepted only ready wordmark state");
        };
        Image::new(protocol).render(area, buf);
        if std::mem::take(relief_pending) {
            // Sixel/Kitty/iTerm encoding uses several large temporary image
            // buffers. They are all dead after the first render; ask Darwin's
            // allocator to return free pages once, never once per frame.
            let _ = haider_platform::allocator_pressure_relief();
        }
    }

    fn state_label(&self) -> &'static str {
        match &self.state {
            WordmarkState::Deferred(_) => "deferred",
            WordmarkState::Ready { .. } => "ready",
            WordmarkState::Failed => "failed",
        }
    }
}

/// Environment evidence that this terminal answers graphics-capability
/// queries (so the query's response reader is guaranteed its answer and can
/// never eat a user keystroke). Conservative allowlist: the terminals that
/// speak Kitty graphics or iTerm2 inline images and ANSWER queries.
///
/// Pure over an env lookup so tests can drive it without touching the
/// process environment (edition 2024 makes `set_var` unsafe).
#[must_use]
pub fn graphics_terminal_likely(env: &dyn Fn(&str) -> Option<String>) -> bool {
    if env("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    if env("TERM").is_some_and(|term| term.contains("kitty") || term.contains("ghostty")) {
        return true;
    }
    if env("TERM_PROGRAM").is_some_and(|program| {
        matches!(
            program.as_str(),
            "iTerm.app" | "WezTerm" | "ghostty" | "rio" | "vscode"
        )
    }) {
        return true;
    }
    if env("WEZTERM_EXECUTABLE").is_some() || env("KONSOLE_VERSION").is_some() {
        return true;
    }
    false
}
