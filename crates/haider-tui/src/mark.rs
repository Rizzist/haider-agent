//! The حيدر wordmark as half-block pixel art.
//!
//! WHY: a terminal cell cannot scale a glyph, so the Arabic mark rendered as
//! text is stuck at one cell tall — the owner's screenshot showed it reading
//! as a smudge next to the Latin wordmark. Drawing it on a pixel grid and
//! emitting Unicode half blocks (`▀ ▄ █`) doubles the vertical resolution
//! and lets the mark occupy the space its dignity asks for.
//!
//! A second, unplanned win: the pixel art needs no bidi and no shaping. The
//! text tiers depend on the EMULATOR to join and mirror Arabic (see
//! `sanctum`); these maps are just blocks, so the mark reads identically in
//! an emulator that cannot shape a single Arabic glyph.
//!
//! LETTERFORMS: derived from the real script, not from memory. The four
//! Arabic Presentation Forms of حيدر in visual order — REH ISOLATED
//! (U+FEAD), DAL FINAL (U+FEAA), YEH MEDIAL (U+FEF4), HAH INITIAL (U+FEA3)
//! — were rasterized from macOS GeezaPro at several pixel sizes, and the
//! maps below are hand-cleaned from that reference so every stroke lands on
//! a half-block PAIR (a 2px-tall stroke renders as a solid `█` rather than a
//! thin `▀`, which is what makes the mark read as ink instead of noise).
//!
//! RTL: the maps are drawn in VISUAL order, left to right on screen —
//! `ر` `ـد` `ـيـ` `حـ` — so `ح`, the word's first letter, is rightmost.
//! Nothing downstream reorders them.
//!
//! DIGNITY (sanctum rules 2): whole or nothing. A frame too narrow for the
//! full map falls back to the single-line text mark; the art is never
//! clipped, scaled, or partially drawn.

/// The launcher/boot banner: 28 × 8 pixels → 28 cols × 4 terminal rows.
///
/// ```text
///  ر            ـد        ـيـ            حـ
///  tail+body    upright   tooth+2 dots   shoulder wedge
/// ```
/// The long run across rows 4-5 is the baseline (kashida) every joined
/// letter sits on; `ر` stands clear of it because `د` does not join
/// forward, which is exactly why the word breaks there.
///
/// `حـ` is the letter that makes or breaks the mark: its identity is the
/// OPEN bowl under the head stroke (GeezaPro shows a top bar sweeping into
/// a drop at the letter's right edge, counter open beneath). So the head is
/// two strokes — bar on the top pair, drop on the next pair at the frame's
/// right edge into the baseline — never a filled wedge; filling the bowl
/// reads as a blob, not a letter.
pub const BANNER: [&str; 8] = [
    "..........##........#######.",
    "..........##........#######.",
    ".....##...##....##.......###",
    ".....##...##....##.......###",
    "....##..####################",
    "....##..####################",
    ".####.........##..##........",
    ".####.........##..##........",
];

/// The session-header mark: 16 × 4 pixels → 16 cols × 2 terminal rows, so it
/// spans BOTH header lines beside the info block (owner item 6).
///
/// TUI6 item 7: the original 24-wide raster read too wide/blocky beside
/// `[ ← main ]` at header scale — this is its two-thirds rework (24 → 16
/// cols exactly), derived by re-scaling the SAME GeezaPro reference the
/// 24-map was hand-cleaned from, letter by letter: `ر` body 2 px with the
/// tail clear of the baseline, `ـد` upright on the baseline, `ـيـ` tooth
/// with the two dots straddling beneath it, `حـ` head bar sweeping one
/// pixel right into the drop at the frame's right edge (never a filled
/// wedge — the open bowl IS the letter). The 28×8 launcher/boot banner is
/// untouched.
///
/// At two rows the baseline and the descenders share one cell row: the
/// baseline is the upper half and `ر`'s tail and `ي`'s two dots are the
/// lower half, so they read as `█` bumps hanging off a `▀` rule. The
/// TUI5-era note called 24 px the floor for the dots; at 16 the dots
/// narrow to the minimum 2-px pair with a 2-px gap and still register —
/// the real floor is the GAP, which survives the rework. Below THAT the
/// mark would be a mangled `ى`, which the dignity rule forbids, so there
/// is still no one-row tier and no narrower map.
pub const HEADER: [&str; 4] = [
    ".##..##..##.###.",
    ".##..##..##..###",
    "....############",
    "##.....##..##...",
];

/// Cells the banner needs (its map width).
pub const BANNER_COLS: u16 = 28;
/// Terminal rows the banner occupies (half the map's pixel rows).
pub const BANNER_ROWS: u16 = 4;
/// Cells the header mark needs (TUI6 item 7: two-thirds of the original
/// 24).
pub const HEADER_COLS: u16 = 16;
/// Terminal rows the header mark occupies.
pub const HEADER_ROWS: u16 = 2;

/// Breathing room the banner asks for on each side before it may be drawn —
/// the mark is ceremony, not a sign wedged against the frame.
pub const BANNER_MARGIN: u16 = 2;

/// Render a `.`/`#` pixel map as half-block rows, two pixel rows per row.
///
/// An odd-height map is padded with a blank pixel row, so callers cannot
/// silently lose the last stroke.
#[must_use]
pub fn half_blocks(map: &[&str]) -> Vec<String> {
    let width = map.iter().map(|row| row.len()).max().unwrap_or(0);
    let padded: Vec<String> = map
        .iter()
        .map(|row| format!("{row:<width$}").replace(' ', "."))
        .collect();
    let blank = ".".repeat(width);
    let mut rows = Vec::with_capacity(padded.len().div_ceil(2));
    let mut index = 0;
    while index < padded.len() {
        let top = &padded[index];
        let bottom = padded.get(index + 1).unwrap_or(&blank);
        rows.push(
            top.chars()
                .zip(bottom.chars())
                .map(|(t, b)| match (t == '#', b == '#') {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect(),
        );
        index += 2;
    }
    rows
}

/// The banner's rendered rows.
#[must_use]
pub fn banner_rows() -> Vec<String> {
    half_blocks(&BANNER)
}

/// The header mark's rendered rows.
#[must_use]
pub fn header_rows() -> Vec<String> {
    half_blocks(&HEADER)
}

/// Whole-or-nothing gate for the centered banner (dignity rule 2): the map
/// plus its margins must fit the frame, else the caller draws the one-line
/// text mark instead.
#[must_use]
pub const fn banner_fits(width: u16) -> bool {
    width >= BANNER_COLS + BANNER_MARGIN * 2
}

/// Whole-or-nothing gate for the header mark: it shares its rows with the
/// back chip and the info block, so it needs room for all three.
#[must_use]
pub const fn header_fits(width: u16, reserved: u16) -> bool {
    width >= HEADER_COLS + reserved
}
