//! The `usage_reset_apply` agent tool. A first call parks a deferred human
//! approval and returns `approval_pending`; a call with `approval_id` reads
//! the journaled decision and result. The tool never consumes a credit
//! itself (see [`crate::usage_reset`]).

use super::*;
use crate::usage_reset::{self, UnresolvedApproval};
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    USAGE_RESET_APPLY_RESULT_EXTENSION, UsageResetApplyResultV1, UsageResetApplyStatusV1,
};

const JOURNAL_PAGE_SIZE: usize = 256;

impl BrokerToolDispatcher {
    pub(super) async fn execute_usage_reset_apply(
        &self,
        run_id: &RunId,
        call_id: &str,
        args: &serde_json::Value,
    ) -> Result<ToolDispatchResult, HaiderError> {
        let request: haider_tools::UsageResetApply =
            serde_json::from_value(args.clone()).map_err(|error| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!("invalid usage_reset_apply arguments: {error}"),
                    false,
                )
            })?;
        let policy_intent = usage_reset::intent(
            request.approval_id.as_deref().unwrap_or_default(),
            &request.account,
        );
        let policy_denial = {
            let policy = self.policy.lock().await;
            usage_reset::policy_denial_reason(&policy, &policy_intent)
        };
        if let Some(reason) = policy_denial {
            return Ok(self.permission_denial_result(reason).await);
        }
        let service = self
            .output
            .store
            .hub()
            .usage_report_service()
            .map_err(hub_error)?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::CredentialMissing,
                    "usage subsystem is not configured",
                    false,
                )
            })?;
        if !service.reset_apply_supported(&request.account) {
            return usage_reset_tool_result(&usage_reset::failed(
                request.approval_id.as_deref().unwrap_or_default(),
                &request.account,
                "provider_reset_not_supported",
            ));
        }
        if let Some(approval_id) = request.approval_id.as_deref() {
            let result = self
                .usage_reset_status(&request.account, approval_id)
                .await?
                .unwrap_or_else(|| {
                    usage_reset::failed(approval_id, &request.account, "approval_not_found")
                });
            return usage_reset_tool_result(&result);
        }

        let approval_id = usage_reset::approval_id(&[
            self.session_id.as_str(),
            run_id.as_str(),
            call_id,
            request.account.as_str(),
        ]);
        if let Some(existing) = self
            .usage_reset_status(&request.account, &approval_id)
            .await?
        {
            return usage_reset_tool_result(&existing);
        }
        let menu = usage_reset::approval_menu(&approval_id, &request.account);
        let intent = usage_reset::intent(&approval_id, &request.account);
        let effect = intent.effect.clone();
        append_payloads(
            &self.output.store,
            &self.output.device_id,
            run_id,
            self.branch_id.as_ref(),
            &self.output.event_ids,
            vec![
                EventPayload::Effect(EffectPhase::Intent(intent)),
                EventPayload::Effect(EffectPhase::Authorized {
                    effect,
                    verdict: AuthorizationVerdict::Ask {
                        menu: menu.id.clone(),
                    },
                }),
                EventPayload::MenuOpened(menu),
            ],
        )
        .await?;
        usage_reset_tool_result(&usage_reset::result(
            &approval_id,
            &request.account,
            UsageResetApplyStatusV1::ApprovalPending,
        ))
    }

    /// Reads one parked approval from the session journal: its terminal
    /// result when journaled, otherwise the status derived from the answer
    /// and dispatch facts. `None` means no such approval was opened.
    async fn usage_reset_status(
        &self,
        account: &CredentialAlias,
        approval_id: &str,
    ) -> Result<Option<UsageResetApplyResultV1>, HaiderError> {
        let effect = usage_reset::effect_id(approval_id);
        let mut cursor = 0;
        let mut menu_id = None;
        let mut facts = UnresolvedApproval::default();
        loop {
            let page = self
                .output
                .store
                .read_for(
                    haider_platform::phase_trace::StoreReadCaller::WorkerPermission,
                    &self.session_id,
                    cursor,
                    JOURNAL_PAGE_SIZE,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |event| event.seq);
            for event in page {
                let Ok(payload) = event.payload.decode_event() else {
                    continue;
                };
                match payload {
                    EventPayload::MenuOpened(Menu {
                        kind:
                            MenuKind::DeferredApproval {
                                approval_id: stored_id,
                                effect: EffectClass::UsageResetApply,
                                account: Some(stored_account),
                            },
                        id,
                        ..
                    }) if stored_id == approval_id && stored_account == *account => {
                        menu_id = Some(id);
                    }
                    EventPayload::MenuAnswered(answer)
                        if menu_id.as_ref() == Some(&answer.menu)
                            && answer.via == AnswerVia::Rpc =>
                    {
                        facts.answer = Some(
                            usage_reset::answered_option_key(answer.option_key.as_deref())
                                .to_owned(),
                        );
                        facts.answer_generation = Some(event.worker_generation);
                    }
                    EventPayload::Effect(EffectPhase::Dispatched {
                        effect: dispatched_effect,
                    }) if dispatched_effect == effect => {
                        facts.dispatched_generation = Some(event.worker_generation);
                    }
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { kind, data },
                        ..
                    }) if kind == USAGE_RESET_APPLY_RESULT_EXTENSION => {
                        let Ok(result) = serde_json::from_value::<UsageResetApplyResultV1>(data)
                        else {
                            continue;
                        };
                        if result.approval_id == approval_id && result.account == *account {
                            return Ok(Some(result));
                        }
                    }
                    _ => {}
                }
            }
        }
        if menu_id.is_none() {
            return Ok(None);
        }
        Ok(Some(usage_reset::unresolved_status(
            approval_id,
            account,
            &facts,
            self.output.store.worker_generation(),
        )))
    }
}

fn usage_reset_tool_result(
    result: &UsageResetApplyResultV1,
) -> Result<ToolDispatchResult, HaiderError> {
    let preview = serde_json::to_string(result).map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("cannot encode usage reset result: {error}"),
            false,
        )
    })?;
    Ok(ToolDispatchResult::Completed(BoundedResult {
        preview,
        truncated: false,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: if result.status == UsageResetApplyStatusV1::Failed {
            ToolResultStatus::Failed
        } else {
            ToolResultStatus::Completed
        },
        reason: result.reason.clone(),
        presentation: None,
        orchestration: None,
    }))
}
