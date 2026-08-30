#![allow(clippy::expect_used)]

//! Response-open deadline regression through the production daemon and
//! headless client reducer. The injected provider never returns a stream.

mod support;

use async_trait::async_trait;
use haider_client::{
    DaemonLifetime, EnsureOptions, HeadlessEvent, HeadlessFailureCode, HeadlessOutcome,
    HeadlessRunRequest, ResolvedProfile, required_headless_features, run_headless,
};
use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ProviderFactoryConfig, ResolvedTurnProvider,
};
use haider_protocol::error::{ErrorAction, ErrorCode, HaiderError};
use haider_protocol::provider::CapabilityDoc;
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_provider::{FakeProvider, Provider, ProviderError, ProviderStream, TurnRequest};
use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::{ready_with_dependencies, test_root};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

struct NeverOpensProvider {
    capabilities: FakeProvider,
    requests: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
}

struct InFlightRequest(Arc<AtomicUsize>);

impl Drop for InFlightRequest {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Provider for NeverOpensProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.capabilities.capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let _guard = InFlightRequest(Arc::clone(&self.in_flight));
        std::future::pending().await
    }
}

#[derive(Clone)]
struct NeverOpensFactory {
    provider: Arc<NeverOpensProvider>,
}

#[async_trait]
impl ProviderFactory for NeverOpensFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.provider.clone(),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

/// Gate regression: the real headless client receives a structured provider
/// timeout and an Errored terminal before its three-second deadline. Dropping
/// the bounded open future must release the only provider request guard.
#[tokio::test]
async fn never_opening_provider_is_structured_terminal_before_client_deadline() {
    let root = test_root("provider-deadline-");
    let store = root.path().join("store");
    let runtime = root.path().join("runtime");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&store).expect("store dir");
    fs::create_dir_all(&runtime).expect("runtime dir");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let store = fs::canonicalize(store).expect("canonical store");
    let runtime = fs::canonicalize(runtime).expect("canonical runtime");
    let workspace = fs::canonicalize(workspace).expect("canonical workspace");

    let requests = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(NeverOpensProvider {
        capabilities: FakeProvider::new(Vec::new()),
        requests: Arc::clone(&requests),
        in_flight: Arc::clone(&in_flight),
    });
    let config = DaemonConfig::new("provider-deadline-profile", &store, &runtime);
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(NeverOpensFactory { provider }),
            providers: BTreeSet::from(["fake".to_owned()]),
        },
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies).await;
    let profile = ResolvedProfile {
        profile_id: config.profile_id.clone(),
        store_dir: store,
        runtime_dir: runtime,
        endpoint_path: config.endpoint_path(),
        default_provider: "fake".into(),
        default_model: "fake-model".into(),
        default_max_tokens: 4_096,
    };
    let ensure = EnsureOptions {
        required_features: required_headless_features(SessionPermissionOverridesV1::default()),
        startup_deadline: Duration::from_secs(5),
        daemon_binary: None,
        client: haider_client::ClientConfig::default(),
        daemon_lifetime: DaemonLifetime::Persistent,
    };
    let (output, mut events) = mpsc::channel::<HeadlessEvent>(64);
    let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
    let started = Instant::now();
    let result = timeout(
        Duration::from_millis(2_900),
        run_headless(
            &profile,
            ensure,
            HeadlessRunRequest {
                cwd: workspace.to_string_lossy().into_owned(),
                prompt: "never open".into(),
                attachments: Vec::new(),
                durable_attachments: Vec::new(),
                provider: Some("fake".into()),
                model: Some("fake-model".into()),
                max_tokens: 64,
                budget: Default::default(),
                seed: None,
                replay_of: None,
                journal_pin: true,
                detached: false,
                permission_overrides: SessionPermissionOverridesV1::default(),
                trust_hooks: false,
                timeout: Some(Duration::from_secs(3)),
                terminal_grace: Duration::from_secs(1),
            },
            output,
        ),
    )
    .await
    .expect("headless client exits before its three-second deadline")
    .expect("deadline failure is a structured run result");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(result.outcome, HeadlessOutcome::Errored);
    assert!(result.terminal_seq.is_some(), "the client reduced Errored");
    let failure = result.failure.expect("typed run failure");
    assert_eq!(
        failure.code,
        HeadlessFailureCode::Run(ErrorCode::ProviderTimeout)
    );
    assert!(!failure.retryable, "no full retry fits before the deadline");
    assert!(failure.message.contains("reason=deadline_exhausted"));
    let presentation = failure.presentation.expect("provider timeout presentation");
    assert_eq!(presentation.subcode.as_str(), "provider-timeout");
    assert_eq!(presentation.allowed_actions, vec![ErrorAction::None]);
    drain.await.expect("headless event drain");

    timeout(Duration::from_millis(250), async {
        while in_flight.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed-out provider request is not orphaned");
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
