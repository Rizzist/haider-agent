//! `usage.reset_apply` and the human answer to its deferred approval menu.
//! The two-phase contract is documented in [`crate::usage_reset`].

use super::*;
use crate::usage_reset;
use haider_protocol::effect::{AuthorizationVerdict, EffectOutcome};
use haider_protocol::usage::{
    USAGE_RESET_APPLY_RESULT_EXTENSION, UsageResetApplyResultV1, UsageResetApplyStatusV1,
};

/// Journal coordinates shared by the events of one approval stage.
struct ResetEventScope<'a> {
    session_id: &'a SessionId,
    branch_id: Option<haider_protocol::ids::BranchId>,
    run_id: Option<RunId>,
    agent_id: Option<AgentId>,
    authority_epoch: u64,
    causation_id: &'a EventId,
}

impl<'a> ResetEventScope<'a> {
    /// Follow-up events inherit the committed menu answer's coordinates.
    fn after(answered: &'a RawEnvelope) -> Self {
        Self {
            session_id: &answered.session_id,
            branch_id: answered.branch_id.clone(),
            run_id: answered.run_id.clone(),
            agent_id: answered.agent_id.clone(),
            authority_epoch: answered.authority_epoch,
            causation_id: &answered.event_id,
        }
    }
}

/// The durable menu opening of an earlier request with the same approval id.
struct ParkedApproval {
    account: CredentialAlias,
    menu_id: MenuId,
    seq: u64,
    worker_generation: u64,
}

