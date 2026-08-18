//! Terminal markdown for assistant text (F2d) — deterministic, LINE-STABLE
//! span styling on theme slots.
//!
//! The renderer maps markdown constructs in agent message text to styled
//! spans without ever changing the LINE geometry the jump machinery and
//! scroll anchors depend on:
//!
//! * LINE-STABILITY LAW: [`render_markdown`] emits exactly one [`MdLine`]
//!   per source line (`text.split('\n')`) — headings, bullets, and fence
//!   delimiters restyle their line, never add or remove one. The fence's
//!   language line is preserved verbatim.
//! * BYTE-CONTENT PRESERVATION: the rendered plain text equals the source
//!   minus ONLY the marker characters actually consumed by matched pairs
//!   (`**`/`*`/`_`/`` ` `` pairs and `***` triples). Unterminated spans —
//!   the streaming case: a chunk may end mid-`**` — render as literal
//!   text; nested emphasis is best-effort but never drops a character.
//! * RE-ENTRANCY: the parser is a pure function of the text, so a
//!   streaming prefix re-renders per frame; when a span's closing marker
//!   arrives the SAME line restyles (marker characters collapse into the
//!   span) without disturbing any earlier line.
//!
//! Wrapping ([`wrap_spans`]) reproduces the transcript's pre-wrap walker
//! (render.rs `wrap_pre_line`) over kind-tagged GRAPHEME CLUSTERS, so the
//! styled path breaks rows exactly where plain wrapping of the RENDERED
//! text would — styling adds or removes zero rows, and an emoji (VS16,
//! ZWJ family, flag, skin-tone) stays one unbreakable unit whose width
//! matches ratatui's renderer.

/// The style vocabulary a markdown span can carry. Kinds are semantic —
/// the renderer maps them onto THEME SLOTS (style.rs), never raw colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdKind {
    /// Unstyled body text.
    Text,
    /// `**bold**` — emphasized ink.
    Bold,
    /// `*italic*` / `_italic_`.
    Italic,
    /// `***both***`, or nesting resolved to both.
    BoldItalic,
    /// `` `inline code` `` — the accent-on-soft-ground pill.
    Code,
    /// A line INSIDE a fenced code block.
    CodeBlock,
    /// A fence delimiter line (```` ``` ````/```` ```lang ````) — restyled,
    /// preserved verbatim (language line included).
    Fence,
    /// The `#`-run and following space of a heading line.
    HeadingMark,
    /// Heading text — emphasis hierarchy by level.
    Heading,
    /// A list marker: `- ` / `* ` bullets, `1. ` / `2) ` numbers (with
    /// their leading indentation), aligned exactly as authored.
    ListMark,
    /// The streaming cursor `▮` appended by the transcript renderer.
    Cursor,
    /// Table chrome (G5): box-drawing borders in the grid layout and the
    /// record-separator rules in the stacked layout — frame ink, never
    /// content.
    TableBorder,
}

/// Column alignment from a GFM delimiter cell (G5): `---`/`:--` left,
/// `:-:` center, `--:` right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdAlign {
    Left,
    Center,
    Right,
}

/// Which table line a tagged [`MdLine`] is (G5). The parser only ever
/// emits Header, then Delimiter, then Body rows — in that order, as one
/// contiguous run — so consecutive table-tagged lines are always ONE
/// table at draw time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdTableRole {
    Header,
    Delimiter,
    Body,
}

/// The width-agnostic table facts riding one source line (G5): per-cell
/// span vectors (trimmed, `\|` unescaped, inline markdown parsed) plus
/// the table's column alignments (one per column — the column COUNT).
/// Body cells are padded/truncated to the header's column count at parse
/// time (GFM ragged-row rule). WIDTH stays out of this struct entirely:
/// the grid/wrapped/stacked choice happens at draw time in
/// [`layout_table`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdTableRow {
    pub role: MdTableRole,
    pub cells: Vec<Vec<MdSpan>>,
    pub aligns: Vec<MdAlign>,
}

/// One styled run of characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdSpan {
    pub text: String,
    pub kind: MdKind,
}

