//! Provider-authored tool calls that must never dispatch: arguments that are
//! not a JSON object, and arguments cut off by the response-token limit. Each
//! is closed with a durable failed result, and the model receives an empty
//! argument object paired with that result. Only malformed calls consume the
//! strike (`invalid_tool_call_result`).

use super::*;
use haider_protocol::tool::ToolResultData;

const OUTPUT_LIMIT_SUBCODE: &str = "output_limit_truncation_repeated";
const OUTPUT_LIMIT_TITLE: &str = "Tool call exceeded the output limit";

/// Both live driving and journal recovery advance once per truncated provider
/// response, even when that response contains more than one open tool call.
pub(super) fn advance_output_truncation_count(current: u8) -> u8 {
    current.saturating_add(1).min(3)
}

pub(super) fn repeated_output_truncation_on_next(current: u8) -> bool {
    current >= 2
}

/// The kind-specific facts of one unexecuted call's failed result.
struct UnexecutedToolFailure<'a> {
    /// `error.kind` in the model-visible diagnostic.
    kind: &'static str,
    message: &'a str,
    /// The model-visible instruction for the repair continuation.
    repair: &'static str,
    data: ToolResultData,
    presentation: ErrorPresentation,
}

impl HarnessActor {
    /// Closes a provider-authored tool call whose streamed argument buffer is
    /// not a JSON object. Commit the failed call/result pair before permitting
    /// one repair continuation. Raw arguments remain in the journal; the model
    /// receives an empty object paired with an explicit invalid-call result.
    pub(super) async fn close_malformed_tool_failure(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        call_id: &str,
        error: &ProviderError,
        repaired: bool,
    ) -> Result<(Block, Message), DriveError> {
        let Some(index) = tools.iter().position(|tool| tool.call_id == call_id) else {
            return Err(DriveError::Provider(provider_protocol_error(format!(
                "provider ended unknown tool call `{call_id}`",
            ))));
        };
        let failure = UnexecutedToolFailure {
            kind: "invalid_tool_call",
            message: &error.message,
            repair: "Resend the tool call with valid JSON object arguments matching its schema. A second consecutive malformed call terminates the run.",
            data: ToolResultData::InvalidToolCall {
                tool: tools[index].name.clone(),
                message: error.message.clone(),
                repaired: Some(repaired),
            },
            presentation: tool_error_presentation(
                "invalid-tool-call",
                "Invalid tool call",
                &error.message,
            ),
        };
        self.close_unexecuted_tool_call(run_id, tools, index, failure)
            .await
    }

    /// Output-limit recovery for a provider round that finished at
    /// `MaxTokens`. Every call without a `ToolCallEnd` (absent from
    /// `assistant_blocks`) is still accumulating arguments; each is closed
    /// with a durable `output_limit_truncation` result and never dispatched.
    /// Returns whether any call was truncated, once per provider response.
    pub(super) async fn close_output_limited_tool_calls(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        assistant_blocks: &mut Vec<Block>,
        tool_results: &mut Vec<Message>,
        repeated: bool,
    ) -> Result<bool, DriveError> {
        let completed_call_ids = assistant_blocks
            .iter()
            .filter_map(|block| match block {
                Block::ToolCall { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let partial_call_ids = tools
            .iter()
            .filter(|tool| !completed_call_ids.contains(tool.call_id.as_str()))
            .map(|tool| tool.call_id.clone())
            .collect::<Vec<_>>();
        for call_id in &partial_call_ids {
            let (block, result) = self
                .close_output_limit_tool_failure(run_id, tools, call_id, repeated)
                .await?;
            assistant_blocks.push(block);
            tool_results.push(result);
        }
        Ok(!partial_call_ids.is_empty())
    }

    /// Closes a call that never received ToolCallEnd because the provider hit
    /// its response limit. Raw partial bytes remain durable for diagnosis,
    /// while the call itself is never dispatched.
    async fn close_output_limit_tool_failure(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        call_id: &str,
        repeated: bool,
    ) -> Result<(Block, Message), DriveError> {
        let Some(index) = tools.iter().position(|tool| tool.call_id == call_id) else {
            return Err(DriveError::Provider(provider_protocol_error(format!(
                "provider truncated unknown tool call `{call_id}`",
            ))));
        };
        let message = "the provider stopped at its output limit before the tool arguments were complete; the truncated tool call was not executed";
        let failure = UnexecutedToolFailure {
            kind: if repeated {
                "output_limit_truncation_repeated"
            } else {
                "output_limit_truncation"
            },
            message,
            repair: "Split large content across multiple smaller tool calls. After three consecutive truncations, choose how to proceed; the run remains active.",
            data: ToolResultData::OutputLimitTruncation {
                tool: tools[index].name.clone(),
                message: message.to_owned(),
                repaired: Some(!repeated),
            },
            presentation: tool_error_presentation(
                if repeated {
                    OUTPUT_LIMIT_SUBCODE
                } else {
                    "output_limit_truncation"
                },
                OUTPUT_LIMIT_TITLE,
                if repeated {
                    "Three consecutive responses stopped before tool arguments were complete. Split the write into smaller calls or choose another approach. The run remains active."
                } else {
                    "The provider stopped before the tool arguments were complete. The truncated call was not executed."
                },
            ),
        };
        self.close_unexecuted_tool_call(run_id, tools, index, failure)
            .await
    }

    /// Commits the failed result for `tools[index]` and removes the call. The
    /// returned block carries an empty argument object; the returned message
    /// carries the model-visible diagnostic.
    async fn close_unexecuted_tool_call(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        failure: UnexecutedToolFailure<'_>,
    ) -> Result<(Block, Message), DriveError> {
        let tool = &tools[index];
        let diagnostic = serde_json::json!({
            "status": "failed",
            "error": {
                "kind": failure.kind,
                "tool": tool.name,
                "message": failure.message,
                "repair": failure.repair,
            },
        });
        let result = BoundedResult {
            preview: diagnostic.to_string(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: Some(failure.data),
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Failed,
            reason: Some(failure.message.to_owned()),
            presentation: Some(failure.presentation),
            orchestration: None,
        };
        let result = tool.correct_result(result);
        self.commit_tool_result_and_completion(run_id, tool, &result)
            .await?;
        let block = Block::ToolCall {
            call_id: tool.call_id.clone(),
            name: tool.name.clone(),
            args: serde_json::json!({}),
        };
        let message = Message::tool_result(tool.call_id.clone(), result.preview, false);
        tools.remove(index);
        Ok((block, message))
    }
}
