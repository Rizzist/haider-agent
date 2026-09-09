//! Task failure is an actor-owned terminal, never inferred from tool output.

use super::*;
use haider_protocol::task_outcome::TaskOutcomeV1;

pub(super) enum CompletedTool {
    Continue(Option<Message>),
    TaskOutcome(ToolAccumulator, TaskOutcomeV1),
}

/// The terminal facts, when present, share the tool settlement transaction.
pub(super) enum ToolSettlement<'a> {
    Continue,
    Streaming,
    TaskFailure(&'a TaskOutcomeV1, &'a CancelToken),
}

impl HarnessActor {
    pub(super) async fn prepare_task_outcome(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        deferred: &[DeferredAccumulator],
        index: usize,
    ) -> Result<CompletedTool, DriveError> {
        let args = parse_tool_args(&tools[index])?;
        let parsed = haider_tools::parse_task_outcome(args.as_ref().clone()).and_then(|outcome| {
            if tools.len() != 1 || !deferred.is_empty() {
                Err(haider_tools::ToolError::invalid_argument(
                    "task_outcome requires a standalone call with no pending tools",
                ))
            } else {
                Ok(outcome)
            }
        });
        match parsed {
            Ok(outcome) => Ok(CompletedTool::TaskOutcome(tools.remove(index), outcome)),
            Err(error) => {
                let reason = error.to_string();
                let result = tools[index].correct_result(task_outcome_result(
                    serde_json::json!({"accepted": false, "error": reason}),
                    Some(reason),
                ));
                self.commit_tool_result_and_completion(run_id, &tools[index], &result)
                    .await?;
                let tool = tools.remove(index);
                Ok(CompletedTool::Continue(Some(Message::tool_result(
                    tool.call_id,
                    result.preview,
                    false,
                ))))
            }
        }
    }

    /// Open narrative closes before the one append that accepts the signal,
    /// settles its tool, and commits RunFailed + Errored. A failed append can
    /// never leave an accepted signal without a terminal for crash recovery.
    pub(super) async fn finish_task_outcome(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        outcome: &TaskOutcomeV1,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, DriveError> {
        let result = tool.correct_result(task_outcome_result(
            serde_json::json!({"accepted": true, "task_outcome": outcome}),
            None,
        ));
        self.commit_tool_settlement(
            run_id,
            tool,
            Some(&result),
            None,
            ToolStatus::Completed,
            ToolSettlement::TaskFailure(outcome, cancel),
        )
        .await?;
        Ok(TurnOutcome {
            state: RunState::Errored,
            finish_reason: FinishReason::Error,
            error: Some(task_failure(outcome)),
        })
    }

    pub(super) fn task_failure_envelopes(
        &self,
        run_id: &RunId,
        outcome: &TaskOutcomeV1,
    ) -> Result<[RawEnvelope; 2], HaiderError> {
        let error = task_failure(outcome);
        let failure = self.uncommitted_envelope(
            run_id,
            EventPayload::RunFailed {
                code: error.code,
                message: sanitized_failure_message(&error.message),
                retryable: false,
                presentation: Some(presentation_for_haider_error(&error)),
            },
            prompt_omit_render(),
        )?;
        let mut terminal = self.uncommitted_envelope(
            run_id,
            EventPayload::RunState(RunState::Errored),
            prompt_omit_render(),
        )?;
        terminal
            .payload
            .insert_metadata("task_outcome", serde_json::json!(outcome));
        terminal
            .payload
            .insert_metadata("task_outcome_version", serde_json::json!(1));
        Ok([failure, terminal])
    }
}

fn task_failure(outcome: &TaskOutcomeV1) -> HaiderError {
    HaiderError::new(ErrorCode::TaskFailed, outcome.reason(), false)
}

fn task_outcome_result(value: serde_json::Value, rejection: Option<String>) -> BoundedResult {
    BoundedResult {
        preview: value.to_string(),
        truncated: false,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: if rejection.is_some() {
            ToolResultStatus::Rejected
        } else {
            ToolResultStatus::Completed
        },
        reason: rejection,
        presentation: None,
    }
}
