//! First-class composer cursor (TUI5 owner wave).
//!
//! The owner's report on v0.0.10: the composer "cursor" was a literal `▮`
//! appended after the text at render time — no movement, no mid-text
//! editing, no selection. This module is the real thing: a grapheme-aware
//! cursor + selection + per-surface input model, owned by the reducer and
//! rendered as a styled CELL (never an appended glyph).
//!
//! Behavior questions resolve in this order (brief law): Claude Code CLI
//! conventions first, then native-input conventions. Each choice is
//! documented at its site.
//!
//! UNITS: `cursor` and `anchor` are BYTE offsets into `text`, always on a
//! grapheme-cluster boundary (unicode-segmentation). Wide glyphs occupy 2
//! display cells (unicode-width) but are ONE grapheme step; combining
//! marks travel with their base. Arabic/RTL text moves in LOGICAL order:
//! `←` is always "toward the start of the string" (the brief's documented
//! choice — full bidi caret GEOMETRY is ledgered graphics-tier).

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Submitted-input ring cap (item 6's "minimal per-session ring").
/// Claude Code keeps a long per-project history; the demo's transient ring
/// keeps the last 50 — enough for any session, never persisted.
const HISTORY_CAP: usize = 50;

/// One surface's composer: draft text, cursor, selection anchor, the
/// column-sticky state for vertical movement, and the submitted-input
/// ring. The WHOLE struct is per-surface (launcher | session | aura,
/// item 9) and transient — nothing here may enter the persistence DTO
/// (item 8; asserted in the persistence tests).
#[derive(Debug, Clone, Default)]
pub struct Composer {
    text: String,
    /// Byte offset on a grapheme boundary, 0..=text.len().
    cursor: usize,
    /// Selection anchor (byte offset). `Some(a)` with `a != cursor` is an
    /// ACTIVE selection; `a == cursor` is a parked anchor (mouse press
    /// before any drag) and renders as no selection.
    anchor: Option<usize>,
    /// Preferred display column for a run of ↑/↓ (column-sticky like every
    /// editor). Any horizontal movement or edit clears it.
    sticky_col: Option<usize>,
    /// Submitted inputs, oldest first (item 6). Transient by law.
    history: Vec<String>,
    /// `Some(i)` while browsing `history[i]`; `None` while editing the
    /// draft.
    history_pos: Option<usize>,
    /// The draft stashed when browsing began — restored by ↓ past the
    /// newest entry (Claude Code behavior). Dropped if the user EDITS a
    /// recalled entry: the edit becomes the new draft (documented,
    /// minimal-ring law).
    history_stash: Option<String>,
}

/// Text-equality against string literals: the pre-TUI5 test corpus (and
/// any reader) compares the composer to its TEXT — the cursor is state,
/// not content.
impl PartialEq<&str> for Composer {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for Composer {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl Composer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the draft (and every cursor/selection/browse state); the
    /// input ring survives — clearing text is not forgetting history.
    pub fn clear(&mut self) {
        self.set_text(String::new());
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The active selection as an ordered byte range, or `None` when the
    /// anchor is unset or zero-width.
    #[must_use]
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<&str> {
        self.selection_range()
            .map(|(start, end)| &self.text[start..end])
    }

    /// Esc's first meaning (item 4): drop the selection, keep the cursor
    /// at its current (active) end.
    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Replace the whole draft (palette Tab completion, slash staging).
    /// Cursor lands at the END (native inputs after programmatic set);
    /// selection and sticky state drop; a history browse detaches (the
    /// set text is the new draft).
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.anchor = None;
        self.sticky_col = None;
        self.history_pos = None;
        self.history_stash = None;
    }

    /// Take the draft for submit: returns the raw text; a non-empty
    /// TRIMMED form is recorded in the input ring (consecutive-dupe
    /// deduped, Claude Code behavior); every per-surface cursor state
    /// resets (item 8's submit-clears law).
    pub fn take_for_submit(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.record_submitted(text.trim());
        self.cursor = 0;
        self.anchor = None;
        self.sticky_col = None;
        self.history_pos = None;
        self.history_stash = None;
        text
    }

    /// Feed the input ring directly — the palette-activation path executes
    /// commands without passing [`Self::take_for_submit`]. Empty and
    /// consecutive-duplicate entries are dropped.
    pub fn record_submitted(&mut self, entry: &str) {
        let entry = entry.trim();
        if entry.is_empty() || self.history.last().map(String::as_str) == Some(entry) {
            return;
        }
        self.history.push(entry.to_owned());
        if self.history.len() > HISTORY_CAP {
            self.history.remove(0);
        }
    }

    // ---- Editing (item 3) ----

    /// Insert at the cursor — never append (the owner's core complaint).
    /// An active selection is REPLACED first (item 4, native-input law).
    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection_if_any();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.after_edit();
    }

    /// ⌫ — delete the selection if active (item 4), else the grapheme
    /// before the cursor.
    pub fn backspace(&mut self) {
        if self.delete_selection_if_any() {
            self.after_edit();
            return;
        }
        if let Some(prev) = prev_boundary(&self.text, self.cursor) {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
        self.after_edit();
    }

    /// Delete (fn⌫ / kDEL) — the selection if active, else the grapheme
    /// after the cursor.
    pub fn delete_forward(&mut self) {
        if self.delete_selection_if_any() {
            self.after_edit();
            return;
        }
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.text.replace_range(self.cursor..next, "");
        }
        self.after_edit();
    }

    /// ⌥⌫ / ⌃W — delete back to the previous word start (readline
    /// backward-kill-word; Claude Code binds both). Kill-family keys
    /// COLLAPSE an active selection and act from the cursor (readline has
    /// no selection concept; item 4 reserves selection-delete for
    /// ⌫/Delete — documented choice).
    pub fn word_backspace(&mut self) {
        self.anchor = None;
        let target = word_left_of(&self.text, self.cursor);
        self.text.replace_range(target..self.cursor, "");
        self.cursor = target;
        self.after_edit();
    }

    /// ⌃K — kill to the end of the logical line; at line end, kill the
    /// newline itself (Emacs/zsh C-k law, so repeated ⌃K eats the buffer).
    pub fn kill_to_line_end(&mut self) {
        self.anchor = None;
        let end = line_end(&self.text, self.cursor);
        if end > self.cursor {
            self.text.replace_range(self.cursor..end, "");
        } else if end < self.text.len() {
            // Cursor sits ON the newline: remove it, joining the lines.
            self.text.replace_range(self.cursor..=self.cursor, "");
        }
        self.after_edit();
    }

    /// ⌃U — kill to the start of the logical line (readline
    /// unix-line-discard scoped to the line; at line start it is a no-op —
    /// documented asymmetry with ⌃K, matching readline).
    pub fn kill_to_line_start(&mut self) {
        self.anchor = None;
        let start = line_start(&self.text, self.cursor);
        if start < self.cursor {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
        self.after_edit();
    }

    // ---- Movement (item 2) ----

    /// ← by one grapheme. With an active selection and NO extend, the
    /// cursor COLLAPSES to the selection's left edge without stepping
    /// (native-input law; Claude Code matches).
    pub fn move_left(&mut self, extend: bool) {
        self.sticky_col = None;
        if !extend && let Some((start, _)) = self.selection_range() {
            self.cursor = start;
            self.anchor = None;
            return;
        }
        self.pre_move(extend);
        if let Some(prev) = prev_boundary(&self.text, self.cursor) {
            self.cursor = prev;
        }
    }

    /// → by one grapheme (collapse-to-right-edge law mirrors
    /// [`Self::move_left`]).
    pub fn move_right(&mut self, extend: bool) {
        self.sticky_col = None;
        if !extend && let Some((_, end)) = self.selection_range() {
            self.cursor = end;
            self.anchor = None;
            return;
        }
        self.pre_move(extend);
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.cursor = next;
        }
    }

    /// ⌥← — to the start of the previous word (mac option-arrow law;
    /// words are unicode word-bound segments containing alphanumerics, so
    /// Arabic and CJK words step correctly).
    pub fn word_left(&mut self, extend: bool) {
        self.sticky_col = None;
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        self.cursor = word_left_of(&self.text, self.cursor);
    }

    /// ⌥→ — to the end of the next word.
    pub fn word_right(&mut self, extend: bool) {
        self.sticky_col = None;
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        self.cursor = word_right_of(&self.text, self.cursor);
    }

    /// Home / ⌃A — logical line start (Claude Code binds ⌃A here).
    pub fn line_home(&mut self, extend: bool) {
        self.sticky_col = None;
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        self.cursor = line_start(&self.text, self.cursor);
    }

    /// End / ⌃E — logical line end.
    pub fn line_end_key(&mut self, extend: bool) {
        self.sticky_col = None;
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        self.cursor = line_end(&self.text, self.cursor);
    }

    /// ↑ across rows, column-sticky. Returns `false` when already on the
    /// first row — the caller's history hook (item 6). The composer has
    /// no soft wrap (overflow scrolls horizontally), so visual rows ARE
    /// the logical lines; the brief's "visual wrapped rows" reduces to
    /// this exactly (documented).
    pub fn line_up(&mut self, extend: bool) -> bool {
        let start = line_start(&self.text, self.cursor);
        if start == 0 {
            return false;
        }
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        let target = *self
            .sticky_col
            .get_or_insert_with(|| display_col(&self.text, self.cursor));
        let prev_end = start - 1; // the '\n' before this line
        let prev_start = line_start(&self.text, prev_end);
        self.cursor = seek_col(&self.text, prev_start, prev_end, target);
        true
    }

    /// ↓ across rows, column-sticky; `false` on the last row (item 6's
    /// forward-history hook).
    pub fn line_down(&mut self, extend: bool) -> bool {
        let end = line_end(&self.text, self.cursor);
        if end >= self.text.len() {
            return false;
        }
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        let target = *self
            .sticky_col
            .get_or_insert_with(|| display_col(&self.text, self.cursor));
        let next_start = end + 1;
        let next_end = line_end(&self.text, next_start);
        self.cursor = seek_col(&self.text, next_start, next_end, target);
        true
    }

    #[must_use]
    pub fn on_first_line(&self) -> bool {
        line_start(&self.text, self.cursor) == 0
    }

    #[must_use]
    pub fn on_last_line(&self) -> bool {
        line_end(&self.text, self.cursor) >= self.text.len()
    }

    // ---- Mouse (item 5) ----

    /// Left button DOWN in the composer: place the cursor at the clicked
    /// boundary and park the anchor there (native caret law — the press
    /// places the caret; a drag then grows a selection from it).
    pub fn press_at(&mut self, byte: usize) {
        let at = snap(&self.text, byte);
        self.cursor = at;
        self.anchor = Some(at);
        self.sticky_col = None;
    }

    /// Drag with the button held: the cursor follows, the anchor stays.
    pub fn drag_to(&mut self, byte: usize) {
        self.cursor = snap(&self.text, byte);
    }

    // ---- History ring (item 6) ----

    /// ↑ on the first row: recall the previous submitted input. The live
    /// draft is stashed on first entry; recall places the cursor at END
    /// (Claude Code behavior). Returns whether anything changed.
    pub fn history_prev(&mut self) -> bool {
        let next_pos = match self.history_pos {
            None if self.history.is_empty() => return false,
            None => {
                self.history_stash = Some(std::mem::take(&mut self.text));
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(pos) => pos - 1,
        };
        self.history_pos = Some(next_pos);
        self.text = self.history[next_pos].clone();
        self.cursor = self.text.len();
        self.anchor = None;
        self.sticky_col = None;
        true
    }

    /// ↓ on the last row: forward through the ring; past the newest entry
    /// the stashed draft comes back (Claude Code behavior).
    pub fn history_next(&mut self) -> bool {
        let Some(pos) = self.history_pos else {
            return false;
        };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.text = self.history[pos + 1].clone();
        } else {
            self.history_pos = None;
            self.text = self.history_stash.take().unwrap_or_default();
        }
        self.cursor = self.text.len();
        self.anchor = None;
        self.sticky_col = None;
        true
    }

    // ---- Internals ----

    /// Extend-law prologue: ⇧+movement anchors on first use (item 4).
    fn pre_move(&mut self, extend: bool) {
        if extend && self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
    }

    fn delete_selection_if_any(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else {
            self.anchor = None;
            return false;
        };
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.anchor = None;
        true
    }

    /// Every edit drops sticky state and detaches a history browse (the
    /// edited text becomes the draft; the pre-browse stash is dropped —
    /// documented minimal-ring law).
    fn after_edit(&mut self) {
        self.sticky_col = None;
        self.history_pos = None;
        self.history_stash = None;
    }
}

/// Largest grapheme boundary `<= byte` (clamped to the text). Stale mouse
/// bytes (one-frame-old hit map) snap instead of panicking.
#[must_use]
pub fn snap(text: &str, byte: usize) -> usize {
    let at = byte.min(text.len());
    if at == text.len() {
        return at;
    }
    // Largest grapheme START <= at: a byte inside a cluster (between a
    // base and its combining mark, or mid-char) is not a caret stop.
    let mut best = 0;
    for (start, _) in text.grapheme_indices(true) {
        if start > at {
            break;
        }
        best = start;
    }
    best
}

fn prev_boundary(text: &str, at: usize) -> Option<usize> {
    text[..at]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
}

fn next_boundary(text: &str, at: usize) -> Option<usize> {
    text[at..].graphemes(true).next().map(|g| at + g.len())
}

/// Byte index of the current logical line's start.
#[must_use]
pub fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// Byte index of the current logical line's end (the `\n`, or text end).
#[must_use]
pub fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

/// Display column (cells) of `at` within its line — wide glyphs count 2.
#[must_use]
pub fn display_col(text: &str, at: usize) -> usize {
    text[line_start(text, at)..at].width()
}

/// The grapheme boundary in `line_start..=line_end` whose display column
/// best matches `target`: the last boundary at or before the target cell
/// (a wide glyph straddling the target keeps the cursor before it).
fn seek_col(text: &str, start: usize, end: usize, target: usize) -> usize {
    let mut col = 0;
    for (offset, g) in text[start..end].grapheme_indices(true) {
        let w = g.width();
        if col + w > target {
            return start + offset;
        }
        col += w;
    }
    end
}

/// The byte offset of the grapheme containing display column `col` of
/// `content` (TUI5 item 5: a click lands the caret at the START of the
/// clicked grapheme — cell-granular floor, the documented choice; past
/// the end it clamps to the end, so clicking the empty right half of a
/// row parks the caret at the line's visible end like every native
/// input).
#[must_use]
pub fn byte_at_col(content: &str, col: usize) -> usize {
    let mut acc = 0;
    for (offset, grapheme) in content.grapheme_indices(true) {
        let w = grapheme.width().max(1);
        if acc + w > col {
            return offset;
        }
        acc += w;
    }
    content.len()
}

/// A word for ⌥-movement: a unicode word-bound segment containing an
/// alphanumeric (whitespace and punctuation runs are skipped over).
fn is_wordy(seg: &str) -> bool {
    seg.chars().any(char::is_alphanumeric)
}

/// Start of the word before `at` (or 0).
#[must_use]
pub fn word_left_of(text: &str, at: usize) -> usize {
    text[..at]
        .split_word_bound_indices()
        .rev()
        .find(|(_, seg)| is_wordy(seg))
        .map_or(0, |(i, _)| i)
}

/// End of the word after `at` (or text end).
#[must_use]
pub fn word_right_of(text: &str, at: usize) -> usize {
    text[at..]
        .split_word_bound_indices()
        .find(|(_, seg)| is_wordy(seg))
        .map_or(text.len(), |(i, seg)| at + i + seg.len())
}
