//! Theme-registry goldens (UI-themes wave): key/name round-trips, the
//! designed solid tokens, and hand-computed blend oracles independent of
//! `Rgb::over`'s arithmetic. The contrast-floor laws live in
//! `ui_themes_tests.rs`.
#![allow(clippy::expect_used)]

use haider_tui::theme::{DARK, DESERT, LIGHT, OASIS, Rgb, Theme, ThemeChoice, ThemeKey};

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

#[test]
fn theme_keys_parse_and_roundtrip_in_menu_order() {
    // Dark is the registry default AND the detection fallback (owner §3).
    assert_eq!(ThemeKey::default(), ThemeKey::Dark);
    assert_eq!(
        ThemeKey::ALL,
        [
            ThemeKey::Light,
            ThemeKey::Dark,
            ThemeKey::Desert,
            ThemeKey::Water,
            ThemeKey::Oasis
        ]
    );
    for key in ThemeKey::ALL {
        assert_eq!(ThemeKey::parse(key.name()), Some(key));
        assert_eq!(key.theme().key, key);
    }
    assert_eq!(ThemeKey::parse("Dark"), None, "names are lowercase-exact");
    assert_eq!(
        ThemeKey::parse("system"),
        None,
        "system is a CHOICE, not a key"
    );
}

#[test]
fn legacy_sim_names_migrate_instead_of_resetting() {
    // A persisted `dawn`/`ivory` from the sim era lands on its successor.
    assert_eq!(ThemeKey::parse("dawn"), Some(ThemeKey::Desert));
    assert_eq!(ThemeKey::parse("ivory"), Some(ThemeKey::Light));
}

#[test]
fn theme_choice_parses_system_and_fixed_names() {
    assert_eq!(ThemeChoice::default(), ThemeChoice::System);
    assert_eq!(ThemeChoice::parse("system"), Some(ThemeChoice::System));
    assert_eq!(
        ThemeChoice::parse("oasis"),
        Some(ThemeChoice::Fixed(ThemeKey::Oasis))
    );
    assert_eq!(ThemeChoice::parse("nope"), None);
    assert_eq!(
        ThemeChoice::MENU,
        [
            ThemeChoice::System,
            ThemeChoice::Fixed(ThemeKey::Light),
            ThemeChoice::Fixed(ThemeKey::Dark),
            ThemeChoice::Fixed(ThemeKey::Desert),
            ThemeChoice::Fixed(ThemeKey::Water),
            ThemeChoice::Fixed(ThemeKey::Oasis),
        ]
    );
    for choice in ThemeChoice::MENU {
        assert_eq!(ThemeChoice::parse(choice.name()), Some(choice));
    }
    // Resolution: System follows detection; Fixed ignores it.
    assert_eq!(
        ThemeChoice::System.resolve(ThemeKey::Light),
        ThemeKey::Light
    );
    assert_eq!(ThemeChoice::System.resolve(ThemeKey::Dark), ThemeKey::Dark);
    assert_eq!(
        ThemeChoice::Fixed(ThemeKey::Desert).resolve(ThemeKey::Dark),
        ThemeKey::Desert
    );
}

#[test]
fn labels_and_families_match_the_design() {
    let expected = [
        (ThemeKey::Light, "Light", false),
        (ThemeKey::Dark, "Dark", true),
        (ThemeKey::Desert, "Desert", false),
        // Oasis is the desert's NIGHT garden — a dark-family palette.
        (ThemeKey::Oasis, "Oasis", true),
    ];
    for (key, label, dark) in expected {
        assert_eq!(key.theme().label, label);
        assert_eq!(key.theme().dark, dark, "{label} family");
    }
}

#[test]
fn hex_decodes_channels() {
    assert_eq!(Rgb::hex(0xfdfdfd), rgb(0xfd, 0xfd, 0xfd));
    assert_eq!(Rgb::hex(0x000000), rgb(0, 0, 0));
    assert_eq!(Rgb::hex(0xffffff), rgb(255, 255, 255));
}

