#![allow(dead_code)]

use std::collections::BTreeSet;

use haider_protocol::DeliveryMode;
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::effect::EffectClass;
use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::CredentialAlias;
use haider_protocol::ids::{
    ArtifactRef, BranchId, DeviceId, EventId, ItemId, MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::tool::{
    DispatchMode, ToolInventoryEntry, ToolInventorySnapshot, ToolManifest, ToolPermissionDefault,
};
use haider_rpc::{
    AccountAddMethod, AttachMode, AttachState, AttachmentId, CancelStatus, Capability, ClientKind,
    CommandId, ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CURSOR_AHEAD,
    ERROR_CODE_PROVIDER_REMOVE_REFUSED, ERROR_CODE_REVISION_CONFLICT, ErrorData,
    FEATURE_ACCOUNT_LOGIN_API_V1, FEATURE_ACCOUNT_MANAGEMENT_V1, FEATURE_ACCOUNT_OAUTH_DEVICE_V1,
    FEATURE_ACCOUNT_OAUTH_PKCE_V1, FEATURE_ACCOUNT_ROTATION_V1, FEATURE_ARTIFACT_PUT_V1,
    FEATURE_BRANCH_CREATE_V1, FEATURE_PROVIDER_CONFIGURE_V1, FEATURE_PROVIDER_MANAGEMENT_V1,
    FEATURE_PROVIDER_MODELS_V1, FEATURE_PROVIDER_REMOVE_V1, FEATURE_SESSION_MUTATION_V1,
    FEATURE_TURN_CONTROL_V1, FEATURE_VAULT_STAGE_V1, Hello, LifecyclePhase, MenuInput,
    ModelDetailWire, OAuthAuthorizationWire, OAuthAvailabilityWire, OAuthFlowId,
    OAuthFlowStatusWire, OAuthReadyRefWire, ObserveRunStateWire, ProtocolError, ProviderActiveWire,
    ProviderApiFamilyWire, ProviderAuthRequirementWire, ProviderAvailabilityWire,
    ProviderDefaultWire, ProviderRemoveRefusalReasonWire, ProviderSummaryWire, RequestBody,
    RequestId, ResponseBody, SecretWire, SeqRange, SessionObserveDigest, SessionReadResult,
    SessionSummary, StagePurpose, SubmitDisposition, Welcome, WireFrame,
};

pub const TEST_FRAME_LIMIT: usize = 1024 * 1024;

pub fn capabilities(values: impl IntoIterator<Item = Capability>) -> BTreeSet<Capability> {
    values.into_iter().collect()
}

pub fn raw_envelope(seq: u64) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("ev-{seq}")),
        seq,
        session_id: SessionId::new("session-1"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("device-1"),
        authority_epoch: 3,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1_753_500_000_000 + seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::json!({
            "type": "future_event",
            "detail": "kept raw"
        }),
    }
}

