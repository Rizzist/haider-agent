//! Goldens for the sim-parity formatters (`fmtTok` / `meterCells` ports).
#![allow(clippy::expect_used)]

use haider_tui::format::{METER_CELLS_DEFAULT, fmt_tok, meter_cells};

#[test]
fn fmt_tok_matches_sim_tiers() {
    // < 1k: verbatim.
    assert_eq!(fmt_tok(0), "0");
    assert_eq!(fmt_tok(842), "842");
    assert_eq!(fmt_tok(999), "999");
    // 1k..10k: one decimal, trailing .0 kept (sim quirk).
    assert_eq!(fmt_tok(1000), "1.0k");
    assert_eq!(fmt_tok(1234), "1.2k");
    assert_eq!(fmt_tok(9000), "9.0k");
    assert_eq!(fmt_tok(9940), "9.9k");
    // 10k..1M: whole k.
    assert_eq!(fmt_tok(10_000), "10k");
    assert_eq!(fmt_tok(131_072), "131k");
    assert_eq!(fmt_tok(200_000), "200k");
    assert_eq!(fmt_tok(400_000), "400k");
    // ≥ 1M: one decimal in M, trailing .0 stripped.
    assert_eq!(fmt_tok(1_000_000), "1M");
    assert_eq!(fmt_tok(1_050_000), "1.1M");
    assert_eq!(fmt_tok(1_500_000), "1.5M");
    assert_eq!(fmt_tok(2_000_000), "2M");
}

#[test]
fn fmt_tok_keeps_the_sim_boundary_quirk() {
    // The sim never promotes 999,999 to the M tier: `1000k`, faithfully kept.
    assert_eq!(fmt_tok(999_999), "1000k");
}

#[test]
fn meter_cells_renders_full_and_empty_cells() {
    assert_eq!(meter_cells(0.0, 10), "▱▱▱▱▱▱▱▱▱▱");
    assert_eq!(meter_cells(1.0, 10), "▰▰▰▰▰▰▰▰▰▰");
    assert_eq!(meter_cells(0.5, 10), "▰▰▰▰▰▱▱▱▱▱");
    assert_eq!(meter_cells(0.24, 10), "▰▰▱▱▱▱▱▱▱▱");
    assert_eq!(meter_cells(0.25, 10), "▰▰▰▱▱▱▱▱▱▱", "half rounds up");
    assert_eq!(meter_cells(0.33, 12), "▰▰▰▰▱▱▱▱▱▱▱▱");
}

#[test]
fn meter_cells_clamps_out_of_range_ratios() {
    assert_eq!(meter_cells(1.7, 4), "▰▰▰▰");
    assert_eq!(meter_cells(-0.3, 4), "▱▱▱▱");
}

#[test]
fn default_meter_width_matches_the_sim() {
    assert_eq!(METER_CELLS_DEFAULT, 10);
    assert_eq!(meter_cells(0.62, METER_CELLS_DEFAULT).chars().count(), 10);
}
