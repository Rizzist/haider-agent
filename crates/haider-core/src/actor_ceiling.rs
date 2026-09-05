//! Workspace evidence is durable before dispatch and sealed with the cap.
use super::*;
use crate::turn_workspace::{self, TreeReceipt};
use haider_protocol::ceiling::{
    INTERNAL_CEILING_EXIT_CODE, InternalCeilingTerminalV1, PartialProgressV1, RunEndReasonV1,
    TurnCeilingV1, WorkspaceReceiptErrorV1, WorkspaceReceiptPhaseV1, WorkspaceStateV1,
};
use serde::{Deserialize, Serialize};

pub(super) const WORKSPACE_RECEIPT_KIND: &str = "turn_workspace_before_v1";
// Each extension remains small enough to travel with ordinary bounded journal
// pages. Chunking adds no truncation or new ceiling on workspace coverage.
const RECEIPT_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum WorkspaceBaseline {
    Captured(TreeReceipt),
    Unavailable(WorkspaceReceiptErrorV1),
}

fn unavailable(phase: WorkspaceReceiptPhaseV1, detail: String) -> WorkspaceBaseline {
    WorkspaceBaseline::Unavailable(WorkspaceReceiptErrorV1 { phase, detail })
}

fn encode_receipt(before: &WorkspaceBaseline) -> Result<Vec<serde_json::Value>, HaiderError> {
    let data = serde_json::to_string(before)
        .map_err(|_| receipt_error("cannot encode pre-turn workspace receipt"))?;
    let digest = blake3::hash(data.as_bytes()).to_hex().to_string();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < data.len() {
        let mut end = (start + RECEIPT_CHUNK_BYTES).min(data.len());
        while !data.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&data[start..end]);
        start = end;
    }
    let parts = chunks.len();
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(part, bytes)| {
            serde_json::json!({"part":part,"parts":parts,"digest":digest,
                "bytes":bytes})
        })
        .collect())
}

#[derive(Deserialize)]
struct ReceiptChunk {
    part: usize,
    parts: usize,
    digest: String,
    bytes: String,
}

impl HarnessActor {
    pub(super) async fn prepare_ceiling_workspace(
        &self,
        run_id: &RunId,
        restoring: bool,
        cancel: &CancelToken,
    ) -> Result<(Option<WorkspaceBaseline>, Option<Vec<serde_json::Value>>), HaiderError> {
        let Some(root) = self.config.ceiling_workspace.clone() else {
            return Ok((None, None));
        };
        if restoring {
            let mut cursor = 0;
            let mut bytes = Vec::new();
            let mut next_part = 0;
            let mut identity = None;
            loop {
                if cancel.is_cancelled() {
                    return Err(receipt_error("workspace receipt recovery cancelled"));
                }
                let page = self
                    .store
                    .read(&self.config.session_id, cursor, 256)
                    .await?;
                if page.is_empty() {
                    break;
                }
                for event in &page {
                    if event.run_id.as_ref() == Some(run_id)
                        && event
                            .payload
                            .get("event")
                            .and_then(serde_json::Value::as_str)
                            == Some("completed")
                        && event
                            .payload
                            .pointer("/item/kind")
                            .and_then(serde_json::Value::as_str)
                            == Some(WORKSPACE_RECEIPT_KIND)
                    {
                        let chunk: ReceiptChunk = serde_json::from_value(
                            event.payload["item"]["data"].clone(),
                        )
                        .map_err(|_| receipt_error("retained workspace receipt is invalid"))?;
                        let expected = identity.get_or_insert((chunk.parts, chunk.digest.clone()));
                        if chunk.part != next_part
                            || *expected != (chunk.parts, chunk.digest.clone())
                            || chunk.parts == 0
                        {
                            return Err(receipt_error(
                                "retained workspace receipt chunk order is invalid",
                            ));
                        }
                        bytes.extend_from_slice(chunk.bytes.as_bytes());
                        next_part += 1;
                        if next_part == chunk.parts {
                            if blake3::hash(&bytes).to_hex().as_str() != chunk.digest {
                                return Err(receipt_error(
                                    "retained workspace receipt digest differs",
                                ));
                            }
                            let before = serde_json::from_slice(&bytes).map_err(|_| {
                                receipt_error("retained workspace receipt is invalid")
                            })?;
                            return Ok((Some(before), None));
                        }
                    }
                }
                cursor = page.last().map_or(cursor, |event| event.seq);
            }
            // Never call today's tree the pre-turn tree of a recovered run.
            // Legacy journals cannot retroactively prove workspace equality.
            return Ok((
                Some(unavailable(
                    WorkspaceReceiptPhaseV1::Before,
                    "recovered run has no complete pre-turn workspace receipt".into(),
                )),
                None,
            ));
        }
        let before = match turn_workspace::capture_cancellable(root, cancel.clone()).await {
            Ok(receipt) => WorkspaceBaseline::Captured(receipt),
            Err(error) => unavailable(WorkspaceReceiptPhaseV1::Before, error.message),
        };
        let data = encode_receipt(&before)?;
        // Piggyback the first request-attempt transaction; provider dispatch
        // cannot happen unless this exact baseline is durable too.
        Ok((Some(before), Some(data)))
    }

