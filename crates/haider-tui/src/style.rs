//! Theme tokens → ratatui styles. The ONLY place `theme::Rgb` meets
//! `ratatui::style::Color`, so widgets stay expressible purely in tokens.

use crate::theme::{Rgb, Theme};
use ratatui::style::{Color, Modifier, Style};

impl From<Rgb> for Color {
    fn from(rgb: Rgb) -> Self {
        Color::Rgb(rgb.r, rgb.g, rgb.b)
    }
}

/// Style presets every widget draws from — one vocabulary, three themes.
impl Theme {
    /// Body text on the theme ground.
    #[must_use]
    pub fn text_style(&self) -> Style {
        Style::default().fg(self.text.into()).bg(self.bg.into())
    }

    /// Emphasized ink (headings, the active row).
    #[must_use]
    pub fn bright_style(&self) -> Style {
        Style::default().fg(self.bright.into())
    }

    /// Secondary ink (metadata, hints).
    #[must_use]
    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.dim.into())
    }

    /// Barely-there ink (rules, pending markers).
    #[must_use]
    pub fn faint_style(&self) -> Style {
        Style::default().fg(self.faint.into())
    }

    /// The gold accent (prompt ❯, active states, the mark).
    #[must_use]
    pub fn gold_style(&self) -> Style {
        Style::default().fg(self.gold.into())
    }

    /// The maroon identity ink (frames, names).
    #[must_use]
    pub fn maroon_style(&self) -> Style {
        Style::default().fg(self.maroon.into())
    }

    /// Panel frames.
    #[must_use]
    pub fn frame_style(&self) -> Style {
        Style::default().fg(self.frame.into())
    }

    /// The state badge, per visual class — the sim's badge vocabulary
    /// (tui.js:5531-5564): outlined states carry only their ink (the `[ ]`
    /// chip form stands in for the border); fills carry badge_fg on the
    /// state ground.
    #[must_use]
    pub fn badge_style(&self, tone: crate::projection::BadgeTone) -> Style {
        use crate::projection::BadgeTone;
        let outline = |ink: Rgb| Style::default().fg(ink.into());
        let filled = |ground: Rgb| {
            Style::default()
                .fg(self.badge_fg.into())
                .bg(ground.into())
                .add_modifier(Modifier::BOLD)
        };
        match tone {
            BadgeTone::Idle => outline(self.dim),
            BadgeTone::Restful => outline(self.gold),
            BadgeTone::Attention => outline(self.warn),
            BadgeTone::EffectUnknown => outline(self.err),
            BadgeTone::Active => filled(self.gold),
            BadgeTone::Tool => filled(self.maroon),
            BadgeTone::Compacting => filled(self.warn),
            BadgeTone::Error => filled(self.err),
        }
    }

    /// The composer input field.
    #[must_use]
    pub fn input_style(&self) -> Style {
        Style::default()
            .fg(self.bright.into())
            .bg(self.input_bg.into())
    }

    /// The composer's cursor CELL (TUI5 item 1): reverse-video against the
    /// input band — a gold block carrying the glyph under it in `badge_fg`
    /// (the theme bg, the same filled-badge contrast rule). Gold is the
    /// accent on every theme, so the block reads on light AND dark grounds; at
    /// end-of-text the cell is this style over a plain space. STEADY by
    /// law — no blink, no `animated()` term (zero-idle-wakeup).
    #[must_use]
    pub fn cursor_style(&self) -> Style {
        Style::default()
            .fg(self.badge_fg.into())
            .bg(self.gold.into())
    }

    /// The composer's selection band (TUI5 item 4): the shared selBg
    /// ground under the bright draft ink — the hover-band law (ground
    /// shifts, ink stays), matching the transcript drag-select highlight.
    #[must_use]
    pub fn composer_selection_style(&self) -> Style {
        Style::default()
            .fg(self.bright.into())
            .bg(self.sel_bg.into())
    }

    /// The blocking input-replacement menu's ground (sim `InputMenu`:
    /// warn border on a gold-soft field).
    #[must_use]
    pub fn menu_style(&self) -> Style {
        Style::default()
            .fg(self.text.into())
            .bg(self.gold_soft.into())
    }

    /// The agent-body left rail (sim AgentRow `border-left: goldSoft`).
    #[must_use]
    pub fn rail_style(&self) -> Style {
        Style::default().fg(self.gold_soft.into())
    }

    /// The launcher's dignity rule — gold at half strength (sim `.rule`
    /// opacity 0.5, tui.js:4280-4286).
    #[must_use]
    pub fn rule_style(&self) -> Style {
        Style::default().fg(self.gold_half.into())
    }

    /// Hover band: the selection ground alone, span inks untouched (sim
    /// `:hover { background: selBg }`).
    #[must_use]
    pub fn hover_style(&self) -> Style {
        Style::default().bg(self.sel_bg.into())
    }

    /// The sticky origin band (sim StickyLine, tui.js:4597-4623 — owner
    /// item 11). The CSS is REAL chrome: `background: ${bg}f0` +
    /// `backdrop-filter: blur` + `border-bottom: 1px solid frame`. A cell
    /// can neither blur nor alpha-cover the row beneath, so a bg-tinted
    /// ground renders as no band at all — the owner's complaint — and per
    /// his direction the band takes the barBg tint instead. The
    /// border-bottom ports as the frame-colored underline across the row.
    #[must_use]
    pub fn sticky_style(&self) -> Style {
        Style::default()
            .fg(self.bright.into())
            .bg(self.bar_bg.into())
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(self.frame.into())
    }

    /// The sticky band under the pointer (sim `&:hover`, tui.js:4614-4617:
    /// opaque ground + maroon ink — a real hover, not just
    /// `cursor: pointer`): the selBg hover shift with the maroon ink, the
    /// band's bottom edge kept.
    #[must_use]
    pub fn sticky_hover_style(&self) -> Style {
        Style::default()
            .fg(self.maroon.into())
            .bg(self.sel_bg.into())
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(self.frame.into())
    }

    /// The selected row/option.
    #[must_use]
    pub fn selection_style(&self) -> Style {
        Style::default()
            .fg(self.bright.into())
            .bg(self.sel_bg.into())
    }

    /// Success / warning / error inks.
    #[must_use]
    pub fn ok_style(&self) -> Style {
        Style::default().fg(self.ok.into())
    }

    #[must_use]
    pub fn warn_style(&self) -> Style {
        Style::default().fg(self.warn.into())
    }

    #[must_use]
    pub fn err_style(&self) -> Style {
        Style::default().fg(self.err.into())
    }

    // ---- TUI4d item 14: the sim's animations, terminal-faithful ----

    /// The sim's `pulse` keyframes (tui.js:3943-3946: opacity 1 ↔ 0.35) on
    /// a terminal cell: the ON phase carries the full ink, the OFF phase
    /// the SAME hue at the sim's 0.35 midpoint over the theme ground. One
    /// shared phase clock (`AppModel::anim_phase`, ticked by the runtime
    /// only while something animates) drives every pulsing element.
    #[must_use]
    pub fn pulse_ink(&self, ink: Rgb, phase: u8) -> Style {
        if phase.is_multiple_of(2) {
            Style::default().fg(ink.into())
        } else {
            Style::default().fg(ink.over(self.bg, 350).into())
        }
    }

    /// Dim an already-built style's foreground on the OFF phase — the
    /// pulse for composed chrome (the status badge, the talk chip) whose
    /// ink came from another preset. Non-RGB or absent foregrounds pass
    /// through untouched.
    #[must_use]
    pub fn pulse(&self, style: Style, phase: u8) -> Style {
        if phase.is_multiple_of(2) {
            return style;
        }
        match style.fg {
            Some(ratatui::style::Color::Rgb(r, g, b)) => {
                style.fg(Rgb { r, g, b }.over(self.bg, 350).into())
            }
            _ => style,
        }
    }

    /// The launcher rail's `railShimmer` (tui.js:3948-3951): a
    /// gold→maroon→gold gradient scrolling behind a 2px sliver. One
    /// terminal cell sees the gradient pass as its ink cycling gold →
    /// maroon → gold; a 3-phase cycle on the shared 600 ms clock lands the
    /// sim's 1.8 s linear period exactly — subtle, never strobing.
    #[must_use]
    pub fn rail_shimmer_style(&self, phase: u8) -> Style {
        let ink = if phase % 3 == 1 {
            self.maroon
        } else {
            self.gold
        };
        Style::default().fg(ink.into())
    }
}
