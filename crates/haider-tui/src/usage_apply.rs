//! UI bookkeeping for the daemon-owned deferred reset approval.
//! No menu or approval answer is created here. Durable menu/result events
//! rebuild pending and terminal receipts during normal session replay.

use std::collections::{BTreeMap, BTreeSet};

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{CredentialAlias, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::MenuKind;
use haider_protocol::usage::{
    USAGE_RESET_APPLY_RESULT_EXTENSION, UsageResetApplyResultV1, UsageResetApplyStatusV1,
};

use crate::usage_detail::{DetailRow, DetailTone, reset_result_rows};

#[derive(Debug, Default)]
pub struct UsageApplyState {
    in_flight: BTreeSet<(SessionId, CredentialAlias)>,
    receipts: BTreeMap<(SessionId, String), UsageResetApplyResultV1>,
    errors: BTreeMap<CredentialAlias, String>,
}

impl UsageApplyState {
    #[must_use]
    pub fn pending_session(&self, account: &CredentialAlias) -> Option<&SessionId> {
        self.in_flight
            .iter()
            .find(|(_, alias)| alias == account)
            .map(|(session, _)| session)
            .or_else(|| {
                self.receipts.iter().find_map(|((session, _), result)| {
                    (&result.account == account
                        && matches!(
                            result.status,
                            UsageResetApplyStatusV1::ApprovalPending
                                | UsageResetApplyStatusV1::Unknown
                        ))
                    .then_some(session)
                })
            })
    }

    pub fn pending(&self, account: &CredentialAlias) -> bool {
        self.pending_session(account).is_some()
    }

    pub fn begin(&mut self, session: SessionId, account: CredentialAlias) {
        self.errors.remove(&account);
        self.in_flight.insert((session, account));
    }

    /// Accept a correlated RPC receipt and release its request latch.
    /// Durable journal replay uses `observe` and cannot acknowledge a
    /// newer request merely because it shares the account and session.
    pub fn record(&mut self, session: &SessionId, result: UsageResetApplyResultV1) {
        self.in_flight
            .remove(&(session.clone(), result.account.clone()));
        self.errors.remove(&result.account);
        self.record_receipt(session, result);
    }

    fn record_receipt(&mut self, session: &SessionId, result: UsageResetApplyResultV1) {
        let key = (session.clone(), result.approval_id.clone());
        // A replayed MenuOpened or a late initial RPC receipt must not
        // replace an already-observed terminal provider result.
        if self.receipts.get(&key).is_some_and(|old| {
            old.status != UsageResetApplyStatusV1::ApprovalPending
                && result.status == UsageResetApplyStatusV1::ApprovalPending
        }) {
            return;
        }
        self.receipts.insert(key, result);
    }

    pub fn failed(&mut self, account: CredentialAlias, reason: String) {
        self.in_flight.retain(|(_, alias)| alias != &account);
        self.errors.insert(account, reason);
    }

    pub fn observe(&mut self, session: &SessionId, payload: &EventPayload) {
        match payload {
            EventPayload::MenuOpened(menu) => {
                if let MenuKind::DeferredApproval {
                    approval_id,
                    effect: EffectClass::UsageResetApply,
                    account: Some(account),
                    ..
                } = &menu.kind
                {
                    self.record_receipt(
                        session,
                        UsageResetApplyResultV1 {
                            approval_id: approval_id.clone(),
                            account: account.clone(),
                            status: UsageResetApplyStatusV1::ApprovalPending,
                            windows_reset: None,
                            reason: None,
                        },
                    );
                }
            }
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            }) if kind == USAGE_RESET_APPLY_RESULT_EXTENSION => {
                if let Ok(result) = serde_json::from_value(data.clone()) {
                    // Replayed receipts can belong to an older request in
                    // this session. Only its correlated RPC reply releases
                    // the current request latch.
                    self.record_receipt(session, result);
                }
            }
            _ => {}
        }
    }

    #[must_use]
    pub fn rows(&self, account: &CredentialAlias) -> Vec<DetailRow> {
        let mut rows = Vec::new();
        if self.in_flight.iter().any(|(_, alias)| alias == account) {
            rows.push(DetailRow::new(
                "reset request",
                "requesting approval · awaiting daemon receipt",
                DetailTone::Normal,
            ));
        }
        if let Some(reason) = self.errors.get(account) {
            rows.push(DetailRow::new(
                "reset request",
                format!("request failed · {reason}"),
                DetailTone::Warn,
            ));
        }
        for ((session, _), result) in &self.receipts {
            if &result.account == account {
                rows.push(DetailRow::new(
                    "session",
                    session.to_string(),
                    DetailTone::Dim,
                ));
                rows.extend(reset_result_rows(result));
            }
        }
        rows
    }
}