impl MdSpan {
    fn new(text: impl Into<String>, kind: MdKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

/// One rendered line — exactly one per source line, by construction.
/// A table line carries its cell facts in `table` (G5) while `spans`
/// keeps a pipe-separated degradation so the plain/copy path stays
/// lossless-enough.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MdLine {
    pub spans: Vec<MdSpan>,
    pub table: Option<MdTableRow>,
}

impl MdLine {
    /// The line's rendered plain text (markers consumed by matched pairs
    /// already removed). Table lines degrade to `| a | b |` —
    /// pipe-separated cells with pad/truncate applied and `\|` unescaped
    /// (the G5 copy-path choice); the delimiter line stays verbatim.
    #[must_use]
    pub fn plain(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    /// Append the streaming cursor `▮` (G5 hoist of the transcript's
    /// inline push). On a table row the cursor also rides the LAST CELL
    /// so the grid/stacked draw — which reads cells, not line spans —
    /// keeps the cursor visible mid-table.
    pub fn push_cursor(&mut self) {
        let cursor = MdSpan::new("▮", MdKind::Cursor);
        if let Some(table) = &mut self.table
            && let Some(cell) = table.cells.last_mut()
        {
            cell.push(cursor.clone());
        }
        self.spans.push(cursor);
    }
}

/// Render markdown to styled lines. One [`MdLine`] per source line —
/// the LINE-STABILITY LAW — with fence state carried across lines. GFM
/// pipe tables (G5) need one line of lookahead (a header is only a
/// header once its delimiter row lands), so the walk is index-based; a
/// header whose delimiter never arrives falls through to [`block_line`]
/// and renders as an ordinary paragraph — the streaming prefix
/// reclassifies cleanly on the next frame.
#[must_use]
pub fn render_markdown(text: &str) -> Vec<MdLine> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            // Delimiter lines toggle the fence and render VERBATIM —
            // restyled, never consumed (language line preserved).
            in_fence = !in_fence;
            out.push(MdLine {
                spans: vec![MdSpan::new(line, MdKind::Fence)],
                table: None,
            });
            i += 1;
            continue;
        }
        if in_fence {
            // Code block interior: no inline parsing — code is literal.
            out.push(MdLine {
                spans: vec![MdSpan::new(line, MdKind::CodeBlock)],
                table: None,
            });
            i += 1;
            continue;
        }
        if let Some(consumed) = try_table(&lines[i..], &mut out) {
            i += consumed;
            continue;
        }
        out.push(block_line(line));
        i += 1;
    }
    out
}

/// A block-level marker at the head of a line; `mark_end` is the byte
/// offset just past the marker. Factored out of [`block_line`] (G5) so
/// the table probe can refuse marked lines as table headers without
/// duplicating the marker grammar.
enum BlockMark {
    Heading { mark_end: usize },
    List { mark_end: usize },
}

/// Detect a heading / bullet / numbered-list marker — exactly the checks
/// [`block_line`] styles, in the same order.
fn block_mark(line: &str) -> Option<BlockMark> {
    // Heading: up to 3 leading spaces, 1-6 `#`, then a space. The mark
    // stays visible (no characters are consumed) — only restyled.
    let indent_len = line.len() - line.trim_start_matches(' ').len();
    if indent_len > 3 {
        return None;
    }
    let after_indent = &line[indent_len..];
    let hashes = after_indent.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes)
        && after_indent[hashes..].starts_with(' ')
        && !after_indent[hashes..].trim().is_empty()
    {
        return Some(BlockMark::Heading {
            mark_end: indent_len + hashes + 1,
        });
    }
    // Bullet: `- ` / `* ` after the indent (the space is required, so
    // `*emphasis*` at a line start is never mistaken for a bullet).
    for marker in ["- ", "* "] {
        if after_indent.starts_with(marker) && !after_indent[marker.len()..].trim().is_empty() {
            return Some(BlockMark::List {
                mark_end: indent_len + marker.len(),
            });
        }
    }
    // Numbered list: 1-3 digits, `.` or `)`, then a space.
    let digits = after_indent
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    if (1..=3).contains(&digits) {
        let rest = &after_indent[digits..];
        if (rest.starts_with(". ") || rest.starts_with(") ")) && !rest[2..].trim().is_empty() {
            return Some(BlockMark::List {
                mark_end: indent_len + digits + 2,
            });
        }
    }
    None
}

