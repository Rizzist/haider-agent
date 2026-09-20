//! Foreground process captures use the existing task_output tool. The registry
//! holds only session-scoped CAS references; raw captures never become pages.

use super::tasks::TaskFacade;
use haider_protocol::ids::SessionId;
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_tools::{ProcessOutputChunk, ProcessResult, ToolError, ToolResult};

impl TaskFacade {
    pub(crate) async fn retain_foreground_capture(
        &self,
        session: &SessionId,
        result: &mut ProcessResult,
    ) -> ToolResult<String> {
        let (artifact, safe) = if let Some(artifact) = &result.artifact {
            let bytes = self
                .hub
                .get_internal_artifact(artifact)
                .await
                .map_err(|error| ToolError::cas(error.message))?;
            let chunks: Vec<ProcessOutputChunk> = serde_json::from_slice(&bytes)
                .map_err(|error| ToolError::cas(format!("invalid process capture: {error}")))?;
            (
                artifact.clone(),
                haider_tools::redact_process_output(&chunks)?,
            )
        } else {
            let bytes = serde_json::to_vec(&result.inline_output)
                .map_err(|error| ToolError::cas(error.to_string()))?;
            let artifact = self
                .hub
                .put_internal_artifact(bytes)
                .await
                .map_err(|error| ToolError::cas(error.message))?;
            (
                artifact,
                haider_tools::redact_process_output(&result.inline_output)?,
            )
        };
        let registry = self.hub.task_registry();
        registry.retain_capture(
            session,
            format!("capture:{}", result.effect),
            artifact.clone(),
        );
        // Conversation-local alias: deterministic across sessions (the caller
        // chose the call id), so the provider-facing paging pointer can name
        // it without leaking volatile effect identity. A repeated call id
        // re-points its alias at the most recent capture; the effect handle
        // above stays unique.
        registry.retain_capture(session, format!("cap:{}", result.call_id), artifact.clone());
        result.artifact = Some(artifact);
        Ok(safe)
    }

    pub(crate) async fn foreground_capture_page(
        &self,
        session: &SessionId,
        handle: &str,
        cursor: Option<u64>,
    ) -> ToolResult<BoundedResult> {
        let artifact = if let Some(artifact) = self.hub.task_registry().capture(session, handle) {
            artifact
        } else {
            self.restore_foreground_capture(session, handle).await?
        };
        let bytes = self
            .hub
            .get_internal_artifact(&artifact)
            .await
            .map_err(|error| ToolError::cas(error.message))?;
        let chunks: Vec<ProcessOutputChunk> = serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::cas(format!("invalid process capture: {error}")))?;
        let safe = haider_tools::redact_process_output(&chunks)?;
        capture_page(handle, &safe, cursor.unwrap_or(0))
    }
    pub(crate) async fn restore_foreground_capture(
        &self,
        session: &SessionId,
        handle: &str,
    ) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        // `capture:<effect>` names exactly one execution; `cap:<call_id>` is
        // the conversation-local alias, which a repeated call id re-points at
        // the most recent matching capture, mirroring in-memory registration.
        let alias_call = handle.strip_prefix("cap:");
        let effect = handle.strip_prefix("capture:").unwrap_or_default();
        let matches_handle = |record: &haider_protocol::graph::ProcessSignalRecorded| {
            alias_call.map_or(record.effect_id.as_str() == effect, |call| {
                record.call_id == call
            })
        };
        let mut cursor = 0;
        let mut signal = None;
        let mut latest = None;
        let mut user_commands = std::collections::HashSet::new();
        'scan: loop {
            let page = self
                .hub
                .read_internal_session_for(
                    haider_platform::phase_trace::StoreReadCaller::ForegroundCapture,
                    session,
                    cursor,
                    256,
                )
                .await
                .map_err(|error| ToolError::cas(error.message))?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                let Ok(event) = envelope.payload.decode_event() else {
                    continue;
                };
                match event {
                    haider_protocol::EventPayload::Item(
                        haider_protocol::item::ItemEvent::Completed { item, .. },
                    ) => {
                        if let Some(origin) =
                            haider_protocol::item::UserCommandOriginV1::from_extension_item(&item)
                            && let Some(run) = envelope.run_id
                        {
                            user_commands.insert((run, origin.call_id));
                        }
                    }
                    haider_protocol::EventPayload::ProcessSignalRecorded(record)
                        if matches_handle(&record) =>
                    {
                        // Direct commands have no ToolResult. Their daemon-minted
                        // origin and terminal process signal own the same capture.
                        if user_commands.contains(&(record.run_id.clone(), record.call_id.clone()))
                            && let Some(artifact) = record.artifact
                        {
                            latest = Some(artifact);
                            signal = None;
                        } else {
                            signal = Some((record.run_id, record.call_id));
                        }
                    }
                    haider_protocol::EventPayload::ToolResult { call_id, result }
                        if signal.as_ref().is_some_and(|(run, call)| {
                            envelope.run_id.as_ref() == Some(run) && &call_id == call
                        }) =>
                    {
                        if let Some(artifact) = result.artifact {
                            latest = Some(artifact);
                            signal = None;
                        }
                    }
                    _ => {}
                }
                // An effect handle has one owner; only an alias keeps scanning
                // for a more recent capture under the same call id.
                if alias_call.is_none() && latest.is_some() {
                    break 'scan;
                }
            }
        }
        if let Some(artifact) = latest {
            self.hub
                .task_registry()
                .retain_capture(session, handle.to_owned(), artifact.clone());
            return Ok(artifact);
        }
        Err(ToolError::invalid_argument(
            "unknown capture in this session",
        ))
    }
}

fn capture_page(handle: &str, safe: &str, cursor: u64) -> ToolResult<BoundedResult> {
    let start = usize::try_from(cursor).unwrap_or(usize::MAX);
    if start > safe.len() || !safe.is_char_boundary(start) {
        return Err(ToolError::invalid_argument(
            "capture cursor must be a UTF-8 boundary within output_bytes",
        ));
    }
    let end = safe.floor_char_boundary(
        start
            .saturating_add(haider_tools::ORCHESTRATION_PREVIEW_MAX_BYTES)
            .min(safe.len()),
    );
    let exhausted = end == safe.len();
    let preview = serde_json::json!({
        "task_id": handle,
        "output_bytes": safe.len(),
        "chunk": &safe[start..end],
        "next_cursor": end,
        "exhausted": exhausted,
        "paging": "task_output(task_id, cursor=next_cursor); cursors count secret-redacted UTF-8 bytes",
    }).to_string();
    Ok(BoundedResult {
        preview,
        truncated: false,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: Some(end.to_string()),
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    })
}

#[cfg(test)]
#[path = "foreground_capture_tests.rs"]
mod tests;
