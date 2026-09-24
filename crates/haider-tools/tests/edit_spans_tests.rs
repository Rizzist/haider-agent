//! 973-tui-toolview repair 4: the edit tool measures every replacement's
//! line span WHILE applying it, and those spans are the only source a
//! transcript numbers an edit's diff from. These laws check the spans
//! against an independent character-provenance oracle.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::tool::EditSpanV1;
use haider_tools::{FsEdit, FsEditChange, apply_fs_edit_text};

fn edit(old: &str, new: &str) -> FsEditChange {
    FsEditChange::new(old, new)
}

fn apply(original: &str, edits: Vec<FsEditChange>) -> (String, Vec<EditSpanV1>) {
    let (text, _, spans) =
        apply_fs_edit_text(&FsEdit::many("f.txt", edits), original.to_owned()).expect("valid edit");
    (text, spans)
}

fn span(
    edit_index: u32,
    old: Option<u32>,
    old_lines: u32,
    new: Option<u32>,
    new_lines: u32,
) -> EditSpanV1 {
    EditSpanV1 {
        edit_index,
        occurrence: 0,
        old_start_line: old,
        old_line_count: old_lines,
        new_start_line: new,
        new_line_count: new_lines,
    }
}

/// Verify 4, live case 1: a single edit whose new text overlaps an earlier
/// copy of itself. TODO was line 3; the new braces are lines 3 and 4.
#[test]
fn a_self_overlapping_single_edit_is_measured_where_it_landed() {
    let (text, spans) = apply("fn f() {\n}\nTODO\n", vec![edit("TODO", "}\n}")]);
    assert_eq!(text, "fn f() {\n}\n}\n}\n");
    assert_eq!(spans, vec![span(0, Some(3), 1, Some(3), 2)]);
}

/// Verify 4, live case 2: a later edit consumes part of an earlier edit's
/// new text. The first replacement's inserted text no longer exists intact
/// (no post-edit line); its pre-edit line (1) is still true. The second
/// replacement's old text was partly produced by the first (no pre-edit
/// line); its new text is on line 1.
#[test]
fn a_later_edit_over_an_earlier_edits_text_is_measured_honestly() {
    let (text, spans) = apply(
        "foo(1);\nbar(1)\n",
        vec![edit("foo(1)", "bar(1)"), edit("1);", "2);")],
    );
    assert_eq!(text, "bar(2);\nbar(1)\n");
    assert_eq!(
        spans,
        vec![span(0, Some(1), 1, None, 1), span(1, None, 1, Some(1), 1)]
    );
}

/// Verify 4's probe named cases (`9-verify4-opus/probe/anchors_probe.out`),
/// against its own truth column (placed line, final line, alive).
#[test]
fn the_verifiers_probe_cases_measure_their_truth() {
    // A-single-selfoverlap: truth (2, 2, alive).
    let (text, spans) = apply("a\nX\n", vec![edit("X", "a\na")]);
    assert_eq!(text, "a\na\na\n");
    assert_eq!(spans, vec![span(0, Some(2), 1, Some(2), 2)]);
    // B-multi-partial-overlap: truth [(1, -, dead), (1, 1, alive)].
    let (text, spans) = apply("xac\nxb\n", vec![edit("xa", "xb"), edit("bc", "Q")]);
    assert_eq!(text, "xQ\nxb\n");
    assert_eq!(spans[0].old_start_line, Some(1));
    assert_eq!(spans[0].new_start_line, None, "consumed by the second edit");
    assert_eq!(
        spans[1].old_start_line, None,
        "partly produced by the first edit"
    );
    assert_eq!(spans[1].new_start_line, Some(1));
    // C-multi-independent: truth [(1,1), (3,3)].
    let (_, spans) = apply("p\nq\nr\n", vec![edit("p", "P"), edit("r", "R")]);
    assert_eq!(
        spans,
        vec![
            span(0, Some(1), 1, Some(1), 1),
            span(1, Some(3), 1, Some(3), 1)
        ]
    );
}

/// Verify 3's boundary case: the joined line shifts `c` from line 3 (before)
/// to line 2 (after) — both measured, neither inferred.
#[test]
fn a_deleted_line_boundary_is_measured_on_both_sides() {
    let (text, spans) = apply("a\nb\nc\n", vec![edit("a\n", "a"), edit("c", "C")]);
    assert_eq!(text, "ab\nC\n");
    assert_eq!(
        spans,
        vec![
            span(0, Some(1), 1, Some(1), 1),
            span(1, Some(3), 1, Some(2), 1)
        ]
    );
}

#[test]
fn replace_all_records_every_occurrence_in_order() {
    let (text, spans) = apply(
        "x\ny\nx\nx\n",
        vec![FsEditChange::new("x", "X\nX").replace_all(true)],
    );
    assert_eq!(text, "X\nX\ny\nX\nX\nX\nX\n");
    let lines: Vec<(u32, Option<u32>, Option<u32>)> = spans
        .iter()
        .map(|span| (span.occurrence, span.old_start_line, span.new_start_line))
        .collect();
    assert_eq!(
        lines,
        vec![
            (0, Some(1), Some(1)),
            (1, Some(3), Some(4)),
            (2, Some(4), Some(6))
        ]
    );
}

