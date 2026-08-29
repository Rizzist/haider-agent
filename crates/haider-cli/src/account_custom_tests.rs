#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::future::Future;
use std::process::ExitCode;
use std::sync::Mutex;

use haider_client::ClientError;
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_rpc::{
    ErrorData, ProviderApiFamilyWire, ProviderAvailabilityWire, ProviderProbeFailureWire,
    ProviderSummaryWire, RequestBody, ResponseBody, SnapshotAvailabilityWire,
};

use super::{
    AccountClient, AccountCommand, AccountError, SecretInput, custom_document, execute,
    parse_account_command,
};

struct FakeClient {
    requests: Mutex<Vec<RequestBody>>,
    responses: Mutex<VecDeque<ResponseBody>>,
}

impl FakeClient {
    fn new(responses: impl IntoIterator<Item = ResponseBody>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn requests(&self) -> Vec<RequestBody> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl AccountClient for FakeClient {
    fn request(
        &self,
        request: RequestBody,
    ) -> impl Future<Output = Result<ResponseBody, ClientError>> + Send {
        self.requests.lock().expect("request lock").push(request);
        std::future::ready(Ok(self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .expect("fake response")))
    }
}

fn provider() -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: "router".into(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:8080".into()),
        response_open_timeout_ms: Some(45_000),
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: vec!["alpha".into(), "beta".into()],
        model_details: Vec::new(),
        inventory_fetched_at_ms: Some(1),
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Advisory,
        auth_methods: vec![AuthMethod::ApiKey],
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("alpha".into()),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

fn descriptor() -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new("router"),
        provider: "router".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "router api key".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

#[test]
fn add_parser_accepts_every_secret_source_and_redacts_inline_keys() {
    let command = parse_account_command(&[
        "add".into(),
        "router".into(),
        "--base-url".into(),
        "http://127.0.0.1:8080".into(),
        "--api-key".into(),
        "never-print-this".into(),
        "--api-family".into(),
        "anthropic".into(),
        "--response-open-timeout".into(),
        "45s".into(),
        "--chunk-idle-timeout".into(),
        "90s".into(),
        "--semantic-progress-timeout".into(),
        "5m".into(),
        "--json".into(),
    ])
    .expect("valid add");
    let debug = format!("{command:?}");
    assert!(!debug.contains("never-print-this"));
    assert!(debug.contains("[REDACTED]"));
    assert!(matches!(
        command,
        AccountCommand::Add(super::CustomAccountOptions {
            secret: Some(SecretInput::Direct(_)),
            api_family: Some(ProviderApiFamilyWire::AnthropicMessages),
            response_open_timeout_ms: Some(45_000),
            chunk_idle_timeout_ms: Some(90_000),
            semantic_progress_timeout_ms: Some(300_000),
            json: true,
            ..
        })
    ));
}

#[test]
fn parser_requires_one_explicit_auth_choice_on_add() {
    let error = parse_account_command(&[
        "add".into(),
        "router".into(),
        "--base-url".into(),
        "http://127.0.0.1:8080".into(),
    ])
    .expect_err("missing auth is refused");
    assert!(error.contains("API-key source or --no-auth"));
}

#[test]
fn update_accepts_auth_mode_changes_but_rejects_api_family_changes() {
    let no_auth = parse_account_command(&["update".into(), "router".into(), "--no-auth".into()])
        .expect("auth mode is mutable in place");
    assert!(matches!(
        no_auth,
        AccountCommand::Update(super::CustomAccountOptions {
            secret: Some(SecretInput::NoAuth),
            ..
        })
    ));

    let family = parse_account_command(&[
        "update".into(),
        "router".into(),
        "--base-url".into(),
        "https://router.example.test".into(),
        "--api-family".into(),
        "anthropic".into(),
    ])
    .expect_err("API family is immutable");
    assert!(family.contains("does not change --api-family"));
}

/// MUTATION CHECK: dropping `probe_vault_reference`, consuming the stage at
/// configure time, or skipping login changes this exact request sequence.
#[tokio::test]
async fn keyed_add_reuses_one_stage_for_discovery_and_login() {
    let client = FakeClient::new([
        ResponseBody::ProviderList {
            providers: Vec::new(),
            revision: 8,
            availability: Some(SnapshotAvailabilityWire::Available),
        },
        ResponseBody::VaultStage {
            stage_id: "stage".into(),
            vault_reference: "vaultref-one".into(),
            expires_at_ms: 99,
        },
        ResponseBody::ProviderConfigure {
            provider: provider(),
            revision: 9,
        },
        ResponseBody::AccountLoginApi {
            descriptor: descriptor(),
        },
    ]);
    let result = execute(
        &client,
        AccountCommand::Add(super::CustomAccountOptions {
            alias: "router".into(),
            base_url: Some("http://127.0.0.1:8080".into()),
            secret: Some(SecretInput::Direct(haider_rpc::SecretWire::new(
                "fixture-secret",
            ))),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            response_open_timeout_ms: Some(45_000),
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
            json: true,
        }),
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    let requests = client.requests();
    assert_eq!(requests.len(), 4);
    assert!(matches!(requests[0], RequestBody::ProviderList { .. }));
    assert!(matches!(requests[1], RequestBody::VaultStage { .. }));
    assert!(matches!(
        &requests[2],
        RequestBody::ProviderConfigure {
            models,
            probe_vault_reference: Some(reference),
            expected_revision: 8,
            ..
        } if models.is_empty() && reference == "vaultref-one"
    ));
    assert!(matches!(
        &requests[3],
        RequestBody::AccountLoginApi {
            vault_reference,
            replace_existing: false,
            ..
        } if vault_reference == "vaultref-one"
    ));
}

/// MUTATION CHECK: a no-auth add must go directly from inventory to
/// configure, carrying neither staged bytes nor an ephemeral vault reference.
#[tokio::test]
async fn no_auth_add_skips_vault_and_login_and_discovers_empty_input() {
    let mut configured = provider();
    configured.auth_methods.clear();
    let client = FakeClient::new([
        ResponseBody::ProviderList {
            providers: Vec::new(),
            revision: 3,
            availability: Some(SnapshotAvailabilityWire::Available),
        },
        ResponseBody::ProviderConfigure {
            provider: configured,
            revision: 4,
        },
    ]);
    let result = execute(
        &client,
        AccountCommand::Add(super::CustomAccountOptions {
            alias: "router".into(),
            base_url: Some("http://127.0.0.1:8080".into()),
            secret: Some(SecretInput::NoAuth),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
            json: true,
        }),
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    let requests = client.requests();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        &requests[1],
        RequestBody::ProviderConfigure {
            auth_requirement: Some(haider_rpc::ProviderAuthRequirementWire::None),
            models,
            probe_vault_reference: None,
            ..
        } if models.is_empty()
    ));
}

#[tokio::test]
async fn probe_refreshes_alias_and_reports_exact_namespaced_models() {
    let client = FakeClient::new([ResponseBody::ProviderModelsRefresh {
        provider: provider(),
        revision: 12,
    }]);
    let result = execute(
        &client,
        AccountCommand::Probe {
            alias: "router".into(),
            json: true,
        },
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    assert!(matches!(
        client.requests().as_slice(),
        [RequestBody::ProviderModelsRefresh { provider }] if provider == "router"
    ));

    let document = custom_document(
        "probe",
        "router",
        provider(),
        std::time::Duration::from_millis(17),
    );
    assert_eq!(document.operation, "probe");
    assert_eq!(document.auth_state, "authenticated");
    assert!(document.reachable);
    assert_eq!(document.latency_ms, 17);
    assert_eq!(document.model_count, 2);
    assert_eq!(
        document.models,
        vec!["router/alpha".to_owned(), "router/beta".to_owned()]
    );
}

#[tokio::test]
async fn probe_preserves_the_typed_failure_class() {
    let client = FakeClient::new([ResponseBody::Error {
        code: "provider_error".into(),
        message: "connection refused".into(),
        retryable: true,
        data: Some(ErrorData::ProviderProbeFailed {
            provider: "router".into(),
            failure: ProviderProbeFailureWire::Unreachable,
        }),
    }]);
    let error = execute(
        &client,
        AccountCommand::Probe {
            alias: "router".into(),
            json: true,
        },
    )
    .await
    .expect_err("probe failure");
    assert!(matches!(
        error,
        AccountError::Rpc {
            data: Some(ErrorData::ProviderProbeFailed {
                provider,
                failure: ProviderProbeFailureWire::Unreachable,
            }),
            ..
        } if provider == "router"
    ));
}

/// MUTATION CHECK: a replacement key explicitly selects API-key mode while
/// immutable family stays absent, then the staged key is consumed by login.
#[tokio::test]
async fn update_base_key_and_timeout_preserves_immutable_shape() {
    let mut updated = provider();
    updated.endpoint = Some("https://router.example.test/v1".into());
    updated.response_open_timeout_ms = Some(90_000);
    updated.chunk_idle_timeout_ms = Some(120_000);
    updated.semantic_progress_timeout_ms = Some(360_000);
    let client = FakeClient::new([
        ResponseBody::ProviderList {
            providers: vec![provider()],
            revision: 20,
            availability: Some(SnapshotAvailabilityWire::Available),
        },
        ResponseBody::VaultStage {
            stage_id: "update-stage".into(),
            vault_reference: "update-vault-ref".into(),
            expires_at_ms: 101,
        },
        ResponseBody::ProviderConfigure {
            provider: updated,
            revision: 21,
        },
        ResponseBody::AccountLoginApi {
            descriptor: descriptor(),
        },
    ]);
    let result = execute(
        &client,
        AccountCommand::Update(super::CustomAccountOptions {
            alias: "router".into(),
            base_url: Some("https://router.example.test/v1".into()),
            secret: Some(SecretInput::Direct(haider_rpc::SecretWire::new(
                "replacement-secret",
            ))),
            api_family: None,
            response_open_timeout_ms: Some(90_000),
            chunk_idle_timeout_ms: Some(120_000),
            semantic_progress_timeout_ms: Some(360_000),
            trust: None,
            json: true,
        }),
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    let requests = client.requests();
    assert_eq!(requests.len(), 4);
    assert!(matches!(
        &requests[2],
        RequestBody::ProviderConfigure {
            api_family: None,
            origin: Some(origin),
            auth_requirement: Some(haider_rpc::ProviderAuthRequirementWire::ApiKey),
            response_open_timeout_ms: Some(90_000),
            chunk_idle_timeout_ms: Some(120_000),
            semantic_progress_timeout_ms: Some(360_000),
            models,
            probe_vault_reference: Some(reference),
            expected_revision: 20,
            ..
        } if origin == "https://router.example.test/v1"
            && models.is_empty()
            && reference == "update-vault-ref"
    ));
    assert!(matches!(
        &requests[3],
        RequestBody::AccountLoginApi {
            provider,
            alias: Some(alias),
            vault_reference,
            replace_existing: true,
            ..
        } if provider == "router" && alias == "router" && vault_reference == "update-vault-ref"
    ));
}

/// MUTATION CHECK: switching to no-auth must update the provider first, then
/// remove the now-inapplicable vaulted credential at the returned revision.
#[tokio::test]
async fn update_to_no_auth_reuses_the_provider_and_removes_its_key() {
    let mut configured = provider();
    configured.auth_methods.clear();
    let client = FakeClient::new([
        ResponseBody::ProviderList {
            providers: vec![provider()],
            revision: 30,
            availability: Some(SnapshotAvailabilityWire::Available),
        },
        ResponseBody::ProviderConfigure {
            provider: configured,
            revision: 31,
        },
        ResponseBody::AccountList {
            descriptors: vec![descriptor()],
            revision: Some(31),
            provider_active: Vec::new(),
            provider_defaults: Vec::new(),
            availability: Some(SnapshotAvailabilityWire::Available),
        },
        ResponseBody::AccountRemove {
            removed_alias: haider_protocol::ids::CredentialAlias::new("router"),
            replacement_active_alias: None,
            revision: 32,
        },
    ]);
    let result = execute(
        &client,
        AccountCommand::Update(super::CustomAccountOptions {
            alias: "router".into(),
            base_url: None,
            secret: Some(SecretInput::NoAuth),
            api_family: None,
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
            json: true,
        }),
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    let requests = client.requests();
    assert_eq!(requests.len(), 4);
    assert!(matches!(
        &requests[1],
        RequestBody::ProviderConfigure {
            provider,
            auth_requirement: Some(haider_rpc::ProviderAuthRequirementWire::None),
            probe_vault_reference: None,
            expected_revision: 30,
            ..
        } if provider == "router"
    ));
    assert!(matches!(requests[2], RequestBody::AccountList { .. }));
    assert!(matches!(
        &requests[3],
        RequestBody::AccountRemove {
            alias,
            expected_revision: Some(31),
            ..
        } if alias == "router"
    ));
}
