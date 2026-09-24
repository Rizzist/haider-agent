//! Authoritative line spans of an edit call (973-tui-toolview).
//!
//! The filesystem edit applies its replacements one at a time. While it
//! does, this tracker records, for every replacement:
//!
//! * the line where the replaced text started in the PRE-edit file — when
//!   that text was one contiguous run of the original file (an earlier
//!   replacement in the same call may have produced part of it, or deleted
//!   something from the middle of it; then there is no true pre-edit line);
//! * the line where the inserted text starts in the POST-edit file — when it
//!   survived the rest of the call intact (a later replacement may have
//!   changed it; then there is no true post-edit line).
//!
//! Positions are tracked in bytes through a map of the regions the call
//! produced, so no line is ever INFERRED from text: a transcript reader
//! numbers a diff from these spans or not at all.

use haider_protocol::tool::EditSpanV1;

/// A region of the current text produced by a replacement, and the extent
/// of the ORIGINAL text it stands in for. Sorted, disjoint.
#[derive(Debug, Clone, Copy)]
struct Produced {
    cur_start: usize,
    cur_end: usize,
    orig_start: usize,
    orig_end: usize,
}

impl Produced {
    const fn cur_len(&self) -> usize {
        self.cur_end - self.cur_start
    }

    const fn orig_len(&self) -> usize {
        self.orig_end - self.orig_start
    }
}

#[derive(Debug, Clone)]
struct Pending {
    edit_index: u32,
    occurrence: u32,
    old_start_line: Option<u32>,
    old_line_count: u32,
    /// The inserted text's range in the CURRENT text.
    cur_start: usize,
    cur_end: usize,
    new_line_count: u32,
    /// Still intact: no later replacement touched it.
    alive: bool,
}

/// Measures every replacement of one edit call as it is applied.
#[derive(Debug, Clone)]
pub struct SpanTracker {
    original: String,
    produced: Vec<Produced>,
    pending: Vec<Pending>,
}

fn line_at(text: &str, offset: usize) -> Option<u32> {
    let before = text.get(..offset)?;
    u32::try_from(before.bytes().filter(|byte| *byte == b'\n').count() + 1).ok()
}

fn line_count(text: &str) -> u32 {
    u32::try_from(text.lines().count()).unwrap_or(u32::MAX)
}

