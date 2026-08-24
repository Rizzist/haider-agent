//! Interest-gated Haider Code account-plan polling.
//!
//! The endpoint is provider authority: cadence and allowance state are
//! published, never inferred. The loop resolves the active account afresh on
//! every request and revision-fences the response before publication.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use haider_accounts::{MemoryVault, SecretHandle, Vault};
use haider_protocol::credential::CredentialDescriptor;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    HaiderCodeAllowanceStateV1, HaiderCodePlanOutcomeV1, HaiderCodePlanSnapshotV1,
};
use haider_provider::{
    HAIDER_CODE_ACCOUNT_URL, HAIDER_CODE_PROVIDER_NAME, parse_haider_code_account,
};
use haider_rpc::WireFrame;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::accounts::{
    ManagementSnapshot, ValidatedIdentity, ValidationError, ValidationFailureKind,
};
use crate::oauth::CredentialBroker;
use crate::session_hub::SessionHub;

pub(crate) const PLAN_REFRESH_FALLBACK: Duration = Duration::from_secs(60);
pub(crate) const PLAN_REFRESH_FLOOR: Duration = Duration::from_secs(15);
const PLAN_RESPONSE_LIMIT: usize = 256 * 1024;
const PLAN_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PLAN_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanTransientFailure {
    Timeout,
    Network,
    HttpStatus(u16),
    MalformedPayload,
    ResponseTooLarge,
    CredentialUnavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlanFetchOutcome {
    Snapshot(PlanSnapshot),
    Unauthorized,
    Transient(PlanTransientFailure),
}

/// Provider-number provenance that the public plan-status DTO intentionally
/// does not encode. Only integer JSON tokens enter this shape; a decimal
/// token remains available in `snapshot` for display but cannot later be
/// rounded back into ledger authority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PlanMeterValues {
    pub weekly_percent_remaining: Option<u64>,
    pub credits: Option<i64>,
    pub hold: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanSnapshot {
    pub snapshot: HaiderCodePlanSnapshotV1,
    pub meter: PlanMeterValues,
}

impl PlanSnapshot {
    #[cfg(test)]
    pub(crate) fn without_meter(snapshot: HaiderCodePlanSnapshotV1) -> Self {
        Self {
            snapshot,
            meter: PlanMeterValues::default(),
        }
    }
}

#[async_trait::async_trait]
pub(crate) trait HaiderCodePlanHttp: Send + Sync {
    async fn get_account(&self, credential: &SecretHandle) -> PlanFetchOutcome;
}

pub(crate) struct ProductionHaiderCodePlanHttp {
    client: Option<reqwest::Client>,
}

impl ProductionHaiderCodePlanHttp {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(PLAN_CONNECT_TIMEOUT)
            .timeout(PLAN_REQUEST_TIMEOUT)
            .build()
            .ok();
        Self { client }
    }
}

#[async_trait::async_trait]
impl HaiderCodePlanHttp for ProductionHaiderCodePlanHttp {
    async fn get_account(&self, credential: &SecretHandle) -> PlanFetchOutcome {
        let Some(client) = &self.client else {
            return PlanFetchOutcome::Transient(PlanTransientFailure::Network);
        };
        let mut authorization = Zeroizing::new(Vec::with_capacity(
            b"Bearer ".len() + credential.expose_secret().len(),
        ));
        authorization.extend_from_slice(b"Bearer ");
        authorization.extend_from_slice(credential.expose_secret());
        let mut header = match HeaderValue::from_bytes(&authorization) {
            Ok(header) => header,
            Err(_) => {
                return PlanFetchOutcome::Transient(PlanTransientFailure::CredentialUnavailable);
            }
        };
        header.set_sensitive(true);

        let response = match client
            .get(HAIDER_CODE_ACCOUNT_URL)
            .header(AUTHORIZATION, header)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                return PlanFetchOutcome::Transient(PlanTransientFailure::Timeout);
            }
            Err(_) => return PlanFetchOutcome::Transient(PlanTransientFailure::Network),
        };
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return classify_account_response(status, &[]);
        }
        if response
            .content_length()
            .is_some_and(|length| length > PLAN_RESPONSE_LIMIT as u64)
        {
            return PlanFetchOutcome::Transient(PlanTransientFailure::ResponseTooLarge);
        }

        let mut response = response;
        let mut body = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(chunk) => chunk,
                Err(error) if error.is_timeout() => {
                    return PlanFetchOutcome::Transient(PlanTransientFailure::Timeout);
                }
                Err(_) => return PlanFetchOutcome::Transient(PlanTransientFailure::Network),
            };
            let Some(chunk) = chunk else {
                break;
            };
            if body.len().saturating_add(chunk.len()) > PLAN_RESPONSE_LIMIT {
                return PlanFetchOutcome::Transient(PlanTransientFailure::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        classify_account_response(status, &body)
    }
}

