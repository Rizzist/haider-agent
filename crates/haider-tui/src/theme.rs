//! The theme registry — one identity (warm ground · gold accent · maroon
//! ink), four designed renderings (UI-themes wave, owner spec §2).
//!
//! Every widget reads tokens from a [`Theme`] resolved once per frame;
//! nothing hardcodes a color (`every_surface_uses_theme_slots` enforces
//! this mechanically). The registry began as `/tui`-sim parity; this wave
//! supersedes the sim's `THEMES` with palettes designed for the terminal:
//!
//! * [`LIGHT`]  — warm paper-white ground, ink-dark text, muted gold.
//! * [`DARK`]   — the identity's night rendering, refreshed: deep warm
//!   ground, aged gold, ember maroon, every pairing re-contrasted.
//! * [`DESERT`] — sand ground, burnished amber, dusk-plum ink.
//! * [`WATER`] — sea-glass ground, tide teal, coral dusk (desert's contrast).
//! * [`OASIS`]  — palm-night ground, moonlit spring text, date gold.
//!
//! Terminals have no alpha channel, so translucent chrome (`rgba` in the
//! sim era) is pre-blended over the theme ground at compile time via
//! [`Rgb::over`], keeping ink + alpha literals auditable in source.
//!
//! CONTRAST LAW (pinned in `ui_themes_tests`): every ink slot clears a
//! WCAG-derived floor against the ground it renders on — body text ≥ 6.5:1,
//! emphasis ≥ 8:1, metadata ≥ 3.4:1, accents/states ≥ 3.2:1, filled-badge
//! foregrounds ≥ 2.7:1 — and `faint` stays inside its barely-there band.

/// A concrete sRGB color. Truecolor terminals render it exactly; the `--plain`
/// fallback ignores colors entirely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// `Rgb::hex(0xfaf6ee)` — the light theme's paper ground.
    #[must_use]
    pub const fn hex(v: u32) -> Self {
        Self {
            r: ((v >> 16) & 0xff) as u8,
            g: ((v >> 8) & 0xff) as u8,
            b: (v & 0xff) as u8,
        }
    }

    /// Alpha-blend `self` over `base` with opacity in per-mille (0..=1000):
    /// translucent chrome pre-blended onto a solid ground. Rounds half-up,
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

/// Which rendering is active. `Dark` is the default — it is also the
/// detection fallback (owner spec §3: undetectable terminal → dark).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeKey {
    Light,
    #[default]
    Dark,
    Desert,
    Water,
    Oasis,
}

impl ThemeKey {
    /// The fixed themes in `/theme`-menu order (the `system` entry lives a
    /// level up, in [`ThemeChoice`]).
    pub const ALL: [ThemeKey; 5] = [
        ThemeKey::Light,
        ThemeKey::Dark,
        ThemeKey::Desert,
        ThemeKey::Water,
        ThemeKey::Oasis,
    ];

    /// Parse a theme name. The retired sim-era names stay parseable so a
    /// persisted `dawn`/`ivory` migrates instead of silently resetting:
    /// `dawn` was the sand theme (→ `desert`), `ivory` the light one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "desert" => Some(Self::Desert),
            "water" => Some(Self::Water),
            "oasis" => Some(Self::Oasis),
            // Legacy sim-era names (pre-UI-themes persistence migrates).
            "dawn" => Some(Self::Desert),
            "ivory" => Some(Self::Light),
            _ => None,
        }
    }

    /// The `/theme` argument name for this key.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Desert => "desert",
            Self::Water => "water",
            Self::Oasis => "oasis",
        }
    }

    /// The token set for this key.
    #[must_use]
    pub const fn theme(self) -> &'static Theme {
        match self {
            Self::Light => &LIGHT,
            Self::Dark => &DARK,
            Self::Desert => &DESERT,
            Self::Water => &WATER,
            Self::Oasis => &OASIS,
        }
    }
}

/// How the active theme is CHOSEN (owner spec §3). `System` follows the
/// terminal's detected light/dark appearance, re-evaluated on every boot;
/// a `Fixed` key pins one rendering. This is TUI-local display state —
/// persisted in the profile dir, never daemon truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Fixed(ThemeKey),
}

impl ThemeChoice {
    /// The `/theme` picker's rows, in menu order: `system` first (the
    /// default), then the fixed themes.
    /// DERIVED from [`ThemeKey::ALL`] so a new registry theme can never
    /// miss the picker (the hand-rolled list silently dropped `water` —
    /// live-probe catch on v0.0.62).
    pub const MENU: [ThemeChoice; ThemeKey::ALL.len() + 1] = {
        let mut menu = [ThemeChoice::System; ThemeKey::ALL.len() + 1];
        let mut i = 0;
        while i < ThemeKey::ALL.len() {
            menu[i + 1] = ThemeChoice::Fixed(ThemeKey::ALL[i]);
            i += 1;
        }
        menu
    };

