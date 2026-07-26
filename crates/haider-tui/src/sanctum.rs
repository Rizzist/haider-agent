//! The sanctum — the shahada centerpiece of the boot screen and launcher.
//!
//! Dignity rules (§ TUI0 brief, binding for every renderer):
//! 1. The sanctum region NEVER shares a frame, line, or box with errors,
//!    warnings, or status noise. Widgets place a [`SanctumLine`] alone.
//! 2. The text is never truncated or ellipsized: it renders whole or not at
//!    all ([`SanctumLine::fit`] — grace over clutter).
//! 3. It never appears in logs, error strings, metrics, or addresses — it is
//!    display-only ceremony.
//!
//! Terminal reality: ratatui emits logical-order UTF-8; the EMULATOR does
//! bidi/shaping (macOS Terminal.app shapes Arabic correctly; some emulators
//! do not). OWNER DECISION 2026-07-26: Arabic is the default tier everywhere
//! (sim parity — the identity leads); `HAIDER_SHAHADA=translit` selects the
//! transliteration tier — a first-class rendering, not a degradation — for
//! emulators that cannot shape. Rule 2 still governs both tiers.

/// The shahada, Arabic tier (matches the sim's `SHAHADA`).
pub const SHAHADA_ARABIC: &str = "لا إله إلا الله محمد رسول الله";

/// The shahada, transliteration tier — precomposed Latin (no RTL, no
/// shaping), safe in every UTF-8 terminal.
pub const SHAHADA_TRANSLIT: &str = "lā ilāha illā llāh · muḥammadun rasūlu llāh";

/// The boot-mark word above the sanctum (the sim's `mark`).
pub const MARK_ARABIC: &str = "حيدر";

/// The mark in the transliteration tier — the name itself, the lion.
pub const MARK_TRANSLIT: &str = "ḤAYDAR";

/// Which rendering tier is active. Arabic is the default (owner decision,
/// sim parity); translit serves emulators without shaping.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SanctumTier {
    #[default]
    Arabic,
    Translit,
}

/// A sanctum line ready for placement. Constructing one is the ONLY way to
/// obtain sanctum text for rendering, which keeps the dignity rules at one
/// choke point.
#[derive(Clone, Copy, Debug)]
pub struct SanctumLine {
    tier: SanctumTier,
}

impl SanctumLine {
    #[must_use]
    pub fn new(tier: SanctumTier) -> Self {
        Self { tier }
    }

    #[must_use]
    pub fn tier(self) -> SanctumTier {
        self.tier
    }

    /// The full text of this tier.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self.tier {
            SanctumTier::Arabic => SHAHADA_ARABIC,
            SanctumTier::Translit => SHAHADA_TRANSLIT,
        }
    }

    /// The mark (the small word above the sanctum).
    #[must_use]
    pub fn mark(self) -> &'static str {
        match self.tier {
            SanctumTier::Arabic => MARK_ARABIC,
            SanctumTier::Translit => MARK_TRANSLIT,
        }
    }

    /// Whole-or-nothing fit: the text if it fits in `width` columns, else
    /// `None`. Never a truncation. Width is counted in chars — a conservative
    /// stand-in for display cells that both tiers satisfy (no wide glyphs).
    #[must_use]
    pub fn fit(self, width: usize) -> Option<&'static str> {
        let text = self.text();
        (text.chars().count() <= width).then_some(text)
    }
}
