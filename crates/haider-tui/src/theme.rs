//! Desert Dawn theme system — one identity (sand · maroon · gold), three renderings.
//!
//! Parity source: the `/tui` sim (`next-diffforge/src/pages/tui.js`, `THEMES`).
//! Every widget reads tokens from a [`Theme`]; nothing hardcodes a color.
//!
//! Terminals have no alpha channel, so the sim's `rgba(base, α)` overlays are
//! pre-blended over the theme background at compile time via [`Rgb::over`],
//! keeping the same ink + alpha literals as the sim so parity stays auditable
//! in source. The sim's `glow` gradient has no terminal equivalent and is
//! intentionally not a token.

/// A concrete sRGB color. Truecolor terminals render it exactly; the `--plain`
/// fallback ignores colors entirely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// `Rgb::hex(0xf3ead9)` — the sim's `#f3ead9`.
    #[must_use]
    pub const fn hex(v: u32) -> Self {
        Self {
            r: ((v >> 16) & 0xff) as u8,
            g: ((v >> 8) & 0xff) as u8,
            b: (v & 0xff) as u8,
        }
    }

    /// Alpha-blend `self` over `base` with opacity in per-mille (0..=1000):
    /// the sim's `rgba(self, alpha/1000)` on a `base` ground. Rounds half-up,
    /// matching the real-valued blend to the nearest channel step.
    #[must_use]
    pub const fn over(self, base: Rgb, alpha_pm: u16) -> Rgb {
        const fn ch(base: u8, over: u8, alpha_pm: u16) -> u8 {
            let a = alpha_pm as u32;
            ((base as u32 * (1000 - a) + over as u32 * a + 500) / 1000) as u8
        }
        Rgb {
            r: ch(base.r, self.r, alpha_pm),
            g: ch(base.g, self.g, alpha_pm),
            b: ch(base.b, self.b, alpha_pm),
        }
    }
}

/// Which of the three renderings is active. `Dawn` is the default.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeKey {
    #[default]
    Dawn,
    Ivory,
    Dark,
}

impl ThemeKey {
    /// The `/theme` argument names, in menu order.
    pub const ALL: [ThemeKey; 3] = [ThemeKey::Dawn, ThemeKey::Ivory, ThemeKey::Dark];

    /// Parse a `/theme` argument (sim: `dawn · ivory · dark`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "dawn" => Some(Self::Dawn),
            "ivory" => Some(Self::Ivory),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// The `/theme` argument name for this key.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dawn => "dawn",
            Self::Ivory => "ivory",
            Self::Dark => "dark",
        }
    }

    /// The token set for this key.
    #[must_use]
    pub const fn theme(self) -> &'static Theme {
        match self {
            Self::Dawn => &DAWN,
            Self::Ivory => &IVORY,
            Self::Dark => &DARK,
        }
    }
}

/// The full token set every widget draws from. Field names mirror the sim's
/// `THEMES` keys (camelCase → snake_case).
#[derive(Debug)]
pub struct Theme {
    pub key: ThemeKey,
    /// Human label shown in the status bar and `/theme` menu.
    pub label: &'static str,
    pub bg: Rgb,
    pub bar_bg: Rgb,
    pub frame: Rgb,
    pub text: Rgb,
    pub bright: Rgb,
    pub dim: Rgb,
    pub faint: Rgb,
    pub gold: Rgb,
    pub gold_soft: Rgb,
    pub maroon: Rgb,
    pub maroon_soft: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub err: Rgb,
    /// Foreground on filled state badges (the theme bg for contrast).
    pub badge_fg: Rgb,
    pub input_bg: Rgb,
    pub sel_bg: Rgb,
}

/// Desert Dawn — sand ground, maroon ink, burnished gold. The default.
pub const DAWN: Theme = {
    let bg = Rgb::hex(0xf3ead9);
    let maroon = Rgb::hex(0x7c2d12);
    let gold = Rgb::hex(0x9a6a08);
    let white = Rgb::hex(0xffffff);
    Theme {
        key: ThemeKey::Dawn,
        label: "Desert Dawn",
        bg,
        bar_bg: maroon.over(bg, 50),
        frame: maroon.over(bg, 280),
        text: Rgb::hex(0x5f4a2e),
        bright: Rgb::hex(0x3d2d18),
        dim: Rgb::hex(0xa08a63),
        faint: Rgb::hex(0xcdbd9a),
        gold,
        gold_soft: gold.over(bg, 140),
        maroon,
        maroon_soft: maroon.over(bg, 80),
        ok: Rgb::hex(0x4c7a45),
        warn: Rgb::hex(0xb45309),
        err: Rgb::hex(0xb91c1c),
        badge_fg: bg,
        input_bg: white.over(bg, 280),
        sel_bg: maroon.over(bg, 100),
    }
};

/// Ivory Light — near-white ground, soot ink, deep gold.
pub const IVORY: Theme = {
    let bg = Rgb::hex(0xfbf9f3);
    let ink = Rgb::hex(0x262019);
    let maroon = Rgb::hex(0x8c2f12);
    let gold = Rgb::hex(0x8a6206);
    Theme {
        key: ThemeKey::Ivory,
        label: "Ivory Light",
        bg,
        bar_bg: ink.over(bg, 40),
        frame: ink.over(bg, 300),
        text: Rgb::hex(0x3d372e),
        bright: Rgb::hex(0x191512),
        dim: Rgb::hex(0x8a8175),
        faint: Rgb::hex(0xd9d3c6),
        gold,
        gold_soft: gold.over(bg, 120),
        maroon,
        maroon_soft: maroon.over(bg, 70),
        ok: Rgb::hex(0x2f6e3f),
        warn: Rgb::hex(0xa3540a),
        err: Rgb::hex(0xb02121),
        badge_fg: bg,
        input_bg: ink.over(bg, 35),
        sel_bg: ink.over(bg, 80),
    }
};

/// Dark — near-black ground, aged gold, ember maroon.
pub const DARK: Theme = {
    let bg = Rgb::hex(0x14100a);
    let gold = Rgb::hex(0xd4af37);
    let maroon = Rgb::hex(0xe0703f);
    let white = Rgb::hex(0xffffff);
    Theme {
        key: ThemeKey::Dark,
        label: "Dark",
        bg,
        bar_bg: gold.over(bg, 50),
        frame: gold.over(bg, 260),
        text: Rgb::hex(0xcbbc9a),
        bright: Rgb::hex(0xf2ead2),
        dim: Rgb::hex(0x8a7b5e),
        faint: Rgb::hex(0x463c2a),
        gold,
        gold_soft: gold.over(bg, 120),
        maroon,
        maroon_soft: maroon.over(bg, 100),
        ok: Rgb::hex(0x7db98a),
        warn: Rgb::hex(0xffb347),
        err: Rgb::hex(0xff7a6b),
        badge_fg: bg,
        input_bg: white.over(bg, 45),
        sel_bg: gold.over(bg, 100),
    }
};
