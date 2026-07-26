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

    /// The filled state badge (gold ground, theme-bg ink).
    #[must_use]
    pub fn badge_style(&self) -> Style {
        Style::default()
            .fg(self.badge_fg.into())
            .bg(self.gold.into())
            .add_modifier(Modifier::BOLD)
    }

    /// The composer input field.
    #[must_use]
    pub fn input_style(&self) -> Style {
        Style::default()
            .fg(self.bright.into())
            .bg(self.input_bg.into())
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
}
