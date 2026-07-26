//! Sanctum dignity rules + boot checklist projection goldens.
#![allow(clippy::expect_used)]

use haider_protocol::state::ReadinessCheck;
use haider_tui::boot::{CheckMarker, boot_subline, check_rows, launcher_subline};
use haider_tui::sanctum::{SHAHADA_ARABIC, SHAHADA_TRANSLIT, SanctumLine, SanctumTier};

fn check(name: &str, ok: bool) -> ReadinessCheck {
    ReadinessCheck {
        name: name.to_owned(),
        ok,
        duration_ms: 0,
    }
}

#[test]
fn sanctum_tiers_expose_their_texts() {
    assert_eq!(SanctumLine::new(SanctumTier::Arabic).text(), SHAHADA_ARABIC);
    assert_eq!(
        SanctumLine::new(SanctumTier::Translit).text(),
        SHAHADA_TRANSLIT
    );
    assert_eq!(
        SanctumTier::default(),
        SanctumTier::Translit,
        "terminal default is the shaping-safe tier"
    );
    assert_eq!(SanctumLine::new(SanctumTier::Arabic).mark(), "حيدر");
    assert_eq!(SanctumLine::new(SanctumTier::Translit).mark(), "ḤAYDAR");
}

#[test]
fn sanctum_renders_whole_or_not_at_all() {
    let line = SanctumLine::new(SanctumTier::Translit);
    let width = SHAHADA_TRANSLIT.chars().count();
    assert_eq!(line.fit(width), Some(SHAHADA_TRANSLIT));
    assert_eq!(line.fit(width + 20), Some(SHAHADA_TRANSLIT));
    assert_eq!(
        line.fit(width - 1),
        None,
        "never truncated, never ellipsized"
    );
    assert_eq!(line.fit(0), None);
}

#[test]
fn translit_tier_is_shaping_safe() {
    // No RTL codepoints, no Arabic block — safe in terminals that cannot
    // shape Arabic; this is what makes the tier dignified, not degraded.
    assert!(
        SHAHADA_TRANSLIT
            .chars()
            .all(|c| (c as u32) < 0x0600 || (c as u32) > 0x06FF)
    );
    assert!(!SHAHADA_TRANSLIT.contains('\u{200f}'));
}

#[test]
fn check_rows_walk_done_current_pending() {
    let checks = vec![
        check("store open · journal replayed", true),
        check("provider handshake", true),
        check("hooks loaded", false),
        check("worker warm · mesh probe", false),
    ];
    let rows = check_rows(&checks);
    assert_eq!(
        rows.iter().map(|r| r.marker).collect::<Vec<_>>(),
        vec![
            CheckMarker::Done,
            CheckMarker::Done,
            CheckMarker::Current,
            CheckMarker::Pending
        ]
    );
    assert_eq!(rows[0].line(), "✓ store open · journal replayed");
    assert_eq!(rows[2].line(), "◌ hooks loaded");
    assert_eq!(rows[3].line(), "· worker warm · mesh probe");
}

#[test]
fn all_ok_checks_have_no_current() {
    let checks = [check("a", true), check("b", true)];
    let rows = check_rows(&checks);
    assert!(rows.iter().all(|r| r.marker == CheckMarker::Done));
    assert!(check_rows(&[]).is_empty());
}

#[test]
fn sublines_match_the_sim_shapes() {
    assert_eq!(boot_subline("0.0.5"), "v0.0.5 · starting up");
    assert_eq!(launcher_subline("0.0.5"), "v0.0.5 · the lion");
}