/// Classify one non-fence line and parse its inline spans.
fn block_line(line: &str) -> MdLine {
    match block_mark(line) {
        Some(BlockMark::Heading { mark_end }) => MdLine {
            spans: vec![
                MdSpan::new(&line[..mark_end], MdKind::HeadingMark),
                MdSpan::new(&line[mark_end..], MdKind::Heading),
            ],
            table: None,
        },
        Some(BlockMark::List { mark_end }) => {
            let mut spans = vec![MdSpan::new(&line[..mark_end], MdKind::ListMark)];
            inline_spans(&line[mark_end..], MdKind::Text, &mut spans);
            MdLine { spans, table: None }
        }
        None => {
            let mut spans = Vec::new();
            inline_spans(line, MdKind::Text, &mut spans);
            if spans.is_empty() {
                spans.push(MdSpan::new("", MdKind::Text));
            }
            MdLine { spans, table: None }
        }
    }
}

// ---- G5: GFM pipe-table parse (width-agnostic) ----

/// Probe `lines[0..]` for a GFM pipe table: a header row (at least one
/// unescaped `|`, not a heading/bullet/numbered line), a delimiter row
/// with the SAME cell count (`---` / `:-:` / `--:` cells), then body
/// rows for as long as lines keep an unescaped pipe (a blank line, a
/// pipe-less line, or a fence opener ends the table). Emits one
/// table-tagged [`MdLine`] per consumed source line — LINE STABILITY
/// holds — and returns how many lines were consumed.
fn try_table(lines: &[&str], out: &mut Vec<MdLine>) -> Option<usize> {
    let header = lines[0];
    if !has_unescaped_pipe(header) || block_mark(header).is_some() {
        return None;
    }
    let delimiter = lines.get(1)?;
    let header_cells = split_cells(header);
    if header_cells.is_empty() {
        return None;
    }
    let aligns = parse_delimiter(delimiter, header_cells.len())?;
    out.push(table_line(&header_cells, MdTableRole::Header, &aligns));
    out.push(MdLine {
        // The delimiter line stays VERBATIM on the plain/copy path; the
        // draw path reads only its role (the grid draws its own rule).
        spans: vec![MdSpan::new(*delimiter, MdKind::Text)],
        table: Some(MdTableRow {
            role: MdTableRole::Delimiter,
            cells: Vec::new(),
            aligns: aligns.clone(),
        }),
    });
    let mut consumed = 2;
    while let Some(line) = lines.get(consumed) {
        if line.trim_start().starts_with("```") || !has_unescaped_pipe(line) {
            break;
        }
        out.push(table_line(&split_cells(line), MdTableRole::Body, &aligns));
        consumed += 1;
    }
    Some(consumed)
}

/// Does the line contain a `|` that is not escaped as `\|`?
fn has_unescaped_pipe(line: &str) -> bool {
    let mut prev = ' ';
    for ch in line.chars() {
        if ch == '|' && prev != '\\' {
            return true;
        }
        prev = ch;
    }
    false
}

/// Split a row line into trimmed cell strings on unescaped pipes. `\|`
/// becomes a literal `|` inside its cell (the backslash is consumed —
/// the one escape this renderer honours). Edge pipes are optional per
/// GFM: a leading/trailing pipe's empty outer segment is dropped.
fn split_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let mut segments: Vec<String> = vec![String::new()];
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                if let Some(last) = segments.last_mut() {
                    last.push('|');
                }
            }
            '|' => segments.push(String::new()),
            _ => {
                if let Some(last) = segments.last_mut() {
                    last.push(ch);
                }
            }
        }
    }
    if trimmed.starts_with('|') && !segments.is_empty() {
        segments.remove(0);
    }
    if trimmed.ends_with('|') && !trimmed.ends_with("\\|") && !segments.is_empty() {
        segments.pop();
    }
    segments
        .into_iter()
        .map(|segment| segment.trim().to_owned())
        .collect()
}

/// Parse the delimiter row: cell count must EQUAL the header's, and each
/// cell must be `-`s with optional edge colons (at least one dash). The
/// colons carry the table's column alignment.
fn parse_delimiter(line: &str, expected: usize) -> Option<Vec<MdAlign>> {
    let cells = split_cells(line);
    if cells.len() != expected {
        return None;
    }
    cells.iter().map(|cell| delimiter_align(cell)).collect()
}

/// One delimiter cell → its alignment, or None if it is not a delimiter.
fn delimiter_align(cell: &str) -> Option<MdAlign> {
    let left = cell.starts_with(':');
    let right = cell.len() > usize::from(left) && cell.ends_with(':');
    let dashes = &cell[usize::from(left)..cell.len() - usize::from(right)];
    if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => MdAlign::Center,
        (false, true) => MdAlign::Right,
        _ => MdAlign::Left,
    })
}

