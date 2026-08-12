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
//!
//! SOFT WRAP (TUI6 items 1-5): a logical line longer than the render
//! budget wraps into VISUAL rows at grapheme boundaries ([`wrap_rows`]).
//! Wrap points are pure geometry over (text, budget) — derived on demand
//! by the renderer each frame and by ↑/↓ each keypress, NEVER stored —
//! so a resize reflows by construction. The renderer feeds the current
//! budget back through a `Cell` ([`Composer::set_wrap_budget`], the
//! `scroll_max` pattern) so navigation walks the SAME rows the frame
//! painted while the reducer keeps calling `line_up`/`line_down`
//! untouched (the stays-put law).

use haider_protocol::ids::ArtifactRef;
use haider_protocol::tool::AttachmentBlock;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Submitted-input ring cap (item 6's "minimal per-session ring").
/// Claude Code keeps a long per-project history; the demo's transient ring
/// keeps the last 50 — enough for any session, never persisted.
const HISTORY_CAP: usize = 50;

/// What one pending attachment will become on the wire (B4b). The chip
/// holds the half known at ISSUANCE (mime / line count); the daemon's
/// verified content address arrives with the `artifact.put` reply and
/// completes the [`AttachmentBlock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingKind {
    /// `/attach <path>` — a magic-sniffed image.
    Image { mime: String },
    /// The big-paste pill — UTF-8 pasted text stored by ref, never copied
    /// into the tree (tool.rs's intended composer-token vocabulary).
    PastedText { lines: u32 },
    /// `/attach <path>` text fallback (G2) — a UTF-8 file stored by ref.
    /// Carries the sanitized BASENAME because the wire block names the
    /// file for the model; the chip shows `name · N lines`.
    File { name: String, lines: u32 },
    /// `/attach <path.pdf>` — first-class PDF with verified page count.
    Pdf { name: String, pages: u32 },
}

impl PendingKind {
    #[must_use]
    pub const fn glyph(&self) -> &'static str {
        match self {
            Self::Pdf { .. } => "▤",
            Self::Image { .. } | Self::PastedText { .. } | Self::File { .. } => "⌁",
        }
    }
}

/// One attachment the draft will carry on its next submit (B4b).
///
/// It lives ON the composer so the draft stash/restore round trip parks
/// and revives it with the text: a surface switch (or a `/branch` switch,
/// which never swaps the session's draft) cannot lose a chip. `artifact`
/// is `None` while the receipt-free upload is in flight; only the daemon's
/// `artifact.put` reply — its VERIFIED content address — completes it
/// (nothing local mints a ref the CAS cannot honor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttachment {
    /// Client-side correlation id: `artifact.put` is receipt-free (no
    /// command id), so the link's request context carries this back.
    pub upload: u64,
    /// Chip label ("photo.png · 41 KB", "[Pasted 12 lines]").
    pub label: String,
    pub kind: PendingKind,
    /// The daemon's verified CAS address, once the upload answers.
    pub artifact: Option<ArtifactRef>,
}

