#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use haider_accounts::{MemoryVault, SecretHandle, Vault};
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    HaiderCodeAllowanceStateV1, HaiderCodeHoldV1, HaiderCodePlanOutcomeV1,
    HaiderCodePlanSnapshotV1, HaiderCodeWeeklyAllowanceV1,
};
use tokio::sync::{Notify, watch};

use super::haider_code_plan::{
    ActivePlanAccount, HaiderCodePlanHttp, PlanAccountSource, PlanCredentialSource,
    PlanFetchOutcome, PlanInterestSource, PlanMeterValues, PlanSnapshot, PlanTransientFailure,
    cadence, classify_account_response, published_outcome, run_plan_poller,
};

fn descriptor(alias: &str) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: "haider-code".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: format!("{alias} identity"),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
    }
}

struct FakeAccounts {
    active: Mutex<Option<ActivePlanAccount>>,
    changes: watch::Sender<u64>,
}

impl FakeAccounts {
    fn new(revision: u64, descriptor: CredentialDescriptor) -> Self {
        Self {
            active: Mutex::new(Some(ActivePlanAccount { descriptor })),
            changes: watch::Sender::new(revision),
        }
    }

    fn switch(&self, revision: u64, descriptor: CredentialDescriptor) {
        *self.active.lock().expect("account lock") = Some(ActivePlanAccount { descriptor });
        self.changes.send_replace(revision);
    }
}

