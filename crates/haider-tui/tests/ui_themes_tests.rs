//! UI-themes wave laws (owner spec, ui-themes branch):
//!   §1 launcher-as-session — a compact header band (wordmark · version ·
//!      device) over a top-aligned content column; the BIG centered art and
//!      the shahada stay on the boot splash EXACTLY as before.
//!   §2 palettes — a deliberately designed light mode, a refreshed dark,
//!      `desert` and `oasis`; every surface legible (contrast floors).
//!   §3 theme system — system-default detection (COLORFGBG / OSC 11,
//!      undetectable → dark), the `/theme` numbered arrow-highlight picker,
//!      TUI-local persistence in the profile dir.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppModel, Hit, Screen};
use haider_tui::render::render;
use haider_tui::sanctum::SHAHADA_ARABIC;
use haider_tui::theme::{ThemeChoice, ThemeKey};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

mod common;
use common::launcher_model;

fn draw(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<(Rect, Hit)>, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rows.push(line);
    }
    (rows, hits, terminal)
}

// ---- §1: launcher-as-session layout ----

#[test]
fn launcher_renders_header_band_not_centered_art() {
    let model = launcher_model();
    assert_eq!(model.screen, Screen::Launcher);
    let (rows, _, _) = draw(&model, 100, 30);
    // The header band sits AT THE TOP: the compact 24×2 mark art spans
    // band lines 0-1, the product mark + version + device beside it.
    let art = haider_tui::mark::header_rows();
    assert!(
        rows[0].contains(art[0].trim_end()),
        "compact mark, band line 1:\n{}",
        rows.join("\n")
    );
    assert!(rows[1].contains(art[1].trim_end()), "compact mark, line 2");
    assert!(rows[0].contains("haider v"), "wordmark + version in band");
    assert!(
        rows[0].contains(&model.identity.device),
        "device name in band"
    );
    assert!(
        rows[1].contains("provider anthropic"),
        "identity info on band line 2"
    );
    // Band line 3 is the closing frame rule.
    assert!(
        rows[2].chars().filter(|c| *c == '─').count() as u16 >= 90,
        "the band closes with a full-width rule"
    );
    // NO centered art: the 28×4 banner and the shahada are boot ceremony.
    let banner = haider_tui::mark::banner_rows();
    assert!(
        !rows.iter().any(|row| row.contains(banner[2].trim_end())),
        "the big banner may not render on the launcher"
    );
    assert!(
        !rows.iter().any(|row| row.contains(SHAHADA_ARABIC)),
        "the shahada may not render on the launcher"
    );
    // The content column is TOP-ALIGNED under the band, not centered:
    // the recent-sessions head sits directly below the rule's breathing
    // row, in the top quarter of a 30-row frame.
    let recent_y = rows
        .iter()
        .position(|row| row.contains("recent sessions"))
        .expect("recent sessions head");
    assert!(
        recent_y <= 6,
        "content is top-aligned under the band (got row {recent_y})"
    );
}

#[test]
fn boot_splash_keeps_centered_shahada() {
    // §1's second half: ONLY the settled launcher changed — the boot
    // splash keeps the big centered art and the shahada exactly as today.
    let model = AppModel::new();
    assert_eq!(model.screen, Screen::Boot);
    let (rows, _, _) = draw(&model, 100, 30);
    let banner = haider_tui::mark::banner_rows();
    for row in &banner {
        assert!(
            rows.iter().any(|line| line.contains(row.trim_end())),
            "boot keeps the whole big banner (row {row:?})"
        );
    }
    // Centering is measured on the WHOLE 28-cell art block (the map rows
    // carry internal leading blanks that are part of the block).
    let art_ink = banner[2].trim();
    let internal_left = banner[2].chars().take_while(|c| *c == ' ').count();
    let banner_y = rows
        .iter()
        .position(|row| row.contains(art_ink))
        .expect("banner row");
    let ink_col = rows[banner_y].find(art_ink).expect("ink column");
    let ink_col = rows[banner_y][..ink_col].chars().count();
    let block_left = ink_col - internal_left;
    let block_right = 100 - block_left - haider_tui::mark::BANNER_COLS as usize;
    assert!(
        block_left.abs_diff(block_right) <= 2,
        "boot art stays CENTERED (left {block_left}, right {block_right})"
    );
    // The shahada renders on the boot splash once the boot script carries
    // it? No — boot has never drawn the shahada; the LAUNCHER did. The
    // owner's directive keeps the big-art + shahada ceremony in the
    // boot/loading splash: the shahada now renders there, whole or not at
    // all (dignity rule 2).
    assert!(
        rows.iter().any(|row| row.contains(SHAHADA_ARABIC)),
        "the shahada lives on the boot splash:\n{}",
        rows.join("\n")
    );
}

