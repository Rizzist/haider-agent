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
//! (render.rs `wrap_pre_line`) over kind-tagged characters, so the styled
//! path breaks rows exactly where plain wrapping of the RENDERED text
//! would — styling adds or removes zero rows.

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MdLine {
    pub spans: Vec<MdSpan>,
}

impl MdLine {
    /// The line's rendered plain text (markers consumed by matched pairs
    /// already removed).
    #[must_use]
    pub fn plain(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

/// Render markdown to styled lines. One [`MdLine`] per source line —
/// the LINE-STABILITY LAW — with fence state carried across lines.
#[must_use]
pub fn render_markdown(text: &str) -> Vec<MdLine> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            // Delimiter lines toggle the fence and render VERBATIM —
            // restyled, never consumed (language line preserved).
            in_fence = !in_fence;
            out.push(MdLine {
                spans: vec![MdSpan::new(line, MdKind::Fence)],
            });
            continue;
        }
        if in_fence {
            // Code block interior: no inline parsing — code is literal.
            out.push(MdLine {
                spans: vec![MdSpan::new(line, MdKind::CodeBlock)],
            });
            continue;
        }
        out.push(block_line(line));
    }
    out
}

/// Classify one non-fence line and parse its inline spans.
fn block_line(line: &str) -> MdLine {
    // Heading: up to 3 leading spaces, 1-6 `#`, then a space. The mark
    // stays visible (no characters are consumed) — only restyled.
    let indent_len = line.len() - line.trim_start_matches(' ').len();
    if indent_len <= 3 {
        let after_indent = &line[indent_len..];
        let hashes = after_indent.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes)
            && after_indent[hashes..].starts_with(' ')
            && !after_indent[hashes..].trim().is_empty()
        {
            let mark_end = indent_len + hashes + 1;
            return MdLine {
                spans: vec![
                    MdSpan::new(&line[..mark_end], MdKind::HeadingMark),
                    MdSpan::new(&line[mark_end..], MdKind::Heading),
                ],
            };
        }
        // Bullet: `- ` / `* ` after the indent (the space is required, so
        // `*emphasis*` at a line start is never mistaken for a bullet).
        for marker in ["- ", "* "] {
            if after_indent.starts_with(marker) && !after_indent[marker.len()..].trim().is_empty() {
                let mark_end = indent_len + marker.len();
                let mut spans = vec![MdSpan::new(&line[..mark_end], MdKind::ListMark)];
                inline_spans(&line[mark_end..], MdKind::Text, &mut spans);
                return MdLine { spans };
            }
        }
        // Numbered list: 1-3 digits, `.` or `)`, then a space.
        let digits = after_indent
            .chars()
            .take_while(char::is_ascii_digit)
            .count();
        if (1..=3).contains(&digits) {
            let rest = &after_indent[digits..];
            if (rest.starts_with(". ") || rest.starts_with(") "))
                && !rest[2..].trim().is_empty()
            {
                let mark_end = indent_len + digits + 2;
                let mut spans = vec![MdSpan::new(&line[..mark_end], MdKind::ListMark)];
                inline_spans(&line[mark_end..], MdKind::Text, &mut spans);
                return MdLine { spans };
            }
        }
    }
    let mut spans = Vec::new();
    inline_spans(line, MdKind::Text, &mut spans);
    if spans.is_empty() {
        spans.push(MdSpan::new("", MdKind::Text));
    }
    MdLine { spans }
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
pub fn wrap_spans(spans: &[MdSpan], budget: usize) -> Vec<Vec<MdSpan>> {
    use unicode_width::UnicodeWidthChar;
    let budget = budget.max(1);
    // Flatten to (char, kind), expanding tabs — kinds ride each cell.
    let mut cells: Vec<(char, MdKind)> = Vec::new();
    for span in spans {
        for ch in span.text.chars() {
            if ch == '\t' {
                for _ in 0..4 {
                    cells.push((' ', span.kind));
                }
            } else {
                cells.push((ch, span.kind));
            }
        }
    }
    let mut rows: Vec<Vec<(char, MdKind)>> = Vec::new();
    let mut row: Vec<(char, MdKind)> = Vec::new();
    let mut row_width = 0usize;
    let mut i = 0usize;
    while i < cells.len() {
        let is_space = cells[i].0 == ' ';
        let mut end = i;
        let mut run_width = 0usize;
        while end < cells.len() && (cells[end].0 == ' ') == is_space {
            run_width += UnicodeWidthChar::width(cells[end].0).unwrap_or(0);
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
                    row.push(cells[j]);
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
        for &(ch, kind) in &cells[i..end] {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch_width > budget {
                continue;
            }
            if row_width + ch_width > budget {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push((ch, kind));
            row_width += ch_width;
        }
        i = end;
    }
    rows.push(row);
    rows.into_iter().map(compress).collect()
}

/// Merge adjacent same-kind cells back into spans.
fn compress(cells: Vec<(char, MdKind)>) -> Vec<MdSpan> {
    let mut out: Vec<MdSpan> = Vec::new();
    for (ch, kind) in cells {
        match out.last_mut() {
            Some(last) if last.kind == kind => last.text.push(ch),
            _ => out.push(MdSpan::new(String::from(ch), kind)),
        }
    }
    out
}