impl PlanAccountSource for FakeAccounts {
    fn active_account(&self) -> Option<ActivePlanAccount> {
        self.active.lock().expect("account lock").clone()
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

struct FakeCredentials {
    vault: MemoryVault,
    resolutions: Mutex<Vec<CredentialAlias>>,
}

impl FakeCredentials {
    fn new(credentials: &[(&str, &[u8])]) -> Self {
        let vault = MemoryVault::new();
        for (alias, secret) in credentials {
            vault
                .put(&CredentialAlias::new(*alias), secret)
                .expect("seed credential");
        }
        Self {
            vault,
            resolutions: Mutex::new(Vec::new()),
        }
    }

    fn aliases(&self) -> Vec<CredentialAlias> {
        self.resolutions.lock().expect("resolution lock").clone()
    }
}

#[async_trait::async_trait]
impl PlanCredentialSource for FakeCredentials {
    async fn resolve(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, PlanTransientFailure> {
        self.resolutions
            .lock()
            .expect("resolution lock")
            .push(descriptor.alias.clone());
        self.vault
            .resolve(&descriptor.alias)
            .map_err(|_| PlanTransientFailure::CredentialUnavailable)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Publication {
    recipients: Vec<String>,
    alias: CredentialAlias,
    outcome: HaiderCodePlanOutcomeV1,
}

struct FakeInterest {
    recipients: Mutex<Vec<String>>,
    changes: watch::Sender<u64>,
    publications: Mutex<Vec<Publication>>,
    captures: Mutex<Vec<(CredentialAlias, HaiderCodePlanSnapshotV1, PlanMeterValues)>>,
}

impl FakeInterest {
    fn new(recipients: Vec<String>) -> Self {
        Self {
            recipients: Mutex::new(recipients),
            changes: watch::Sender::new(0),
            publications: Mutex::new(Vec::new()),
            captures: Mutex::new(Vec::new()),
        }
    }

    fn publications(&self) -> Vec<Publication> {
        self.publications.lock().expect("publication lock").clone()
    }

    fn captures(&self) -> Vec<(CredentialAlias, HaiderCodePlanSnapshotV1, PlanMeterValues)> {
        self.captures.lock().expect("capture lock").clone()
    }

    fn wake(&self) {
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

#[async_trait::async_trait]
impl PlanInterestSource for FakeInterest {
    async fn recipients(&self) -> Vec<String> {
        self.recipients.lock().expect("recipient lock").clone()
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn publish(
        &self,
        recipients: &[String],
        account_alias: CredentialAlias,
        outcome: HaiderCodePlanOutcomeV1,
    ) {
        self.publications
            .lock()
            .expect("publication lock")
            .push(Publication {
                recipients: recipients.to_vec(),
                alias: account_alias,
                outcome,
            });
    }

    async fn capture(
        &self,
        account_alias: CredentialAlias,
        snapshot: HaiderCodePlanSnapshotV1,
        meter: PlanMeterValues,
    ) {
        self.captures
            .lock()
            .expect("capture lock")
            .push((account_alias, snapshot, meter));
    }

    async fn clear(&self, _account_alias: &CredentialAlias) {}
}

struct FakeHttp {
    responses: Mutex<VecDeque<PlanFetchOutcome>>,
    calls: AtomicUsize,
    secrets: Mutex<Vec<Vec<u8>>>,
    block_first: bool,
    first_started: Notify,
    release_first: Notify,
}

impl FakeHttp {
    fn new(responses: impl IntoIterator<Item = PlanFetchOutcome>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: AtomicUsize::new(0),
            secrets: Mutex::new(Vec::new()),
            block_first: false,
            first_started: Notify::new(),
            release_first: Notify::new(),
        }
    }

    fn blocking_first(responses: impl IntoIterator<Item = PlanFetchOutcome>) -> Self {
        Self {
            block_first: true,
            ..Self::new(responses)
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl HaiderCodePlanHttp for FakeHttp {
    async fn get_account(&self, credential: &SecretHandle) -> PlanFetchOutcome {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.secrets
            .lock()
            .expect("secret record lock")
            .push(credential.expose_secret().to_vec());
        if call == 0 && self.block_first {
            self.first_started.notify_one();
            self.release_first.notified().await;
        }
        self.responses
            .lock()
            .expect("response lock")
            .pop_front()
            .unwrap_or(PlanFetchOutcome::Transient(PlanTransientFailure::Network))
    }
}

fn snapshot(refresh_after_s: Option<u64>) -> HaiderCodePlanSnapshotV1 {
    HaiderCodePlanSnapshotV1 {
        plan: Some("go".into()),
        plan_label: Some("Go".into()),
        weekly_allowance: Some(HaiderCodeWeeklyAllowanceV1 {
            percent_remaining: Some(100.0),
            state: Some(HaiderCodeAllowanceStateV1::Ok),
            resets_at_ms: None,
            grace_until_ms: None,
        }),
        usage_credits_usd: Some(0.0),
        auto_topup_enabled: Some(false),
        hold: None,
        models_live: Some(14),
        refresh_after_s,
        cached: Some(false),
    }
}

fn fetched_snapshot(refresh_after_s: Option<u64>) -> PlanFetchOutcome {
    PlanFetchOutcome::Snapshot(PlanSnapshot::without_meter(snapshot(refresh_after_s)))
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    for _ in 0..100 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(predicate(), "condition did not become true");
}

async fn start_fake_poller(
    accounts: Arc<FakeAccounts>,
    credentials: Arc<FakeCredentials>,
    interest: Arc<FakeInterest>,
    http: Arc<FakeHttp>,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let (stop, receiver) = watch::channel(false);
    let task = tokio::spawn(run_plan_poller(
        accounts,
        credentials,
        interest,
        http,
        receiver,
    ));
    (stop, task)
}

/// MUTATION CHECK: replace `snapshot.refresh_after_s` with the literal `60`
/// in `cadence`. Expected runtime failure: the second HTTP call is still
/// absent when the provider-published 23-second deadline arrives.
#[tokio::test(start_paused = true)]
async fn refresh_after_s_drives_the_next_poll() {
    let accounts = Arc::new(FakeAccounts::new(1, descriptor("first")));
    let credentials = Arc::new(FakeCredentials::new(&[("first", b"hk-first")]));
    let interest = Arc::new(FakeInterest::new(vec!["connection".into()]));
    let http = Arc::new(FakeHttp::new([
        fetched_snapshot(Some(23)),
        fetched_snapshot(Some(23)),
    ]));
    let (stop, task) = start_fake_poller(accounts, credentials, interest, Arc::clone(&http)).await;

    wait_for(|| http.call_count() == 1).await;
    tokio::time::advance(Duration::from_secs(22)).await;
    tokio::task::yield_now().await;
    assert_eq!(http.call_count(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for(|| http.call_count() == 2).await;

    stop.send_replace(true);
    task.await.expect("poller joins");
}

/// MUTATION CHECK: treat an interest/model wake as a new polling deadline.
/// Expected runtime failure: the second request fires at t+1 instead of
/// preserving the provider-published t+23 cadence.
#[tokio::test(start_paused = true)]
async fn interest_wakes_do_not_bypass_the_server_cadence() {
    let accounts = Arc::new(FakeAccounts::new(1, descriptor("first")));
    let credentials = Arc::new(FakeCredentials::new(&[("first", b"hk-first")]));
    let interest = Arc::new(FakeInterest::new(vec!["connection".into()]));
    let http = Arc::new(FakeHttp::new([
        fetched_snapshot(Some(23)),
        fetched_snapshot(Some(23)),
    ]));
    let (stop, task) = start_fake_poller(
        accounts,
        credentials,
        Arc::clone(&interest),
        Arc::clone(&http),
    )
    .await;

    wait_for(|| http.call_count() == 1).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    interest.wake();
    tokio::task::yield_now().await;
    assert_eq!(http.call_count(), 1);
    tokio::time::advance(Duration::from_secs(21)).await;
    tokio::task::yield_now().await;
    assert_eq!(http.call_count(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for(|| http.call_count() == 2).await;

    stop.send_replace(true);
    task.await.expect("poller joins");
}

/// MUTATION CHECK: remove the 15-second floor or change the absent-field
/// fallback from 60 seconds. Expected runtime failure: a hostile zero cadence
/// spins immediately or an older payload no longer polls once per minute.
#[test]
fn cadence_clamps_bad_values_and_defaults_only_when_absent() {
    assert_eq!(cadence(&snapshot(Some(0))), Duration::from_secs(15));
    assert_eq!(cadence(&snapshot(Some(37))), Duration::from_secs(37));
    assert_eq!(cadence(&snapshot(None)), Duration::from_secs(60));
}

/// MUTATION CHECK: put ledger capture in the client publication/replay path.
/// Expected runtime failure: waking interest replays the held frame and
/// increments captures from one to two without another provider arrival.
#[tokio::test]
async fn one_provider_arrival_is_captured_once_not_on_cached_replay() {
    let accounts = Arc::new(FakeAccounts::new(1, descriptor("first")));
    let credentials = Arc::new(FakeCredentials::new(&[("first", b"hk-first")]));
    let interest = Arc::new(FakeInterest::new(vec!["connection".into()]));
    let mut status = snapshot(Some(60));
    status
        .weekly_allowance
        .as_mut()
        .expect("weekly allowance")
        .percent_remaining = Some(61.0);
    let http = Arc::new(FakeHttp::new([PlanFetchOutcome::Snapshot(PlanSnapshot {
        snapshot: status,
        meter: PlanMeterValues {
            weekly_percent_remaining: Some(61),
            credits: None,
            hold: None,
        },
    })]));
    let (stop, task) = start_fake_poller(
        accounts,
        credentials,
        Arc::clone(&interest),
        Arc::clone(&http),
    )
    .await;

    wait_for(|| interest.captures().len() == 1 && interest.publications().len() == 1).await;
    interest.wake();
    wait_for(|| interest.publications().len() == 2).await;
    assert_eq!(http.call_count(), 1);
    let captures = interest.captures();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].0, CredentialAlias::new("first"));
    assert_eq!(captures[0].2.weekly_percent_remaining, Some(61));

    stop.send_replace(true);
    task.await.expect("poller joins");
}

/// MUTATION CHECK: deserialize an unknown allowance state as `Ok`. Expected
/// runtime failure: this assertion observes a fabricated healthy state for a
/// future provider value instead of preserving the unknown string.
#[test]
fn unknown_allowance_state_never_becomes_ok() {
    let parsed = haider_provider::parse_haider_code_account(
        br#"{"weekly_allowance":{"percent_remaining":99,"state":"warming"}}"#,
    )
    .expect("tolerant payload");
    assert_eq!(
        parsed
            .weekly_allowance
            .as_ref()
            .and_then(|allowance| allowance.state.clone()),
        Some(HaiderCodeAllowanceStateV1::Unknown("warming".into()))
    );
    assert!(matches!(
        published_outcome(parsed),
        HaiderCodePlanOutcomeV1::Indeterminate { .. }
    ));
}

/// MUTATION CHECK: add `deny_unknown_fields` or make any optional account
/// field required. Expected runtime failure: this forward-compatible payload
/// is rejected instead of retaining honest `None` values.
#[test]
fn payload_ignores_unknown_fields_and_defaults_absent_optionals() {
    let parsed = haider_provider::parse_haider_code_account(
        br#"{
          "plan":"future-plan",
          "weekly_allowance":{"state":"future-state","server_note":"new"},
          "new_top_level":{"nested":true}
        }"#,
    )
    .expect("forward-compatible payload");
    assert_eq!(parsed.plan.as_deref(), Some("future-plan"));
    assert_eq!(parsed.refresh_after_s, None);
    assert_eq!(parsed.hold, None);
    assert_eq!(parsed.usage_credits_usd, None);
}

/// MUTATION CHECK: reconstruct basis points from the public `f64` plan DTO
/// instead of retaining the source JSON-number kind. Expected runtime
/// failure: the decimal token incorrectly gains integer meter provenance.
#[test]
fn meter_provenance_accepts_only_provider_integer_tokens() {
    let integer = classify_account_response(
        200,
        br#"{"weekly_allowance":{"percent_remaining":61},"usage_credits_usd":9,"hold":{"api_locked":false}}"#,
    );
    let PlanFetchOutcome::Snapshot(integer) = integer else {
        panic!("integer plan snapshot");
    };
    assert_eq!(integer.meter.weekly_percent_remaining, Some(61));
    assert_eq!(integer.meter.credits, Some(9));
    assert_eq!(integer.meter.hold, None);
    assert_eq!(
        integer
            .snapshot
            .hold
            .as_ref()
            .and_then(|hold| hold.api_locked),
        Some(false),
        "structured account hold state remains in plan status"
    );

    let decimal = classify_account_response(
        200,
        br#"{"weekly_allowance":{"percent_remaining":61.0},"usage_credits_usd":9.0}"#,
    );
    let PlanFetchOutcome::Snapshot(decimal) = decimal else {
        panic!("decimal plan snapshot");
    };
    assert_eq!(
        decimal
            .snapshot
            .weekly_allowance
            .as_ref()
            .and_then(|allowance| allowance.percent_remaining),
        Some(61.0),
        "the plan-status display still preserves the provider float"
    );
    assert_eq!(decimal.meter.weekly_percent_remaining, None);
    assert_eq!(decimal.meter.credits, None);
}

/// MUTATION CHECK: classify a literal HTTP 401 as transport failure or a
/// generic HTTP status. Expected runtime failure: the provider-authenticated
/// response no longer becomes the distinct actionable unauthorized outcome.
#[test]
fn literal_http_401_is_the_only_unauthorized_status_classification() {
    assert_eq!(
        classify_account_response(401, b"ignored"),
        PlanFetchOutcome::Unauthorized
    );
    assert_eq!(
        classify_account_response(403, b"ignored"),
        PlanFetchOutcome::Transient(PlanTransientFailure::HttpStatus(403))
    );
}

async fn publications_for(outcome: PlanFetchOutcome) -> Vec<Publication> {
    let accounts = Arc::new(FakeAccounts::new(1, descriptor("first")));
    let credentials = Arc::new(FakeCredentials::new(&[("first", b"hk-first")]));
    let interest = Arc::new(FakeInterest::new(vec!["connection".into()]));
    let http = Arc::new(FakeHttp::new([outcome]));
    let (stop, task) = start_fake_poller(
        accounts,
        credentials,
        Arc::clone(&interest),
        Arc::clone(&http),
    )
    .await;
    wait_for(|| http.call_count() == 1).await;
    tokio::task::yield_now().await;
    stop.send_replace(true);
    task.await.expect("poller joins");
    interest.publications()
}

/// MUTATION CHECK: collapse `Unauthorized`, provider hold, and `Network` into
/// one plan-check failure. Expected runtime failure: 401 loses its typed
/// unauthorized frame, hold loses its reason-bearing halted snapshot, or a
/// network outage incorrectly publishes account state.
#[tokio::test]
async fn unauthorized_hold_and_network_are_three_distinct_outcomes() {
    let unauthorized = publications_for(PlanFetchOutcome::Unauthorized).await;
    assert!(matches!(
        unauthorized.as_slice(),
        [Publication {
            outcome: HaiderCodePlanOutcomeV1::Unauthorized,
            ..
        }]
    ));

    let mut held = snapshot(Some(60));
    held.hold = Some(HaiderCodeHoldV1 {
        api_locked: Some(true),
        subscribe_banned: Some(false),
        reason: Some("verify billing".into()),
    });
    let halted = publications_for(PlanFetchOutcome::Snapshot(PlanSnapshot::without_meter(
        held,
    )))
    .await;
    assert!(matches!(
        halted.as_slice(),
        [Publication {
            outcome: HaiderCodePlanOutcomeV1::Halted { snapshot },
            ..
        }] if snapshot.hold.as_ref().and_then(|hold| hold.reason.as_deref()) == Some("verify billing")
    ));

    let network =
        publications_for(PlanFetchOutcome::Transient(PlanTransientFailure::Network)).await;
    assert!(network.is_empty());
}

/// MUTATION CHECK: let an attachment/model wake reset the 401 fallback
/// deadline to now. Expected runtime failure: the second request fires before
/// the 60-second field-absent fallback expires.
#[tokio::test(start_paused = true)]
async fn unauthorized_is_replayed_without_bypassing_fallback_cadence() {
    let accounts = Arc::new(FakeAccounts::new(1, descriptor("first")));
    let credentials = Arc::new(FakeCredentials::new(&[("first", b"hk-first")]));
    let interest = Arc::new(FakeInterest::new(vec!["connection".into()]));
    let http = Arc::new(FakeHttp::new([
        PlanFetchOutcome::Unauthorized,
        PlanFetchOutcome::Unauthorized,
    ]));
    let (stop, task) = start_fake_poller(
        accounts,
        credentials,
        Arc::clone(&interest),
        Arc::clone(&http),
    )
    .await;

    wait_for(|| http.call_count() == 1 && interest.publications().len() == 1).await;
    interest.wake();
    wait_for(|| interest.publications().len() == 2).await;
    tokio::time::advance(Duration::from_secs(59)).await;
    tokio::task::yield_now().await;
    assert_eq!(http.call_count(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for(|| http.call_count() == 2).await;
    assert!(
        interest.publications().iter().all(|publication| matches!(
            publication.outcome,
            HaiderCodePlanOutcomeV1::Unauthorized
        ))
    );

    stop.send_replace(true);
    task.await.expect("poller joins");
}

/// MUTATION CHECK: retain the first resolved `SecretHandle` across polls or
/// remove the post-fetch revision fence. Expected runtime failure: the old
/// key is used twice or the old account's response is published after the
/// active alias switches.
#[tokio::test]
async fn poller_follows_an_account_switch_during_a_request() {
    let accounts = Arc::new(FakeAccounts::new(1, descriptor("old")));
    let credentials = Arc::new(FakeCredentials::new(&[
        ("old", b"hk-old"),
        ("new", b"hk-new"),
    ]));
    let interest = Arc::new(FakeInterest::new(vec!["connection".into()]));
    let http = Arc::new(FakeHttp::blocking_first([
        fetched_snapshot(Some(60)),
        fetched_snapshot(Some(60)),
    ]));
    let (stop, task) = start_fake_poller(
        Arc::clone(&accounts),
        Arc::clone(&credentials),
        Arc::clone(&interest),
        Arc::clone(&http),
    )
    .await;

    http.first_started.notified().await;
    accounts.switch(2, descriptor("new"));
    http.release_first.notify_one();
    wait_for(|| http.call_count() == 2 && interest.publications().len() == 1).await;

    assert_eq!(
        credentials.aliases(),
        vec![CredentialAlias::new("old"), CredentialAlias::new("new")]
    );
    let publications = interest.publications();
    assert_eq!(publications[0].alias, CredentialAlias::new("new"));

    stop.send_replace(true);
    task.await.expect("poller joins");
}

/// MUTATION CHECK: delete the empty-recipient gate before credential
/// resolution. Expected runtime failure: an idle daemon performs an HTTP
/// account request despite having no active Haider Code session.
#[tokio::test(start_paused = true)]
async fn no_active_provider_session_means_no_network_request() {
    let accounts = Arc::new(FakeAccounts::new(1, descriptor("first")));
    let credentials = Arc::new(FakeCredentials::new(&[("first", b"hk-first")]));
    let interest = Arc::new(FakeInterest::new(Vec::new()));
    let http = Arc::new(FakeHttp::new([fetched_snapshot(Some(15))]));
    let (stop, task) = start_fake_poller(accounts, credentials, interest, Arc::clone(&http)).await;

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::task::yield_now().await;
    assert_eq!(http.call_count(), 0);

    stop.send_replace(true);
    task.await.expect("poller joins");
}

/// MUTATION CHECK: remove the provider predicate from the real SessionHub
/// interest adapter. Expected runtime failure: attaching only the OpenAI
/// session produces a recipient and would enable Haider Code network polls.
#[tokio::test]
async fn real_session_hub_interest_requires_an_attached_haider_code_session() {
    use haider_core::{SessionCreateCommand, SqliteStoreHandle};
    use haider_protocol::ids::{DeviceId, EventId, SessionId};
    use haider_rpc::{AttachMode, Capability, RequestBody, RequestId, WireFrame};

    use crate::accounts::ConnectionTransport;
    use crate::session_hub::{FrameSendError, FrameSink, SessionHub, SessionHubConfig};

    #[derive(Default)]
    struct DiscardSink;

    impl FrameSink for DiscardSink {
        fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
            Ok(())
        }
    }

    fn create_command(session_id: &SessionId, provider: &str) -> SessionCreateCommand {
        SessionCreateCommand {
            command_id: format!("create-interest-{provider}"),
            request_digest: format!("digest-interest-{provider}"),
            request_json: format!(r#"{{"provider":"{provider}"}}"#),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: provider.into(),
            model: "test-model".into(),
            max_tokens: 4_096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "haider-code-interest-test-v1".into(),
            event_id: EventId::new(format!("created-interest-{provider}")),
            device_id: DeviceId::new("haider-code-interest-test"),
        }
    }

    let root = tempfile::tempdir().expect("temporary store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let openai_session = SessionId::new("interest-openai");
    let haider_code_session = SessionId::new("interest-haider-code");
    hub.create_internal_session(create_command(&openai_session, "openai"))
        .await
        .expect("create OpenAI session");
    hub.create_internal_session(create_command(&haider_code_session, "haider-code"))
        .await
        .expect("create Haider Code session");

    assert!(PlanInterestSource::recipients(&hub).await.is_empty());

    let capabilities = std::collections::BTreeSet::from([Capability::View, Capability::Control]);
    let openai_connection = hub
        .open_connection(
            capabilities.clone(),
            Arc::new(DiscardSink),
            ConnectionTransport::LocalSameUid,
        )
        .expect("OpenAI connection");
    openai_connection
        .request(
            RequestId::new("attach-interest-openai"),
            RequestBody::SessionAttach {
                session_id: openai_session,
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach OpenAI session");
    assert!(PlanInterestSource::recipients(&hub).await.is_empty());

    let haider_code_connection = hub
        .open_connection(
            capabilities,
            Arc::new(DiscardSink),
            ConnectionTransport::LocalSameUid,
        )
        .expect("Haider Code connection");
    haider_code_connection
        .request(
            RequestId::new("attach-interest-haider-code"),
            RequestBody::SessionAttach {
                session_id: haider_code_session,
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach Haider Code session");
    assert_eq!(PlanInterestSource::recipients(&hub).await.len(), 1);

    drop(haider_code_connection);
    drop(openai_connection);
    hub.shutdown().await.expect("hub shuts down");
    store.close().await.expect("store closes");
}