#[test]
fn crlf_and_eof_without_newline_count_lines_by_newline_bytes() {
    let (_, spans) = apply("a\r\nb\r\nc", vec![edit("c", "C\r\nD")]);
    assert_eq!(spans, vec![span(0, Some(3), 1, Some(3), 2)]);
    let (_, spans) = apply("a\r\nb\r\nc\r\n", vec![edit("a\r\n", "a"), edit("c", "Z")]);
    assert_eq!(spans[1], span(1, Some(3), 1, Some(2), 1));
}

#[test]
fn a_deletion_before_an_edit_maps_back_past_the_deleted_lines() {
    // Delete "y\n" (line 2), then edit "z" (line 3 before, line 2 after).
    let (text, spans) = apply("x\ny\nz", vec![edit("y\n", ""), edit("z", "Z")]);
    assert_eq!(text, "x\nZ");
    assert_eq!(spans[1], span(1, Some(3), 1, Some(2), 1));
    // The deletion's own post-edit line is where the text vanished.
    assert_eq!(spans[0].new_line_count, 0);
}

#[test]
fn the_wire_field_is_additive() {
    // Omitted when empty; an older reader's JSON (no field) still parses.
    let effect = haider_protocol::tool::ToolFileEffect {
        kind: haider_protocol::tool::ToolFileEffectKind::Edit,
        name: "f.txt".into(),
        path: "f.txt".into(),
        absolute_path: "/w/f.txt".into(),
        bytes: 3,
        edit_spans: Vec::new(),
    };
    let json = serde_json::to_value(&effect).unwrap();
    assert!(json.get("edit_spans").is_none(), "{json}");
    let parsed: haider_protocol::tool::ToolFileEffect = serde_json::from_value(json).unwrap();
    assert_eq!(parsed, effect);
    let with = haider_protocol::tool::ToolFileEffect {
        edit_spans: vec![span(0, Some(3), 1, None, 2)],
        ..effect
    };
    let json = serde_json::to_value(&with).unwrap();
    assert_eq!(json["edit_spans"][0]["old_start_line"], 3);
    assert!(
        json["edit_spans"][0].get("new_start_line").is_none(),
        "unproven sides are omitted"
    );
    let round: haider_protocol::tool::ToolFileEffect = serde_json::from_value(json).unwrap();
    assert_eq!(round, with);
}

// ---- the provenance oracle ---------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ch {
    c: char,
    /// Index of this char in the ORIGINAL text, when it is original.
    origin: Option<usize>,
    /// (edit, occurrence, char offset in its new text), when produced.
    producer: Option<(u32, u32, usize)>,
}

/// One replacement's true lines, per character: the original line of every
/// replaced char (None if the run is not one contiguous original run), and
/// the final line of every inserted char (None if any was consumed).
struct Truth {
    old_lines: Option<Vec<usize>>,
    new_lines: Option<Vec<usize>>,
}

fn line_of_char(chars: &[char], index: usize) -> usize {
    chars[..index].iter().filter(|c| **c == '\n').count() + 1
}

/// Apply `edits` with fs_edit's semantics on a char-provenance vector.
/// (edit index, occurrence) of one replacement.
type Key = (u32, u32);
/// One replacement as applied: its key, the original line of each replaced
/// char (when contiguous), and how many chars it inserted.
type Applied = (Key, Option<Vec<usize>>, usize);

fn oracle(original: &str, edits: &[(String, String, bool)]) -> (String, Vec<(Key, Truth)>) {
    let original_chars: Vec<char> = original.chars().collect();
    let mut text: Vec<Ch> = original_chars
        .iter()
        .enumerate()
        .map(|(i, c)| Ch {
            c: *c,
            origin: Some(i),
            producer: None,
        })
        .collect();
    let mut applied: Vec<Applied> = Vec::new();
    for (index, (old, new, all)) in edits.iter().enumerate() {
        let current: String = text.iter().map(|ch| ch.c).collect();
        let starts: Vec<usize> = if *all {
            current
                .match_indices(old.as_str())
                .map(|(at, _)| at)
                .collect()
        } else {
            current.find(old.as_str()).into_iter().collect()
        };
        let old_chars = old.chars().count();
        let new_chars: Vec<char> = new.chars().collect();
        let mut shift: isize = 0;
        for (occurrence, byte_at) in starts.into_iter().enumerate() {
            let char_at = current[..byte_at]
                .chars()
                .count()
                .checked_add_signed(shift)
                .unwrap();
            let replaced = &text[char_at..char_at + old_chars];
            let contiguous = replaced.iter().all(|ch| ch.origin.is_some())
                && replaced
                    .windows(2)
                    .all(|w| w[1].origin == w[0].origin.map(|o| o + 1));
            let old_lines = contiguous.then(|| {
                replaced
                    .iter()
                    .map(|ch| line_of_char(&original_chars, ch.origin.unwrap()))
                    .collect()
            });
            let produced: Vec<Ch> = new_chars
                .iter()
                .enumerate()
                .map(|(k, c)| Ch {
                    c: *c,
                    origin: None,
                    producer: Some((index as u32, occurrence as u32, k)),
                })
                .collect();
            text.splice(char_at..char_at + old_chars, produced);
            shift += new_chars.len() as isize - old_chars as isize;
            applied.push((
                (index as u32, occurrence as u32),
                old_lines,
                new_chars.len(),
            ));
        }
    }
    let final_chars: Vec<char> = text.iter().map(|ch| ch.c).collect();
    let truths = applied
        .into_iter()
        .map(|(key, old_lines, new_len)| {
            let positions: Vec<Option<usize>> = (0..new_len)
                .map(|k| {
                    text.iter()
                        .position(|ch| ch.producer == Some((key.0, key.1, k)))
                })
                .collect();
            let intact = positions.iter().all(Option::is_some)
                && positions.windows(2).all(|w| w[1] == w[0].map(|p| p + 1));
            let new_lines = intact.then(|| {
                positions
                    .iter()
                    .map(|p| line_of_char(&final_chars, p.unwrap()))
                    .collect()
            });
            (
                key,
                Truth {
                    old_lines,
                    new_lines,
                },
            )
        })
        .collect();
    (final_chars.into_iter().collect(), truths)
}

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const TOKENS: [&str; 8] = ["a", "b", "}", "\n", "\r\n", "ab", "é", "a\n"];

