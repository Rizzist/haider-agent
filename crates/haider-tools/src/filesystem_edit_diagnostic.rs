//! Similarity is diagnostic only: the edit engine still requires exact bytes.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

const CANDIDATE_MAX_CHARS: usize = 512;
const CANDIDATE_MAX_LINES: usize = 16;

struct Candidate {
    numerator: usize,
    denominator: usize,
    line: usize,
    preview: String,
    truncated: bool,
}

/// Rank line windows by shared character bigrams after removing whitespace.
/// Scanning stays linear in file length with at most 16 bounded lines retained
/// and 512 characters per comparison. No file-sized index is built.
pub(crate) fn nearest_anchor_candidate(path: &Path, text: &str, anchor: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    if crate::redact::is_sensitive_path(path) {
        return Some("nearest candidate withheld for a sensitive path; re-read through the owner-authorized artifact".into());
    }
    let line_count = anchor.lines().count().clamp(1, CANDIDATE_MAX_LINES);
    let anchor_preview: String = anchor.chars().take(CANDIDATE_MAX_CHARS).collect();
    let anchor_normalized = normalize(&anchor_preview);
    let anchor_pairs = bigrams(&anchor_normalized);
    let mut window = VecDeque::with_capacity(line_count);
    let mut best: Option<Candidate> = None;
    let mut private_key = false;
    let mut lines = text.lines().enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        // Redact before clipping: truncating a token first could leave a
        // recognizable credential prefix outside the redactor's patterns.
        // Streaming state also protects a window beginning inside a PEM key.
        let redacted = crate::redact::redact_line_with_private_key_state(line, &mut private_key);
        let clipped: String = redacted.text.chars().take(CANDIDATE_MAX_CHARS).collect();
        let line_truncated = clipped.len() < redacted.text.len();
        window.push_back((clipped, line_truncated));
        if window.len() > line_count {
            window.pop_front();
        }
        // A short file still supplies a candidate, but longer files use full
        // windows to compare the same number of lines as the requested anchor.
        if window.len() < line_count && lines.peek().is_some() {
            continue;
        }
        let mut preview = String::new();
        let mut chars_left = CANDIDATE_MAX_CHARS;
        let mut truncated = false;
        for (line_index, (line, line_truncated)) in window.iter().enumerate() {
            if line_index > 0 && chars_left > 0 {
                preview.push('\n');
                chars_left -= 1;
            }
            for character in line.chars() {
                if chars_left == 0 {
                    truncated = true;
                    break;
                }
                preview.push(character);
                chars_left -= 1;
            }
            truncated |= line_truncated;
            if truncated {
                break;
            }
        }
        let normalized = normalize(&preview);
        let pairs = bigrams(&normalized);
        let numerator = if normalized == anchor_normalized {
            // Includes one-character and whitespace-only anchors.
            pairs.len() + anchor_pairs.len() + 1
        } else {
            2 * pairs.intersection(&anchor_pairs).count()
        };
        let denominator = pairs.len() + anchor_pairs.len() + 1;
        if best
            .as_ref()
            .is_none_or(|best| numerator * best.denominator > best.numerator * denominator)
        {
            best = Some(Candidate {
                numerator,
                denominator,
                line: index + 2 - window.len(),
                preview,
                truncated,
            });
        }
    }
    best.map(
        |Candidate {
             line,
             preview,
             truncated,
             ..
         }| {
            let difference = if !truncated
                && anchor_preview.len() == anchor.len()
                && normalize(&preview) == anchor_normalized
            {
                "whitespace differs"
            } else {
                "text differs"
            };
            format!(
                "nearest candidate at line {line} ({difference}; suggestion only{}): {preview:?}",
                if truncated { "; preview truncated" } else { "" },
            )
        },
    )
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn bigrams(text: &str) -> HashSet<(char, char)> {
    text.chars().zip(text.chars().skip(1)).collect()
}