/// Build one table-tagged [`MdLine`]: cells pad/truncate to the column
/// count (GFM ragged-row rule), each cell's content inline-parses through
/// the ordinary span machinery, and the line's own spans reconstruct a
/// pipe-separated `| a | b |` shape for the plain/copy path.
fn table_line(raw_cells: &[String], role: MdTableRole, aligns: &[MdAlign]) -> MdLine {
    let cells: Vec<Vec<MdSpan>> = (0..aligns.len())
        .map(|c| {
            let mut spans = Vec::new();
            inline_spans(
                raw_cells.get(c).map_or("", String::as_str),
                MdKind::Text,
                &mut spans,
            );
            spans
        })
        .collect();
    let mut spans = vec![MdSpan::new("| ", MdKind::Text)];
    for (idx, cell) in cells.iter().enumerate() {
        if idx > 0 {
            spans.push(MdSpan::new(" | ", MdKind::Text));
        }
        spans.extend(cell.iter().cloned());
    }
    spans.push(MdSpan::new(" |", MdKind::Text));
    MdLine {
        spans,
        table: Some(MdTableRow {
            role,
            cells,
            aligns: aligns.to_vec(),
        }),
    }
}

/// Remap a nested parse onto its surrounding emphasis (best-effort
/// nesting: `**a *b* c**` → b is BoldItalic; characters are NEVER
/// dropped, only re-kinded).
const fn nest(outer: MdKind, inner: MdKind) -> MdKind {
    match (outer, inner) {
        // Code keeps its pill inside any emphasis.
        (_, MdKind::Code) => MdKind::Code,
        (MdKind::Bold, MdKind::Italic) | (MdKind::Italic, MdKind::Bold) => MdKind::BoldItalic,
        (MdKind::Bold, MdKind::Text) => MdKind::Bold,
        (MdKind::Italic, MdKind::Text) => MdKind::Italic,
        (outer, MdKind::Text) => outer,
        (_, inner) => inner,
    }
}

/// Parse inline markdown within one line into `out`, tagging plain runs
/// with `base` (so nested parses inherit their surrounding emphasis).
fn inline_spans(text: &str, base: MdKind, out: &mut Vec<MdSpan>) {
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let mut i = 0usize;
    let flush = |plain: &mut String, out: &mut Vec<MdSpan>| {
        if !plain.is_empty() {
            out.push(MdSpan::new(std::mem::take(plain), base));
        }
    };
    while i < chars.len() {
        match chars[i] {
            '`' => {
                // Inline code: the next backtick on the line closes it.
                if let Some(close) = find_char(&chars, i + 1, '`') {
                    flush(&mut plain, out);
                    let content: String = chars[i + 1..close].iter().collect();
                    out.push(MdSpan::new(content, MdKind::Code));
                    i = close + 1;
                } else {
                    plain.push('`');
                    i += 1;
                }
            }
            '*' => {
                let run = run_len(&chars, i, '*');
                if run >= 3
                    && let Some(close) = find_marker(&chars, i + 3, "***")
                    && close > i + 3
                {
                    flush(&mut plain, out);
                    let content: String = chars[i + 3..close].iter().collect();
                    out.push(MdSpan::new(content, nest(MdKind::BoldItalic, MdKind::Text)));
                    i = close + 3;
                } else if run >= 2
                    && let Some(close) = find_marker(&chars, i + 2, "**")
                    && close > i + 2
                {
                    flush(&mut plain, out);
                    let content: String = chars[i + 2..close].iter().collect();
                    let start = out.len();
                    inline_spans(&content, MdKind::Bold, out);
                    remap_nested(out, start, MdKind::Bold);
                    i = close + 2;
                } else if run == 1
                    && i + 1 < chars.len()
                    && chars[i + 1] != ' '
                    && let Some(close) = emphasis_close(&chars, i + 1, '*')
                {
                    flush(&mut plain, out);
                    let content: String = chars[i + 1..close].iter().collect();
                    let start = out.len();
                    inline_spans(&content, MdKind::Italic, out);
                    remap_nested(out, start, MdKind::Italic);
                    i = close + 1;
                } else {
                    for _ in 0..run.max(1) {
                        plain.push('*');
                    }
                    i += run.max(1);
                }
            }
            '_' => {
                // `_em_` only at word boundaries, so snake_case survives.
                let open_ok = i == 0 || !chars[i - 1].is_alphanumeric();
                if open_ok
                    && i + 1 < chars.len()
                    && chars[i + 1] != ' '
                    && let Some(close) = underscore_close(&chars, i + 1)
                {
                    flush(&mut plain, out);
                    let content: String = chars[i + 1..close].iter().collect();
                    out.push(MdSpan::new(content, nest(MdKind::Italic, MdKind::Text)));
                    i = close + 1;
                } else {
                    plain.push('_');
                    i += 1;
                }
            }
            c => {
                plain.push(c);
                i += 1;
            }
        }
    }
    flush(&mut plain, out);
}