impl PendingAttachment {
    /// The wire block, once (and only once) the upload completed.
    #[must_use]
    pub fn ready_block(&self) -> Option<AttachmentBlock> {
        let artifact = self.artifact.clone()?;
        Some(match &self.kind {
            PendingKind::Image { mime } => AttachmentBlock::Image {
                artifact,
                mime: mime.clone(),
                width: None,
                height: None,
            },
            PendingKind::PastedText { lines } => AttachmentBlock::PastedText {
                artifact,
                lines: *lines,
            },
            PendingKind::File { name, lines } => AttachmentBlock::File {
                artifact,
                name: name.clone(),
                lines: *lines,
            },
            PendingKind::Pdf { name, pages } => AttachmentBlock::Pdf {
                artifact,
                name: name.clone(),
                pages: *pages,
                delivery: haider_protocol::tool::PdfDeliveryMode::ExtractedText,
            },
        })
    }
}

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
    /// Preferred display column for a run of ↑/↓ (column-sticky like
    /// every editor), TAGGED with the wrap budget that MINTED it
    /// (TUI6.2 fix 1, review r2 finding 1): the column is wrap-ROW
    ///-relative, so it is geometry state — a budget change (resize,
    /// draft swap, restore) makes it meaningless. Consumption validates
    /// the mint tag and re-derives on mismatch, so a stale column is
    /// unrepresentable at the only sites that read it. Any horizontal
    /// movement or edit still clears it outright.
    sticky_col: Option<(usize, usize)>,
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
    /// Text-mutation counter (TUI5.1 fix 2): every change to `text` bumps
    /// it, and every rendered composer hit carries the revision it was
    /// drawn from — the press/drag path drops any hit whose revision (or
    /// surface) no longer matches, so a stale frame can never move the
    /// caret into fresh text. Selection/cursor moves do NOT bump it: they
    /// leave the rendered windows byte-accurate.
    revision: u64,
    /// The display-cell budget the renderer last wrapped this composer's
    /// text into (TUI6). Frame FEEDBACK, not wrap state: the render sets
    /// it every frame ([`crate::render`]'s `scroll_max` discipline) and
    /// ↑/↓ re-derive the visual rows from it on demand — no wrap POINT is
    /// ever stored, so the model cannot go stale across a resize (the
    /// next frame overwrites the budget before the next keypress can
    /// read it). `0` is the headless degenerate (nothing has rendered):
    /// visual rows fall back to the logical lines.
    wrap_budget: std::cell::Cell<usize>,
    /// Pending attachment chips (B4b) — refs the NEXT submit carries.
    /// Deliberately untouched by [`Self::take_for_submit`] /
    /// [`Self::take_silent`] / history browsing: a slash command or a
    /// free-text menu answer consumes the TEXT, never the chips. Only
    /// [`Self::take_ready_attachments`] (a real turn) drains them.
    attachments: Vec<PendingAttachment>,
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

    /// The text-mutation revision this composer is at (TUI5.1 fix 2).
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Frame feedback from the renderer (TUI6): the display-cell budget
    /// its wrap geometry used this frame. `&self` by design — the render
    /// borrows the model shared, exactly like `scroll_max`.
    pub fn set_wrap_budget(&self, budget: usize) {
        self.wrap_budget.set(budget);
    }

    /// The last-fed wrap budget (TUI6.1: the draft-swap seam carries the
    /// ACTIVE composer's budget onto a restored draft — a parked draft's
    /// own cell is as old as its last render).
    #[must_use]
    pub fn wrap_budget(&self) -> usize {
        self.wrap_budget.get()
    }

    /// The visual rows of the current draft at the last rendered budget —
    /// the SAME geometry the frame painted (or one row per logical line
    /// while nothing has rendered).
    #[must_use]
    pub fn visual_rows(&self) -> Vec<WrapRow> {
        wrap_rows(&self.text, self.wrap_budget.get())
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
        self.revision = self.revision.wrapping_add(1);
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
        self.revision = self.revision.wrapping_add(1);
        self.record_submitted(text.trim());
        self.cursor = 0;
        self.anchor = None;
        self.sticky_col = None;
        self.history_pos = None;
        self.history_stash = None;
        text
    }

    /// Take the draft WITHOUT recording it (review P3-9): the slash-submit
    /// path lets [`crate::app::AppModel`]'s `execute_slash` record the
    /// CANONICAL command form instead, so a verbatim-vs-canonical pair
    /// can never double-record past the consecutive-dupe check.
    pub fn take_silent(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.revision = self.revision.wrapping_add(1);
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

    // ---- Pending attachments (B4b) ----

    #[must_use]
    pub fn attachments(&self) -> &[PendingAttachment] {
        &self.attachments
    }

    #[must_use]
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }

    /// Any chip whose upload has not answered yet — the submit gate.
    #[must_use]
    pub fn has_uploading_attachment(&self) -> bool {
        self.attachments.iter().any(|chip| chip.artifact.is_none())
    }

    pub fn push_attachment(&mut self, chip: PendingAttachment) {
        self.attachments.push(chip);
    }

    /// ⌫ at the very start of the draft removes the NEWEST chip (chip-UI
    /// law). Returns its label for the caller's display truth.
    pub fn remove_newest_attachment(&mut self) -> Option<String> {
        self.attachments.pop().map(|chip| chip.label)
    }

    /// The daemon's verified content address landed for `upload`. A chip
    /// the user already removed stays removed — the reply resurrects
    /// nothing (the CAS keeps the orphan bytes; nothing references them).
    pub fn complete_attachment(&mut self, upload: u64, artifact: ArtifactRef) -> bool {
        match self
            .attachments
            .iter_mut()
            .find(|chip| chip.upload == upload)
        {
            Some(chip) => {
                chip.artifact = Some(artifact);
                true
            }
            None => false,
        }
    }

    /// The upload for `upload` failed — the chip dies (a chip whose bytes
    /// the CAS never accepted would submit a dangling ref). Returns the
    /// label for the honest notice.
    pub fn fail_attachment(&mut self, upload: u64) -> Option<String> {
        let index = self
            .attachments
            .iter()
            .position(|chip| chip.upload == upload)?;
        Some(self.attachments.remove(index).label)
    }

    /// Drop every chip still awaiting its upload reply (the disconnect
    /// sweep: `artifact.put` is receipt-free, so no reply survives a
    /// socket). Returns how many died, for the honest notice.
    pub fn drop_uploading_attachments(&mut self) -> usize {
        let before = self.attachments.len();
        self.attachments.retain(|chip| chip.artifact.is_some());
        before - self.attachments.len()
    }

    /// Drain the chips into wire blocks for a REAL submit. Callers gate on
    /// [`Self::has_uploading_attachment`] first, so by construction every
    /// chip is ready; a not-yet-ready straggler is dropped rather than
    /// submitted as a dangling ref.
    pub fn take_ready_attachments(&mut self) -> Vec<AttachmentBlock> {
        std::mem::take(&mut self.attachments)
            .iter()
            .filter_map(PendingAttachment::ready_block)
            .collect()
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
        if !extend {
            // Review P1-1: a plain click PARKS the anchor at the caret
            // (press_at) for ⇧/drag extension; a plain move must drop it
            // or the next step materializes a phantom 1-grapheme
            // selection that eats text and hijacks ⌃C.
            self.anchor = None;
        }
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
        if !extend {
            self.anchor = None; // review P1-1, as move_left
        }
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
            // Review P3-7: collapse to the selection's LEFT edge first,
            // then travel (native option-arrow law).
            if let Some((start, _)) = self.selection_range() {
                self.cursor = start;
            }
            self.anchor = None;
        }
        self.cursor = word_left_of(&self.text, self.cursor);
    }

    /// ⌥→ — to the end of the next word.
    pub fn word_right(&mut self, extend: bool) {
        self.sticky_col = None;
        self.pre_move(extend);
        if !extend {
            if let Some((_, end)) = self.selection_range() {
                self.cursor = end; // review P3-7, as word_left
            }
            self.anchor = None;
        }
        self.cursor = word_right_of(&self.text, self.cursor);
    }

    /// Home / ⌃A — LOGICAL line start (Claude Code binds ⌃A here).
    ///
    /// The TUI6 pairing, documented (brief item 3): ↑/↓ move across
    /// VISUAL (wrapped) rows, Home/End jump to LOGICAL line edges —
    /// arrows are visual, jumps are logical, the Claude Code convention.
    /// On a wrapped line Home therefore crosses wrap points to byte 0 of
    /// the line, never to the current visual row's start.
    pub fn line_home(&mut self, extend: bool) {
        self.sticky_col = None;
        self.pre_move(extend);
        if !extend {
            if let Some((start, _)) = self.selection_range() {
                self.cursor = start; // review P3-7
            }
            self.anchor = None;
        }
        self.cursor = line_start(&self.text, self.cursor);
    }

    /// End / ⌃E — LOGICAL line end (the ↑↓-visual / Home-End-logical
    /// pairing documented at [`Self::line_home`]).
    pub fn line_end_key(&mut self, extend: bool) {
        self.sticky_col = None;
        self.pre_move(extend);
        if !extend {
            if let Some((_, end)) = self.selection_range() {
                self.cursor = end; // review P3-7
            }
            self.anchor = None;
        }
        self.cursor = line_end(&self.text, self.cursor);
    }

    /// ↑ across VISUAL rows, column-sticky. Returns `false` when already
    /// on the first row — the caller's history hook (item 6).
    ///
    /// TUI6 re-scope (directed): TUI5 shipped this over LOGICAL lines
    /// with the documented reduction "the composer has no soft wrap, so
    /// visual rows ARE the logical lines". The owner's soft-wrap law
    /// (TUI6 items 1/3) removed the reduction: rows here are the wrapped
    /// [`WrapRow`]s the renderer paints (same [`wrap_rows`] geometry via
    /// [`Self::visual_rows`]), restoring the TUI5 brief's ORIGINAL
    /// wording. Headless (budget 0) the rows degrade to the logical
    /// lines, so the pre-TUI6 unit corpus still describes this function.
    pub fn line_up(&mut self, extend: bool) -> bool {
        // Review P3-6: plain ↑ with an active selection COLLAPSES to its
        // start (native inputs) and consumes the press — recall needs a
        // second, selection-free ↑.
        if !extend && let Some((sel_start, _)) = self.selection_range() {
            self.cursor = sel_start;
            self.anchor = None;
            self.sticky_col = None;
            return true;
        }
        let rows = self.visual_rows();
        let at = visual_row_of(&rows, self.cursor);
        if at == 0 {
            // TUI5.1 fix 4: ⇧↑ on the FIRST row extends the selection to
            // the buffer start (item 4's outer-edge law); plain ↑ still
            // falls through to the caller's history hook.
            if extend && self.cursor > 0 {
                self.pre_move(true);
                self.sticky_col = None;
                self.cursor = 0;
                return true;
            }
            return false;
        }
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        let target = self.sticky_col_for(rows[at]);
        self.cursor = seek_col_in_row(&self.text, rows[at - 1], target);
        true
    }

    /// ↓ across VISUAL rows, column-sticky; `false` on the last row
    /// (item 6's forward-history hook). Wrap semantics as [`Self::line_up`]
    /// (the TUI6 re-scope note there).
    pub fn line_down(&mut self, extend: bool) -> bool {
        // Review P3-6 mirror: plain ↓ collapses to the selection END.
        if !extend && let Some((_, sel_end)) = self.selection_range() {
            self.cursor = sel_end;
            self.anchor = None;
            self.sticky_col = None;
            return true;
        }
        let rows = self.visual_rows();
        let at = visual_row_of(&rows, self.cursor);
        if at + 1 >= rows.len() {
            // TUI5.1 fix 4 mirror: ⇧↓ on the LAST row extends to the
            // buffer end.
            if extend && self.cursor < self.text.len() {
                self.pre_move(true);
                self.sticky_col = None;
                self.cursor = self.text.len();
                return true;
            }
            return false;
        }
        self.pre_move(extend);
        if !extend {
            self.anchor = None;
        }
        let target = self.sticky_col_for(rows[at]);
        self.cursor = seek_col_in_row(&self.text, rows[at + 1], target);
        true
    }

    /// On the first VISUAL row (TUI6 — was logical; identical while no
    /// wrap budget is known).
    #[must_use]
    pub fn on_first_line(&self) -> bool {
        visual_row_of(&self.visual_rows(), self.cursor) == 0
    }

    /// On the last VISUAL row (TUI6 mirror of [`Self::on_first_line`]).
    #[must_use]
    pub fn on_last_line(&self) -> bool {
        let rows = self.visual_rows();
        visual_row_of(&rows, self.cursor) + 1 >= rows.len()
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
        self.revision = self.revision.wrapping_add(1);
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
        self.revision = self.revision.wrapping_add(1);
        self.cursor = self.text.len();
        self.anchor = None;
        self.sticky_col = None;
        true
    }

    /// The sticky column for a ↑/↓ step from the visual row `at` — THE
    /// one consumption seam of the mint-tagged cache (TUI6.2 fix 1,
    /// review r2 finding 1: budget 13's cached col 4 walked budget 5's
    /// rows to byte 24 where the current-geometry col 2 lands 22; the
    /// parked-draft path preserved the same stale column, and
    /// ⇧-extension shares this path). A cached column minted under any
    /// OTHER budget is re-derived from the caret's position in the
    /// CURRENT row and re-minted — stale reads are unrepresentable.
    fn sticky_col_for(&mut self, row: WrapRow) -> usize {
        let budget = self.wrap_budget.get();
        match self.sticky_col {
            Some((minted, col)) if minted == budget => col,
            _ => {
                let col = cluster_cols(&self.text[row.start..self.cursor]);
                self.sticky_col = Some((budget, col));
                col
            }
        }
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
    ///
    /// TUI5.1 fix 1: it also NORMALIZES cursor and anchor to grapheme
    /// boundaries of the NEW text and bumps the hit revision. An edit can
    /// merge clusters around the caret (inserting a ZWJ between 👩👩,
    /// deleting the x from 🇦x🇧 joins the flags) leaving byte offsets
    /// INSIDE the merged grapheme — the render styles only grapheme
    /// starts, so the cursor cell would vanish and further edits could
    /// split the cluster. Normalizing here, at the single seam every
    /// mutation passes through, makes interior-byte state unrepresentable
    /// post-edit rather than patched per-callsite.
    fn after_edit(&mut self) {
        self.cursor = nearest_boundary(&self.text, self.cursor);
        self.anchor = self
            .anchor
            .map(|anchor| nearest_boundary(&self.text, anchor))
            .filter(|anchor| *anchor != self.cursor);
        self.revision = self.revision.wrapping_add(1);
        self.sticky_col = None;
        self.history_pos = None;
        self.history_stash = None;
    }
}

