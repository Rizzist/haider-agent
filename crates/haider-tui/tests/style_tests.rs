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
fn badge_style_is_bold_gold_ground_with_theme_ink() {
    for key in ThemeKey::ALL {
        let theme = key.theme();
        let style = theme.badge_style();
        assert_eq!(style.fg, Some(theme.badge_fg.into()));
        assert_eq!(style.bg, Some(theme.gold.into()));
        assert!(style.add_modifier.contains(Modifier::BOLD));
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
