//! 973-tui-toolview repair 4, end to end: the REAL edit application
//! (`haider_tools::apply_fs_edit_text`) measures spans, the transcript
//! numbers the diff from them (`toolview::tool_diff`), and every number a
//! row shows must be the true line — checked against an independent
//! character-provenance oracle over two million random edit calls
//! (CRLF, a last line without a newline, multi-byte text, `replace_all`,
//! overlapping and self-overlapping edits).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_tools::{FsEdit, FsEditChange, apply_fs_edit_text};
use haider_tui::toolview::{self as tv, DiffKind};
use serde_json::json;

#[derive(Clone, Copy)]
struct Ch {
    c: char,
    origin: Option<usize>,
    producer: Option<(usize, usize, usize)>,
}

fn line_of_char(chars: &[char], index: usize) -> usize {
    chars[..index].iter().filter(|c| **c == '\n').count() + 1
}

/// Per replacement: the ORIGINAL line of each replaced char (when they are
/// one contiguous original run) and the FINAL line of each inserted char
/// (when they survive intact and in order).
type CharLines = (Option<Vec<usize>>, Option<Vec<usize>>);

fn oracle(original: &str, call: &[(String, String, bool)]) -> (String, Vec<Vec<CharLines>>) {
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
    let mut per_edit: Vec<Vec<(Option<Vec<usize>>, usize)>> = Vec::new();
    for (index, (old, new, all)) in call.iter().enumerate() {
        let current: String = text.iter().map(|ch| ch.c).collect();
        let starts: Vec<usize> = if *all {
            current
                .match_indices(old.as_str())
                .map(|(at, _)| at)
                .collect()
        } else {
            current.find(old.as_str()).into_iter().collect()
        };
        let old_len = old.chars().count();
        let new_chars: Vec<char> = new.chars().collect();
        let mut shift: isize = 0;
        let mut occurrences = Vec::new();
        for (occurrence, byte_at) in starts.into_iter().enumerate() {
            let at = current[..byte_at]
                .chars()
                .count()
                .checked_add_signed(shift)
                .unwrap();
            let replaced = &text[at..at + old_len];
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
                    producer: Some((index, occurrence, k)),
                })
                .collect();
            text.splice(at..at + old_len, produced);
            shift += new_chars.len() as isize - old_len as isize;
            occurrences.push((old_lines, new_chars.len()));
        }
        per_edit.push(occurrences);
    }
    let final_chars: Vec<char> = text.iter().map(|ch| ch.c).collect();
    let truths = per_edit
        .into_iter()
        .enumerate()
        .map(|(index, occurrences)| {
            occurrences
                .into_iter()
                .enumerate()
                .map(|(occurrence, (old_lines, new_len))| {
                    let positions: Vec<Option<usize>> = (0..new_len)
                        .map(|k| {
                            text.iter()
                                .position(|ch| ch.producer == Some((index, occurrence, k)))
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
                    (old_lines, new_lines)
                })
                .collect()
        })
        .collect();
    (final_chars.into_iter().collect(), truths)
}

/// Char offsets where each `str::lines()` row of `text` starts.
fn row_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if !(line.is_empty()) {
            starts.push(offset);
        }
        offset += line.chars().count();
    }
    starts.truncate(text.lines().count());
    starts
}

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
            current = if all {
                current.replace(&old, &new)
            } else {
                current.replacen(&old, &new, 1)
            };
            (old, new, all)
        })
        .collect()
}

