//! Numbered prefix paging for explicit file reads. Redaction precedes paging,
//! so PEM state and secrets spanning the requested range are still recognized.

use crate::{CasSink, FsRead, ResultBounds, ToolResult};
use haider_protocol::tool::{BoundedResult, ToolResultStatus, ToolTruncation};

pub(crate) async fn bounded_file_read<C: CasSink>(
    contents: String,
    operation: &FsRead,
    bounds: ResultBounds,
    cas: &mut C,
) -> ToolResult<BoundedResult> {
    let redacted =
        crate::redact::ExplicitReadPaths::new(&operation.path).redact(&operation.path, &contents);
    let budget = bounds.max_preview_bytes;
    let offset = operation.offset.unwrap_or(1);
    let limit = operation.limit.unwrap_or(crate::FILE_PREVIEW_MAX_LINES);
    let column = operation.column.unwrap_or(1);
    let mut preview = String::new();
    let mut next = None;
    let mut selected_redaction = false;
    for (index, (line, original_line)) in redacted
        .text
        .split_inclusive('\n')
        .zip(contents.split_inclusive('\n'))
        .enumerate()
        .skip(offset - 1)
    {
        let number = index + 1;
        if index - (offset - 1) >= limit {
            if operation.limit.is_none() {
                next = Some((number, 1));
            }
            break;
        }
        selected_redaction |= line != original_line;
        let start_column = if number == offset { column } else { 1 };
        let start = line
            .char_indices()
            .nth(start_column - 1)
            .map_or(line.len(), |(i, _)| i);
        let line = &line[start..];
        let label = format!("{number}: ");
        if preview.len().saturating_add(label.len()) > budget {
            next = Some((number, start_column));
            break;
        }
        let remaining = budget.saturating_sub(preview.len().saturating_add(label.len()));
        let end = line.floor_char_boundary(remaining.min(line.len()));
        // Never skip a line to fit a later one. The continuation stays on the
        // same character when the remaining budget cannot hold a UTF-8 scalar.
        if end == 0 && !line.is_empty() {
            next = Some((number, start_column));
            break;
        }
        preview.push_str(&label);
        preview.push_str(&line[..end]);
        if end < line.len() {
            next = Some((number, start_column + line[..end].chars().count()));
            break;
        }
    }
    let paged = next.is_some();
    if let Some((offset, column)) = next {
        let mut args = serde_json::json!({
            "path": operation.path,
            "offset": offset,
            "column": column,
        });
        if let Some(limit) = operation.limit {
            args["limit"] = serde_json::json!(limit - (offset - operation.offset.unwrap_or(1)));
        }
        // Like the typed provenance footer, continuation metadata is outside
        // the content byte allowance, so even tiny explicit bounds can page.
        preview.push_str(&format!(
            "\n[File preview truncated; continue with fs_read({args})]\n"
        ));
    }
    let truncated = paged || selected_redaction;
    let truncation =
        truncated.then(|| ToolTruncation::from_bytes(contents.as_bytes(), preview.len()));
    let artifact = if paged {
        Some(cas.put_owned(contents.into_bytes()).await?)
    } else {
        None
    };
    let mut result = BoundedResult {
        preview,
        truncated,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    };
    if let Some(truncation) = truncation {
        result.declare_truncation(truncation);
    }
    Ok(result)
}

#[cfg(test)]
#[path = "file_preview_tests.rs"]
mod tests;
