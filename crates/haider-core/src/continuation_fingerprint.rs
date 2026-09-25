//! Comparison-only content for the turn loop guards (`ContinuationProgress` in
//! the actor). The journal and the model always receive the original data.
//!
//! Structural rule (973 loop-guard ruling), not a list of noise formats:
//! - Unicode NFKC, a Latin fold of look-alike Cyrillic/Greek letters, and
//!   whitespace collapse everywhere.
//! - Tool results and assistant text mask every digit run as `#`, so latency,
//!   clock times, dates, epochs, counters and numeric nonces never look new.
//!   Digits glued to a preceding letter or `_` stay (they name something, such
//!   as `chunk5`, `mod12`, `v0`), so a new chunk path is progress.
//! - Letters are never masked: a new git SHA, UUID or base64 token is progress.
//!   Accepted residual: noise that changes letters (nonces, etags) makes a
//!   new result; the action-level guard (same tool and arguments, whatever
//!   the result) bounds such loops.
//! - Tool results compare an unordered multiset: each line's `,`/`;`/`:`-separated
//!   items are sorted, then lines are sorted, so a reordered list is a repeat.
//! - Assistant text also folds case; text and argument strings cap runs of one
//!   punctuation character at [`MAX_PUNCTUATION_RUN`].
//! - Tool arguments keep canonical JSON (sorted keys, exact numbers); only
//!   string values are normalized, and their digits are never masked: a call
//!   with different arguments is a different call.

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

/// Longest run of one repeated punctuation character kept in assistant text
/// and argument strings. Three keeps `...`/`../` intact while `?????` equals
/// `???`.
const MAX_PUNCTUATION_RUN: usize = 3;

/// Marker for a masked digit run.
const DIGIT_MASK: char = '#';

/// Latin look-alikes of Cyrillic and Greek letters (a subset of the Unicode
/// UTS #39 confusables that render identically to ASCII in common fonts).
fn fold_confusable(ch: char) -> char {
    match ch {
        'а' | 'α' => 'a',
        'А' | 'Α' => 'A',
        'В' | 'Β' => 'B',
        'с' | 'ϲ' => 'c',
        'С' | 'Ϲ' => 'C',
        'ԁ' => 'd',
        'е' => 'e',
        'Е' | 'Ε' => 'E',
        'һ' => 'h',
        'Н' | 'Η' => 'H',
        'і' | 'ι' => 'i',
        'І' | 'Ι' => 'I',
        'ј' => 'j',
        'Ј' => 'J',
        'К' | 'Κ' => 'K',
        'ӏ' => 'l',
        'М' | 'Μ' => 'M',
        'Ν' => 'N',
        'о' | 'ο' => 'o',
        'О' | 'Ο' => 'O',
        'р' | 'ρ' => 'p',
        'Р' | 'Ρ' => 'P',
        'ԛ' => 'q',
        'ѕ' => 's',
        'Ѕ' => 'S',
        'Т' | 'Τ' => 'T',
        'υ' => 'u',
        'ν' => 'v',
        'ԝ' => 'w',
        'х' | 'χ' => 'x',
        'Х' | 'Χ' => 'X',
        'у' => 'y',
        'У' | 'Υ' => 'Y',
        'Ζ' => 'Z',
        other => other,
    }
}

fn unicode_fold(input: &str) -> String {
    input.nfkc().map(fold_confusable).collect()
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Replaces every digit run with [`DIGIT_MASK`], except runs that continue an
/// identifier (directly preceded by a letter or `_`).
fn mask_digits(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut previous: Option<char> = None;
    let mut keeping = false;
    for ch in input.chars() {
        if ch.is_ascii_digit() {
            let starts_run = !previous.is_some_and(|p| p.is_ascii_digit());
            if starts_run {
                keeping = previous.is_some_and(|p| p.is_alphabetic() || p == '_');
                if !keeping {
                    out.push(DIGIT_MASK);
                }
            }
            if keeping {
                out.push(ch);
            }
        } else {
            out.push(ch);
        }
        previous = Some(ch);
    }
    out
}

fn cap_punctuation_runs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut run_char = None;
    let mut run_len = 0;
    for ch in input.chars() {
        if run_char == Some(ch) {
            run_len += 1;
        } else {
            run_char = Some(ch);
            run_len = 1;
        }
        if !ch.is_ascii_punctuation() || run_len <= MAX_PUNCTUATION_RUN {
            out.push(ch);
        }
    }
    out
}

/// Assistant prose: Unicode fold, case fold, digits masked, punctuation runs
/// capped, whitespace collapsed. Blank text stays empty (never progress).
pub(super) fn assistant_text(input: &str) -> String {
    let folded = unicode_fold(input).to_lowercase();
    collapse_whitespace(&cap_punctuation_runs(&mask_digits(&folded)))
}