/// Check every numbered row the transcript would show for one call.
/// Returns how many numbered rows it checked, or `None` if the tool
/// rejected the call.
fn check_rows(original: &str, call: &[(String, String, bool)]) -> Option<usize> {
    let changes: Vec<FsEditChange> = call
        .iter()
        .map(|(old, new, all)| FsEditChange::new(old.clone(), new.clone()).replace_all(*all))
        .collect();
    let (text, _, spans) =
        apply_fs_edit_text(&FsEdit::many("f.txt", changes), original.to_owned()).ok()?;
    let (oracle_text, truths) = oracle(original, call);
    assert_eq!(text, oracle_text);
    let args = json!({
        "path": "f.txt",
        "edits": call
            .iter()
            .map(|(old, new, all)| json!({"old": old, "new": new, "replace_all": all}))
            .collect::<Vec<_>>(),
    });
    let diff = tv::tool_diff("fs_edit", &args, &spans).expect("an edit diff");
    let mut checked = 0usize;
    let mut pair = 0usize;
    let (mut i, mut j) = (0usize, 0usize);
    for row in &diff.lines {
        if row.kind == DiffKind::Gap {
            pair += 1;
            i = 0;
            j = 0;
            continue;
        }
        let (old, new, _) = &call[pair];
        let old_rows = row_starts(old);
        let new_rows = row_starts(new);
        // Which occurrence's truth: numbers only exist for one occurrence.
        let truth = truths[pair].first();
        let old_truth = |k: usize| {
            truth
                .and_then(|(lines, _)| lines.as_ref())
                .and_then(|lines| lines.get(old_rows[k]).copied())
        };
        let new_truth = |k: usize| {
            truth
                .and_then(|(_, lines)| lines.as_ref())
                .and_then(|lines| lines.get(new_rows[k]).copied())
        };
        let expected = match row.kind {
            DiffKind::Removed => {
                let t = old_truth(i);
                i += 1;
                vec![t]
            }
            DiffKind::Added => {
                let t = new_truth(j);
                j += 1;
                vec![t]
            }
            DiffKind::Context => {
                let t = vec![new_truth(j), old_truth(i)];
                i += 1;
                j += 1;
                t
            }
            DiffKind::Gap => unreachable!(),
        };
        if let Some(number) = row.number {
            checked += 1;
            assert!(
                truths[pair].len() == 1,
                "a number for a multi-occurrence edit: {original:?} {call:?} {spans:?}"
            );
            assert!(
                expected.contains(&Some(number)),
                "WRONG NUMBER {number} for {row:?} (truth {expected:?}): file={original:?} call={call:?} spans={spans:?}"
            );
            if row.kind == DiffKind::Context {
                // A context row takes its post-edit number when proven.
                let chosen = if expected[0].is_some() {
                    expected[0]
                } else {
                    expected[1]
                };
                assert!(
                    chosen == Some(number) || expected[0].is_none(),
                    "context row number from the wrong side: {row:?} {expected:?}"
                );
            }
        }
    }
    Some(checked)
}

/// The owner's rule, measured: never a wrong line number, over ≥ 2,000,000
/// random calls through the real tool and the real transcript diff.
#[test]
fn every_shown_edit_line_number_is_true_over_two_million_random_calls() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let (mut accepted, mut numbered) = (0usize, 0usize);
    for _ in 0..2_000_000 {
        let original = random_text(&mut rng, 8);
        let call = random_call(&mut rng, &original);
        if let Some(checked) = check_rows(&original, &call) {
            accepted += 1;
            numbered += checked;
        }
    }
    println!("ROW_SWEEP calls=2000000 accepted={accepted} numbered_rows_checked={numbered}");
    assert!(
        accepted > 500_000 && numbered > 1_000_000,
        "{accepted} {numbered}"
    );
}

/// Verify 4's two live cases and verify 3's boundary, as rows.
#[test]
fn the_verifiers_cases_show_their_true_numbers() {
    let rows = |original: &str, call: &[(&str, &str)]| {
        let call: Vec<(String, String, bool)> = call
            .iter()
            .map(|(old, new)| ((*old).to_owned(), (*new).to_owned(), false))
            .collect();
        assert!(check_rows(original, &call).is_some());
        let changes = call
            .iter()
            .map(|(old, new, _)| FsEditChange::new(old.clone(), new.clone()))
            .collect();
        let (_, _, spans) =
            apply_fs_edit_text(&FsEdit::many("f", changes), original.to_owned()).unwrap();
        let args = json!({"path": "f", "edits": call.iter().map(|(o, n, _)| json!({"old": o, "new": n})).collect::<Vec<_>>()});
        tv::tool_diff("fs_edit", &args, &spans)
            .unwrap()
            .lines
            .into_iter()
            .map(|line| (line.kind, line.number, line.text))
            .collect::<Vec<_>>()
    };
    // Self-overlapping single edit: TODO was line 3; braces land on 3 and 4.
    assert_eq!(
        rows("fn f() {\n}\nTODO\n", &[("TODO", "}\n}")]),
        vec![
            (DiffKind::Removed, Some(3), "TODO".to_owned()),
            (DiffKind::Added, Some(3), "}".to_owned()),
            (DiffKind::Added, Some(4), "}".to_owned()),
        ]
    );
    // A later edit consumed the first edit's new text: edit 1 keeps its
    // true pre-edit line, its added row is unnumbered; edit 2's old text
    // was partly produced (unnumbered), its new text is line 1.
    assert_eq!(
        rows("foo(1);\nbar(1)\n", &[("foo(1)", "bar(1)"), ("1);", "2);")]),
        vec![
            (DiffKind::Removed, Some(1), "foo(1)".to_owned()),
            (DiffKind::Added, None, "bar(1)".to_owned()),
            (DiffKind::Gap, None, String::new()),
            (DiffKind::Removed, None, "1);".to_owned()),
            (DiffKind::Added, Some(1), "2);".to_owned()),
        ]
    );
    // Verify 3's boundary case, now with its TRUE numbers.
    assert_eq!(
        rows("a\nb\nc\n", &[("a\n", "a"), ("c", "C")]),
        vec![
            (DiffKind::Context, Some(1), "a".to_owned()),
            (DiffKind::Gap, None, String::new()),
            (DiffKind::Removed, Some(3), "c".to_owned()),
            (DiffKind::Added, Some(2), "C".to_owned()),
        ]
    );
}
