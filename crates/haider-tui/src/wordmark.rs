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
    /// Build the wordmark from an explicit terminal hint, or fall back to
    /// the crafted half-block art. Startup must never give a capability
    /// reader ownership of stdin: even an answering terminal can receive
    /// user typing in the same read as its reply.
    #[must_use]
    pub fn detect() -> Option<Self> {
        let kind = graphics_protocol_from_env(&|name| std::env::var(name).ok())?;
        let font = ratatui::crossterm::terminal::window_size()
            .ok()
            .filter(|size| {
                size.columns > 0
                    && size.rows > 0
                    && size.width >= size.columns
                    && size.height >= size.rows
            })
            .map_or(ratatui_image::FontSize::new(10, 20), |size| {
                ratatui_image::FontSize::new(size.width / size.columns, size.height / size.rows)
            });
        // The library's replacement constructor reads stdin. Use the
        // non-reading constructor until it offers an injected query reader.
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(font);
        picker.set_protocol_type(kind);
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

/// Environment evidence for a graphics-capable terminal. Detection itself
/// only uses explicit protocol hints and never reads a capability response.
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

fn graphics_protocol_from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<ProtocolType> {
    if env("KITTY_WINDOW_ID").is_some()
        || env("TERM").is_some_and(|term| term.contains("kitty") || term.contains("ghostty"))
        || env("TERM_PROGRAM").as_deref() == Some("ghostty")
    {
        return Some(ProtocolType::Kitty);
    }
    if env("WEZTERM_EXECUTABLE").is_some()
        || env("ITERM_SESSION_ID").is_some()
        || env("TERM_PROGRAM").is_some_and(|program| {
            matches!(program.as_str(), "iTerm.app" | "WezTerm" | "rio" | "vscode")
        })
    {
        return Some(ProtocolType::Iterm2);
    }
    None
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn explicit_graphics_hints_choose_a_protocol_without_a_stdin_reader() {
        for (name, value, protocol) in [
            ("TERM", "xterm-kitty", ProtocolType::Kitty),
            ("TERM", "xterm-ghostty", ProtocolType::Kitty),
            ("KITTY_WINDOW_ID", "fixture", ProtocolType::Kitty),
            ("TERM_PROGRAM", "ghostty", ProtocolType::Kitty),
            ("TERM_PROGRAM", "iTerm.app", ProtocolType::Iterm2),
            ("TERM_PROGRAM", "WezTerm", ProtocolType::Iterm2),
            ("WEZTERM_EXECUTABLE", "fixture", ProtocolType::Iterm2),
            ("ITERM_SESSION_ID", "fixture", ProtocolType::Iterm2),
            ("TERM_PROGRAM", "rio", ProtocolType::Iterm2),
            ("TERM_PROGRAM", "vscode", ProtocolType::Iterm2),
        ] {
            assert_eq!(
                graphics_protocol_from_env(&|key| (key == name).then(|| value.into())),
                Some(protocol)
            );
        }
        for terminal in ["tmux-256color", "xterm-256color", "dumb"] {
            assert_eq!(
                graphics_protocol_from_env(&|key| (key == "TERM").then(|| terminal.into())),
                None
            );
        }
    }
}