impl HubConnection {
    pub(super) async fn usage_reset_apply(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        account: CredentialAlias,
    ) -> Result<(), SessionHubError> {
        let Some(service) = self.hub.usage_report_service()? else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "usage subsystem is not configured",
                false,
                None,
            );
        };
        if !service.reset_apply_supported(&account) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "account does not expose an applicable reset operation",
                false,
                None,
            );
        }
        let approval_id = usage_reset::approval_id(&[session_id.as_str(), command_id.as_str()]);

        if let Some(parked) = self.parked_usage_reset(&session_id, &approval_id).await? {
            if parked.account != account {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "command id was already used for another usage reset account",
                    false,
                    None,
                );
            }
            return self.send_usage_reset_pending(
                request_id,
                approval_id,
                account,
                parked.menu_id,
                parked.seq,
                parked.worker_generation,
            );
        }

        let latest_seq = self.hub.inner.store.latest_seq(&session_id).await?;
        let mut head = self
            .hub
            .inner
            .store
            .read_for(
                haider_platform::phase_trace::StoreReadCaller::SessionHubRpc,
                &session_id,
                latest_seq.saturating_sub(1),
                1,
            )
            .await?;
        let Some(head) = head.pop() else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        };
        let menu = usage_reset::approval_menu(&approval_id, &account);
        let menu_id = menu.id.clone();
        let intent = usage_reset::intent(&approval_id, &account);
        let effect = intent.effect.clone();
        let scope = ResetEventScope {
            session_id: &session_id,
            branch_id: None,
            run_id: None,
            agent_id: None,
            authority_epoch: head.authority_epoch,
            causation_id: &head.event_id,
        };
        let mut envelopes = self.hub.usage_reset_envelopes(
            &scope,
            &approval_id,
            "approval",
            vec![
                (
                    "intent",
                    EventPayload::Effect(EffectPhase::Intent(intent)),
                    PromptRender::Omit,
                ),
                (
                    "ask",
                    EventPayload::Effect(EffectPhase::Authorized {
                        effect,
                        verdict: AuthorizationVerdict::Ask {
                            menu: menu_id.clone(),
                        },
                    }),
                    PromptRender::Omit,
                ),
                ("open", EventPayload::MenuOpened(menu), PromptRender::Omit),
            ],
        )?;
        self.hub.append(&mut envelopes).await?;
        let opening = &envelopes[2];
        self.send_usage_reset_pending(
            request_id,
            approval_id,
            account,
            menu_id,
            opening.seq,
            opening.worker_generation,
        )
    }

    /// Scans the session journal for an earlier opening of `approval_id`,
    /// which makes a repeated command id an idempotent replay.
    async fn parked_usage_reset(
        &self,
        session_id: &SessionId,
        approval_id: &str,
    ) -> Result<Option<ParkedApproval>, SessionHubError> {
        let mut cursor = 0;
        loop {
            let page = self
                .hub
                .inner
                .store
                .read_for(
                    haider_platform::phase_trace::StoreReadCaller::SessionHubRpc,
                    session_id,
                    cursor,
                    REPLAY_PAGE_SIZE,
                )
                .await?;
            if page.is_empty() {
                return Ok(None);
            }
            let page_len = page.len();
            cursor = page.last().map_or(cursor, |event| event.seq);
            for event in page {
                let Ok(EventPayload::MenuOpened(Menu {
                    kind:
                        MenuKind::DeferredApproval {
                            approval_id: stored_id,
                            effect: EffectClass::UsageResetApply,
                            account: Some(account),
                        },
                    id: menu_id,
                    ..
                })) = event.payload.decode_event()
                else {
                    continue;
                };
                if stored_id == approval_id {
                    return Ok(Some(ParkedApproval {
                        account,
                        menu_id,
                        seq: event.seq,
                        worker_generation: event.worker_generation,
                    }));
                }
            }
            if page_len < REPLAY_PAGE_SIZE {
                return Ok(None);
            }
        }
    }

    fn send_usage_reset_pending(
        &self,
        request_id: RequestId,
        approval_id: String,
        account: CredentialAlias,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::UsageResetApply {
                result: usage_reset::result(
                    &approval_id,
                    &account,
                    UsageResetApplyStatusV1::ApprovalPending,
                ),
                menu_id,
                request_seq,
                worker_generation,
            },
        })
    }

    /// True when `request_seq` is the opening of this exact deferred-approval
    /// menu. Its durable opening is the authority for answering it after a
    /// worker-generation change.
    pub(super) async fn deferred_approval_opening(
        &self,
        session_id: &SessionId,
        request_seq: u64,
        menu_id: &MenuId,
    ) -> Result<bool, SessionHubError> {
        let opening = self
            .hub
            .inner
            .store
            .read_for(
                haider_platform::phase_trace::StoreReadCaller::SessionHubRpc,
                session_id,
                request_seq.saturating_sub(1),
                1,
            )
            .await?
            .into_iter()
            .find(|envelope| envelope.seq == request_seq);
        let Some(opening) = opening else {
            return Ok(false);
        };
        let Ok(EventPayload::MenuOpened(menu)) = opening.payload.decode_event() else {
            return Ok(false);
        };
        Ok(menu.id == *menu_id && matches!(menu.kind, MenuKind::DeferredApproval { .. }))
    }

    /// Claims one committed human answer. The durable `Dispatched` fact is
    /// written before handing provider work to a hub-owned task. A restart
    /// never resumes that task, even when its provider outcome is unknown.
    pub(super) async fn finish_usage_reset_approval(
        &self,
        answered: &RawEnvelope,
        account: &CredentialAlias,
        approval_id: &str,
        answer: &DurableMenuAnswer,
    ) -> Result<(), SessionHubError> {
        let approved =
            usage_reset::answered_option_key(answer.option_key.as_deref(), answer.option_index)
                == usage_reset::APPROVE_OPTION_KEY;
        let effect = usage_reset::effect_id(approval_id);
        let scope = ResetEventScope::after(answered);
        let final_verdict = if approved {
            AuthorizationVerdict::Allow
        } else {
            AuthorizationVerdict::Deny {
                reason: "denied by user".into(),
            }
        };
        let mut authorization = vec![(
            "authorized-final",
            EventPayload::Effect(EffectPhase::Authorized {
                effect: effect.clone(),
                verdict: final_verdict,
            }),
            PromptRender::Omit,
        )];
        if approved {
            authorization.push((
                "dispatched",
                EventPayload::Effect(EffectPhase::Dispatched {
                    effect: effect.clone(),
                }),
                PromptRender::Omit,
            ));
        }
        let mut authorization =
            self.hub
                .usage_reset_envelopes(&scope, approval_id, "authorization", authorization)?;
        self.hub.append(&mut authorization).await?;

        let hub = self.hub.clone();
        let answered = answered.clone();
        let account = account.clone();
        let approval_id = approval_id.to_owned();
        // The hub joins these tasks during graceful shutdown and aborts them
        // on forced shutdown. An aborted POST remains unconfirmed in the
        // journal and is deliberately never retried by a poll or restart.
        let mut tasks = lock(&self.hub.inner.actor_tasks)?;
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Task(
                "usage reset dispatch interrupted by daemon drain".into(),
            ));
        }
        tasks.push(tokio::spawn(async move {
            if let Err(error) = hub
                .complete_usage_reset_approval(&answered, &account, &approval_id, approved)
                .await
            {
                tracing::error!(%approval_id, ?error, "usage reset completion failed");
            }
        }));
        Ok(())
    }
}