/// Re-kind the spans a nested parse just appended onto their surrounding
/// emphasis.
fn remap_nested(out: &mut [MdSpan], start: usize, outer: MdKind) {
    for span in &mut out[start..] {
        span.kind = match span.kind {
            k if k == outer => k,
            MdKind::Text => outer,
            inner => nest(outer, inner),
        };
    }
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == needle)
}

fn run_len(chars: &[char], at: usize, of: char) -> usize {
    (at..chars.len()).take_while(|&j| chars[j] == of).count()
}

/// Find `marker` (a run of identical chars) starting at or after `from`.
fn find_marker(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    let len = m.len();
    if chars.len() < len {
        return None;
    }
    (from..=chars.len() - len).find(|&j| chars[j..j + len] == m[..])
}

/// Closing single `*`: content is non-empty and the char before the
/// closer is not a space (so `2 * 3 * 4` stays literal).
fn emphasis_close(chars: &[char], from: usize, of: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == of && j > from && chars[j - 1] != ' ')
}

/// Closing `_` at a word boundary.
fn underscore_close(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len()).find(|&j| {
        chars[j] == '_'
            && j > from
            && chars[j - 1] != ' '
            && chars.get(j + 1).is_none_or(|next| !next.is_alphanumeric())
    })
}

/// Wrap one line's styled spans to `budget` display cells — the SAME
/// pre-wrap walk as the transcript's plain `wrap_pre_line` (explicit
/// spaces preserved, breaks at space-run boundaries, overlong runs
/// hard-split, tabs expand to 4 cells, unrepresentable glyphs dropped),
/// carried out over kind-tagged characters so styling can never move a
/// break. Every produced row fits the budget; the final row lands even
/// when empty (blank lines keep their rail row).
#[must_use]
/// Display width of ONE grapheme cluster, consistent with ratatui's
/// renderer (which measures each grapheme with unicode-width). Emoji —
/// including VS16 (⚠️), ZWJ families (👨‍👩‍👧), regional-indicator flags,
/// and skin-tone sequences — stay a single unbreakable unit that never
/// splits mid-cluster, and a cluster with no positive width still claims
/// one cell so it cannot collapse to nothing. This matches what the
/// composer already does (`cluster_cells`); the transcript/markdown path
/// used to measure per CHAR, which split emoji and mis-summed their width.
fn cluster_cells(cluster: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(cluster).max(1)
}

