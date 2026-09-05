//! Operator-authored delegation through the ordinary broker and child collector.
use super::*;

impl HarnessActor {
    pub(super) async fn drive_agent_spawn(
        &mut self,
        run_id: &RunId,
        spawn: haider_protocol::headless::AgentSpawnSpecV1,
        checkpoint: Option<ChildWaitCheckpoint>,
        cancel: &CancelToken,
    ) -> TurnOutcome {
        let mut tools = Vec::new();
        let mut deferred = Vec::new();
        // Keep recovery ownership before the first fallible read/publication.
        let recovery_tickets = checkpoint
            .as_ref()
            .map(|checkpoint| {
                checkpoint
                    .tools
                    .iter()
                    .map(|entry| entry.ticket.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut result = self
            .execute_agent_spawn(run_id, spawn, checkpoint, cancel, &mut tools, &mut deferred)
            .await;
        if result.is_ok()
            && let Some(guard) = self.config.finalization_guard.clone()
        {
            result = match guard.before_done_after_requests(run_id, 0).await {
                Ok(FinalizationGuardDecision::AllowDone) => Ok(()),
                Ok(FinalizationGuardDecision::Continue { .. }) => {
                    Err(DriveError::Store(HaiderError::new(
                        ErrorCode::WorkflowUnfinished,
                        "direct delegation completed but parent workflow obligations remain",
                        false,
                    )))
                }
                Ok(FinalizationGuardDecision::ConfirmRequired(menu)) => {
                    match self
                        .commit_payload(
                            run_id,
                            EventPayload::MenuClosed {
                                menu: menu.id,
                                reason: MenuCloseReason::Dismissed,
                            },
                            prompt_omit_render(),
                        )
                        .await
                    {
                        Ok(_) => Err(DriveError::Store(HaiderError::new(
                            ErrorCode::WorkflowUnfinished,
                            "direct delegation cannot finish a parent workflow that requires human confirmation",
                            false,
                        ))),
                        Err(error) => Err(DriveError::Store(error)),
                    }
                }
                Err(error) => Err(DriveError::Store(error)),
            };
        }
        if result.is_err() {
            // A recovered ticket need not inhabit the new dispatcher's live
            // map. Its collector owns cancellation and the original accepted
            // run's bounded terminal/reap tail, including durable handoff.
            if let Some(dispatcher) = self.dispatcher.as_ref().map(Arc::clone) {
                let cleanup = CancelToken::new();
                cleanup.cancel();
                let mut cancelled = HashSet::new();
                for ticket in recovery_tickets
                    .iter()
                    .chain(deferred.iter().map(|pending| &pending.ticket))
                {
                    if cancelled.insert(ticket.id.clone()) {
                        let _ = dispatcher.collect_deferred(ticket, &cleanup).await;
                    }
                }
                let _ = dispatcher.cancel_outstanding_deferred().await;
            }
        }
        match result {
            Ok(()) => self.finish_outcome(run_id, FinishReason::EndTurn).await,
            Err(DriveError::Cancelled) => {
                self.cancelled_outcome_with_items(run_id, &mut None, &mut None, &mut tools)
                    .await
            }
            Err(error) => {
                self.drive_error_outcome_with_items(run_id, &mut None, &mut None, &mut tools, error)
                    .await
            }
        }
    }

    async fn execute_agent_spawn(
        &mut self,
        run_id: &RunId,
        spawn: haider_protocol::headless::AgentSpawnSpecV1,
        checkpoint: Option<ChildWaitCheckpoint>,
        cancel: &CancelToken,
        tools: &mut Vec<ToolAccumulator>,
        deferred: &mut Vec<DeferredAccumulator>,
    ) -> Result<(), DriveError> {
        if cancel.is_cancelled() && checkpoint.is_none() {
            return Err(DriveError::Cancelled);
        }
        let call_id = format!("agent-cli-{run_id}");
        let args = serde_json::to_string(&spawn).map_err(|error| {
            DriveError::Store(HaiderError::new(
                ErrorCode::InvalidArgument,
                error.to_string(),
                false,
            ))
        })?;
        let mut cursor = 0;
        let mut existing: Option<(ItemId, String)> = None;
        let mut existing_result = None;
        let mut report = None;
        let mut spawn_published = false;
        let mut spawn_item_published = false;
        loop {
            let page = self
                .store
                .read(&self.config.session_id, cursor, 256)
                .await
                .map_err(DriveError::Store)?;
            if page.is_empty() {
                break;
            }
            for event in page {
                cursor = event.seq;
                if event.run_id.as_ref() != Some(run_id) {
                    continue;
                }
                let Ok(payload) = event.payload.decode_event() else {
                    continue;
                };
                match payload {
                    EventPayload::Item(ItemEvent::Started {
                        item_id,
                        item: TurnItem::ToolCall { call_id: id, .. },
                    }) if id == call_id => existing = Some((item_id, String::new())),
                    EventPayload::Item(ItemEvent::Delta {
                        item_id,
                        delta: ItemDelta::ToolArgs { fragment },
                    }) => {
                        if let Some((id, text)) = existing.as_mut()
                            && id == &item_id
                        {
                            text.push_str(&fragment);
                        }
                    }
                    EventPayload::ToolResult {
                        call_id: id,
                        result,
                    } if id == call_id => existing_result = Some(result),
                    EventPayload::AgentSpawned(_) => spawn_published = true,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::ChildSpawn { .. },
                        ..
                    }) => spawn_item_published = true,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::ChildResult { report: result },
                        ..
                    }) => report = Some(result),
                    _ => {}
                }
            }
        }
        if let Some(checkpoint) = checkpoint {
            for entry in checkpoint.tools {
                if !spawn_published {
                    self.commit_payload(
                        run_id,
                        EventPayload::AgentSpawned(entry.ticket.manifest.clone()),
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                }
                if !spawn_item_published {
                    self.commit_closed_item(
                        run_id,
                        TurnItem::ChildSpawn {
                            agent: entry.ticket.manifest.agent.clone(),
                        },
                    )
                    .await?;
                }
                tools.push(ToolAccumulator {
                    item_id: entry.tool_item_id,
                    call_id: entry.call_id.clone(),
                    name: entry.tool_name,
                    args: entry.args,
                    requested_name: None,
                    parsed_args: OnceLock::new(),
                });
                deferred.push(DeferredAccumulator {
                    call_id: entry.call_id,
                    ticket: entry.ticket,
                    report_emitted: entry.report_emitted,
                    child_result_emitted: entry.child_result_emitted,
                    tool_result_emitted: entry.tool_result_emitted,
                    item_completed: entry.item_completed,
                });
            }
        } else if let Some(result) = existing_result {
            if report
                .as_ref()
                .is_some_and(|report| report.verified != ReportVerification::Red)
            {
                return Ok(());
            }
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::InvalidArgument,
                result.reason.unwrap_or(result.preview),
                false,
            )));
        } else {
            if let Some((item_id, text)) = existing {
                if !args.starts_with(&text) {
                    return Err(DriveError::Store(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "direct spawn arguments disagree with their durable pin",
                        false,
                    )));
                }
                tools.push(ToolAccumulator {
                    item_id,
                    call_id: call_id.clone(),
                    name: "spawn_subagent".into(),
                    args: text,
                    requested_name: None,
                    parsed_args: OnceLock::new(),
                });
            } else {
                self.start_tool(run_id, tools, call_id.clone(), "spawn_subagent".into())
                    .await?;
            }
            let consumed = tools[0].args.len();
            if consumed < args.len() {
                self.apply_tool_delta(run_id, tools, &call_id, args[consumed..].to_owned())
                    .await?;
            }
            if self
                .complete_tool(run_id, tools, deferred, &call_id, cancel)
                .await?
                .is_some()
            {
                return Err(DriveError::Store(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "agent spawn was rejected; inspect the durable tool result",
                    false,
                )));
            }
        }
        if cancel.is_cancelled() {
            return Err(DriveError::Cancelled);
        }
        self.commit_state(
            run_id,
            RunState::Waiting {
                reason: WaitReason::LocalChild,
            },
        )
        .await
        .map_err(DriveError::Store)?;
        self.settle_deferred_tools(run_id, tools, deferred, cancel)
            .await?;
        // Collection owns the report and closes every tool item before Done.
        // A red ChildResult is a failed public operation, not a successful CLI receipt.
        let mut cursor = 0;
        loop {
            let page = self
                .store
                .read(&self.config.session_id, cursor, 256)
                .await
                .map_err(DriveError::Store)?;
            if page.is_empty() {
                break;
            }
            for event in page {
                cursor = event.seq;
                if event.run_id.as_ref() == Some(run_id)
                    && let Ok(EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::ChildResult { report },
                        ..
                    })) = event.payload.decode_event()
                    && report.verified == ReportVerification::Red
                {
                    return Err(DriveError::Store(HaiderError::new(
                        ErrorCode::Internal,
                        report.summary,
                        false,
                    )));
                }
            }
        }
        Ok(())
    }
}