impl SessionHub {
    async fn complete_usage_reset_approval(
        &self,
        answered: &RawEnvelope,
        account: &CredentialAlias,
        approval_id: &str,
        approved: bool,
    ) -> Result<(), SessionHubError> {
        let effect = usage_reset::effect_id(approval_id);
        let scope = ResetEventScope::after(answered);
        let result = if approved {
            self.apply_approved_usage_reset(account, approval_id)
                .await?
        } else {
            usage_reset::result(approval_id, account, UsageResetApplyStatusV1::Denied)
        };
        let delivered_to_active_run = self
            .notify_usage_reset_result(&answered.session_id, approval_id, &result)
            .await?;

        let data = serde_json::to_value(&result).map_err(|error| {
            SessionHubError::Task(format!("cannot encode usage reset result: {error}"))
        })?;
        let mut terminal = Vec::new();
        if approved {
            let outcome = if result.status == UsageResetApplyStatusV1::Applied {
                EffectOutcome::Ok
            } else {
                EffectOutcome::Failed {
                    error: result
                        .reason
                        .clone()
                        .unwrap_or_else(|| "usage_reset_failed".into()),
                }
            };
            terminal.push((
                "outcome",
                EventPayload::Effect(EffectPhase::Outcome {
                    effect,
                    outcome,
                    freshness: None,
                    workspace_mutation: None,
                }),
                PromptRender::Omit,
            ));
        }
        terminal.push((
            "result",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("usage-reset-result-{approval_id}")),
                item: TurnItem::Extension {
                    kind: USAGE_RESET_APPLY_RESULT_EXTENSION.into(),
                    data,
                },
            }),
            // An active run already received the result as a steer; an idle
            // session sees it verbatim on its next turn.
            if delivered_to_active_run {
                PromptRender::Omit
            } else {
                PromptRender::Verbatim
            },
        ));
        let mut terminal =
            self.usage_reset_envelopes(&scope, approval_id, "result event", terminal)?;
        self.append(&mut terminal).await?;
        Ok(())
    }

    async fn apply_approved_usage_reset(
        &self,
        account: &CredentialAlias,
        approval_id: &str,
    ) -> Result<UsageResetApplyResultV1, SessionHubError> {
        let Some(service) = self.usage_report_service()? else {
            return Ok(usage_reset::failed(
                approval_id,
                account,
                "usage_subsystem_unavailable",
            ));
        };
        Ok(match service.apply_reset(account, approval_id).await {
            Ok(windows_reset) => UsageResetApplyResultV1 {
                windows_reset: Some(windows_reset),
                ..usage_reset::result(approval_id, account, UsageResetApplyStatusV1::Applied)
            },
            Err(error) => usage_reset::failed(approval_id, account, error.reason),
        })
    }

    /// Steers the result into the session's active run, if any. A delivery
    /// failure is logged and falls back to the journaled prompt item.
    async fn notify_usage_reset_result(
        &self,
        session_id: &SessionId,
        approval_id: &str,
        result: &UsageResetApplyResultV1,
    ) -> Result<bool, SessionHubError> {
        let notice = format!(
            "Usage reset approval result (typed JSON): {}",
            serde_json::to_string(result).map_err(|error| {
                SessionHubError::Task(format!("cannot encode usage reset notice: {error}"))
            })?
        );
        match crate::tasks::TaskFacade::new(self.clone())
            .deliver_notification(
                session_id,
                usage_reset::notification_id(approval_id),
                &notice,
            )
            .await
        {
            Ok(delivered) => Ok(delivered),
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %approval_id,
                    ?error,
                    "usage reset result notification fell back to queued delivery"
                );
                Ok(false)
            }
        }
    }

    fn usage_reset_envelopes(
        &self,
        scope: &ResetEventScope<'_>,
        approval_id: &str,
        what: &str,
        events: Vec<(&str, EventPayload, PromptRender)>,
    ) -> Result<Vec<EventEnvelope<RawPayload>>, SessionHubError> {
        events
            .into_iter()
            .map(|(stage, payload, prompt)| {
                Ok(EventEnvelope {
                    schema_version: SCHEMA_VERSION,
                    event_id: usage_reset::event_id(stage, approval_id),
                    seq: 0,
                    session_id: scope.session_id.clone(),
                    branch_id: scope.branch_id.clone(),
                    run_id: scope.run_id.clone(),
                    agent_id: scope.agent_id.clone(),
                    device_id: self.inner.device_id.clone(),
                    authority_epoch: scope.authority_epoch,
                    worker_generation: self.worker_generation(),
                    causation_id: Some(scope.causation_id.clone()),
                    correlation_id: None,
                    committed_at_ms: 0,
                    render: RenderTargets {
                        ui: true,
                        durable: true,
                        prompt,
                    },
                    payload: RawPayload::from_event(payload).map_err(|error| {
                        SessionHubError::Task(format!("cannot encode usage reset {what}: {error}"))
                    })?,
                })
            })
            .collect()
    }
}