    pub(super) async fn ceiling_terminal(
        &self,
        run_id: &RunId,
        status: &RequestBudgetStatusV1,
        before: &WorkspaceBaseline,
        last_request_ordinal: u64,
        cancel: &CancelToken,
    ) -> Result<InternalCeilingTerminalV1, HaiderError> {
        let root = self
            .config
            .ceiling_workspace
            .clone()
            .ok_or_else(|| receipt_error("ceiling workspace is absent"))?;
        let comparison = match before {
            WorkspaceBaseline::Captured(before) => {
                turn_workspace::capture_cancellable(root, cancel.clone())
                    .await
                    .map(|after| (before, after))
                    .map_err(|error| WorkspaceReceiptErrorV1 {
                        phase: WorkspaceReceiptPhaseV1::After,
                        detail: error.message,
                    })
            }
            WorkspaceBaseline::Unavailable(error) => Err(error.clone()),
        };
        let mut calls = 0;
        let mut cursor = 0;
        loop {
            let page = self
                .store
                .read(&self.config.session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            for event in &page {
                if event.run_id.as_ref() == Some(run_id)
                    && event
                        .payload
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        == Some("tool_result")
                {
                    // Call IDs may repeat in sequential provider requests.
                    // Each correlated durable result row is one completed call.
                    calls += 1;
                }
            }
            cursor = page.last().map_or(cursor, |event| event.seq);
        }
        let mut terminal = InternalCeilingTerminalV1 {
            end_reason: RunEndReasonV1::HarnessInternalCeiling,
            internal_cap_detected: true,
            exit_code: INTERNAL_CEILING_EXIT_CODE,
            ceilings: TurnCeilingV1 {
                soft: status.budget.tranche,
                hard: status.budget.hard_cap,
                used: status.used,
            },
            continuation: status.continuation.clone(),
            workspace_state: None,
            workspace_before: match before {
                WorkspaceBaseline::Captured(receipt) => Some(receipt.digest()),
                WorkspaceBaseline::Unavailable(_) => None,
            },
            workspace_after: None,
            workspace_receipt_error: None,
            partial_progress: PartialProgressV1 {
                files_written: None,
                files_deleted: None,
                tool_calls: calls,
                last_request_ordinal,
            },
        };
        match comparison {
            Ok((before, after)) => {
                terminal.workspace_state = Some(if before.is_same(&after) {
                    WorkspaceStateV1::Untouched
                } else {
                    WorkspaceStateV1::Mutated
                });
                terminal.workspace_after = Some(after.digest());
                terminal.partial_progress.files_written = Some(before.files_written(&after));
                terminal.partial_progress.files_deleted = Some(before.files_deleted(&after));
            }
            Err(error) => terminal.workspace_receipt_error = Some(error),
        }
        Ok(terminal)
    }
}

fn receipt_error(message: &str) -> HaiderError {
    HaiderError::new(ErrorCode::Internal, message, false)
}
