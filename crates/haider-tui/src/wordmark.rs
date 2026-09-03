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
const BANNER_IMAGE_SIZE: Size = Size::new(crate::mark::BANNER_COLS, crate::mark::BANNER_ROWS);
const HEADER_IMAGE_SIZE: Size = Size::new(crate::mark::HEADER_COLS, crate::mark::HEADER_ROWS);

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
    /// no terminal image protocol owns an encoded payload yet.
    Deferred(Picker),
    /// Fixed protocols preserve both existing display footprints without
    /// retaining the resizeable source image. The decoded PNG and encoder
    /// scratch buffers are dropped as soon as this state is constructed.
    Ready {
        banner: Protocol,
        header: Protocol,
        relief_pending: bool,
    },
    /// Decode/encode failures are one-shot: a bad asset or backend must not
    /// retry the same allocations on every frame.
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
    /// wordmark until a frame actually has a visible image slot, or return
    /// `None` to fall back to `crate::mark`.
    ///
    /// Returns `None` when the terminal offers only half-blocks (the crafted
    /// art is cleaner there) or the capability query fails. A later image
    /// decode/encode failure leaves the half-block fallback in place.
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
    /// Useful for embedders and tests that perform capability detection.
    #[must_use]
    #[doc(hidden)]
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

    /// Whether a real draw has constructed the fixed terminal protocols.
    #[must_use]
    #[doc(hidden)]
    pub fn is_initialized(&self) -> bool {
        matches!(&self.state, WordmarkState::Ready { .. })
    }

    /// Prepare the fixed protocols at most once, and only for a cell rectangle
    /// large enough to display one of the two existing wordmark footprints.
    /// The renderer calls this before clearing the half-block fallback so a
    /// decode/encode failure always leaves useful content on screen.
    pub(crate) fn prepare(&mut self, area: Rect) -> bool {
        if wordmark_size(area).is_none() {
            return false;
        }
        if matches!(&self.state, WordmarkState::Deferred(_)) {
            let WordmarkState::Deferred(picker) =
                std::mem::replace(&mut self.state, WordmarkState::Failed)
            else {
                unreachable!("wordmark state was checked immediately before replacement");
            };
            let Ok(image) = image::load_from_memory(WORDMARK_PNG) else {
                return false;
            };
            let Ok(banner) =
                picker.new_protocol(image.clone(), BANNER_IMAGE_SIZE, Resize::Fit(None))
            else {
                return false;
            };
            let Ok(header) = picker.new_protocol(image, HEADER_IMAGE_SIZE, Resize::Fit(None))
            else {
                return false;
            };
            self.state = WordmarkState::Ready {
                banner,
                header,
                relief_pending: true,
            };
        }
        matches!(&self.state, WordmarkState::Ready { .. })
    }

    /// Render the fixed protocol matching `area`. This safely prepares itself;
    /// callers that clear fallback cells first must call [`Self::prepare`]
    /// before that clear so initialization failure keeps the fallback visible.
    pub fn render_into(&mut self, area: Rect, buf: &mut Buffer) {
        let Some(size) = wordmark_size(area) else {
            return;
        };
        if !self.prepare(area) {
            return;
        }
        let WordmarkState::Ready {
            banner,
            header,
            relief_pending,
        } = &mut self.state
        else {
            unreachable!("prepare accepted only ready wordmark state");
        };
        let protocol = if size == BANNER_IMAGE_SIZE {
            banner
        } else {
            header
        };
        Image::new(protocol).render(area, buf);
        if std::mem::take(relief_pending) {
            // Sixel/Kitty/iTerm encoding uses several temporary image buffers.
            // They are all dead after this first render; on Darwin, ask the
            // allocator to return free pages once rather than once per frame.
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

fn wordmark_size(area: Rect) -> Option<Size> {
    if area.width >= BANNER_IMAGE_SIZE.width && area.height >= BANNER_IMAGE_SIZE.height {
        Some(BANNER_IMAGE_SIZE)
    } else if area.width >= HEADER_IMAGE_SIZE.width && area.height >= HEADER_IMAGE_SIZE.height {
        Some(HEADER_IMAGE_SIZE)
    } else {
        None
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
