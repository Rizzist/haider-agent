//! Theme-token goldens: the blended values below were computed by hand from
//! the sim's `rgba()` literals (`next-diffforge/src/pages/tui.js`, `THEMES`),
//! so they are an oracle independent of `Rgb::over`'s arithmetic.
#![allow(clippy::expect_used)]

use haider_tui::theme::{DARK, DAWN, IVORY, Rgb, Theme, ThemeKey};

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

#[test]
fn theme_keys_parse_and_roundtrip_in_menu_order() {
    assert_eq!(ThemeKey::default(), ThemeKey::Dawn);
    assert_eq!(
        ThemeKey::ALL,
        [ThemeKey::Dawn, ThemeKey::Ivory, ThemeKey::Dark]
    );
    for key in ThemeKey::ALL {
        assert_eq!(ThemeKey::parse(key.name()), Some(key));
        assert_eq!(key.theme().key, key);
    }
    assert_eq!(ThemeKey::parse("desert"), None);
    assert_eq!(ThemeKey::parse("Dawn"), None, "names are lowercase-exact");
}

#[test]
fn labels_match_the_sim() {
    assert_eq!(DAWN.label, "Desert Dawn");
    assert_eq!(IVORY.label, "Ivory Light");
    assert_eq!(DARK.label, "Dark");
}

#[test]
fn hex_decodes_channels() {
    assert_eq!(Rgb::hex(0xf3ead9), rgb(0xf3, 0xea, 0xd9));
    assert_eq!(Rgb::hex(0x000000), rgb(0, 0, 0));
    assert_eq!(Rgb::hex(0xffffff), rgb(255, 255, 255));
}

#[test]
fn over_blends_extremes_exactly() {
    let base = Rgb::hex(0x14100a);
    let ink = Rgb::hex(0xd4af37);
    assert_eq!(ink.over(base, 0), base, "alpha 0 keeps the ground");
    assert_eq!(ink.over(base, 1000), ink, "alpha 1 is the ink");
}

#[test]
fn dawn_solid_tokens_match_the_sim_hexes() {
    assert_eq!(DAWN.bg, Rgb::hex(0xf3ead9));
    assert_eq!(DAWN.text, Rgb::hex(0x5f4a2e));
    assert_eq!(DAWN.bright, Rgb::hex(0x3d2d18));
    assert_eq!(DAWN.dim, Rgb::hex(0xa08a63));
    assert_eq!(DAWN.faint, Rgb::hex(0xcdbd9a));
    assert_eq!(DAWN.gold, Rgb::hex(0x9a6a08));
    assert_eq!(DAWN.maroon, Rgb::hex(0x7c2d12));
    assert_eq!(DAWN.ok, Rgb::hex(0x4c7a45));
    assert_eq!(DAWN.warn, Rgb::hex(0xb45309));
    assert_eq!(DAWN.err, Rgb::hex(0xb91c1c));
    assert_eq!(DAWN.badge_fg, DAWN.bg);
}

#[test]
fn dawn_blends_match_hand_computed_goldens() {
    assert_eq!(DAWN.bar_bg, rgb(237, 225, 207)); // rgba(maroon, .05)
    assert_eq!(DAWN.frame, rgb(210, 181, 161)); // rgba(maroon, .28)
    assert_eq!(DAWN.gold_soft, rgb(231, 216, 188)); // rgba(gold, .14)
    assert_eq!(DAWN.maroon_soft, rgb(233, 219, 201)); // rgba(maroon, .08)
    assert_eq!(DAWN.input_bg, rgb(246, 240, 228)); // rgba(white, .28)
    assert_eq!(DAWN.sel_bg, rgb(231, 215, 197)); // rgba(maroon, .10)
}

#[test]
fn ivory_blends_match_hand_computed_goldens() {
    assert_eq!(IVORY.bg, Rgb::hex(0xfbf9f3));
    assert_eq!(IVORY.bar_bg, rgb(242, 240, 234)); // rgba(ink, .04)
    assert_eq!(IVORY.frame, rgb(187, 184, 178)); // rgba(ink, .30)
    assert_eq!(IVORY.gold_soft, rgb(237, 231, 215)); // rgba(gold, .12)
    assert_eq!(IVORY.maroon_soft, rgb(243, 235, 227)); // rgba(maroon, .07)
    assert_eq!(IVORY.input_bg, rgb(244, 241, 235)); // rgba(ink, .035)
    assert_eq!(IVORY.sel_bg, rgb(234, 232, 226)); // rgba(ink, .08)
}

#[test]
fn dark_blends_match_hand_computed_goldens() {
    assert_eq!(DARK.bg, Rgb::hex(0x14100a));
    assert_eq!(DARK.bar_bg, rgb(30, 24, 12)); // rgba(gold, .05)
    assert_eq!(DARK.frame, rgb(70, 57, 22)); // rgba(gold, .26)
    assert_eq!(DARK.gold_soft, rgb(43, 35, 15)); // rgba(gold, .12)
    assert_eq!(DARK.maroon_soft, rgb(40, 26, 15)); // rgba(maroon, .10)
    assert_eq!(DARK.input_bg, rgb(31, 27, 21)); // rgba(white, .045)
    assert_eq!(DARK.sel_bg, rgb(39, 32, 15)); // rgba(gold, .10)
}

#[test]
fn every_theme_keeps_ink_tokens_distinct_from_its_ground() {
    for key in ThemeKey::ALL {
        let theme: &Theme = key.theme();
        for (name, token) in [
            ("frame", theme.frame),
            ("text", theme.text),
            ("bright", theme.bright),
            ("dim", theme.dim),
            ("gold", theme.gold),
            ("maroon", theme.maroon),
            ("ok", theme.ok),
            ("warn", theme.warn),
            ("err", theme.err),
        ] {
            assert_ne!(
                token, theme.bg,
                "{}: {name} must be visible on the ground",
                theme.label
            );
        }
    }
}