/// Canonical JSON: object keys in stable order and numbers/booleans exact.
/// String values are NFKC-normalized, punctuation-capped and
/// whitespace-collapsed; their digits are kept.
pub(super) fn arguments(input: &Value) -> String {
    fn normalize(value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(collapse_whitespace(&cap_punctuation_runs(
                &text.nfkc().collect::<String>(),
            ))),
            Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), normalize(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    normalize(input).to_string()
}

/// Model-visible tool output as an unordered multiset of lines, each line an
/// unordered multiset of `,`/`;`/`:`-separated items (so `Found: a, b`
/// and `Found: b, a` match), digits masked.
pub(super) fn result(input: &str) -> String {
    let masked = mask_digits(&unicode_fold(input));
    let mut lines: Vec<String> = masked
        .lines()
        .filter_map(|line| {
            let mut items: Vec<String> = line
                .split([',', ';', ':'])
                .map(collapse_whitespace)
                .filter(|item| !item.is_empty())
                .collect();
            items.sort_unstable();
            (!items.is_empty()).then(|| items.join(", "))
        })
        .collect();
    lines.sort_unstable();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn digit_mask_keeps_identifier_digits_only() {
        assert_eq!(mask_digits("took 101ms at 12:00:05"), "took #ms at #:#:#");
        assert_eq!(
            mask_digits("chunk5 mod12/x v0.1.2 id_7"),
            "chunk5 mod12/x v0.#.# id_7"
        );
    }

    #[test]
    fn assistant_text_folds_unicode_case_space_digits_and_punctuation_runs() {
        assert_eq!(assistant_text("  Ｓａｍｅ\n  answer  "), "same answer");
        assert_eq!(assistant_text("\u{2003}\n "), "");
        assert_eq!(assistant_text("count 1"), assistant_text("count 22"));
        assert_eq!(assistant_text("took 5ms"), assistant_text("took 17ms"));
        assert_eq!(
            assistant_text("same answer...."),
            assistant_text("same answer......")
        );
        assert_eq!(assistant_text("SamE answer"), assistant_text("same ANSWER"));
        assert_ne!(assistant_text("same answer"), assistant_text("new answer"));
        assert_ne!(assistant_text("item5 done"), assistant_text("item6 done"));
    }

    #[test]
    fn arguments_order_keys_and_normalize_strings_but_keep_digits() {
        let a = json!({"query": "same  topic ", "limit": 5, "nested": [" a\nb "]});
        let b: Value =
            serde_json::from_str(r#"{"nested":["a b"],"limit":5,"query":" same topic"}"#).unwrap();
        assert_eq!(arguments(&a), arguments(&b));
        assert_eq!(
            arguments(&json!({"query": "same topic?????"})),
            arguments(&json!({"query": "same topic???"}))
        );
        assert_ne!(
            arguments(&json!({"path": "part-1.txt"})),
            arguments(&json!({"path": "part-2.txt"}))
        );
        assert_ne!(
            arguments(&json!({"path": "../x"})),
            arguments(&json!({"path": "./x"}))
        );
        assert_ne!(
            arguments(&json!({"limit": 5})),
            arguments(&json!({"limit": 6}))
        );
    }

    /// Incidental variation from the round-3/4 adversarial probes: repeats.
    #[test]
    fn result_masks_digit_noise_order_and_confusables() {
        let pairs = [
            ("same result; took 101ms", "same result; took 129ms"),
            (
                "same result (finished in 3.32s)",
                "same result (finished in 9.92s)",
            ),
            ("[12:00:01] same result", "[12:00:59] same result"),
            (
                "Date: Thu, 24 Sep 2026 12:00:01 GMT; same result",
                "Date: Thu, 24 Sep 2026 12:00:02 GMT; same result",
            ),
            ("same result at 1727136001", "same result at 1727136030"),
            ("same result; nonce 007919", "same result; nonce 015838"),
            (
                "same result; time=2026-09-24T00:00:01Z",
                "same result; time=2026-09-24T00:00:30Z",
            ),
            ("same result; request_id=1", "same result; request_id=30"),
            (
                "Found: alpha, beta, gamma, delta",
                "Found: delta, gamma, alpha, beta",
            ),
            ("b.txt\na.txt\nc.txt", "c.txt\n\na.txt\nb.txt"),
            ("same result aaaaaaaa", "same result аaаaаaаa"),
            ("Completed files: 1", "Completed files: 2"),
        ];
        for (first, second) in pairs {
            assert_eq!(result(first), result(second), "{first} vs {second}");
        }
    }

    /// Real progress from the round-4 false-positive probes: distinct.
    #[test]
    fn result_keeps_letters_identifiers_and_new_lines() {
        let pairs = [
            (
                "HEAD is now 356a192b7913b04c54574d18c28d46e6395428ab",
                "HEAD is now da4b9237bacccdf19c0760cab7aec4a8359010b0",
            ),
            (
                "wrote /tmp/out/run1/chunk1file",
                "wrote /tmp/out/run2/chunk2file",
            ),
            ("wrote src/mod1/part1x.rs", "wrote src/mod2/part2x.rs"),
            ("wrote chunk_0001.bin", "wrote chunk_0002.bin"),
            ("contents: stage one done", "contents: stage two done"),
            (
                "   Compiling crate1 v0.1.1",
                "   Compiling crate1 v0.1.1\n   Compiling crate2 v0.1.2",
            ),
            (
                "id 550e8400-e29b-41d4-a716-446655440000",
                "id 6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            ),
            ("etag a0b0ca", "etag a0b0cb"),
            ("new page A", "new page B"),
        ];
        for (first, second) in pairs {
            assert_ne!(result(first), result(second), "{first} vs {second}");
        }
        // Multiplicity is kept: one more identical line is a changed multiset.
        assert_ne!(result("ok\nok"), result("ok"));
    }
}
