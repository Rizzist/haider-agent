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
//! (banner 28×4, header 16×2), so the layout is identical — only the fidelity
//! of the mark changes.
//!
//! DIGNITY (sanctum rule 2, mirrored from `crate::mark`): whole or nothing.
//! When no real graphics protocol is present, [`Wordmark::detect`] returns
//! `None` and the caller draws the crafted half-block art, which reads cleaner
//! at header scale than a half-block DOWNSAMPLE of the image would.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::StatefulWidget as _;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};

/// The bundled wordmark: حيدر in Damascus Naskh, Desert-Dawn gold, transparent
/// background so it composites over any terminal ground. Regenerate with
/// `assets/gen-wordmark.swift` (CoreText, which shapes Arabic natively).
const WORDMARK_PNG: &[u8] = include_bytes!("../assets/wordmark-haider.png");

/// A terminal-graphics wordmark, ready to render into a reserved cell rect.
///
/// Held behind a `RefCell` in `AppModel` because the render pass takes
/// `&AppModel` while the protocol needs `&mut` to (re)encode on a size change.
pub struct Wordmark {
    protocol: StatefulProtocol,
    kind: ProtocolType,
}

impl std::fmt::Debug for Wordmark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never recurse into the protocol (it owns image bytes): AppModel's
        // derive(Debug) only needs a stable, cheap summary.
        f.debug_struct("Wordmark")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Wordmark {
    /// Query the terminal for a graphics protocol and build the wordmark, or
    /// `None` to fall back to `crate::mark`.
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
        let picker = Picker::from_query_stdio().ok()?;
        let kind = picker.protocol_type();
        if matches!(kind, ProtocolType::Halfblocks) {
            return None;
        }
        let image = image::load_from_memory(WORDMARK_PNG).ok()?;
        let protocol = picker.new_resize_protocol(image);
        Some(Self { protocol, kind })
    }

    /// The detected protocol (for diagnostics/tests).
    #[must_use]
    pub fn kind(&self) -> ProtocolType {
        self.kind
    }

    /// Render the wordmark, fit + centered, into `area`. The caller must have
    /// reserved `area` (drawn nothing there); the widget resizes+encodes to the
    /// cell rect once and caches until the rect changes.
    pub fn render_into(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        StatefulImage::<StatefulProtocol>::default()
            .resize(Resize::Fit(None))
            .render(area, buf, &mut self.protocol);
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
