//! Follow-up obligations are distinct from source completion and wake admission.
//! Additive journal tags use the raw-envelope extension contract. A handled
//! receipt names the consuming attempt and committed evidence; run success is
//! deliberately not a receipt. Delivery is at least once, not exactly-once effects.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::ids::{AgentId, BranchId, EventId, RunId, SessionId};
use crate::state::RunState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_COMPLETION_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionControl {
    Claim,
    Handled,
    Dismiss,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionSource {
    Monitor,
    Task,
    Menu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    Pending,
    Admitting,
    Active,
    AwaitingReceipt,
    Parked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionParkReason {
    RequestRepair,
    ProviderRecovery,
    Budget,
    Interrupted,
    AttemptsExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionObligation {
    pub obligation_id: String,
    pub source: CompletionSource,
    pub source_id: String,
    pub session_id: SessionId,
    pub branch_id: Option<BranchId>,
    pub agent_id: Option<AgentId>,
    pub created_seq: u64,
    pub action: String,
    #[serde(default)]
    pub report_only: bool,
    pub status: CompletionStatus,
    pub attempt: u32,
    pub run_id: Option<RunId>,
    pub accepted_seq: Option<u64>,
    pub worker_generation: u64,
    pub park_reason: Option<CompletionParkReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// The common prefix is part of the durable raw-envelope event namespace.
#[allow(clippy::enum_variant_names)]
pub enum CompletionEvent {
    CompletionAttempt {
        obligation_id: String,
        attempt: u32,
        worker_generation: u64,
    },
    CompletionAdmitted {
        obligation_id: String,
        attempt: u32,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
    },
    CompletionDelivered {
        obligation_id: String,
        attempt: u32,
    },
    CompletionParked {
        obligation_id: String,
        attempt: u32,
        reason: CompletionParkReason,
    },
    CompletionResumed {
        obligation_id: String,
        attempt: u32,
    },
    CompletionHandled {
        obligation_id: String,
        attempt: u32,
        run_id: RunId,
        evidence_event_ids: Vec<EventId>,
    },
    CompletionDismissed {
        obligation_id: String,
        attempt: u32,
    },
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CompletionProjection {
    pub pending: BTreeMap<String, CompletionObligation>,
    settled: BTreeSet<String>,
    menus: BTreeSet<String>,
    monitors: BTreeMap<String, String>,
    terminal: BTreeMap<String, (CompletionStatus, Option<CompletionParkReason>)>,
}

impl CompletionProjection {
    pub fn is_settled(&self, id: &str) -> bool {
        self.settled.contains(id)
    }

    pub fn apply(&mut self, envelope: &RawEnvelope) {
        let value = &envelope.payload;
        // Copied history is never authority to wake an independent fork.
        if value.get("type").and_then(|v| v.as_str()) == Some("session_forked") {
            *self = Self::default();
            return;
        }
        let kind = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if kind.starts_with("completion_")
            && let Ok(event) = value.decode::<CompletionEvent>()
        {
            self.apply_event(event);
            return;
        }
        if value.get("type").and_then(|v| v.as_str()) == Some("monitor_report_pending") {
            if let Some(report) = value.pointer("/pending/report")
                && report.get("session_id").and_then(|v| v.as_str())
                    == Some(envelope.session_id.as_str())
                && let Some(id) = report.get("report_id").and_then(|v| v.as_str())
            {
                let action = report
                    .pointer("/action/follow_up")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("Report this monitor result to its owner");
                self.insert(envelope, CompletionSource::Monitor, id, action, None);
                if let Some(o) = self.pending.get_mut(&format!("monitor:{id}")) {
                    o.report_only = report
                        .pointer("/action/follow_up")
                        .and_then(|v| v.as_str())
                        .is_none_or(|s| s.trim().is_empty());
                }

                if let Some(watch) = report.get("monitor_id").and_then(|v| v.as_str()) {
                    self.monitors.insert(format!("monitor:{id}"), watch.into());
                }
            }
            return;
        }
        if value.get("type").and_then(|v| v.as_str()) == Some("monitor_removed")
            && value.get("reason").and_then(|v| v.as_str()) == Some("removed")
        {
            if let Some(watch) = value.get("monitor_id").and_then(|v| v.as_str()) {
                let ids = self
                    .monitors
                    .iter()
                    .filter(|(_, owner)| owner.as_str() == watch)
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    self.pending.remove(&id);
                    self.settled.insert(id);
                }
            }
            return;
        }
        if kind == "task_completed"
            && let Some(crate::task::TaskEventPayload::TaskCompleted(task)) =
                crate::task::TaskEventPayload::from_payload_value(value)
        {
            let consumer = task.completion_consumer;
            self.insert(
                envelope,
                CompletionSource::Task,
                task.task.as_str(),
                "Review the completed task; claim explicitly before taking a follow-up action",
                consumer.as_ref().map(|binding| binding.run_id.clone()),
            );
            if let Some(binding) = consumer
                && let Some(o) = self.pending.get_mut(&format!("task:{}", task.task))
            {
                o.branch_id = binding.branch_id;
            }
            return;
        }
        if kind == "run_retried" {
            if let Ok(crate::retry::RunRetryEventPayload::RunRetried { failed_run_id, .. }) =
                value.decode()
                && let Some(run) = &envelope.run_id
            {
                for o in self
                    .pending
                    .values_mut()
                    .filter(|o| o.run_id.as_ref() == Some(&failed_run_id))
                {
                    if o.attempt >= MAX_COMPLETION_ATTEMPTS {
                        o.status = CompletionStatus::Parked;
                        o.park_reason = Some(CompletionParkReason::AttemptsExhausted);
                        continue;
                    }
                    o.attempt += 1;
                    o.run_id = Some(run.clone());
                    o.branch_id = envelope.branch_id.clone();
                    o.agent_id = envelope.agent_id.clone();
                    o.accepted_seq = Some(envelope.seq);
                    o.worker_generation = envelope.worker_generation;
                    o.status = CompletionStatus::Active;
                    o.park_reason = None;
                }
            }
            return;
        }
        if !matches!(
            kind,
            "menu_opened" | "menu_answered" | "menu_closed" | "run_failed" | "run_state"
        ) {
            return;
        }
        match value.decode_event() {
            Ok(EventPayload::MenuOpened(menu)) if menu.origin == "request_input" => {
                self.menus.insert(menu.id.to_string());
            }
            Ok(EventPayload::MenuAnswered(answer)) if self.menus.remove(answer.menu.as_str()) => {
                self.insert(
                    envelope,
                    CompletionSource::Menu,
                    answer.menu.as_str(),
                    "Continue from the recorded answer; do not ask or answer this menu again",
                    envelope.run_id.clone(),
                );
            }
            Ok(EventPayload::MenuClosed { menu, .. }) => {
                self.menus.remove(menu.as_str());
            }
            Ok(EventPayload::RunFailed {
                code,
                retryable,
                presentation,
                ..
            }) => {
                let reason = match code {
                    crate::error::ErrorCode::RequestBudgetExceeded
                    | crate::error::ErrorCode::BudgetExhausted => CompletionParkReason::Budget,
                    crate::error::ErrorCode::Internal
                        if retryable
                            && presentation.as_ref().is_some_and(|p| {
                                p.subcode.as_str() == "run-recovery-interrupted"
                            }) =>
                    {
                        CompletionParkReason::Interrupted
                    }
                    crate::error::ErrorCode::ProviderError
                        if presentation
                            .as_ref()
                            .is_some_and(|p| p.subcode.as_str() == "quota-exhausted") =>
                    {
                        CompletionParkReason::ProviderRecovery
                    }
                    _ if retryable => CompletionParkReason::ProviderRecovery,
                    _ => CompletionParkReason::RequestRepair,
                };
                self.terminal(envelope, CompletionStatus::Parked, Some(reason));
            }
            Ok(EventPayload::RunState(RunState::Done)) => {
                self.terminal(envelope, CompletionStatus::AwaitingReceipt, None)
            }
            Ok(EventPayload::RunState(RunState::Cancelled)) => self.terminal(
                envelope,
                CompletionStatus::Parked,
                Some(CompletionParkReason::Interrupted),
            ),
            Ok(EventPayload::RunState(RunState::Errored))
                if envelope
                    .run_id
                    .as_ref()
                    .is_some_and(|run| !self.terminal.contains_key(run.as_str())) =>
            {
                self.terminal(
                    envelope,
                    CompletionStatus::Parked,
                    Some(CompletionParkReason::Interrupted),
                );
            }
            _ => {}
        }
    }

    fn insert(
        &mut self,
        envelope: &RawEnvelope,
        source: CompletionSource,
        source_id: &str,
        action: &str,
        run_id: Option<RunId>,
    ) {
        let prefix = match source {
            CompletionSource::Monitor => "monitor",
            CompletionSource::Task => "task",
            CompletionSource::Menu => "menu",
        };
        let id = format!("{prefix}:{source_id}");
        if self.settled.contains(&id) {
            return;
        }
        self.pending
            .entry(id.clone())
            .or_insert_with(|| CompletionObligation {
                obligation_id: id.clone(),
                source,
                source_id: source_id.into(),
                session_id: envelope.session_id.clone(),
                branch_id: envelope.branch_id.clone(),
                agent_id: envelope.agent_id.clone(),
                created_seq: envelope.seq,
                action: action.chars().take(4000).collect(),
                report_only: false,
                status: if run_id.is_some() {
                    CompletionStatus::Active
                } else {
                    CompletionStatus::Pending
                },
                attempt: u32::from(run_id.is_some()),
                accepted_seq: run_id.as_ref().map(|_| envelope.seq),
                run_id,
                worker_generation: envelope.worker_generation,
                park_reason: None,
            });
        if let Some(o) = self.pending.get_mut(&id)
            && let Some(run) = &o.run_id
            && let Some((status, reason)) = self.terminal.get(run.as_str())
        {
            o.status = *status;
            o.park_reason = *reason;
        }
    }

    fn terminal(
        &mut self,
        envelope: &RawEnvelope,
        status: CompletionStatus,
        reason: Option<CompletionParkReason>,
    ) {
        let Some(run) = &envelope.run_id else {
            return;
        };
        self.terminal.insert(run.to_string(), (status, reason));
        for obligation in self
            .pending
            .values_mut()
            .filter(|o| o.run_id.as_ref() == Some(run))
        {
            obligation.status = status;
            obligation.park_reason = reason;
        }
    }

    fn apply_event(&mut self, event: CompletionEvent) {
        match event {
            CompletionEvent::CompletionAttempt {
                obligation_id,
                attempt,
                worker_generation,
            } => {
                if let Some(o) = self.pending.get_mut(&obligation_id)
                    && attempt == o.attempt.saturating_add(1)
                    && attempt <= MAX_COMPLETION_ATTEMPTS
                {
                    o.attempt = attempt;
                    o.worker_generation = worker_generation;
                    o.status = CompletionStatus::Admitting;
                    o.run_id = None;
                    o.accepted_seq = None;
                    o.park_reason = None;
                }
            }
            CompletionEvent::CompletionAdmitted {
                obligation_id,
                attempt,
                run_id,
                accepted_seq,
                worker_generation,
            } => {
                if let Some(o) = self.pending.get_mut(&obligation_id)
                    && attempt == o.attempt
                {
                    let (status, reason) = self
                        .terminal
                        .get(run_id.as_str())
                        .copied()
                        .unwrap_or((CompletionStatus::Admitting, None));
                    o.run_id = Some(run_id);
                    o.accepted_seq = Some(accepted_seq);
                    o.worker_generation = worker_generation;
                    o.status = status;
                    o.park_reason = reason;
                }
            }
            CompletionEvent::CompletionDelivered {
                obligation_id,
                attempt,
            } => {
                if let Some(o) = self.pending.get_mut(&obligation_id)
                    && attempt == o.attempt
                    && o.status == CompletionStatus::Admitting
                {
                    o.status = CompletionStatus::Active;
                }
            }
            CompletionEvent::CompletionParked {
                obligation_id,
                attempt,
                reason,
            } => {
                if let Some(o) = self.pending.get_mut(&obligation_id)
                    && attempt == o.attempt
                {
                    o.status = CompletionStatus::Parked;
                    o.park_reason = Some(reason);
                }
            }
            CompletionEvent::CompletionResumed {
                obligation_id,
                attempt,
            } => {
                if let Some(o) = self.pending.get_mut(&obligation_id)
                    && attempt == o.attempt
                {
                    o.status = CompletionStatus::Pending;
                    o.park_reason = None;
                    o.run_id = None;
                }
            }
            CompletionEvent::CompletionHandled {
                obligation_id,
                attempt,
                run_id,
                evidence_event_ids,
            } => {
                if self
                    .pending
                    .get(&obligation_id)
                    .is_some_and(|o| o.attempt == attempt && o.run_id.as_ref() == Some(&run_id))
                    && !evidence_event_ids.is_empty()
                {
                    self.pending.remove(&obligation_id);
                    self.settled.insert(obligation_id);
                }
            }
            CompletionEvent::CompletionDismissed {
                obligation_id,
                attempt,
            } => {
                if self
                    .pending
                    .get(&obligation_id)
                    .is_some_and(|o| o.attempt == attempt)
                {
                    self.pending.remove(&obligation_id);
                    self.settled.insert(obligation_id);
                }
            }
        }
    }
}