pub fn wrap_spans(spans: &[MdSpan], budget: usize) -> Vec<Vec<MdSpan>> {
    use unicode_segmentation::UnicodeSegmentation;
    let budget = budget.max(1);
    // Flatten to (grapheme-cluster, kind), expanding tabs — kinds ride each
    // cell, and each cell is a whole cluster so emoji never split.
    let mut cells: Vec<(String, MdKind)> = Vec::new();
    for span in spans {
        for cluster in span.text.graphemes(true) {
            if cluster == "\t" {
                for _ in 0..4 {
                    cells.push((" ".to_owned(), span.kind));
                }
            } else {
                cells.push((cluster.to_owned(), span.kind));
            }
        }
    }
    let mut rows: Vec<Vec<(String, MdKind)>> = Vec::new();
    let mut row: Vec<(String, MdKind)> = Vec::new();
    let mut row_width = 0usize;
    let mut i = 0usize;
    while i < cells.len() {
        let is_space = cells[i].0 == " ";
        let mut end = i;
        let mut run_width = 0usize;
        while end < cells.len() && (cells[end].0 == " ") == is_space {
            run_width += cluster_cells(&cells[end].0);
            end += 1;
        }
        if row_width + run_width <= budget {
            row.extend_from_slice(&cells[i..end]);
            row_width += run_width;
            i = end;
            continue;
        }
        if is_space {
            // Fill to the edge, break, carry the remaining spaces.
            let mut j = i;
            loop {
                while row_width < budget && j < end {
                    row.push(cells[j].clone());
                    row_width += 1;
                    j += 1;
                }
                if j >= end {
                    break;
                }
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            i = end;
            continue;
        }
        if run_width <= budget {
            rows.push(std::mem::take(&mut row));
            row = cells[i..end].to_vec();
            row_width = run_width;
            i = end;
            continue;
        }
        if row_width > 0 {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        for cell in &cells[i..end] {
            let cell_width = cluster_cells(&cell.0);
            if cell_width > budget {
                continue;
            }
            if row_width + cell_width > budget {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push(cell.clone());
            row_width += cell_width;
        }
        i = end;
    }
    rows.push(row);
    rows.into_iter().map(compress).collect()
}

/// Merge adjacent same-kind cells (grapheme clusters) back into spans.
fn compress(cells: Vec<(String, MdKind)>) -> Vec<MdSpan> {
    let mut out: Vec<MdSpan> = Vec::new();
    for (cluster, kind) in cells {
        match out.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(&cluster),
            _ => out.push(MdSpan::new(cluster, kind)),
        }
    }
    out
}

// ---- G5: table layout (width-aware, DRAW time) ----

/// Lay out one table — the contiguous run of table-tagged lines — for
/// the CURRENT budget. Every returned row already fits the budget (the
/// caller must not re-wrap). Three modes, chosen per draw so a resize
/// flips them with no state:
///
/// * NATURAL GRID — every column at its natural width `N_c` (the widest
///   header/body cell) when `sum(N) + chrome <= budget`;
/// * WRAPPED GRID — otherwise, when the column floors `M_c =
///   max(longest unbreakable word, 3)` still fit, the width above the
///   floors is distributed proportionally to each column's natural
///   headroom and cells wrap inside their column ([`wrap_spans`]), row
///   height = tallest cell, alignment honoured;
/// * STACKED — below the breakpoint: per BODY row, one bold
///   `Header: value` logical line per column wrapped to the FULL budget,
///   rows separated by a `min(budget, 48)`-cell rule; the header row
///   never emits a block of its own and alignment hints are ignored.
///
/// Chrome is `│` at every column boundary plus one space of padding on
/// each side of each cell (`3*columns + 1` cells), with a full outer
/// border — the Claude Code grid the owner pinned.
#[must_use]
pub fn layout_table(rows: &[&MdTableRow], budget: usize) -> Vec<Vec<MdSpan>> {
    let budget = budget.max(1);
    let Some(first) = rows.first() else {
        return Vec::new();
    };
    let aligns = &first.aligns;
    let columns = aligns.len();
    if columns == 0 {
        return Vec::new();
    }
    // Header cells render BOLD — re-kinded through `nest` so inline
    // styling inside a header cell keeps its identity (code pill, italic
    // marries to bold-italic).
    let header_cells: Vec<Vec<MdSpan>> = rows
        .iter()
        .find(|row| row.role == MdTableRole::Header)
        .map(|row| row.cells.iter().map(|cell| embolden(cell)).collect())
        .unwrap_or_else(|| vec![Vec::new(); columns]);
    let body: Vec<&MdTableRow> = rows
        .iter()
        .filter(|row| row.role == MdTableRole::Body)
        .copied()
        .collect();
    // MEASURE: natural width and floor per column, header included.
    let mut natural = vec![0usize; columns];
    let mut floor = vec![3usize; columns];
    for cells in std::iter::once(&header_cells).chain(body.iter().map(|row| &row.cells)) {
        for (c, cell) in cells.iter().enumerate().take(columns) {
            natural[c] = natural[c].max(spans_width(cell));
            floor[c] = floor[c].max(longest_word(cell));
        }
    }
    let chrome = 3 * columns + 1;
    if natural.iter().sum::<usize>() + chrome <= budget {
        return grid(&header_cells, &body, &natural, aligns);
    }
    if floor.iter().sum::<usize>() + chrome <= budget {
        let widths = distribute(&natural, &floor, budget - chrome);
        return grid(&header_cells, &body, &widths, aligns);
    }
    stacked(&header_cells, &body, budget)
}

/// Re-kind a cell's spans onto Bold (header emphasis) via `nest`.
fn embolden(cell: &[MdSpan]) -> Vec<MdSpan> {
    cell.iter()
        .map(|span| MdSpan::new(span.text.clone(), nest(MdKind::Bold, span.kind)))
        .collect()
}

/// Display width of a cell's spans — the same cell arithmetic as
/// [`wrap_spans`]: tabs expand to 4, unrepresentable glyphs count 0.
fn spans_width(spans: &[MdSpan]) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    let text: String = spans.iter().map(|span| span.text.as_str()).collect();
    text.graphemes(true)
        .map(|cluster| {
            if cluster == "\t" {
                4
            } else {
                cluster_cells(cluster)
            }
        })
        .sum()
}

/// The widest unbreakable (non-space) run in a cell — the column's wrap
/// floor. Tabs break words exactly as [`wrap_spans`] expands them to
/// spaces.
fn longest_word(spans: &[MdSpan]) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    let text: String = spans.iter().map(|span| span.text.as_str()).collect();
    let mut widest = 0usize;
    let mut current = 0usize;
    for cluster in text.graphemes(true) {
        if cluster == " " || cluster == "\t" {
            widest = widest.max(current);
            current = 0;
        } else {
            current += cluster_cells(cluster);
        }
    }
    widest.max(current)
}