/// The grapheme boundary of `text` NEAREST `byte` (ties round FORWARD —
/// after an edit the caret stays past what was just typed/joined). The
/// post-edit normalization seam (TUI5.1 fix 1).
#[must_use]
pub fn nearest_boundary(text: &str, byte: usize) -> usize {
    let floor = snap(text, byte);
    if floor == byte {
        return byte;
    }
    let ceil = next_boundary(text, floor).unwrap_or(text.len());
    if byte - floor < ceil - byte {
        floor
    } else {
        ceil
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

/// One VISUAL row of wrapped composer text (TUI6 items 1-5): the byte
/// range of `text` it shows (never containing the `\n`) and whether it is
/// the LAST visual row of its logical line — the row that owns the
/// line-end caret cell (the position ON the `\n`, or the text end).
///
/// A caret byte exactly ON a wrap point belongs to the FOLLOWING row (it
/// renders at that row's first cell). That is the documented no-affinity
/// reduction: native inputs disambiguate the two positions with cursor
/// affinity state; the port keeps the model affinity-free and picks the
/// next-row reading everywhere — render, click mapping and ↑/↓ all agree
/// through [`visual_row_of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapRow {
    /// Absolute byte offset of the row's first grapheme.
    pub start: usize,
    /// Absolute byte offset one past the row's last grapheme.
    pub end: usize,
    /// Last visual row of its logical line.
    pub line_last: bool,
}

/// THE cluster width policy (TUI6.1 fix 3, review r1 finding 3): a
/// grapheme cluster costs its display width, and a ZERO-WIDTH cluster —
/// a combining mark or ZWJ standing alone at a line start, reachable by
/// paste — costs ONE real cell (the renderer gives it a space base, the
/// way terminals conventionally present a bare combining mark). Every
/// consumer of composer geometry MUST price clusters through this one
/// function: `wrap_rows`, `seek_col`, the sticky-column math, click
/// mapping ([`byte_at_col`]) and the renderer's span builder — render,
/// click and navigation can then never disagree about which cell a byte
/// lives in.
#[must_use]
pub fn cluster_cells(grapheme: &str) -> usize {
    grapheme.width().max(1)
}

/// Display cells of a whole slice under the SAME policy — grapheme-wise,
/// never `str::width` (which prices a standalone zero-width cluster at 0
/// and would skew every column right of it by one).
#[must_use]
pub fn cluster_cols(text: &str) -> usize {
    text.graphemes(true).map(cluster_cells).sum()
}

/// Wrap `text` into visual rows of at most `budget` display cells,
/// breaking ONLY at grapheme boundaries — the `nearest_boundary`/
/// `after_edit` discipline extended to wrap points (a wrap point and a
/// caret stop are the same kind of place, so a row can never orphan a
/// combining mark or split a ZWJ family). Pure geometry over
/// (text, budget): derived fresh by every caller, never stored, which is
/// what makes a resize reflow by construction (TUI6 item 5).
///
/// `budget == 0` is the headless degenerate (no renderer has spoken):
/// every logical line is one visual row. A single grapheme WIDER than
/// the budget still gets a row of its own — the walk always advances,
/// the terminal clips the overflow.
#[must_use]
pub fn wrap_rows(text: &str, budget: usize) -> Vec<WrapRow> {
    let mut rows = Vec::new();
    let mut line_offset = 0;
    for line in text.split('\n') {
        if budget == 0 || cluster_cols(line) <= budget {
            rows.push(WrapRow {
                start: line_offset,
                end: line_offset + line.len(),
                line_last: true,
            });
        } else {
            let mut row_start = 0;
            let mut col = 0;
            for (offset, grapheme) in line.grapheme_indices(true) {
                let width = cluster_cells(grapheme);
                if offset > row_start && col + width > budget {
                    rows.push(WrapRow {
                        start: line_offset + row_start,
                        end: line_offset + offset,
                        line_last: false,
                    });
                    row_start = offset;
                    col = 0;
                }
                col += width;
            }
            rows.push(WrapRow {
                start: line_offset + row_start,
                end: line_offset + line.len(),
                line_last: true,
            });
        }
        line_offset += line.len() + 1;
    }
    rows
}

/// The index of the visual row that renders the caret at `cursor`: the
/// row containing it, where a caret ON a wrap point belongs to the
/// following row and a caret at a line's end belongs to the line's LAST
/// row (the [`WrapRow`] no-affinity law).
#[must_use]
pub fn visual_row_of(rows: &[WrapRow], cursor: usize) -> usize {
    rows.iter()
        .position(|row| {
            cursor >= row.start && (cursor < row.end || (row.line_last && cursor == row.end))
        })
        .unwrap_or(rows.len().saturating_sub(1))
}

/// [`seek_col`] scoped to one visual row: the caret lands at the row's
/// grapheme boundary nearest `target` — but a WRAP row's end boundary is
/// the next row's start (no-affinity), so ↑/↓ into a full wrap row stop
/// at its last grapheme instead of sliding through to the row below.
fn seek_col_in_row(text: &str, row: WrapRow, target: usize) -> usize {
    let sought = seek_col(text, row.start, row.end, target);
    if sought == row.end && !row.line_last {
        prev_boundary(text, sought).map_or(row.start, |prev| prev.max(row.start))
    } else {
        sought
    }
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

/// The grapheme boundary in `line_start..=line_end` whose display column
/// best matches `target`: the last boundary at or before the target cell
/// (a wide glyph straddling the target keeps the cursor before it).
fn seek_col(text: &str, start: usize, end: usize, target: usize) -> usize {
    let mut col = 0;
    for (offset, g) in text[start..end].grapheme_indices(true) {
        let w = cluster_cells(g);
        if col + w > target {
            return start + offset;
        }
        col += w;
    }
    end
}

/// The byte offset of the caret boundary NEAREST display column `col`
/// of `content` (TUI5 item 5, review P3-4): a single-cell grapheme takes
/// the caret at its start; clicking the RIGHT cell of a wide glyph
/// rounds the caret to after it (native behavior). Past the end it
/// clamps to the end, so clicking the empty right half of a row parks
/// the caret at the line's visible end like every native input.
#[must_use]
pub fn byte_at_col(content: &str, col: usize) -> usize {
    let mut acc = 0;
    for (offset, grapheme) in content.grapheme_indices(true) {
        let w = cluster_cells(grapheme);
        if acc + w > col {
            if col - acc >= w.div_ceil(2).max(1) {
                return offset + grapheme.len();
            }
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