fn random_text(rng: &mut Rng, max_tokens: usize) -> String {
    (0..rng.below(max_tokens + 1))
        .map(|_| TOKENS[rng.below(TOKENS.len())])
        .collect()
}

/// A random edit call; `old` is usually cut from the text as it will stand
/// when the edit runs, so the tool accepts a useful share of calls.
fn random_call(rng: &mut Rng, original: &str) -> Vec<(String, String, bool)> {
    let mut current = original.to_owned();
    (0..1 + rng.below(3))
        .map(|_| {
            let old = if !current.is_empty() && rng.below(4) != 0 {
                let chars: Vec<char> = current.chars().collect();
                let start = rng.below(chars.len());
                let len = 1 + rng.below((chars.len() - start).min(4));
                chars[start..start + len].iter().collect()
            } else {
                let text = random_text(rng, 2);
                if text.is_empty() {
                    "a".to_owned()
                } else {
                    text
                }
            };
            let new = random_text(rng, 3);
            let all = rng.below(5) == 0;
            if all {
                current = current.replace(&old, &new);
            } else {
                current = current.replacen(&old, &new, 1);
            }
            (old, new, all)
        })
        .collect()
}

/// For every span the tool hands out, each proven side equals the oracle's
/// truth: the pre-edit line of the replaced text's first char, the
/// post-edit line of the inserted text's first char — and a side is only
/// proven when the oracle has a truth for it.
fn check_call(original: &str, call: &[(String, String, bool)], stats: &mut [usize; 3]) -> bool {
    let changes: Vec<FsEditChange> = call
        .iter()
        .map(|(old, new, all)| FsEditChange::new(old.clone(), new.clone()).replace_all(*all))
        .collect();
    let Ok((text, _, spans)) =
        apply_fs_edit_text(&FsEdit::many("f.txt", changes), original.to_owned())
    else {
        return false;
    };
    let (oracle_text, truths) = oracle(original, call);
    assert_eq!(text, oracle_text, "the tool's text and the oracle's agree");
    assert_eq!(spans.len(), truths.len(), "one span per replacement");
    for (span, (key, truth)) in spans.iter().zip(&truths) {
        assert_eq!((span.edit_index, span.occurrence), *key);
        if let Some(line) = span.old_start_line {
            stats[0] += 1;
            let expected = truth
                .old_lines
                .as_ref()
                .and_then(|lines| lines.first().copied());
            assert_eq!(
                Some(line as usize),
                expected,
                "pre-edit line wrong: {original:?} {call:?} {span:?}"
            );
        }
        if let Some(line) = span.new_start_line {
            stats[1] += 1;
            if let Some(lines) = truth.new_lines.as_ref() {
                if let Some(first) = lines.first() {
                    assert_eq!(
                        line as usize, *first,
                        "post-edit line wrong: {original:?} {call:?} {span:?}"
                    );
                }
            } else {
                panic!("post-edit line claimed for consumed text: {original:?} {call:?} {span:?}");
            }
        }
    }
    stats[2] += 1;
    true
}

#[test]
fn every_measured_line_is_true_over_two_million_random_calls() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut stats = [0usize; 3];
    for _ in 0..2_000_000 {
        let original = random_text(&mut rng, 8);
        let call = random_call(&mut rng, &original);
        check_call(&original, &call, &mut stats);
    }
    println!(
        "SPAN_SWEEP calls=2000000 accepted={} proven_old={} proven_new={}",
        stats[2], stats[0], stats[1]
    );
    assert!(
        stats[2] > 500_000,
        "the sweep exercises accepted calls: {stats:?}"
    );
    assert!(
        stats[0] > 500_000 && stats[1] > 500_000,
        "and proven numbers: {stats:?}"
    );
}
