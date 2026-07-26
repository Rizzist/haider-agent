//! Theme→ratatui style bridge goldens.
#![allow(clippy::expect_used)]

use haider_tui::theme::{DAWN, Rgb, ThemeKey};
use ratatui::style::{Color, Modifier};

#[test]
fn rgb_converts_to_ratatui_truecolor() {
    let color: Color = Rgb {
        r: 154,
        g: 106,
        b: 8,
    }
    .into();
    assert_eq!(color, Color::Rgb(154, 106, 8));
}

#[test]
fn badge_styles_follow_the_state_vocabulary() {
    use haider_tui::projection::BadgeTone;
    for key in ThemeKey::ALL {
        let theme = key.theme();
        // Active work: gold fill with theme-bg ink.
        let active = theme.badge_style(BadgeTone::Active);
        assert_eq!(active.fg, Some(theme.badge_fg.into()));
        assert_eq!(active.bg, Some(theme.gold.into()));
        assert!(active.add_modifier.contains(Modifier::BOLD));
        // Restful states: outlined gold, no fill.
        let muted = theme.badge_style(BadgeTone::Muted);
        assert_eq!(muted.fg, Some(theme.gold.into()));
        assert_eq!(muted.bg, None);
        // Machinery / attention / failure fills.
        assert_eq!(
            theme.badge_style(BadgeTone::Tool).bg,
            Some(theme.maroon.into())
        );
        assert_eq!(
            theme.badge_style(BadgeTone::Attention).bg,
            Some(theme.warn.into())
        );
        assert_eq!(
            theme.badge_style(BadgeTone::Error).bg,
            Some(theme.err.into())
        );
    }
}

#[test]
fn text_style_grounds_on_the_theme_bg() {
    let style = DAWN.text_style();
    assert_eq!(style.fg, Some(Color::Rgb(0x5f, 0x4a, 0x2e)));
    assert_eq!(style.bg, Some(Color::Rgb(0xf3, 0xea, 0xd9)));
}

#[test]
fn selection_and_input_styles_use_their_soft_grounds() {
    for key in ThemeKey::ALL {
        let theme = key.theme();
        assert_eq!(theme.selection_style().bg, Some(theme.sel_bg.into()));
        assert_eq!(theme.input_style().bg, Some(theme.input_bg.into()));
    }
}