    /// Parse a `/theme` argument or a persisted choice name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        if name == "system" {
            return Some(Self::System);
        }
        ThemeKey::parse(name).map(Self::Fixed)
    }

    /// The argument/persistence name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Fixed(key) => key.name(),
        }
    }

    /// Resolve to a concrete key: `System` takes the detected terminal
    /// appearance (already fallen back to dark when undetectable).
    #[must_use]
    pub const fn resolve(self, detected: ThemeKey) -> ThemeKey {
        match self {
            Self::System => detected,
            Self::Fixed(key) => key,
        }
    }
}

/// The full token set every widget draws from — semantic slots, resolved
/// once per frame at the top of `render`.
#[derive(Debug)]
pub struct Theme {
    pub key: ThemeKey,
    /// Human label shown in the status bar and `/theme` menu.
    pub label: &'static str,
    /// True for the night-family palettes (dark · oasis) — what `system`
    /// resolution and OSC-11 ground sync care about.
    pub dark: bool,
    /// The ground every surface sits on.
    pub bg: Rgb,
    /// The sticky/origin band's tinted ground.
    pub bar_bg: Rgb,
    /// Panel frames, rules, chip borders.
    pub frame: Rgb,
    /// Body ink.
    pub text: Rgb,
    /// Emphasized ink (headings, the active row).
    pub bright: Rgb,
    /// Secondary ink (metadata, hints).
    pub dim: Rgb,
    /// Barely-there ink (pending markers, viewport dots).
    pub faint: Rgb,
    /// The accent — prompt ❯, active states, the running pulse.
    pub gold: Rgb,
    /// The accent's soft ground (menu cards, the agent rail).
    pub gold_soft: Rgb,
    /// Gold at half strength over the ground — the boot splash's dignity
    /// rule under the shahada.
    pub gold_half: Rgb,
    /// The identity ink — the حيدر mark, frames, names.
    pub maroon: Rgb,
    /// The identity ink's soft ground.
    pub maroon_soft: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub err: Rgb,
    /// Foreground on filled state badges (the theme bg for contrast).
    pub badge_fg: Rgb,
    /// The composer band's ground.
    pub input_bg: Rgb,
    /// Selection / hover ground (ink stays, ground shifts).
    pub sel_bg: Rgb,
}

/// Light — warm paper-white ground, ink-dark text, muted gold accent. A
/// palette designed AS light (owner spec §2), not an inverted dark: the
/// ink is near-black for real contrast (13:1 body, 17:1 emphasis) and the
/// accents are deepened to keep ≥ 3.9:1 on paper.
pub const LIGHT: Theme = {
    let bg = Rgb::hex(0xfaf6ee);
    let ink = Rgb::hex(0x14110c);
    let gold = Rgb::hex(0x8a6508);
    let maroon = Rgb::hex(0x7c2d12);
    Theme {
        key: ThemeKey::Light,
        label: "Light",
        dark: false,
        bg,
        bar_bg: ink.over(bg, 40),
        frame: ink.over(bg, 300),
        text: Rgb::hex(0x2e2a24),
        bright: ink,
        dim: Rgb::hex(0x6e675c),
        faint: Rgb::hex(0xd8d1c2),
        gold,
        gold_soft: gold.over(bg, 110),
        gold_half: gold.over(bg, 500),
        maroon,
        maroon_soft: maroon.over(bg, 70),
        ok: Rgb::hex(0x2f6e3f),
        warn: Rgb::hex(0x9c4e06),
        err: Rgb::hex(0xb02121),
        badge_fg: bg,
        input_bg: ink.over(bg, 40),
        sel_bg: ink.over(bg, 90),
    }
};

/// Dark — the identity's night rendering, refreshed: a deeper warm ground,
/// aged gold lifted a step, ember maroon warmed, and the muddy pairings of
/// the sim era (dim 3.2:1, sunken faint) re-contrasted to 5.3:1 / 1.9:1.
pub const DARK: Theme = {
    let bg = Rgb::hex(0x120e08);
    let gold = Rgb::hex(0xd9b544);
    let maroon = Rgb::hex(0xe57a4a);
    let white = Rgb::hex(0xffffff);
    Theme {
        key: ThemeKey::Dark,
        label: "Dark",
        dark: true,
        bg,
        bar_bg: gold.over(bg, 50),
        frame: gold.over(bg, 260),
        text: Rgb::hex(0xd6c7a4),
        bright: Rgb::hex(0xf5eeda),
        dim: Rgb::hex(0x93856a),
        faint: Rgb::hex(0x4a4030),
        gold,
        gold_soft: gold.over(bg, 120),
        gold_half: gold.over(bg, 500),
        maroon,
        maroon_soft: maroon.over(bg, 100),
        ok: Rgb::hex(0x86c493),
        warn: Rgb::hex(0xffb757),
        err: Rgb::hex(0xff8577),
        badge_fg: bg,
        input_bg: white.over(bg, 45),
        sel_bg: gold.over(bg, 100),
    }
};