impl SpanTracker {
    /// Start tracking an edit of `original` (the pre-edit file text).
    #[must_use]
    pub fn new(original: &str) -> Self {
        Self {
            original: original.to_owned(),
            produced: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Where current offset `at` sits in the original text, or `None` when
    /// it lies strictly inside a produced region. A deletion exactly AT
    /// `at` is counted as already passed when `past_deletions` (the offset
    /// names the character AFTER it — a range's start) and as not yet
    /// reached otherwise (the offset closes a range — its end).
    fn to_original(&self, at: usize, past_deletions: bool) -> Option<usize> {
        let mut shift: isize = 0;
        for region in &self.produced {
            if region.cur_start < at && at < region.cur_end {
                return None;
            }
            let passed = if region.cur_len() == 0 && region.cur_start == at {
                past_deletions
            } else {
                region.cur_end <= at
            };
            if passed {
                shift += region.orig_len() as isize - region.cur_len() as isize;
            } else if region.cur_start >= at {
                break;
            }
        }
        at.checked_add_signed(shift)
    }

    /// Replace `old` at byte offset `at` of `text` with `new`, recording the
    /// replacement's span. The caller guarantees `text[at..]` starts with
    /// `old` (the edit already located it).
    pub fn replace(
        &mut self,
        text: &mut String,
        at: usize,
        old: &str,
        new: &str,
        edit_index: u32,
        occurrence: u32,
    ) {
        let end = at + old.len();
        debug_assert_eq!(text.get(at..end), Some(old));
        // PRE-edit line: only for a replaced range that is untouched
        // original text — no produced byte inside it, and no deletion point
        // strictly inside it (the original would not be contiguous there).
        let untouched = self.produced.iter().all(|region| {
            if region.cur_len() == 0 {
                !(at < region.cur_start && region.cur_start < end)
            } else {
                region.cur_end <= at || region.cur_start >= end
            }
        });
        let old_start_line = untouched
            .then(|| self.to_original(at, true))
            .flatten()
            .and_then(|orig| line_at(&self.original, orig));

        // The produced region after this replacement: the replaced range
        // UNIONED with every produced region it overlaps (or whose deletion
        // point it covers) — a region only partly replaced keeps its
        // untouched produced remainder inside the new region, which stands
        // in for the union of their original extents.
        let mut union_start = at;
        let mut union_end = end;
        let mut orig_start = None;
        let mut orig_end = None;
        let mut merged = Vec::new();
        for (index, region) in self.produced.iter().enumerate() {
            let overlaps = if region.cur_len() == 0 {
                at <= region.cur_start && region.cur_start <= end
            } else {
                region.cur_start < end && region.cur_end > at
            };
            if overlaps {
                merged.push(index);
                union_start = union_start.min(region.cur_start);
                union_end = union_end.max(region.cur_end);
                orig_start =
                    Some(orig_start.map_or(region.orig_start, |s: usize| s.min(region.orig_start)));
                orig_end =
                    Some(orig_end.map_or(region.orig_end, |e: usize| e.max(region.orig_end)));
            }
        }
        // A union edge a merged region supplies already carries that region's
        // original edge; only an edge that is the replaced range's own is
        // mapped (mapping a merged region's edge again would count a
        // deletion that sits beside it twice).
        let merged_starts_at = |edge: usize| {
            merged
                .iter()
                .any(|&index| self.produced[index].cur_start == edge)
        };
        let merged_ends_at = |edge: usize| {
            merged
                .iter()
                .any(|&index| self.produced[index].cur_end == edge)
        };
        if !merged_starts_at(union_start)
            && let Some(start) = self.to_original(union_start, false)
        {
            orig_start = Some(orig_start.map_or(start, |s| s.min(start)));
        }
        if !merged_ends_at(union_end)
            && let Some(stop) = self.to_original(union_end, true)
        {
            orig_end = Some(orig_end.map_or(stop, |e| e.max(stop)));
        }
        let orig_start = orig_start.unwrap_or(0);
        let orig_end = orig_end.unwrap_or(orig_start).max(orig_start);

        text.replace_range(at..end, new);
        let delta = new.len() as isize - old.len() as isize;

        // Earlier insertions this replacement touched are no longer intact;
        // everything after it moves by `delta`.
        for span in &mut self.pending {
            if !span.alive {
                continue;
            }
            let touched = if span.cur_start == span.cur_end {
                at < span.cur_start && span.cur_start < end
            } else {
                span.cur_start < end && span.cur_end > at
            };
            if touched {
                span.alive = false;
                continue;
            }
            if span.cur_start >= end {
                span.cur_start = span.cur_start.saturating_add_signed(delta);
                span.cur_end = span.cur_end.saturating_add_signed(delta);
            }
        }
        let mut next: Vec<Produced> = Vec::with_capacity(self.produced.len() + 1);
        for (index, region) in self.produced.iter().enumerate() {
            if merged.contains(&index) {
                continue;
            }
            let mut region = *region;
            if region.cur_start >= end {
                region.cur_start = region.cur_start.saturating_add_signed(delta);
                region.cur_end = region.cur_end.saturating_add_signed(delta);
            }
            next.push(region);
        }
        next.push(Produced {
            cur_start: union_start,
            cur_end: union_end.saturating_add_signed(delta),
            orig_start,
            orig_end,
        });
        next.sort_by_key(|region| (region.cur_start, region.cur_end));
        self.produced = next;

        self.pending.push(Pending {
            edit_index,
            occurrence,
            old_start_line,
            old_line_count: line_count(old),
            cur_start: at,
            cur_end: at + new.len(),
            new_line_count: line_count(new),
            alive: true,
        });
    }

    /// The spans, in application order, against the final `text`.
    #[must_use]
    pub fn finish(self, text: &str) -> Vec<EditSpanV1> {
        self.pending
            .into_iter()
            .map(|span| EditSpanV1 {
                edit_index: span.edit_index,
                occurrence: span.occurrence,
                old_start_line: span.old_start_line,
                old_line_count: span.old_line_count,
                new_start_line: if span.alive {
                    line_at(text, span.cur_start)
                } else {
                    None
                },
                new_line_count: span.new_line_count,
            })
            .collect()
    }
}