// ---- §2: the palettes — every surface legible, no raw colors ----

/// WCAG relative luminance — an oracle independent of the theme module.
fn luminance(rgb: haider_tui::theme::Rgb) -> f64 {
    fn channel(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

fn contrast(a: haider_tui::theme::Rgb, b: haider_tui::theme::Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[test]
fn every_theme_clears_the_contrast_floors() {
    // Owner spec §2: "every surface must be legible". The floors are the
    // design contract each palette was built against — body ink reads at
    // AAA-body strength, metadata at large-text strength, accents and
    // state inks at UI-component strength, on EVERY ground they render on.
    for key in haider_tui::theme::ThemeKey::ALL {
        let theme = key.theme();
        let label = theme.label;
        let floor = |name: &str, ink, ground, min: f64| {
            let ratio = contrast(ink, ground);
            assert!(
                ratio >= min,
                "{label}: {name} contrast {ratio:.2} under the {min} floor"
            );
        };
        // Inks on the page ground.
        floor("text", theme.text, theme.bg, 6.5);
        floor("bright", theme.bright, theme.bg, 8.0);
        floor("dim", theme.dim, theme.bg, 3.4);
        floor("gold", theme.gold, theme.bg, 3.2);
        floor("maroon", theme.maroon, theme.bg, 4.0);
        floor("ok", theme.ok, theme.bg, 3.2);
        floor("warn", theme.warn, theme.bg, 3.2);
        floor("err", theme.err, theme.bg, 3.2);
        // Faint is DESIGNED barely-there: present, never body-legible.
        let faint = contrast(theme.faint, theme.bg);
        assert!(
            (1.35..=2.8).contains(&faint),
            "{label}: faint {faint:.2} outside its barely-there band"
        );
        // Frame chrome: visible but quiet.
        let frame = contrast(theme.frame, theme.bg);
        assert!(
            (1.5..=3.5).contains(&frame),
            "{label}: frame {frame:.2} outside its quiet band"
        );
        // Filled badges: the badge_fg must read on every fill tone.
        floor("badge_fg on gold", theme.badge_fg, theme.gold, 2.7);
        floor("badge_fg on maroon", theme.badge_fg, theme.maroon, 2.7);
        floor("badge_fg on warn", theme.badge_fg, theme.warn, 2.7);
        floor("badge_fg on err", theme.badge_fg, theme.err, 2.7);
        // Tinted grounds: composer band, selection/hover, menu card,
        // sticky bar — the inks that render there must survive the tint.
        floor("bright on input_bg", theme.bright, theme.input_bg, 7.0);
        floor("gold on input_bg", theme.gold, theme.input_bg, 3.0);
        floor("bright on sel_bg", theme.bright, theme.sel_bg, 6.0);
        floor("dim on sel_bg", theme.dim, theme.sel_bg, 2.8);
        floor("maroon on sel_bg", theme.maroon, theme.sel_bg, 3.5);
        floor("text on gold_soft", theme.text, theme.gold_soft, 5.0);
        floor("bright on gold_soft", theme.bright, theme.gold_soft, 6.0);
        floor("bright on bar_bg", theme.bright, theme.bar_bg, 6.5);
        // F2d markdown pairs: the inline-code pill (gold on gold_soft),
        // the code-block interior (body ink on the bar tint), and the
        // fence rule (metadata ink on the bar tint) must read on every
        // theme — the markdown renderer leans on exactly these pairs.
        floor("gold on gold_soft", theme.gold, theme.gold_soft, 3.2);
        floor("text on bar_bg", theme.text, theme.bar_bg, 6.5);
        floor("dim on bar_bg", theme.dim, theme.bar_bg, 3.0);
    }
}

#[test]
fn every_surface_uses_theme_slots() {
    // Mechanical enforcement of the no-raw-color law: in the themed render
    // path, every color routes through the Theme's semantic slots.
    //   * `Color::` may appear ONLY in style.rs — the single Rgb→ratatui
    //     seam (and the one place pulse() re-reads a style's channels).
    //   * `Rgb::hex(` may appear ONLY in theme.rs — palette definitions.
    //   * plain.rs stays entirely colorless (the piped-output law).
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut scanned = 0usize;
    for entry in std::fs::read_dir(&src).expect("src dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let body = std::fs::read_to_string(&path).expect("read source");
        scanned += 1;
        if name != "style.rs" {
            assert!(
                !body.contains("Color::"),
                "{name}: raw `Color::` outside the style seam — route it through a Theme slot"
            );
        }
        if name != "theme.rs" {
            assert!(
                !body.contains("Rgb::hex("),
                "{name}: raw `Rgb::hex(` outside the palette registry"
            );
        }
        if name == "plain.rs" {
            assert!(
                !body.contains("Style") && !body.contains("Color"),
                "plain.rs must stay colorless (piped-output law)"
            );
        }
    }
    assert!(scanned > 20, "the sweep actually walked the render path");
}

#[test]
fn narrow_boot_omits_the_shahada_whole() {
    // Dignity rule 2 travels with the shahada to its new home: at 24
    // columns no fragment of it may appear — whole or nothing.
    let model = AppModel::new();
    let (rows, _, _) = draw(&model, 24, 20);
    for word in ["الله", "محمد", "رسول"] {
        assert!(
            !rows.iter().any(|row| row.contains(word)),
            "sanctum fragment leaked into a narrow boot frame"
        );
    }
}

// ---- §3: the theme system — picker, persistence, detection ----

#[test]
fn theme_picker_lists_and_switches_instantly() {
    let mut model = launcher_model();
    assert_eq!(model.theme_choice, ThemeChoice::System);
    assert_eq!(model.theme, ThemeKey::Dark, "system fell back to dark");
    common::run_slash(&mut model, "/theme");
    assert!(model.theme_picker.is_some(), "bare /theme opens the picker");
    // The card lists every choice, numbered, ● on the committed row.
    let (rows, hits, _) = draw(&model, 100, 30);
    for needle in [
        "1. ● system",
        "2. ○ light",
        "3. ○ dark",
        "4. ○ desert",
        "5. ○ water",
        "6. ○ oasis",
    ] {
        assert!(
            rows.iter().any(|row| row.contains(needle)),
            "picker row {needle:?} missing:\n{}",
            rows.join("\n")
        );
    }
    // Every row is clickable (owner menu law: answer by number OR click).
    for index in 0..5 {
        assert!(
            hits.iter().any(|(_, hit)| *hit == Hit::ThemeOption(index)),
            "picker row {index} has no hit"
        );
    }
    // Moving the highlight PREVIEWS instantly — the resolved theme flips
    // with the row while the committed choice waits.
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Down));
    assert_eq!(model.theme, ThemeKey::Light, "row 2 previews light");
    assert_eq!(
        model.theme_choice,
        ThemeChoice::System,
        "preview commits nothing"
    );
    // A digit commits instantly and closes the picker.
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Char('4')));
    assert!(model.theme_picker.is_none(), "digit committed and closed");
    assert_eq!(model.theme_choice, ThemeChoice::Fixed(ThemeKey::Desert));
    assert_eq!(model.theme, ThemeKey::Desert);
    // esc reverts a preview to the choice held on open.
    common::run_slash(&mut model, "/theme");
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Down));
    assert_eq!(model.theme, ThemeKey::Water, "previewing the next row");
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Esc));
    assert!(model.theme_picker.is_none());
    assert_eq!(model.theme, ThemeKey::Desert, "esc reverted the preview");
    assert_eq!(model.theme_choice, ThemeChoice::Fixed(ThemeKey::Desert));
    // A click commits like a digit (value-carrying hit).
    common::run_slash(&mut model, "/theme");
    model.handle_hit(Hit::ThemeOption(5));
    assert!(model.theme_picker.is_none());
    assert_eq!(model.theme_choice, ThemeChoice::Fixed(ThemeKey::Oasis));
    assert_eq!(model.theme, ThemeKey::Oasis);
}