#[test]
fn over_blends_extremes_exactly() {
    let base = Rgb::hex(0x0f0f0f);
    let ink = Rgb::hex(0xd9b544);
    assert_eq!(ink.over(base, 0), base, "alpha 0 keeps the ground");
    assert_eq!(ink.over(base, 1000), ink, "alpha 1 is the ink");
}

/// S2 flip: the warm paper (#faf6ee) and its warm-cast grays are RETIRED
/// per the owner's pearl-white spec — the ground is a barely-there pearl,
/// the ink neutral near-black, every gray neutral, the gold deepened for
/// the whiter page.
#[test]
fn light_solid_tokens_match_the_design_hexes() {
    assert_eq!(LIGHT.bg, Rgb::hex(0xfdfdfd), "pearl, not warm paper");
    assert_eq!(LIGHT.text, Rgb::hex(0x363636));
    assert_eq!(LIGHT.bright, Rgb::hex(0x171717));
    assert_eq!(LIGHT.dim, Rgb::hex(0x6e6e6e), "neutral gray, no warm cast");
    assert_eq!(LIGHT.faint, Rgb::hex(0xd9d9d9));
    assert_eq!(
        LIGHT.gold,
        Rgb::hex(0x7d5a05),
        "deepened for the white page"
    );
    assert_eq!(LIGHT.maroon, Rgb::hex(0x7c2d12));
    assert_eq!(LIGHT.badge_fg, LIGHT.bg);
}

#[test]
fn desert_solid_tokens_match_the_design_hexes() {
    assert_eq!(DESERT.bg, Rgb::hex(0xf0e4cc), "true sand, not cream");
    assert_eq!(DESERT.gold, Rgb::hex(0x9c6202), "burnished amber");
    assert_eq!(DESERT.maroon, Rgb::hex(0x7b2740), "dusk plum");
    assert_eq!(DESERT.text, Rgb::hex(0x4d3a20));
    assert_eq!(DESERT.badge_fg, DESERT.bg);
}

/// S2 flip: the warm-brown ground (#120e08) and warm cream text are
/// RETIRED per the owner's Codex-register spec — neutral charcoal ground,
/// neutral light-gray text, cool dim. The gold and ember ACCENTS are the
/// identity and stay byte-identical.
#[test]
fn dark_refresh_keeps_the_identity_hexes() {
    assert_eq!(
        DARK.bg,
        Rgb::hex(0x0f0f0f),
        "neutral charcoal, not warm brown"
    );
    assert_eq!(
        DARK.gold,
        Rgb::hex(0xd9b544),
        "aged gold — the kept identity"
    );
    assert_eq!(DARK.maroon, Rgb::hex(0xe57a4a), "ember — the kept identity");
    assert_eq!(
        DARK.text,
        Rgb::hex(0xd4d4d4),
        "neutral light gray, not cream"
    );
    assert_eq!(DARK.dim, Rgb::hex(0x8b949e), "cool dim (owner: 'cool dim')");
}

#[test]
fn oasis_solid_tokens_match_the_design_hexes() {
    assert_eq!(OASIS.bg, Rgb::hex(0x0c1410), "palm-night green-black");
    assert_eq!(OASIS.gold, Rgb::hex(0xd8b04c), "date gold");
    assert_eq!(OASIS.text, Rgb::hex(0xb9cfc0), "moonlit spring");
}

// Hand-computed blend oracles (ink alpha‰ over bg, round half-up) — an
// arithmetic witness independent of `Rgb::over`.

#[test]
fn light_blends_match_hand_computed_goldens() {
    assert_eq!(LIGHT.bar_bg, rgb(244, 244, 244)); // ink 4.0% over pearl
    assert_eq!(LIGHT.frame, rgb(184, 184, 184)); // ink 30%
    assert_eq!(LIGHT.gold_soft, rgb(241, 238, 231)); // gold 9%
    assert_eq!(LIGHT.input_bg, rgb(244, 244, 244)); // ink 4.0%
    assert_eq!(LIGHT.sel_bg, rgb(232, 232, 232)); // ink 9%
}