pub(crate) fn classify_account_response(status: u16, body: &[u8]) -> PlanFetchOutcome {
    if status == 401 {
        return PlanFetchOutcome::Unauthorized;
    }
    if !(200..300).contains(&status) {
        return PlanFetchOutcome::Transient(PlanTransientFailure::HttpStatus(status));
    }
    match parse_haider_code_account(body) {
        Ok(snapshot) => {
            let raw = serde_json::from_slice::<serde_json::Value>(body).ok();
            let integer_at = |pointer: &str| {
                raw.as_ref()
                    .and_then(|value| value.pointer(pointer))
                    .and_then(serde_json::Value::as_u64)
            };
            PlanFetchOutcome::Snapshot(PlanSnapshot {
                snapshot,
                meter: PlanMeterValues {
                    weekly_percent_remaining: integer_at("/weekly_allowance/percent_remaining"),
                    credits: raw
                        .as_ref()
                        .and_then(|value| value.pointer("/usage_credits_usd"))
                        .and_then(serde_json::Value::as_i64),
                    // The current `hold` plan field is structured account
                    // state (flags/reason), not an integer held balance. Do
                    // not coerce that independent fact into a ledger amount.
                    hold: None,
                },
            })
        }
        Err(_) => PlanFetchOutcome::Transient(PlanTransientFailure::MalformedPayload),
    }
}

/// Validates a staged key with the same non-spending account endpoint used by
/// the poller. No response body is copied into a user-visible error.
pub(crate) async fn validate_api_key(secret: &[u8]) -> Result<ValidatedIdentity, ValidationError> {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("haider-code-validation");
    vault.put(&alias, secret).map_err(|_| ValidationError {
        kind: ValidationFailureKind::Unavailable,
        message: "Haider Code credential validation is unavailable".into(),
    })?;
    let credential = vault.resolve(&alias).map_err(|_| ValidationError {
        kind: ValidationFailureKind::Unavailable,
        message: "Haider Code credential validation is unavailable".into(),
    })?;
    let http = ProductionHaiderCodePlanHttp::new();
    match http.get_account(&credential).await {
        PlanFetchOutcome::Snapshot(_) => Ok(ValidatedIdentity {
            identity: "haider code api key".into(),
        }),
        PlanFetchOutcome::Unauthorized => Err(ValidationError {
            kind: ValidationFailureKind::Unauthorized,
            message: "Haider Code rejected the API key".into(),
        }),
        PlanFetchOutcome::Transient(PlanTransientFailure::HttpStatus(403)) => {
            Err(ValidationError {
                kind: ValidationFailureKind::PermissionDenied,
                message: "Haider Code denied access to the account endpoint".into(),
            })
        }
        PlanFetchOutcome::Transient(_) => Err(ValidationError {
            kind: ValidationFailureKind::Unavailable,
            message: "Haider Code credential validation is unavailable".into(),
        }),
    }
}

#[derive(Clone)]
pub(crate) struct ActivePlanAccount {
    pub descriptor: CredentialDescriptor,
}

