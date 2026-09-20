use crate::redact_output_text;
use haider_protocol::file_review::{
    FileDiffHunk, FileDiffLine, FileDiffLineKind, FileReview, FileReviewOperation,
};
use haider_protocol::ids::EffectId;

const CONTEXT_LINES: usize = 3;
const MAX_MATRIX_CELLS: usize = 1_000_000;
const MAX_REVIEW_LINES: usize = 4_096;
const MAX_LINE_BYTES: usize = 8_192;

#[derive(Debug)]
enum DiffOp {
    Context(String),
    Addition(String),
    Removal(String),
}

pub(crate) fn build_file_review(
    effect: EffectId,
    path: String,
    operation: FileReviewOperation,
    old: Option<&str>,
    new: &str,
) -> FileReview {
    // Redact before diffing so retained context and changed text pass through
    // the same consumer as tool output.
    let raw_old = diff_lines(old.unwrap_or_default());
    let raw_new = diff_lines(new);
    let (operations, coarse) = diff_operations(&raw_old, &raw_new);
    let safe_old = redact_output_text(old.unwrap_or_default());
    let safe_new = redact_output_text(new);
    let old_lines = diff_lines(&safe_old);
    let new_lines = diff_lines(&safe_new);
    let operations = redacted_operations(operations, &old_lines, &new_lines);
    let added = operations
        .iter()
        .filter(|op| matches!(op, DiffOp::Addition(_)))
        .count();
    let removed = operations
        .iter()
        .filter(|op| matches!(op, DiffOp::Removal(_)))
        .count();
    let (hunks, bounded) = build_hunks(operations);

    FileReview {
        effect,
        path,
        operation,
        old_digest: old.map(|text| format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())),
        new_digest: format!("blake3:{}", blake3::hash(new.as_bytes()).to_hex()),
        added: u32::try_from(added).unwrap_or(u32::MAX),
        removed: u32::try_from(removed).unwrap_or(u32::MAX),
        hunks,
        truncated: coarse || bounded,
    }
}

fn diff_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').collect()
    }
}

fn redacted_operations(
    operations: Vec<DiffOp>,
    safe_old: &[&str],
    safe_new: &[&str],
) -> Vec<DiffOp> {
    let (mut old_index, mut new_index) = (0, 0);
    operations
        .into_iter()
        .map(|operation| match operation {
            DiffOp::Context(_) => {
                let text = safe_new
                    .get(new_index)
                    .or_else(|| safe_old.get(old_index))
                    .copied()
                    .unwrap_or("[REDACTED]")
                    .to_owned();
                old_index += 1;
                new_index += 1;
                DiffOp::Context(text)
            }
            DiffOp::Removal(_) => {
                let text = safe_old
                    .get(old_index)
                    .copied()
                    .unwrap_or("[REDACTED]")
                    .to_owned();
                old_index += 1;
                DiffOp::Removal(text)
            }
            DiffOp::Addition(_) => {
                let text = safe_new
                    .get(new_index)
                    .copied()
                    .unwrap_or("[REDACTED]")
                    .to_owned();
                new_index += 1;
                DiffOp::Addition(text)
            }
        })
        .collect()
}

fn diff_operations(old: &[&str], new: &[&str]) -> (Vec<DiffOp>, bool) {
    if old.len().saturating_mul(new.len()) > MAX_MATRIX_CELLS {
        return (coarse_diff(old, new), true);
    }
    let width = new.len() + 1;
    let mut lengths = vec![0_u32; (old.len() + 1) * width];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            let cell = old_index * width + new_index;
            lengths[cell] = if old[old_index] == new[new_index] {
                lengths[(old_index + 1) * width + new_index + 1].saturating_add(1)
            } else {
                lengths[(old_index + 1) * width + new_index]
                    .max(lengths[old_index * width + new_index + 1])
            };
        }
    }

    let mut operations = Vec::with_capacity(old.len() + new.len());
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old.len() && new_index < new.len() {
        if old[old_index] == new[new_index] {
            operations.push(DiffOp::Context(old[old_index].to_owned()));
            old_index += 1;
            new_index += 1;
        } else if lengths[(old_index + 1) * width + new_index]
            >= lengths[old_index * width + new_index + 1]
        {
            operations.push(DiffOp::Removal(old[old_index].to_owned()));
            old_index += 1;
        } else {
            operations.push(DiffOp::Addition(new[new_index].to_owned()));
            new_index += 1;
        }
    }
    operations.extend(
        old[old_index..]
            .iter()
            .map(|line| DiffOp::Removal((*line).to_owned())),
    );
    operations.extend(
        new[new_index..]
            .iter()
            .map(|line| DiffOp::Addition((*line).to_owned())),
    );
    (operations, false)
}

