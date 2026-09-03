//! Shared scaffolding for the `tuivirt_*` behaviour-preservation pins
//! (v0.0.970, lane `tuivirt`). The transcript viewport is being
//! re-architected (viewport-only layout, estimated heights, bounded render
//! cache — `docs/testing/v0.0.970/tuivirt-analysis.md`); these helpers let
//! every pin talk about OBSERVABLE OUTPUT ONLY: the text and style of the
//! cells a `TestBackend` frame ends up holding, the hit map, and the two
//! public scroll cells (`scroll_back` / `scroll_max`). Nothing here reaches
//! into the render cache.
//!
//! This is a `#[path]`-free sibling of `tests/common/mod.rs`, kept separate
//! so adding the golden machinery does not recompile every other TUI test
//! binary.
#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, Hit, RuntimeMode, Screen};
use haider_tui::render::{VERSION, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

/// The env flag that REGENERATES goldens instead of checking them.
pub const UPDATE_ENV: &str = "UPDATE_TUIVIRT_GOLDENS";

/// The three terminal sizes every golden is pinned at: a small laptop
/// split, the bench's 118x36, and a wide desktop pane.
pub const SIZES: [(u16, u16); 3] = [(80, 24), (118, 36), (160, 50)];

/// The bench's representative agent line (`w3c3_render_bench_tests`):
/// wraps once at 118 columns, twice at 80.
pub fn agent_row(n: usize) -> String {
    format!(
        "row {n} — a representative agent line with enough words to wrap at \
         a normal terminal width and exercise the measurement path"
    )
}

/// `tests/common::launcher_model`, verbatim (deterministic device name).
pub fn launcher_model() -> AppModel {
    let mut model = AppModel::new();
    model.identity.device = "test-lion-box".to_owned();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model
}

/// The pins' session id.
pub fn session_id() -> SessionId {
    SessionId::new("tuivirt-session")
}

/// A LIVE session on the session screen with an EMPTY transcript (the
/// `f2_markdown_tests::live_session` construction). The bench's
/// `replayed` attaches the demo's first sample session instead, which
/// carries seeded history rows; the pins want a transcript that holds
/// exactly what each test pushes.
pub fn session_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&session_id());
    model.open_session(&session_id());
    model.requests.clear();
    model.screen = Screen::Session;
    model
}

/// A model whose attached session holds `rows` committed agent rows, built
/// the way a REPLAY builds them (the bench's `replayed`).
pub fn replayed(rows: usize) -> AppModel {
    let mut model = session_model();
    for n in 0..rows {
        push_agent(&mut model, &format!("bench-{n}"), &agent_row(n));
    }
    model
}

pub fn apply(model: &mut AppModel, payload: EventPayload) {
    model.projection.apply(&payload);
}

/// One committed assistant message.
pub fn push_agent(model: &mut AppModel, id: &str, text: &str) {
    apply(
        model,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item: TurnItem::AgentMessage {
                text: text.to_owned(),
            },
        }),
    );
}