/// S2: every dark chrome blend now mixes from WHITE over the charcoal —
/// the gold-tinted bar/frame/selection grounds were the "warm wash" the
/// owner retired. The probe ladder's raw SGR constants track these bytes
/// (scripts/tui-probes: sel_bg 39;39;39 · bar_bg 25;25;25).
#[test]
fn dark_blends_match_hand_computed_goldens() {
    assert_eq!(DARK.bar_bg, rgb(25, 25, 25)); // white 4%
    assert_eq!(DARK.frame, rgb(68, 68, 68)); // white 22%
    assert_eq!(DARK.gold_soft, rgb(39, 35, 21)); // gold 12% — the accent's soft ground
    assert_eq!(DARK.input_bg, rgb(26, 26, 26)); // white 4.5%
    assert_eq!(DARK.sel_bg, rgb(39, 39, 39)); // white 10%
}

#[test]
fn desert_blends_match_hand_computed_goldens() {
    assert_eq!(DESERT.bar_bg, rgb(234, 219, 197)); // plum 5%
    assert_eq!(DESERT.frame, rgb(207, 175, 165)); // plum 28%
    assert_eq!(DESERT.gold_soft, rgb(228, 210, 176)); // amber 14%
    assert_eq!(DESERT.input_bg, rgb(244, 236, 218)); // white 28%
    assert_eq!(DESERT.sel_bg, rgb(228, 209, 190)); // plum 10%
}

#[test]
fn oasis_blends_match_hand_computed_goldens() {
    assert_eq!(OASIS.bar_bg, rgb(22, 28, 19)); // gold 5%
    assert_eq!(OASIS.frame, rgb(65, 61, 32)); // gold 26%
    assert_eq!(OASIS.gold_soft, rgb(36, 39, 23)); // gold 12%
    assert_eq!(OASIS.input_bg, rgb(23, 31, 27)); // white 4.5%
    assert_eq!(OASIS.sel_bg, rgb(32, 36, 22)); // gold 10%
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

/// MUTATION CHECK: wash any ink token toward its ground (e.g. water `dim`
/// to `#c5d8dc`, ~1.3:1). Expected RUNTIME failure: the WCAG floor names
/// the theme and token. The original palette pass verified these ratios
/// out-of-band; this law makes them regression-proof — mere distinctness
/// (`token != bg`) let a washed token through.
#[test]
fn every_theme_clears_wcag_contrast_floors() {
    fn luminance(c: haider_tui::theme::Rgb) -> f64 {
        let (r, g, b) = (c.r, c.g, c.b);
        let chan = |v: u8| {
            let s = f64::from(v) / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * chan(r) + 0.7152 * chan(g) + 0.0722 * chan(b)
    }
    fn ratio(a: haider_tui::theme::Rgb, b: haider_tui::theme::Rgb) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }
    for key in ThemeKey::ALL {
        let theme: &Theme = key.theme();
        for (name, token, floor) in [
            ("text", theme.text, 7.0),
            ("bright", theme.bright, 9.0),
            ("dim", theme.dim, 3.0),
            ("gold", theme.gold, 3.0),
            ("maroon", theme.maroon, 3.0),
            ("ok", theme.ok, 3.0),
            ("warn", theme.warn, 3.0),
            ("err", theme.err, 3.0),
        ] {
            let measured = ratio(token, theme.bg);
            assert!(
                measured >= floor,
                "{}: {name} is {measured:.2}:1 on the ground, floor {floor}:1",
                theme.label
            );
        }
    }
}

/// MUTATION CHECK: hand-roll `MENU` again and drop a registry theme.
/// Expected RUNTIME failure: the covering pin names the missing key —
/// the v0.0.62 live probe found `water` absent from the picker while
/// every suite law read MENU itself (self-referential coverage).
#[test]
fn picker_menu_covers_every_registry_theme_once() {
    use haider_tui::theme::ThemeChoice;
    assert_eq!(ThemeChoice::MENU.len(), ThemeKey::ALL.len() + 1);
    assert_eq!(ThemeChoice::MENU[0], ThemeChoice::System);
    for key in ThemeKey::ALL {
        assert_eq!(
            ThemeChoice::MENU
                .iter()
                .filter(|choice| **choice == ThemeChoice::Fixed(key))
                .count(),
            1,
            "{} must appear exactly once in the picker",
            key.name()
        );
    }
}