/// Desert — sand · amber · dusk (owner spec §2: "worthy of the name").
/// True sand ground (not cream), burnished amber accent, and a dusk-plum
/// identity ink where the old dawn kept plain maroon — the palette of the
/// hour the sand cools.
pub const DESERT: Theme = {
    let bg = Rgb::hex(0xf0e4cc);
    let gold = Rgb::hex(0x9c6202);
    let maroon = Rgb::hex(0x7b2740);
    let white = Rgb::hex(0xffffff);
    Theme {
        key: ThemeKey::Desert,
        label: "Desert",
        dark: false,
        bg,
        bar_bg: maroon.over(bg, 50),
        frame: maroon.over(bg, 280),
        text: Rgb::hex(0x4d3a20),
        bright: Rgb::hex(0x2e2110),
        dim: Rgb::hex(0x8a7350),
        faint: Rgb::hex(0xd2bf9b),
        gold,
        gold_soft: gold.over(bg, 140),
        gold_half: gold.over(bg, 500),
        maroon,
        maroon_soft: maroon.over(bg, 80),
        ok: Rgb::hex(0x55702e),
        warn: Rgb::hex(0xa34e08),
        err: Rgb::hex(0xb3231c),
        badge_fg: bg,
        input_bg: white.over(bg, 280),
        sel_bg: maroon.over(bg, 100),
    }
};

/// Water — the desert's contrast (owner ask): where desert is sand under
/// a dusk-plum sky, water is sea-glass under a deep tide. Cool pale ground,
/// ocean-slate ink, tide-teal accent in the gold slot, coral dusk in the
/// maroon slot; ok/warn/err keep the family's semantic hues.
pub const WATER: Theme = {
    let bg = Rgb::hex(0xe3eef0);
    let gold = Rgb::hex(0x0d6678);
    let maroon = Rgb::hex(0xa03a4d);
    let white = Rgb::hex(0xffffff);
    Theme {
        key: ThemeKey::Water,
        label: "Water",
        dark: false,
        bg,
        bar_bg: gold.over(bg, 50),
        frame: gold.over(bg, 280),
        text: Rgb::hex(0x243b47),
        bright: Rgb::hex(0x122730),
        dim: Rgb::hex(0x53707e),
        faint: Rgb::hex(0xb7ccd2),
        gold,
        gold_soft: gold.over(bg, 140),
        gold_half: gold.over(bg, 500),
        maroon,
        maroon_soft: maroon.over(bg, 80),
        ok: Rgb::hex(0x2e7051),
        warn: Rgb::hex(0x9a5b10),
        err: Rgb::hex(0xb3231c),
        badge_fg: bg,
        input_bg: white.over(bg, 280),
        sel_bg: gold.over(bg, 100),
    }
};

/// Oasis — the designed fifth entry (owner spec §3: "fits the identity"):
/// the desert's night garden. Deep palm-green ground, moonlit spring
/// text, date-palm gold, warm ember — the night camp by the water.
pub const OASIS: Theme = {
    let bg = Rgb::hex(0x0c1410);
    let gold = Rgb::hex(0xd8b04c);
    let maroon = Rgb::hex(0xdf8a5a);
    let white = Rgb::hex(0xffffff);
    Theme {
        key: ThemeKey::Oasis,
        label: "Oasis",
        dark: true,
        bg,
        bar_bg: gold.over(bg, 50),
        frame: gold.over(bg, 260),
        text: Rgb::hex(0xb9cfc0),
        bright: Rgb::hex(0xe9f5ec),
        dim: Rgb::hex(0x7c9486),
        faint: Rgb::hex(0x35473d),
        gold,
        gold_soft: gold.over(bg, 120),
        gold_half: gold.over(bg, 500),
        maroon,
        maroon_soft: maroon.over(bg, 100),
        ok: Rgb::hex(0x7fca96),
        warn: Rgb::hex(0xf0b45c),
        err: Rgb::hex(0xff8577),
        badge_fg: bg,
        input_bg: white.over(bg, 45),
        sel_bg: gold.over(bg, 100),
    }
};