/// One user prompt row.
pub fn push_user(model: &mut AppModel, text: &str) {
    apply(
        model,
        EventPayload::UserMessage {
            text: text.to_owned(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        },
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub modifier: Modifier,
}

/// One rendered frame: the cell grid (symbol + style), the display rows
/// (wide-glyph continuation cells dropped so CJK rows read naturally), the
/// hit map, and the transcript rect the frame stamped on the model.
pub struct Snapshot {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Vec<(String, CellStyle)>>,
    pub rows: Vec<String>,
    pub hits: Vec<(Rect, Hit)>,
    pub transcript: Rect,
}

pub fn draw(model: &AppModel, width: u16, height: u16) -> Snapshot {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut cells = Vec::with_capacity(usize::from(height));
    let mut rows = Vec::with_capacity(usize::from(height));
    for y in 0..buffer.area.height {
        let mut row_cells = Vec::with_capacity(usize::from(width));
        let mut text = String::new();
        let mut skip = 0usize;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol().to_owned();
            let style = CellStyle {
                fg: cell.fg,
                bg: cell.bg,
                modifier: cell.modifier,
            };
            if skip > 0 {
                skip -= 1;
            } else {
                let cols = UnicodeWidthStr::width(symbol.as_str());
                text.push_str(&symbol);
                skip = cols.saturating_sub(1);
            }
            row_cells.push((symbol, style));
        }
        cells.push(row_cells);
        rows.push(text);
    }
    Snapshot {
        width,
        height,
        cells,
        rows,
        hits,
        transcript: model.transcript_view.get(),
    }
}

impl Snapshot {
    /// The transcript area's rows, as displayed.
    pub fn transcript_rows(&self) -> Vec<String> {
        let area = self.transcript;
        (area.y..area.y.saturating_add(area.height))
            .filter_map(|y| self.rows.get(usize::from(y)).cloned())
            .collect()
    }

    /// The transcript rows that scrolling moves: everything between the
    /// sticky band row (row 0 of the area) and the jump chip row (its last
    /// row), truncated to the left `width - 30` columns so the right-aligned
    /// `N new · Jump to bottom ↓` chip can never leak into a comparison.
    pub fn transcript_interior(&self) -> Vec<String> {
        let keep = usize::from(self.width.saturating_sub(30));
        let all = self.transcript_rows();
        if all.len() < 3 {
            return Vec::new();
        }
        all[1..all.len() - 1]
            .iter()
            .map(|row| row.chars().take(keep).collect())
            .collect()
    }

    pub fn has_hit(&self, wanted: impl Fn(&Hit) -> bool) -> bool {
        self.hits.iter().any(|(_, hit)| wanted(hit))
    }

    pub fn find_hit(&self, wanted: impl Fn(&Hit) -> bool) -> Option<(Rect, Hit)> {
        self.hits.iter().find(|(_, hit)| wanted(hit)).cloned()
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.rows.iter().any(|row| row.contains(needle))
    }

    pub fn row_containing(&self, needle: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.contains(needle))
    }

    /// The canonical golden text: `NN|display row|` followed by the row's
    /// style runs (`fg/bg/modifier×count`, every cell counted). The crate
    /// version is masked so a release bump never invalidates a golden.
    pub fn dump(&self, name: &str) -> String {
        let mut out = format!(
            "# tuivirt golden · {name} · {}x{}\n",
            self.width, self.height
        );
        let version = format!("v{VERSION}");
        for (y, row) in self.rows.iter().enumerate() {
            let masked = row.replace(&version, "v<VERSION>");
            out.push_str(&format!("{y:02}|{masked}|\n"));
            let mut runs: Vec<(CellStyle, usize)> = Vec::new();
            for (_, style) in &self.cells[y] {
                match runs.last_mut() {
                    Some((last, count)) if last == style => *count += 1,
                    _ => runs.push((*style, 1)),
                }
            }
            let encoded = runs
                .iter()
                .map(|(style, count)| {
                    format!("{:?}/{:?}/{:?}×{count}", style.fg, style.bg, style.modifier)
                })
                .collect::<Vec<_>>()
                .join("; ");
            out.push_str(&format!("   ~ {encoded}\n"));
        }
        out
    }
}

pub fn golden_path(name: &str, width: u16, height: u16) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tuivirt")
        .join(format!("{name}.{width}x{height}.golden"))
}

/// Compare the frame against `tests/fixtures/tuivirt/{name}.{w}x{h}.golden`.
/// `UPDATE_TUIVIRT_GOLDENS=1` rewrites the file instead of checking it.
pub fn check_golden(name: &str, frame: &Snapshot) {
    let path = golden_path(name, frame.width, frame.height);
    let actual = frame.dump(name);
    if std::env::var_os(UPDATE_ENV).is_some_and(|value| !value.is_empty() && value != "0") {
        std::fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
        std::fs::write(&path, &actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {}; run with {UPDATE_ENV}=1 to record it",
            path.display()
        )
    });
    if expected != actual {
        let mismatch = expected
            .lines()
            .zip(actual.lines())
            .enumerate()
            .find(|(_, (want, got))| want != got);
        let detail = match mismatch {
            Some((line, (want, got))) => {
                format!(
                    "first difference at golden line {}:\n  want: {want}\n  got:  {got}",
                    line + 1
                )
            }
            None => format!(
                "line counts differ: want {} got {}",
                expected.lines().count(),
                actual.lines().count()
            ),
        };
        panic!(
            "golden mismatch for {name} @ {}x{} ({}):\n{detail}\n\
             (if the change is intended, rerun with {UPDATE_ENV}=1 and review the diff)",
            frame.width,
            frame.height,
            path.display()
        );
    }
}

/// Assert two frames are cell-for-cell identical (text AND style).
pub fn assert_same_frame(what: &str, left: &Snapshot, right: &Snapshot) {
    let a = left.dump(what);
    let b = right.dump(what);
    if a != b {
        let mismatch = a
            .lines()
            .zip(b.lines())
            .enumerate()
            .find(|(_, (x, y))| x != y);
        panic!(
            "{what}: frames differ — {}",
            mismatch.map_or_else(
                || "line counts differ".to_owned(),
                |(line, (x, y))| format!("line {}:\n  left:  {x}\n  right: {y}", line + 1)
            )
        );
    }
}