#[test]
fn theme_persists_and_reloads() {
    use haider_tui::settings::SettingsStore;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tui-settings.json");
    // Save-if-changed writes the choice; a fresh store reloads it.
    let mut store = SettingsStore::at(path.clone());
    assert_eq!(store.load(), None, "no file yet → defaults");
    store.save_if_changed(ThemeChoice::Fixed(ThemeKey::Desert));
    assert_eq!(
        SettingsStore::at(path.clone()).load(),
        Some(ThemeChoice::Fixed(ThemeKey::Desert))
    );
    // `system` round-trips as a CHOICE — the resolved theme is never
    // persisted, so the next boot re-evaluates the terminal.
    store.save_if_changed(ThemeChoice::System);
    assert_eq!(
        SettingsStore::at(path.clone()).load(),
        Some(ThemeChoice::System)
    );
    // Corrupt bytes and foreign versions mean defaults, never damage.
    std::fs::write(&path, b"{not json").expect("write");
    assert_eq!(SettingsStore::at(path.clone()).load(), None);
    std::fs::write(&path, br#"{"version":9,"theme":"desert"}"#).expect("write");
    assert_eq!(SettingsStore::at(path.clone()).load(), None);
    // A legacy sim-era name migrates through the same parse gate.
    std::fs::write(&path, br#"{"version":1,"theme":"dawn"}"#).expect("write");
    assert_eq!(
        SettingsStore::at(path.clone()).load(),
        Some(ThemeChoice::Fixed(ThemeKey::Desert))
    );
    // And the model applies a reloaded choice exactly as boot does.
    let mut model = launcher_model();
    model.detected_system = ThemeKey::Light;
    let choice = SettingsStore::at(path).load().expect("choice");
    model.apply_theme_choice(choice);
    assert_eq!(
        model.theme,
        ThemeKey::Desert,
        "fixed choice ignores detection"
    );
    model.apply_theme_choice(ThemeChoice::System);
    assert_eq!(model.theme, ThemeKey::Light, "system follows detection");
}

#[test]
fn system_theme_follows_detection_fallback_dark() {
    use haider_tui::runtime::{TerminalAppearance, resolve_system_theme, theme_from_colorfgbg};
    // OSC 11 is the authority when the emulator answers.
    assert_eq!(
        resolve_system_theme(Some(TerminalAppearance::Light), None),
        ThemeKey::Light
    );
    assert_eq!(
        resolve_system_theme(Some(TerminalAppearance::Dark), Some("0;15")),
        ThemeKey::Dark,
        "an answered OSC beats COLORFGBG"
    );
    // COLORFGBG fallback: the LAST field is the background index.
    assert_eq!(theme_from_colorfgbg("15;0"), Some(ThemeKey::Dark));
    assert_eq!(theme_from_colorfgbg("0;15"), Some(ThemeKey::Light));
    assert_eq!(theme_from_colorfgbg("0;default;7"), Some(ThemeKey::Light));
    assert_eq!(theme_from_colorfgbg("default;default"), None);
    assert_eq!(theme_from_colorfgbg("garbage"), None);
    assert_eq!(
        resolve_system_theme(None, Some("15;0")),
        ThemeKey::Dark,
        "COLORFGBG answers when OSC cannot"
    );
    // Undetectable → dark (the owner's fallback law).
    assert_eq!(resolve_system_theme(None, None), ThemeKey::Dark);
    assert_eq!(resolve_system_theme(None, Some("nonsense")), ThemeKey::Dark);
    // And the choice layer: `system` resolves against exactly that.
    assert_eq!(
        ThemeChoice::System.resolve(resolve_system_theme(None, None)),
        ThemeKey::Dark
    );
}

// ---- ui-themes-fix: the live probe's surface + persistence gaps ----

/// The NATURAL typed flow — `/theme` then ⏎ with the palette open, no
/// dismissal. This is exactly what the live probe typed at the launcher
/// and what the exact-match lead jump used to hijack onto the `system`
/// arg row.
fn type_theme_enter(model: &mut AppModel) {
    for c in "/theme".chars() {
        model.handle(common::key(ratatui::crossterm::event::KeyCode::Char(c)));
    }
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));
}