/// Split `available` cells across columns: floors first, then the
/// remainder proportionally to each column's natural headroom
/// (`N_c - M_c`), leftovers one cell at a time left-to-right — fully
/// deterministic, every column at or above its floor, none beyond its
/// natural width.
fn distribute(natural: &[usize], floor: &[usize], available: usize) -> Vec<usize> {
    let mut widths = floor.to_vec();
    let headroom: Vec<usize> = natural
        .iter()
        .zip(floor)
        .map(|(n, f)| n.saturating_sub(*f))
        .collect();
    let want: usize = headroom.iter().sum();
    let mut extra = available.saturating_sub(widths.iter().sum::<usize>());
    if want <= extra {
        for (width, head) in widths.iter_mut().zip(&headroom) {
            *width += head;
        }
        return widths;
    }
    for (c, width) in widths.iter_mut().enumerate() {
        let share = extra * headroom[c] / want;
        *width += share;
    }
    extra -= widths.iter().sum::<usize>() - floor.iter().sum::<usize>();
    let mut c = 0usize;
    let mut stalled = 0usize;
    while extra > 0 && stalled < natural.len() {
        if widths[c] < natural[c] {
            widths[c] += 1;
            extra -= 1;
            stalled = 0;
        } else {
            stalled += 1;
        }
        c = (c + 1) % natural.len();
    }
    widths
}

/// Assemble the bordered grid: top border, header (bold), a rule between
/// header and body (only when a body exists), body rows, bottom border.
/// No rules between body rows — the Claude Code look the goldens pin.
fn grid(
    header: &[Vec<MdSpan>],
    body: &[&MdTableRow],
    widths: &[usize],
    aligns: &[MdAlign],
) -> Vec<Vec<MdSpan>> {
    let mut out = Vec::new();
    out.push(border_row('┌', '┬', '┐', widths));
    grid_row(&mut out, header, widths, aligns);
    if !body.is_empty() {
        out.push(border_row('├', '┼', '┤', widths));
        for row in body {
            grid_row(&mut out, &row.cells, widths, aligns);
        }
    }
    out.push(border_row('└', '┴', '┘', widths));
    out
}

/// One horizontal border line (`┌──┬──┐` family) as a single
/// TableBorder span.
fn border_row(left: char, mid: char, right: char, widths: &[usize]) -> Vec<MdSpan> {
    let mut text = String::new();
    text.push(left);
    for (idx, width) in widths.iter().enumerate() {
        if idx > 0 {
            text.push(mid);
        }
        for _ in 0..width + 2 {
            text.push('─');
        }
    }
    text.push(right);
    vec![MdSpan::new(text, MdKind::TableBorder)]
}