pub(crate) trait PlanAccountSource: Send + Sync {
    fn active_account(&self) -> Option<ActivePlanAccount>;
    fn subscribe(&self) -> watch::Receiver<u64>;
}

impl PlanAccountSource for ManagementSnapshot {
    fn active_account(&self) -> Option<ActivePlanAccount> {
        let view = self.read()?;
        view.descriptors
            .into_iter()
            .find(|descriptor| {
                descriptor.provider == HAIDER_CODE_PROVIDER_NAME && descriptor.active
            })
            .map(|descriptor| ActivePlanAccount { descriptor })
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        ManagementSnapshot::subscribe(self)
    }
}

#[async_trait::async_trait]
pub(crate) trait PlanCredentialSource: Send + Sync {
    async fn resolve(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, PlanTransientFailure>;
}

#[async_trait::async_trait]
impl PlanCredentialSource for CredentialBroker {
    async fn resolve(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, PlanTransientFailure> {
        CredentialBroker::resolve(self, descriptor)
            .await
            .map_err(|_| PlanTransientFailure::CredentialUnavailable)
    }
}

#[async_trait::async_trait]
pub(crate) trait PlanInterestSource: Send + Sync {
    async fn recipients(&self) -> Vec<String>;
    fn subscribe(&self) -> watch::Receiver<u64>;
    fn publish(
        &self,
        recipients: &[String],
        account_alias: CredentialAlias,
        outcome: HaiderCodePlanOutcomeV1,
    );
    async fn capture(
        &self,
        account_alias: CredentialAlias,
        snapshot: HaiderCodePlanSnapshotV1,
        meter: PlanMeterValues,
    );
    async fn clear(&self, account_alias: &CredentialAlias);
}

#[async_trait::async_trait]
impl PlanInterestSource for SessionHub {
    async fn recipients(&self) -> Vec<String> {
        let Ok(attachments) = self.attached_session_connections() else {
            return Vec::new();
        };
        let mut session_matches = HashMap::new();
        let mut recipients = HashSet::new();
        for (connection_id, session_id) in attachments {
            let matches = if let Some(matches) = session_matches.get(&session_id) {
                *matches
            } else {
                let matches = self
                    .session_metadata(&session_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|metadata| metadata.provider == HAIDER_CODE_PROVIDER_NAME);
                session_matches.insert(session_id, matches);
                matches
            };
            if matches {
                recipients.insert(connection_id);
            }
        }
        let mut recipients = recipients.into_iter().collect::<Vec<_>>();
        recipients.sort();
        recipients
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.subscribe_haider_code_plan_changes()
    }

    fn publish(
        &self,
        recipients: &[String],
        account_alias: CredentialAlias,
        outcome: HaiderCodePlanOutcomeV1,
    ) {
        self.publish_haider_code_plan_status(
            recipients,
            WireFrame::HaiderCodePlanStatus {
                provider: HAIDER_CODE_PROVIDER_NAME.into(),
                account_alias,
                outcome,
            },
        );
    }

    async fn capture(
        &self,
        account_alias: CredentialAlias,
        snapshot: HaiderCodePlanSnapshotV1,
        meter: PlanMeterValues,
    ) {
        let _capture_result = self
            .capture_haider_code_plan_status(account_alias, snapshot, meter)
            .await;
    }