/// Non-degenerate picker check: the ROWS render in the frame, not just a
/// state flag.
fn assert_picker_rows(model: &AppModel, surface: &str) {
    assert!(
        model.theme_picker.is_some(),
        "{surface}: /theme + ⏎ must open the picker"
    );
    let (rows, hits, _) = draw(model, 100, 30);
    for needle in ["1. ● system", "3. ○ dark", "6. ○ oasis"] {
        assert!(
            rows.iter().any(|row| row.contains(needle)),
            "{surface}: picker row {needle:?} not RENDERED:\n{}",
            rows.join("\n")
        );
    }
    assert!(
        hits.iter().any(|(_, hit)| *hit == Hit::ThemeOption(0)),
        "{surface}: picker rows must be clickable"
    );
}

#[test]
fn theme_picker_opens_on_every_composer_surface() {
    // Launcher — the owner's primary surface (many terminals open).
    let mut model = launcher_model();
    type_theme_enter(&mut model);
    assert_picker_rows(&model, "launcher");

    // Session.
    let mut model = launcher_model();
    common::hit_session_named(&mut model, "billing-service");
    assert_eq!(model.screen, Screen::Session);
    type_theme_enter(&mut model);
    assert_picker_rows(&model, "session");

    // Aura — its composer runs the same slash dispatch.
    let mut model = launcher_model();
    model.handle_hit(Hit::ExtraRow(haider_tui::app::LauncherRow::Aura));
    assert_eq!(model.screen, Screen::Aura);
    type_theme_enter(&mut model);
    assert_picker_rows(&model, "aura");

    // Subagent — the chip view's composer, when no question card owns it.
    let mut model = launcher_model();
    common::hit_session_named(&mut model, "l1-remote-projects");
    let agent = model.chips[0].agent.clone();
    model.handle_hit(Hit::ChipRow(agent));
    assert_eq!(model.screen, Screen::Subagent);
    type_theme_enter(&mut model);
    assert_picker_rows(&model, "subagent");
}