pub fn transcript() -> Vec<WireFrame> {
    let session_id = SessionId::new("session-1");
    let attachment_id = AttachmentId::new("attachment-1");
    let range = SeqRange {
        start_seq: 5,
        end_seq: 9,
    };

    vec![
        WireFrame::Hello(Hello {
            protocol_min: 1,
            protocol_max: 2,
            client_name: "haider-gui".into(),
            client_version: "0.0.8".into(),
            client_instance_id: "client-instance-1".into(),
            client_kind: ClientKind::Gui,
            capabilities_requested: capabilities([Capability::View, Capability::Control]),
            max_receive_frame: TEST_FRAME_LIMIT as u32,
        }),
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-1".into(),
            daemon_generation: 4,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.8".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View]),
            features: BTreeSet::new(),
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-list"),
            body: RequestBody::SessionList {
                cursor: Some("cursor-after-session-0".into()),
                limit: 50,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-read"),
            body: RequestBody::SessionRead {
                session_id: session_id.clone(),
                range,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-attach"),
            body: RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 4,
                mode: AttachMode::View,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-detach"),
            body: RequestBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-list"),
            body: ResponseBody::SessionList {
                sessions: vec![SessionSummary {
                    session_id: session_id.clone(),
                    head_seq: 9,
                    worker_generation: 7,
                    metadata: None,
                }],
                next_cursor: Some("cursor-after-session-1".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-read"),
            body: ResponseBody::SessionRead {
                result: SessionReadResult {
                    session_id: session_id.clone(),
                    range,
                    head_seq: 9,
                    metadata: None,
                    latest_context_footprint: None,
                    envelopes: vec![raw_envelope(9)],
                },
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-attach"),
            body: ResponseBody::SessionAttach {
                attachment_id: attachment_id.clone(),
                attach_state: AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: 4,
                    replay_through_seq: 9,
                    worker_generation: 7,
                    authority_epoch: 3,
                },
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-detach"),
            body: ResponseBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-menu-success"),
            body: ResponseBody::MenuAnswer { resolution_seq: 10 },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-control"),
            body: ResponseBody::Error {
                code: ERROR_CODE_CAPABILITY_DENIED.into(),
                message: "control capability required".into(),
                retryable: false,
                data: None,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-attach-ahead"),
            body: ResponseBody::Error {
                code: ERROR_CODE_CURSOR_AHEAD.into(),
                message: "requested cursor is beyond the committed head".into(),
                retryable: true,
                data: Some(ErrorData::CursorAhead {
                    requested: 40,
                    head: 10,
                }),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-menu-lost"),
            body: ResponseBody::Error {
                code: ERROR_CODE_ALREADY_RESOLVED.into(),
                message: "an earlier answer won".into(),
                retryable: false,
                data: Some(ErrorData::AlreadyResolved { resolution_seq: 9 }),
            },
        },
        WireFrame::Event {
            attachment_id: attachment_id.clone(),
            session_id: session_id.clone(),
            envelope: raw_envelope(10),
        },
        WireFrame::AttachCaughtUp {
            attachment_id: attachment_id.clone(),
            high_water_seq: 10,
        },
        // Correlated form: carries a request_id so the daemon can answer a CAS
        // loser with a Response { already_resolved }.
        WireFrame::MenuAnswer {
            request_id: Some(RequestId::new("request-menu-1")),
            command_id: CommandId::new("command-1"),
            session_id: session_id.clone(),
            menu_id: MenuId::new("menu-1"),
            request_seq: 8,
            worker_generation: 7,
            option_key: "other".into(),
            option_index: 2,
            input: Some(MenuInput::Text {
                text: "custom answer".into(),
            }),
        },
        // Uncorrelated form: request_id omitted entirely (older/simpler
        // clients), so the field must stay off the wire when absent.
        WireFrame::MenuAnswer {
            request_id: None,
            command_id: CommandId::new("command-2"),
            session_id: SessionId::new("session-1"),
            menu_id: MenuId::new("menu-2"),
            request_seq: 9,
            worker_generation: 7,
            option_key: "submit_secret".into(),
            option_index: 0,
            input: Some(MenuInput::SecretVaultReference {
                vault_reference: "vault-ref-1".into(),
            }),
        },
        WireFrame::Lagged {
            attachment_id: attachment_id.clone(),
            last_queued_seq: 10,
        },
        WireFrame::ServerDraining {
            reason: "upgrade".into(),
            instance_id: "instance-1".into(),
            daemon_generation: 4,
            deadline_unix_ms: 1_753_500_030_000,
        },
        WireFrame::Ping { nonce: 99 },
        WireFrame::Pong { nonce: 99 },
        WireFrame::Request {
            request_id: RequestId::new("request-create"),
            body: RequestBody::SessionCreateWithPermissionOverrides {
                command_id: CommandId::new("command-create"),
                cwd: "/tmp/workspace".into(),
                provider: "anthropic".into(),
                model: "claude-test".into(),
                max_tokens: 4096,
                permission_overrides: None,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-create"),
            body: ResponseBody::SessionCreate {
                session_id: SessionId::new("session-created"),
                created_seq: 1,
                worker_generation: 7,
                metadata: SessionMetadataV1 {
                    cwd: "/tmp/workspace".into(),
                    provider: "anthropic".into(),
                    model: "claude-test".into(),
                    max_tokens: 4096,
                    permission_overrides: None,
                    system_prompt_version: None,
                    created_at_ms: 1_753_500_040_000,
                },
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-submit"),
            body: RequestBody::TurnSubmitWithBranch {
                command_id: CommandId::new("command-submit"),
                session_id: SessionId::new("session-created"),
                worker_generation: 7,
                branch_id: None,
                text: "hello".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-submit"),
            body: ResponseBody::TurnSubmit {
                session_id: SessionId::new("session-created"),
                run_id: RunId::new("run-1"),
                accepted_seq: 3,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-cancel"),
            body: RequestBody::TurnCancel {
                command_id: CommandId::new("command-cancel"),
                session_id: SessionId::new("session-created"),
                worker_generation: 7,
                run_id: RunId::new("run-1"),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-cancel"),
            body: ResponseBody::TurnCancel {
                session_id: SessionId::new("session-created"),
                run_id: RunId::new("run-1"),
                status: CancelStatus::Accepted,
                terminal_seq: None,
            },
        },
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-featured".into(),
            daemon_generation: 5,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.9".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([
                "session_mutation_v1".to_owned(),
                "turn_control_v1".to_owned(),
            ]),
        }),
        WireFrame::ProtocolError(ProtocolError {
            code: "invalid_frame".into(),
            message: "connection framing failed".into(),
            fatal: true,
        }),
        WireFrame::Unknown,
        // ── W3c2 additive account/vault surface (R7) ─────────────────────
        // Existing entries above stay byte-identical; everything below is
        // append-only. The staged "secret" is a golden placeholder pinning
        // the WIRE shape — Debug redaction is pinned separately.
        WireFrame::Request {
            request_id: RequestId::new("request-stage"),
            body: RequestBody::VaultStage {
                stage_id: "stage-1".into(),
                purpose: StagePurpose::ApiKey,
                secret: SecretWire::new("golden-placeholder-key"),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-stage"),
            body: ResponseBody::VaultStage {
                stage_id: "stage-1".into(),
                vault_reference: "vaultref-0123456789abcdef".into(),
                expires_at_ms: 1_753_500_060_000,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-login"),
            body: RequestBody::AccountLoginApi {
                command_id: CommandId::new("command-login"),
                provider: "anthropic".into(),
                alias: Some("work".into()),
                vault_reference: "vaultref-0123456789abcdef".into(),
                validation_model: None,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-login"),
            body: ResponseBody::AccountLoginApi {
                descriptor: golden_descriptor(),
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-accounts"),
            body: RequestBody::AccountList {
                provider: Some("anthropic".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-accounts"),
            body: ResponseBody::AccountList {
                descriptors: vec![golden_descriptor()],
                revision: None,
                provider_active: Vec::new(),
                provider_defaults: Vec::new(),
            },
        },
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-accounts".into(),
            daemon_generation: 6,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.11".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([
                FEATURE_ACCOUNT_LOGIN_API_V1.to_owned(),
                FEATURE_SESSION_MUTATION_V1.to_owned(),
                FEATURE_TURN_CONTROL_V1.to_owned(),
                FEATURE_VAULT_STAGE_V1.to_owned(),
            ]),
        }),
        // ── W5b append-only OAuth PKCE/account.add surface ───────────────
        WireFrame::Request {
            request_id: RequestId::new("request-oauth-start"),
            body: RequestBody::AccountOAuthStart {
                provider: "fake-oauth".into(),
                desired_alias: "work-oauth".into(),
                attempt_id: "attempt-1".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-oauth-start"),
            body: ResponseBody::AccountOAuthStart {
                availability: OAuthAvailabilityWire {
                    available: true,
                    reason: None,
                },
                flow_id: Some(OAuthFlowId::new("oauth-flow-golden")),
                authorization_url: Some(OAuthAuthorizationWire::new(
                    "https://auth.example.invalid/authorize?state=golden-placeholder",
                )),
                provider_origin: Some("https://auth.example.invalid".into()),
                loopback_port: Some(49_152),
                expires_at_ms: Some(1_753_500_060_000),
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-oauth-status"),
            body: RequestBody::AccountOAuthStatus {
                flow_id: OAuthFlowId::new("oauth-flow-golden"),
                attempt_id: "attempt-1".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-oauth-status"),
            body: ResponseBody::AccountOAuthStatus {
                flow_id: OAuthFlowId::new("oauth-flow-golden"),
                status: OAuthFlowStatusWire::Ready {
                    oauth_reference: OAuthReadyRefWire::new("oauth-ready-golden"),
                    identity: "person@example.invalid".into(),
                    expires_at_ms: 1_753_500_360_000,
                },
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-oauth-cancel"),
            body: RequestBody::AccountOAuthCancel {
                flow_id: OAuthFlowId::new("oauth-flow-golden"),
                attempt_id: "attempt-1".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-oauth-cancel"),
            body: ResponseBody::AccountOAuthCancel {
                flow_id: OAuthFlowId::new("oauth-flow-golden"),
                status: OAuthFlowStatusWire::Cancelled,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-account-add"),
            body: RequestBody::AccountAdd {
                command_id: CommandId::new("command-account-add"),
                provider: "fake-oauth".into(),
                alias: "work-oauth".into(),
                auth_method: AccountAddMethod::OAuth,
                flow_id: OAuthFlowId::new("oauth-flow-golden"),
                attempt_id: "attempt-1".into(),
                oauth_reference: OAuthReadyRefWire::new("oauth-ready-golden"),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-account-add"),
            body: ResponseBody::AccountAdd {
                descriptor: golden_oauth_descriptor(),
            },
        },
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-oauth".into(),
            daemon_generation: 7,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.13".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([
                FEATURE_ACCOUNT_LOGIN_API_V1.to_owned(),
                FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned(),
                FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned(),
                FEATURE_SESSION_MUTATION_V1.to_owned(),
                FEATURE_TURN_CONTROL_V1.to_owned(),
                FEATURE_VAULT_STAGE_V1.to_owned(),
            ]),
        }),
        // ── W5c.2a append-only management reads + revision spine ────────
        WireFrame::Response {
            request_id: RequestId::new("request-accounts-managed"),
            body: ResponseBody::AccountList {
                descriptors: vec![golden_descriptor()],
                revision: Some(7),
                provider_active: vec![ProviderActiveWire {
                    provider: "anthropic".into(),
                    alias: golden_descriptor().alias,
                }],
                provider_defaults: vec![ProviderDefaultWire {
                    provider: "anthropic".into(),
                    model: "frontier-anthropic".into(),
                }],
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-providers"),
            body: RequestBody::ProviderList {
                provider: Some("openai".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-providers"),
            body: ResponseBody::ProviderList {
                providers: vec![ProviderSummaryWire {
                    provider: "openai".into(),
                    api_family: ProviderApiFamilyWire::OpenAiResponses,
                    endpoint: Some("https://api.openai.com/v1/responses".into()),
                    models: vec!["frontier-a".into()],
                    model_details: vec![ModelDetailWire {
                        name: "frontier-a".into(),
                        context_window: None,
                    }],
                    auth_methods: vec![AuthMethod::ApiKey],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("frontier-a".into()),
                    enabled: true,
                }],
                revision: 7,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-set-active"),
            body: RequestBody::AccountSetActive {
                command_id: CommandId::new("command-set-active"),
                alias: "work".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-set-active"),
            body: ResponseBody::AccountSetActive {
                descriptor: golden_descriptor(),
                prior_alias: Some(CredentialAlias::new("personal")),
                revision: 8,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-remove"),
            body: RequestBody::AccountRemove {
                command_id: CommandId::new("command-remove"),
                alias: "work".into(),
                expected_revision: Some(8),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-remove"),
            body: ResponseBody::AccountRemove {
                removed_alias: CredentialAlias::new("work"),
                replacement_active_alias: Some(CredentialAlias::new("personal")),
                revision: 9,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-default-model"),
            body: RequestBody::AccountSetDefaultModel {
                command_id: CommandId::new("command-default-model"),
                provider: "openai".into(),
                model: "frontier-a".into(),
                expected_revision: 9,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-default-model"),
            body: ResponseBody::AccountSetDefaultModel {
                provider: ProviderSummaryWire {
                    provider: "openai".into(),
                    api_family: ProviderApiFamilyWire::OpenAiResponses,
                    endpoint: Some("https://api.openai.com/v1/responses".into()),
                    models: vec!["frontier-a".into()],
                    model_details: vec![ModelDetailWire {
                        name: "frontier-a".into(),
                        context_window: None,
                    }],
                    auth_methods: vec![AuthMethod::ApiKey],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("frontier-a".into()),
                    enabled: true,
                },
                revision: 10,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-provider-configure"),
            body: RequestBody::ProviderConfigure {
                command_id: CommandId::new("command-provider-configure"),
                provider: "local-lab".into(),
                api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
                origin: Some("http://127.0.0.1:11434".into()),
                auth_requirement: Some(ProviderAuthRequirementWire::None),
                enabled: true,
                models: vec!["local-frontier-a".into()],
                default_model: Some("local-frontier-a".into()),
                expected_revision: 10,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-provider-configure"),
            body: ResponseBody::ProviderConfigure {
                provider: ProviderSummaryWire {
                    provider: "local-lab".into(),
                    api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
                    endpoint: Some("http://127.0.0.1:11434".into()),
                    models: vec!["local-frontier-a".into()],
                    model_details: vec![ModelDetailWire {
                        name: "local-frontier-a".into(),
                        context_window: None,
                    }],
                    auth_methods: Vec::new(),
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("local-frontier-a".into()),
                    enabled: true,
                },
                revision: 11,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-provider-remove"),
            body: RequestBody::ProviderRemove {
                command_id: CommandId::new("command-provider-remove"),
                provider: "local-lab".into(),
                expected_revision: 11,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-provider-remove"),
            body: ResponseBody::ProviderRemove {
                provider: "local-lab".into(),
                revision: 12,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-provider-remove-blocked"),
            body: ResponseBody::Error {
                code: ERROR_CODE_PROVIDER_REMOVE_REFUSED.into(),
                message: "provider `local-lab` is referenced by credential aliases: lab-a, lab-b"
                    .into(),
                retryable: false,
                data: Some(ErrorData::ProviderRemoveRefused {
                    provider: "local-lab".into(),
                    reason: ProviderRemoveRefusalReasonWire::BlockingAccounts,
                    blocking_aliases: vec!["lab-a".into(), "lab-b".into()],
                }),
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-provider-models-refresh"),
            body: RequestBody::ProviderModelsRefresh {
                provider: "openai-oauth".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-provider-models-refresh"),
            body: ResponseBody::ProviderModelsRefresh {
                provider: ProviderSummaryWire {
                    provider: "openai-oauth".into(),
                    api_family: ProviderApiFamilyWire::OpenAiResponses,
                    endpoint: Some("https://chatgpt.com/backend-api/codex/responses".into()),
                    models: vec!["frontier-a".into(), "frontier-b".into()],
                    model_details: vec![
                        ModelDetailWire {
                            name: "frontier-a".into(),
                            context_window: None,
                        },
                        ModelDetailWire {
                            name: "frontier-b".into(),
                            context_window: None,
                        },
                    ],
                    auth_methods: vec![AuthMethod::OAuth],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("frontier-a".into()),
                    enabled: true,
                },
                revision: 12,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-provider-models-unavailable"),
            body: ResponseBody::Error {
                code: haider_rpc::ERROR_CODE_PROVIDER_ERROR.into(),
                message: "provider catalog unavailable".into(),
                retryable: false,
                data: Some(ErrorData::ProviderModelsUnavailable {
                    provider: "anthropic-oauth".into(),
                    reason: "provider did not serve a list to this credential".into(),
                }),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-revision-conflict"),
            body: ResponseBody::Error {
                code: ERROR_CODE_REVISION_CONFLICT.into(),
                message: "management snapshot changed".into(),
                retryable: true,
                data: Some(ErrorData::RevisionConflict {
                    expected_revision: 6,
                    current_revision: 7,
                }),
            },
        },
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-management".into(),
            daemon_generation: 8,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.13".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([
                FEATURE_ACCOUNT_LOGIN_API_V1.to_owned(),
                FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned(),
                FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned(),
                FEATURE_ACCOUNT_ROTATION_V1.to_owned(),
                FEATURE_PROVIDER_CONFIGURE_V1.to_owned(),
                FEATURE_PROVIDER_MANAGEMENT_V1.to_owned(),
                FEATURE_PROVIDER_MODELS_V1.to_owned(),
                FEATURE_PROVIDER_REMOVE_V1.to_owned(),
                FEATURE_SESSION_MUTATION_V1.to_owned(),
                FEATURE_TURN_CONTROL_V1.to_owned(),
                FEATURE_VAULT_STAGE_V1.to_owned(),
            ]),
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-shell-exec"),
            body: RequestBody::ShellExec {
                command_id: CommandId::new("command-shell-1"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                command: "printf 'exact bytes\\n'".into(),
                cwd: Some("crates/haider-tools".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-shell-exec"),
            body: ResponseBody::ShellExec {
                session_id: SessionId::new("session-1"),
                item_id: ItemId::new("shell-item-1"),
                accepted_seq: 51,
                worker_generation: 7,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-tools-inventory"),
            body: RequestBody::ToolsInventory {
                session_id: SessionId::new("session-1"),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-tools-inventory"),
            body: ResponseBody::ToolsInventory {
                session_id: SessionId::new("session-1"),
                inventory: ToolInventorySnapshot {
                    tools: vec![ToolInventoryEntry {
                        manifest: ToolManifest {
                            name: "process_exec".into(),
                            description: "Run one command".into(),
                            effects: vec![EffectClass::ProcessExec],
                            dispatch: DispatchMode::Await,
                            input_schema: serde_json::json!({"type": "object"}),
                        },
                        default: ToolPermissionDefault::Ask,
                    }],
                    remembered_grants: Vec::new(),
                },
            },
        },
        // W9b append-only create shape. The earlier create frame remains
        // byte-identical because its absent optional field is omitted.
        WireFrame::Request {
            request_id: RequestId::new("request-create-overrides"),
            body: RequestBody::SessionCreateWithPermissionOverrides {
                command_id: CommandId::new("command-create-overrides"),
                cwd: "/tmp/workspace".into(),
                provider: "anthropic".into(),
                model: "claude-test".into(),
                max_tokens: 4096,
                permission_overrides: Some(SessionPermissionOverridesV1 {
                    allow_writes: true,
                    allow_exec: true,
                }),
            },
        },
        // B2a append-only branch shapes. Every earlier frame stays byte-for-
        // byte frozen; main-branch request variants remain source compatible.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-branches".into(),
            daemon_generation: 9,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.13".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([FEATURE_BRANCH_CREATE_V1.to_owned()]),
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-branch-create"),
            body: RequestBody::BranchCreate {
                command_id: CommandId::new("command-branch-create"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                source_branch_id: None,
                fork_node_id: NodeId::new("node-fork-1"),
                fork_seq: 41,
                name: Some("Plan B".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-branch-create"),
            body: ResponseBody::BranchCreate {
                session_id: SessionId::new("session-1"),
                branch_id: BranchId::new("branch-plan-b"),
                source_branch_id: None,
                fork_node_id: NodeId::new("node-fork-1"),
                fork_seq: 41,
                created_seq: 52,
                worker_generation: 7,
                name: "Plan B".into(),
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-turn-on-branch"),
            body: RequestBody::TurnSubmitWithBranch {
                command_id: CommandId::new("command-turn-on-branch"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                branch_id: Some(BranchId::new("branch-plan-b")),
                text: "continue plan B".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-turn-on-branch"),
            body: ResponseBody::TurnSubmitOnBranch {
                session_id: SessionId::new("session-1"),
                run_id: RunId::new("run-plan-b"),
                accepted_seq: 53,
                worker_generation: 7,
                branch_id: BranchId::new("branch-plan-b"),
                disposition: SubmitDisposition::Started,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-compact-on-branch"),
            body: RequestBody::SessionCompactOnBranch {
                command_id: CommandId::new("command-compact-on-branch"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                branch_id: Some(BranchId::new("branch-plan-b")),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-compact-on-branch"),
            body: ResponseBody::SessionCompactOnBranch {
                session_id: SessionId::new("session-1"),
                run_id: RunId::new("manual-compact-plan-b"),
                accepted_seq: 60,
                worker_generation: 7,
                branch_id: BranchId::new("branch-plan-b"),
            },
        },
        // B4a append-only byte-ingress shapes. The upload has no command id:
        // identical bytes deduplicate by their verified content address.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-artifacts".into(),
            daemon_generation: 10,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.13".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([FEATURE_ARTIFACT_PUT_V1.to_owned()]),
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-artifact-put"),
            body: RequestBody::ArtifactPut {
                data_base64: "aGVsbG8=".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-artifact-put"),
            body: ResponseBody::ArtifactPut {
                artifact: ArtifactRef::new(
                    "blake3:ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f",
                ),
                bytes: 5,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-artifact-oversized"),
            body: ResponseBody::Error {
                code: "artifact_too_large".into(),
                message: "decoded artifact exceeds the hard limit".into(),
                retryable: false,
                data: Some(ErrorData::ArtifactTooLarge {
                    actual_bytes: 8_388_609,
                    max_bytes: 8_388_608,
                }),
            },
        },
        // B6k append-only device-flow frames. All preceding golden frames are
        // frozen byte-for-byte; older clients treat the new status as unknown.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-oauth-device".into(),
            daemon_generation: 11,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.13".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([FEATURE_ACCOUNT_OAUTH_DEVICE_V1.to_owned()]),
        }),
        WireFrame::Response {
            request_id: RequestId::new("request-kimi-oauth-start"),
            body: ResponseBody::AccountOAuthStart {
                availability: OAuthAvailabilityWire {
                    available: true,
                    reason: None,
                },
                flow_id: Some(OAuthFlowId::new("oauth-device-flow-golden")),
                authorization_url: Some(OAuthAuthorizationWire::new(
                    "https://auth.kimi.com/device?user_code=ABCD-EFGH",
                )),
                provider_origin: Some("https://auth.kimi.com".into()),
                loopback_port: None,
                expires_at_ms: Some(1_753_500_060_000),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-kimi-oauth-status"),
            body: ResponseBody::AccountOAuthStatus {
                flow_id: OAuthFlowId::new("oauth-device-flow-golden"),
                status: OAuthFlowStatusWire::WaitingDevice,
            },
        },
        // B6a append-only native Gemini family. All prior frames remain
        // byte-for-byte stable and an older tolerant client maps the family
        // discriminant to Unknown.
        WireFrame::Response {
            request_id: RequestId::new("request-gemini-provider"),
            body: ResponseBody::ProviderList {
                providers: vec![ProviderSummaryWire {
                    provider: "gemini".into(),
                    api_family: ProviderApiFamilyWire::GeminiGenerateContent,
                    endpoint: Some("https://generativelanguage.googleapis.com/v1beta".into()),
                    models: vec!["gemini-2.5-flash".into()],
                    model_details: vec![ModelDetailWire {
                        name: "gemini-2.5-flash".into(),
                        context_window: Some(1_048_576),
                    }],
                    auth_methods: vec![AuthMethod::ApiKey],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: None,
                    enabled: true,
                }],
                revision: 12,
            },
        },
        // H1 append-only observation frames. Every preceding frame remains
        // byte-for-byte stable for older transcript consumers.
        WireFrame::Request {
            request_id: RequestId::new("request-observe"),
            body: RequestBody::SessionObserve {
                session_id: session_id.clone(),
                last_event_limit: 20,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-observe"),
            body: ResponseBody::SessionObserve {
                digest: SessionObserveDigest {
                    session_id: SessionId::new("session-1"),
                    head_seq: 9,
                    worker_generation: 7,
                    metadata: None,
                    title: "Observe the durable session".into(),
                    run_state: ObserveRunStateWire::ParkedInput,
                    active_branch_id: None,
                    branches: Vec::new(),
                    main_head_node_id: None,
                    main_head_seq: 0,
                    latest_context_footprint: None,
                    pending_menus: Vec::new(),
                    subagents: Vec::new(),
                    updated_at_ms: 1_753_500_000_009,
                    last_event_kinds: vec!["run_state".into(), "menu_opened".into()],
                },
            },
        },
    ]
}

/// Golden credential descriptor: public global alias, verified display
/// identity in `identity`, never secret material.
pub fn golden_descriptor() -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new("anthropic-0123456789abcdef01234567"),
        provider: "anthropic".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "work".into(),
        status: CredentialStatus::Ok,
        active: true,
    }
}

pub fn golden_oauth_descriptor() -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new("fake-oauth-0123456789abcdef01234567"),
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "person@example.invalid".into(),
        status: CredentialStatus::Ok,
        active: true,
    }
}