fn coarse_diff(old: &[&str], new: &[&str]) -> Vec<DiffOp> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let mut operations = Vec::with_capacity(old.len() + new.len());
    operations.extend(
        old[..prefix]
            .iter()
            .map(|line| DiffOp::Context((*line).to_owned())),
    );
    operations.extend(
        old[prefix..old.len() - suffix]
            .iter()
            .map(|line| DiffOp::Removal((*line).to_owned())),
    );
    operations.extend(
        new[prefix..new.len() - suffix]
            .iter()
            .map(|line| DiffOp::Addition((*line).to_owned())),
    );
    operations.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|line| DiffOp::Context((*line).to_owned())),
    );
    operations
}

fn build_hunks(operations: Vec<DiffOp>) -> (Vec<FileDiffHunk>, bool) {
    let mut intervals = Vec::<(usize, usize)>::new();
    for index in operations
        .iter()
        .enumerate()
        .filter_map(|(index, op)| (!matches!(op, DiffOp::Context(_))).then_some(index))
    {
        let start = index.saturating_sub(CONTEXT_LINES);
        let end = (index + CONTEXT_LINES + 1).min(operations.len());
        if let Some((_, previous_end)) = intervals.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            intervals.push((start, end));
        }
    }

    let mut old_at = vec![1_u32; operations.len() + 1];
    let mut new_at = vec![1_u32; operations.len() + 1];
    for (index, operation) in operations.iter().enumerate() {
        old_at[index + 1] = old_at[index] + u32::from(!matches!(operation, DiffOp::Addition(_)));
        new_at[index + 1] = new_at[index] + u32::from(!matches!(operation, DiffOp::Removal(_)));
    }

    let mut retained = 0_usize;
    let mut truncated = false;
    let mut hunks = Vec::new();
    for (start, end) in intervals {
        if retained >= MAX_REVIEW_LINES {
            truncated = true;
            break;
        }
        let bounded_end = end.min(start + (MAX_REVIEW_LINES - retained));
        truncated |= bounded_end < end;
        let mut lines = Vec::with_capacity(bounded_end - start);
        for (offset, operation) in operations[start..bounded_end].iter().enumerate() {
            let index = start + offset;
            let (kind, old_line, new_line, text) = match operation {
                DiffOp::Context(text) => (
                    FileDiffLineKind::Context,
                    Some(old_at[index]),
                    Some(new_at[index]),
                    text,
                ),
                DiffOp::Addition(text) => {
                    (FileDiffLineKind::Addition, None, Some(new_at[index]), text)
                }
                DiffOp::Removal(text) => {
                    (FileDiffLineKind::Removal, Some(old_at[index]), None, text)
                }
            };
            let mut text = text.clone();
            if text.len() > MAX_LINE_BYTES {
                let mut boundary = MAX_LINE_BYTES;
                while !text.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text.truncate(boundary);
                text.push('…');
                truncated = true;
            }
            lines.push(FileDiffLine {
                kind,
                old_line,
                new_line,
                text,
            });
        }
        retained += lines.len();
        hunks.push(FileDiffHunk {
            old_start: old_at[start],
            old_lines: old_at[end].saturating_sub(old_at[start]),
            new_start: new_at[start],
            new_lines: new_at[end].saturating_sub(new_at[start]),
            lines,
        });
    }
    (hunks, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_numbered_hunks_and_redacts_content() -> Result<(), serde_json::Error> {
        let review = build_file_review(
            EffectId::new("effect-review"),
            "src/lib.rs".into(),
            FileReviewOperation::Edit,
            Some("one\napi_key=sk-abcdefghijklmnopqrstuvwxyz123456\nthree\n"),
            "one\napi_key=sk-zyxwvutsrqponmlkjihgfedcba654321\nthree\nfour\n",
        );
        assert_eq!((review.added, review.removed), (2, 1));
        assert_eq!(review.hunks.len(), 1);
        assert_eq!(review.hunks[0].lines[0].old_line, Some(1));
        let json = serde_json::to_string(&review)?;
        assert!(!json.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!json.contains("zyxwvutsrqponmlkjihgfedcba"));
        assert!(json.contains("REDACTED"));
        Ok(())
    }
}