#[test]
fn theme_commit_persists_from_the_launcher_flow() {
    use haider_tui::runtime::sync_theme_persistence;
    use haider_tui::settings::SettingsStore;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tui-settings.json");
    let mut settings = Some(SettingsStore::at(path.clone()));

    // Boot: resolution applies the default choice WITHOUT committing —
    // the runtime sees zero commits and writes nothing.
    let mut model = launcher_model();
    let mut seen = model.theme_commits;
    sync_theme_persistence(&model, &mut seen, &mut settings);
    assert!(!path.exists(), "boot resolution never writes the file");

    // Preview inside the open picker: still no commit, still no file.
    type_theme_enter(&mut model);
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Down));
    sync_theme_persistence(&model, &mut seen, &mut settings);
    assert!(!path.exists(), "previews never write the file");

    // Commit `system` — the boot DEFAULT — from the launcher picker (the
    // live probe's exact flow: no choice diff, yet the file must land).
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Char('1')));
    assert_eq!(model.theme_choice, ThemeChoice::System);
    sync_theme_persistence(&model, &mut seen, &mut settings);
    assert_eq!(
        SettingsStore::at(path.clone()).load(),
        Some(ThemeChoice::System),
        "a commit that re-affirms the boot default still persists"
    );

    // And a different commit updates it.
    type_theme_enter(&mut model);
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Char('4')));
    sync_theme_persistence(&model, &mut seen, &mut settings);
    assert_eq!(
        SettingsStore::at(path).load(),
        Some(ThemeChoice::Fixed(ThemeKey::Desert))
    );
}