    async fn clear(&self, account_alias: &CredentialAlias) {
        let _clear_result = self.clear_haider_code_plan_status(account_alias).await;
    }
}

pub(crate) fn cadence(snapshot: &HaiderCodePlanSnapshotV1) -> Duration {
    Duration::from_secs(
        snapshot
            .refresh_after_s
            .unwrap_or(PLAN_REFRESH_FALLBACK.as_secs())
            .max(PLAN_REFRESH_FLOOR.as_secs()),
    )
}

pub(crate) fn published_outcome(snapshot: HaiderCodePlanSnapshotV1) -> HaiderCodePlanOutcomeV1 {
    if snapshot.is_halted() {
        return HaiderCodePlanOutcomeV1::Halted { snapshot };
    }
    if matches!(
        snapshot
            .weekly_allowance
            .as_ref()
            .and_then(|allowance| allowance.state.as_ref()),
        Some(HaiderCodeAllowanceStateV1::Ok)
    ) {
        HaiderCodePlanOutcomeV1::Available { snapshot }
    } else {
        HaiderCodePlanOutcomeV1::Indeterminate { snapshot }
    }
}

fn same_account(left: &ActivePlanAccount, right: &ActivePlanAccount) -> bool {
    left.descriptor == right.descriptor
}

async fn wait_until_due_or_changed(
    deadline: Option<Instant>,
    stop: &mut watch::Receiver<bool>,
    accounts: &mut watch::Receiver<u64>,
    interest: &mut watch::Receiver<u64>,
) -> bool {
    match deadline {
        Some(deadline) => tokio::select! {
            _ = stop.changed() => false,
            _ = accounts.changed() => true,
            _ = interest.changed() => true,
            _ = tokio::time::sleep_until(deadline) => true,
        },
        None => tokio::select! {
            _ = stop.changed() => false,
            _ = accounts.changed() => true,
            _ = interest.changed() => true,
        },
    }
}

pub(crate) async fn run_plan_poller(
    account_source: Arc<dyn PlanAccountSource>,
    credential_source: Arc<dyn PlanCredentialSource>,
    interest_source: Arc<dyn PlanInterestSource>,
    http: Arc<dyn HaiderCodePlanHttp>,
    mut stop: watch::Receiver<bool>,
) {
    let mut account_changes = account_source.subscribe();
    let mut interest_changes = interest_source.subscribe();
    let mut deadline = None;
    let mut observed_account: Option<CredentialDescriptor> = None;
    let mut cached_publication: Option<(CredentialAlias, HaiderCodePlanOutcomeV1)> = None;

    loop {
        if *stop.borrow() {
            break;
        }

        let recipients = interest_source.recipients().await;
        if recipients.is_empty() {
            if !wait_until_due_or_changed(
                None,
                &mut stop,
                &mut account_changes,
                &mut interest_changes,
            )
            .await
            {
                break;
            }
            continue;
        }
        let Some(account) = account_source.active_account() else {
            if let Some(descriptor) = observed_account.as_ref() {
                interest_source.clear(&descriptor.alias).await;
            }
            observed_account = None;
            cached_publication = None;
            deadline = None;
            if !wait_until_due_or_changed(
                None,
                &mut stop,
                &mut account_changes,
                &mut interest_changes,
            )
            .await
            {
                break;
            }
            continue;
        };

        let account_changed = observed_account.as_ref() != Some(&account.descriptor);
        if account_changed {
            if let Some(descriptor) = observed_account.as_ref() {
                interest_source.clear(&descriptor.alias).await;
            }
            observed_account = Some(account.descriptor.clone());
            cached_publication = None;
            deadline = Some(Instant::now());
        }
        let due = deadline.is_some_and(|deadline| deadline <= Instant::now());
        if !due {
            if let Some((alias, outcome)) = cached_publication.as_ref()
                && *alias == account.descriptor.alias
            {
                interest_source.publish(&recipients, alias.clone(), outcome.clone());
            }
            if !wait_until_due_or_changed(
                deadline,
                &mut stop,
                &mut account_changes,
                &mut interest_changes,
            )
            .await
            {
                break;
            }
            continue;
        }

        let credential = match credential_source.resolve(&account.descriptor).await {
            Ok(credential) => credential,
            Err(_) => {
                deadline = Some(Instant::now() + PLAN_REFRESH_FALLBACK);
                if !wait_until_due_or_changed(
                    deadline,
                    &mut stop,
                    &mut account_changes,
                    &mut interest_changes,
                )
                .await
                {
                    break;
                }
                continue;
            }
        };
        let fetched = http.get_account(&credential).await;

        // Consume notifications covered by the authoritative reads below.
        // A change racing after these calls remains pending and wakes the
        // next wait, while a switch already observed during I/O cannot cause
        // a duplicate replay after its replacement response is published.
        account_changes.borrow_and_update();
        let Some(current) = account_source.active_account() else {
            interest_source.clear(&account.descriptor.alias).await;
            observed_account = None;
            cached_publication = None;
            deadline = None;
            continue;
        };
        if !same_account(&account, &current) {
            interest_source.clear(&account.descriptor.alias).await;
            observed_account = Some(current.descriptor);
            cached_publication = None;
            deadline = Some(Instant::now());
            continue;
        }
        interest_changes.borrow_and_update();
        let recipients = interest_source.recipients().await;

        match fetched {
            PlanFetchOutcome::Snapshot(reading) => {
                deadline = Some(Instant::now() + cadence(&reading.snapshot));
                interest_source
                    .capture(
                        account.descriptor.alias.clone(),
                        reading.snapshot.clone(),
                        reading.meter,
                    )
                    .await;
                let outcome = published_outcome(reading.snapshot);
                cached_publication = Some((account.descriptor.alias.clone(), outcome.clone()));
                if !recipients.is_empty() {
                    interest_source.publish(&recipients, account.descriptor.alias, outcome);
                }
            }
            PlanFetchOutcome::Unauthorized => {
                interest_source.clear(&account.descriptor.alias).await;
                cached_publication = Some((
                    account.descriptor.alias.clone(),
                    HaiderCodePlanOutcomeV1::Unauthorized,
                ));
                if !recipients.is_empty() {
                    interest_source.publish(
                        &recipients,
                        account.descriptor.alias,
                        HaiderCodePlanOutcomeV1::Unauthorized,
                    );
                }
                // The response carries no server cadence. Retry on the
                // documented fallback without letting interest wakes bypass
                // it; an active-account change still resolves immediately.
                deadline = Some(Instant::now() + PLAN_REFRESH_FALLBACK);
            }
            PlanFetchOutcome::Transient(_) => {
                // Transport failure is not provider account truth: publish
                // nothing, retaining the client's last typed snapshot.
                deadline = Some(Instant::now() + PLAN_REFRESH_FALLBACK);
            }
        }

        let wait_deadline = (!recipients.is_empty()).then_some(deadline).flatten();
        if !wait_until_due_or_changed(
            wait_deadline,
            &mut stop,
            &mut account_changes,
            &mut interest_changes,
        )
        .await
        {
            break;
        }
    }
}

pub(crate) struct HaiderCodePlanPoller {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl HaiderCodePlanPoller {
    pub(crate) fn start_production(
        hub: SessionHub,
        management: ManagementSnapshot,
        broker: CredentialBroker,
    ) -> Self {
        Self::start(
            Arc::new(management),
            Arc::new(broker),
            Arc::new(hub),
            Arc::new(ProductionHaiderCodePlanHttp::new()),
        )
    }

    pub(crate) fn start(
        account_source: Arc<dyn PlanAccountSource>,
        credential_source: Arc<dyn PlanCredentialSource>,
        interest_source: Arc<dyn PlanInterestSource>,
        http: Arc<dyn HaiderCodePlanHttp>,
    ) -> Self {
        let (stop, receiver) = watch::channel(false);
        let task = tokio::spawn(run_plan_poller(
            account_source,
            credential_source,
            interest_source,
            http,
            receiver,
        ));
        Self {
            stop,
            task: Some(task),
        }
    }

    pub(crate) async fn shutdown(&mut self) -> bool {
        self.stop.send_replace(true);
        let Some(task) = self.task.as_mut() else {
            return true;
        };
        let joined = tokio::select! {
            result = task => Some(result.is_ok()),
            _ = tokio::time::sleep(Duration::from_secs(1)) => None,
        };
        if let Some(joined) = joined {
            self.task.take();
            return joined;
        }
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        false
    }

    pub(crate) async fn abort_and_join(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for HaiderCodePlanPoller {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