/// One logical table row → its display rows: every cell wraps to its
/// column width, row height is the tallest cell, short cells pad with
/// blank rows, and per-column alignment places each wrapped line.
fn grid_row(
    out: &mut Vec<Vec<MdSpan>>,
    cells: &[Vec<MdSpan>],
    widths: &[usize],
    aligns: &[MdAlign],
) {
    let empty: Vec<MdSpan> = Vec::new();
    let wrapped: Vec<Vec<Vec<MdSpan>>> = widths
        .iter()
        .enumerate()
        .map(|(c, width)| wrap_spans(cells.get(c).unwrap_or(&empty), *width))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line_idx in 0..height {
        let mut spans = vec![MdSpan::new("│", MdKind::TableBorder)];
        for (c, width) in widths.iter().enumerate() {
            let cell_line = wrapped[c].get(line_idx).unwrap_or(&empty);
            let used = spans_width(cell_line);
            let slack = width.saturating_sub(used);
            let (left_pad, right_pad) = match aligns.get(c).copied().unwrap_or(MdAlign::Left) {
                MdAlign::Left => (0, slack),
                MdAlign::Right => (slack, 0),
                MdAlign::Center => (slack / 2, slack - slack / 2),
            };
            spans.push(MdSpan::new(" ".repeat(left_pad + 1), MdKind::Text));
            spans.extend(cell_line.iter().cloned());
            spans.push(MdSpan::new(" ".repeat(right_pad + 1), MdKind::Text));
            spans.push(MdSpan::new("│", MdKind::TableBorder));
        }
        out.push(spans);
    }
}

/// The stacked record layout below the breakpoint: per body row, one
/// bold `Header: value` line per column wrapped to the full budget (no
/// hanging indent), `min(budget, 48)` rule cells BETWEEN rows, header
/// row never emitted as its own block, empty cells keep their labelled
/// line so every record has the same shape.
fn stacked(header: &[Vec<MdSpan>], body: &[&MdTableRow], budget: usize) -> Vec<Vec<MdSpan>> {
    let mut out = Vec::new();
    let rule = vec![MdSpan::new("─".repeat(budget.min(48)), MdKind::TableBorder)];
    for (r, row) in body.iter().enumerate() {
        if r > 0 {
            out.push(rule.clone());
        }
        for (c, label) in header.iter().enumerate() {
            let mut line: Vec<MdSpan> = label.clone();
            line.push(MdSpan::new(":", MdKind::Bold));
            line.push(MdSpan::new(" ", MdKind::Text));
            if let Some(cell) = row.cells.get(c) {
                line.extend(cell.iter().cloned());
            }
            out.extend(wrap_spans(&line, budget));
        }
    }
    out
}

#[cfg(test)]
mod emoji_width_tests {
    use super::{MdKind, MdSpan, cluster_cells, longest_word, spans_width, wrap_spans};

    fn text(s: &str) -> Vec<MdSpan> {
        vec![MdSpan::new(s, MdKind::Text)]
    }

    /// MUTATION CHECK: measure emoji per-char instead of per grapheme
    /// cluster. Expected failure: a ZWJ family, a VS16 emoji, and a flag
    /// each stay ONE unbreakable cluster whose width matches ratatui's
    /// renderer, and none split mid-cluster when wrapped.
    #[test]
    fn emoji_clusters_stay_one_unit_across_measure_and_wrap() {
        // One grapheme cluster each — never a per-scalar sum.
        for emoji in ["😀", "⚠️", "👨‍👩‍👧", "🇺🇸", "👍🏽"] {
            let w = spans_width(&text(emoji));
            assert_eq!(
                w,
                cluster_cells(emoji),
                "`{emoji}` measures as one cluster, not per-char"
            );
            // longest_word (the wrap floor) agrees — one unbroken run.
            assert_eq!(longest_word(&text(emoji)), w, "`{emoji}` is one word");
        }

        // A cluster is never split across a wrap boundary: wrap a line whose
        // budget lands mid-emoji and every row's rendered text stays valid
        // (the emoji appears whole on exactly one row).
        let line = text("ab 👨‍👩‍👧 cd");
        let rows = wrap_spans(&line, 4);
        let joined: String = rows
            .iter()
            .map(|row| row.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect();
        assert!(
            joined.contains("👨‍👩‍👧"),
            "the family emoji survives wrapping whole"
        );
        assert!(
            rows.iter().all(|row| {
                let t: String = row.iter().map(|s| s.text.as_str()).collect();
                // No row ends or starts with a bare ZWJ/joiner fragment.
                !t.starts_with('\u{200d}') && !t.ends_with('\u{200d}')
            }),
            "no row splits the ZWJ sequence"
        );

        // Plain ASCII is unchanged — one cell per char.
        assert_eq!(spans_width(&text("hello")), 5);
    }
}
