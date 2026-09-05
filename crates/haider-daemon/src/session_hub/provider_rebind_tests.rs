#![allow(clippy::expect_used)]

use super::*;
use crate::auto_hermetic::ProviderLockdownPolicy;

#[derive(Default)]
struct RebindCaptureSink(std::sync::Mutex<Vec<WireFrame>>);

impl FrameSink for RebindCaptureSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("capture frame").push(frame);
        Ok(())
    }
}

fn rebind_policy_profile(
    provider: &str,
    policy: ProviderLockdownPolicy,
) -> haider_rpc::ProviderSummaryWire {
    serde_json::from_value(serde_json::json!({
        "provider": provider,
        "api_family": "openai_chat_completions",
        "endpoint": "http://127.0.0.1:31701/v1",
        "models": ["test-model"],
        "default_model": "test-model",
        "auth_methods": ["api_key"],
        "availability": "available",
        "enabled": true,
        "trust": if policy.is_lockdown() {"lockdown"} else {"full"},
    }))
    .expect("provider profile")
}

/// A held nonterminal turn retains its actual bound policy after ordinary
/// select_model changes durable metadata. Using the latter permits both
/// trust widening and a cross-provider sandbox switch without a new turn.
#[tokio::test]
async fn provider_rebind_guard_uses_frozen_binding_after_nonterminal_model_selection() {
    for (index, frozen_policy, selected_policy) in [
        (
            1,
            ProviderLockdownPolicy::Full,
            ProviderLockdownPolicy::Configured,
        ),
        (
            2,
            ProviderLockdownPolicy::Configured,
            ProviderLockdownPolicy::Full,
        ),
        (
            3,
            ProviderLockdownPolicy::Configured,
            ProviderLockdownPolicy::Configured,
        ),
    ] {
        let root = tempfile::tempdir().expect("store root");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session_id = SessionId::new(format!("rebind-frozen-{index}"));
        let run_id = RunId::new(format!("held-run-{index}"));
        let device_id = DeviceId::new("rebind-policy-test");
        hub.create_internal_session(haider_core::SessionCreateCommand {
            command_id: "create".into(),
            request_digest: "create-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "executing-provider".into(),
            model: "test-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-system".into(),
            event_id: EventId::new("created"),
            device_id: device_id.clone(),
        })
        .await
        .expect("session");
        hub.accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: "accept".into(),
            request_digest: "accept-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "held active turn".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("queued"),
            user_event_id: EventId::new("user"),
            active_event_id: EventId::new("active"),
            device_id: device_id.clone(),
        })
        .await
        .expect("held turn");
        hub.bind_lockdown_turn(&session_id, &run_id, "executing-provider", frozen_policy)
            .expect("freeze policy");
        hub.activate_lockdown_turn(&session_id, &run_id)
            .expect("activate bound policy");
        hub.select_session_model(haider_store::SessionSelectModelCommand {
            command_id: "select".into(),
            request_digest: "select-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: "pending-provider".into(),
            model: "test-model".into(),
            expected_pair: None,
            event_id: EventId::new("selected"),
            device_id,
        })
        .await
        .expect("pending ordinary selection");
        let selection = hub.lock_workflow_selection(&session_id).await;
        assert!(
            hub.session_has_nonterminal_runs(&session_id)
                .await
                .expect("active run")
        );
        let pending = hub
            .session_metadata(&session_id)
            .await
            .expect("metadata")
            .expect("typed");
        assert_eq!(pending.provider, "pending-provider");
        let (frozen_provider, actual_policy) = hub
            .bound_session_lockdown(&session_id)
            .expect("bound policy")
            .expect("binding");
        assert_eq!(frozen_provider, "executing-provider");
        assert_eq!(actual_policy, frozen_policy);
        assert!(!rebind_matches_frozen_policy(
            &frozen_provider,
            actual_policy,
            "pending-provider",
            selected_policy
        ));
        assert!(
            rebind_matches_frozen_policy(
                &pending.provider,
                selected_policy,
                "pending-provider",
                selected_policy
            ),
            "the mutable-metadata comparison would incorrectly admit this rebind"
        );
        drop(selection);
        let descriptor = haider_protocol::credential::CredentialDescriptor {
            alias: haider_protocol::ids::CredentialAlias::new("pending-account"),
            provider: "pending-provider".into(),
            base_url: None,
            auth_method: haider_protocol::credential::AuthMethod::ApiKey,
            identity: "test account".into(),
            status: haider_protocol::credential::CredentialStatus::Ok,
            active: true,
            label: None,
            account_identity: None,
            created_at_ms: None,
        };
        hub.install_accounts(crate::accounts::AccountsFacade {
            login: None,
            oauth: None,
            snapshot: Arc::new(std::sync::Mutex::new(vec![descriptor.clone()])),
            management: crate::accounts::ManagementSnapshot::new(
                1,
                vec![descriptor],
                vec![
                    rebind_policy_profile("executing-provider", frozen_policy),
                    rebind_policy_profile("pending-provider", selected_policy),
                ],
            ),
            vault_supported: false,
            discovery_disabled: true,
            device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
            sources: Arc::new(std::sync::Mutex::new(Vec::new())),
            vault: None,
        })
        .expect("install registry");
        hub.install_creatable_providers(std::collections::BTreeSet::from([
            "executing-provider".into(),
            "pending-provider".into(),
        ]))
        .expect("provider authority");
        let sink = Arc::new(RebindCaptureSink::default());
        let connection = hub
            .open_connection(
                std::collections::BTreeSet::from([
                    haider_rpc::Capability::View,
                    haider_rpc::Capability::Control,
                ]),
                sink.clone(),
                crate::accounts::ConnectionTransport::LocalSameUid,
            )
            .expect("connection");
        connection
            .request(
                RequestId::new("attach"),
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: haider_rpc::AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await
            .expect("control attachment");
        let head = store
            .latest_seq(&session_id)
            .await
            .expect("head before refusal");
        // Exercise the production request dispatcher and RPC handler; this
        // fails if the handler passes mutable metadata to the safe predicate.
        connection
            .request(
                RequestId::new("rebind-frozen"),
                RequestBody::SessionProviderRebind {
                    command_id: haider_rpc::CommandId::new("rebind-command"),
                    session_id: session_id.clone(),
                    worker_generation: store.worker_generation(),
                    provider: "pending-provider".into(),
                    base_url: None,
                    account: Some("pending-account".into()),
                },
            )
            .await
            .expect("rebind request handled");
        let responses = sink
            .0
            .lock()
            .expect("frames")
            .iter()
            .filter_map(|frame| match frame {
                WireFrame::Response { request_id, body }
                    if request_id.as_str() == "rebind-frozen" =>
                {
                    Some(body.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(responses.as_slice(), [ResponseBody::Error { code, retryable: true, .. }]
            if code == haider_rpc::ERROR_CODE_BUSY),
            "expected frozen-policy Busy, got {responses:?}"
        );
        assert_eq!(
            store
                .latest_seq(&session_id)
                .await
                .expect("head after refusal"),
            head
        );
        assert_eq!(
            hub.session_metadata(&session_id)
                .await
                .expect("metadata after refusal")
                .expect("typed")
                .provider_rebind_id,
            None
        );
        drop(connection);
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
}

#[test]
fn provider_rebind_frozen_policy_compares_actual_permission_bits() {
    assert!(rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::Full,
        "b",
        ProviderLockdownPolicy::AutoHermeticDisabled
    ));
    assert!(rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::Configured,
        "a",
        ProviderLockdownPolicy::Configured
    ));
    assert!(!rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::AutoHermetic,
        "a",
        ProviderLockdownPolicy::Configured
    ));
    assert!(!rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::Configured,
        "b",
        ProviderLockdownPolicy::Configured
    ));
}
