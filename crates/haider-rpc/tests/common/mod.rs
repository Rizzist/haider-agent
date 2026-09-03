#![allow(dead_code, clippy::expect_used)]

use std::collections::BTreeSet;

use haider_protocol::DeliveryMode;
use haider_protocol::agent::{
    AgentMessageDelivery, AgentMessageReceipt, AgentMetricsSnapshot, AgentUsageMetrics,
};
use haider_protocol::context::ContextFootprintTruth;
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::effect::EffectClass;
use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::CredentialAlias;
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, DeviceId, EventId, ItemId, MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::peer::{
    PeerDelivery, PeerDescriptor, PeerKind, PeerMessage, PeerReceipt, PeerSender, PeerState,
    PeerTrust,
};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::session_fork::{
    SessionForkDraft, SessionForkPromptSelector, SessionForkProvenance, SessionMetaforkProposal,
    SessionMetaforkRemoval, SessionMetaforkReviewManifest,
};
use haider_protocol::tool::{
    AttachmentBlock, DispatchMode, ToolInventoryEntry, ToolInventorySnapshot, ToolManifest,
    ToolPermissionDefault,
};
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1, UsageWindowV1,
};
use haider_rpc::{
    AccountAddMethod, AttachMode, AttachState, AttachmentId, CancelStatus, Capability, ClientKind,
    CommandId, DeviceCredentialCandidateWire, ERROR_CODE_ALREADY_RESOLVED,
    ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CURSOR_AHEAD, ERROR_CODE_PROVIDER_REMOVE_REFUSED,
    ERROR_CODE_REVISION_CONFLICT, ErrorData, FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1,
    FEATURE_ACCOUNT_LOGIN_API_V1, FEATURE_ACCOUNT_MANAGEMENT_V1, FEATURE_ACCOUNT_OAUTH_DEVICE_V1,
    FEATURE_ACCOUNT_OAUTH_PKCE_V1, FEATURE_ACCOUNT_ROTATION_V1, FEATURE_AGENT_CANCEL_V1,
    FEATURE_ARTIFACT_PUT_V1, FEATURE_BRANCH_CREATE_V1, FEATURE_PROVIDER_CONFIGURE_V1,
    FEATURE_PROVIDER_MANAGEMENT_V1, FEATURE_PROVIDER_MODELS_V1, FEATURE_PROVIDER_REMOVE_V1,
    FEATURE_SESSION_FLEET_IDENTITY_V1, FEATURE_SESSION_FLEET_V1, FEATURE_SESSION_FORK_V1,
    FEATURE_SESSION_MUTATION_V1, FEATURE_SESSION_PROMPT_FORK_V1, FEATURE_SESSION_RENAME_V1,
    FEATURE_TURN_CONTROL_V1, FEATURE_USAGE_REPORT_V1, FEATURE_VAULT_STAGE_V1, FleetAgentStateWire,
    FleetMetricsTotalsWire, FleetNodeWire, FleetRollupWire, FleetStateCountsWire, Hello,
    HookSummaryWire, HookTrustStateWire, LifecyclePhase, MenuInput, ModelDetailWire,
    MonitorActionWire, MonitorDeliveryDedupeWire, MonitorDeliveryReportWire,
    MonitorEventPayloadWire, MonitorEventWire, MonitorReportStatusWire, MonitorSourceKindWire,
    OAuthAuthorizationWire, OAuthAvailabilityWire, OAuthFlowId, OAuthFlowStatusWire,
    OAuthReadyRefWire, ObserveRunStateWire, ProtocolError, ProviderActiveWire,
    ProviderApiFamilyWire, ProviderAuthRequirementWire, ProviderAvailabilityWire,
    ProviderDefaultWire, ProviderRemoveRefusalReasonWire, ProviderSummaryWire, RequestBody,
    RequestId, ResponseBody, SecretWire, SeqRange, SessionFleetSnapshot, SessionObserveDigest,
    SessionReadResult, SessionSummary, StagePurpose, SubmitDisposition, Welcome, WireFrame,
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
        })
        .into(),
    }
}

pub fn transcript() -> Vec<WireFrame> {
    let session_id = SessionId::new("session-1");
    let attachment_id = AttachmentId::new("attachment-1");
    let range = SeqRange {
        start_seq: 5,
        end_seq: 9,
    };
    let fork_metadata = SessionMetadataV1 {
        cwd: "/tmp/workspace".into(),
        provider: "anthropic".into(),
        account_alias: None,
        model: "claude-test".into(),
        max_tokens: 4096,
        system_prompt_version: Some("fork-policy-v1".into()),
        permission_overrides: None,
        interaction_mode: Default::default(),
        title: Some("Chocolate-free child".into()),
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        agent_type: None,
        context_economy: Default::default(),
        created_at_ms: 1_753_500_041_000,
    };
    let metafork_proposal = SessionMetaforkProposal {
        removals: vec![SessionMetaforkRemoval {
            from_seq: 12,
            through_seq: 17,
            reason: "remove the chocolate discussion".into(),
            preview: Some("Chocolate tempering notes…".into()),
            reviewed_events: Vec::new(),
        }],
    };
    let metafork_review_manifest = SessionMetaforkReviewManifest {
        command_id: "command-session-metafork".into(),
        source_session_id: SessionId::new("session-1"),
        worker_generation: 7,
        source_branch_id: None,
        fork_node_id: NodeId::new("node-fork-3"),
        fork_seq: 60,
        name: Some("Chocolate-free child".into()),
        description: "remove parts about chocolate".into(),
        model_proposal: metafork_proposal.clone(),
    };

    let mut frames = vec![
        WireFrame::Hello(Hello {
            protocol_min: 1,
            protocol_max: 2,
            client_name: "haider-gui".into(),
            client_version: "0.0.8".into(),
            client_instance_id: "client-instance-1".into(),
            client_kind: ClientKind::Gui,
            capabilities_requested: capabilities([Capability::View, Capability::Control]),
            max_receive_frame: TEST_FRAME_LIMIT as u32,
            encodings: Vec::new(),
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
            user_command_withheld: false,
            encoding: None,
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
                sealed_replay: false,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-detach"),
            body: RequestBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        },
        // The pre-roster-truth summary shape: an older daemon omits the
        // additive turn/footprint fields entirely — these bytes must stay
        // frozen so older-daemon tolerance keeps a golden witness.
        WireFrame::Response {
            request_id: RequestId::new("request-list"),
            body: ResponseBody::SessionList {
                sessions: vec![SessionSummary {
                    session_id: session_id.clone(),
                    head_seq: 9,
                    worker_generation: 7,
                    run_state: None,
                    run_id: None,
                    seen_at_ms: None,
                    last_activity_ms: None,
                    waiting_why: None,
                    needs_input: None,
                    metadata: None,
                    provider: None,
                    last_model: None,
                    cache_lifetime_hit_basis_points: None,
                    cache_reread_hit_basis_points: None,
                    workspace_cwd: None,
                    turn_count: None,
                    footprint_tokens: None,
                    footprint_truth: None,
                    title: None,
                    agent_metrics: None,
                    parent_session_id: None,
                    kind: None,
                    agent_type: None,
                    effort: None,
                    fast: None,
                    account_alias: None,
                    forked_from: None,
                }],
                next_cursor: Some("cursor-after-session-1".into()),
            },
        },
        // The roster-truth summary shape: turn count plus footprint tokens
        // with the observe honesty marker, for launcher rosters that hydrate
        // from `session.list` without attaching.
        WireFrame::Response {
            request_id: RequestId::new("request-list-roster"),
            body: ResponseBody::SessionList {
                sessions: vec![SessionSummary {
                    session_id: session_id.clone(),
                    head_seq: 9,
                    worker_generation: 7,
                    run_state: None,
                    run_id: None,
                    seen_at_ms: None,
                    last_activity_ms: None,
                    waiting_why: None,
                    needs_input: None,
                    metadata: None,
                    provider: None,
                    last_model: None,
                    cache_lifetime_hit_basis_points: None,
                    cache_reread_hit_basis_points: None,
                    workspace_cwd: None,
                    turn_count: Some(4),
                    footprint_tokens: Some(33_500),
                    footprint_truth: Some(ContextFootprintTruth::Exact),
                    title: None,
                    agent_metrics: None,
                    parent_session_id: None,
                    kind: None,
                    agent_type: None,
                    effort: None,
                    fast: None,
                    account_alias: None,
                    forked_from: None,
                }],
                next_cursor: None,
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
                cache_policy: None,
                interaction_mode: haider_protocol::session::SessionInteractionModeV1::Interactive,
                ssh_scope: None,
                account_alias: None,
                resolve_provider: false,
                resolve_model: false,
                effort: None,
                fast: None,
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
                    account_alias: None,
                    model: "claude-test".into(),
                    max_tokens: 4096,
                    permission_overrides: None,
                    interaction_mode: Default::default(),
                    system_prompt_version: None,
                    title: None,
                    effort: None,
                    fast: false,
                    cache_policy: Default::default(),
                    context_economy: Default::default(),
                    created_at_ms: 1_753_500_040_000,
                    agent_type: None,
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
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::ProtocolError(ProtocolError {
            code: "invalid_frame".into(),
            message: "connection framing failed".into(),
            fatal: true,
            presentation: None,
            failed_write_ids: Vec::new(),
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
                replace_existing: false,
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
                availability: None,
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
            user_command_withheld: false,
            encoding: None,
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
                user_code: None,
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
            user_command_withheld: false,
            encoding: None,
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
                availability: None,
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
                    response_open_timeout_ms: None,
                    chunk_idle_timeout_ms: None,
                    semantic_progress_timeout_ms: None,
                    models: vec!["frontier-a".into()],
                    model_details: vec![ModelDetailWire {
                        name: "frontier-a".into(),
                        display_name: None,
                        context_window: None,
                        supported_efforts: Vec::new(),
                        default_effort: None,
                        supported_speeds: Vec::new(),
                        supports_thinking_type: None,
                    }],
                    inventory_fetched_at_ms: None,
                    inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
                    auth_methods: vec![AuthMethod::ApiKey],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("frontier-a".into()),
                    enabled: true,
                    trust: haider_rpc::ProviderTrustWire::Full,
                }],
                revision: 7,
                availability: None,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-set-active"),
            body: RequestBody::AccountSetActive {
                command_id: CommandId::new("command-set-active"),
                alias: "work".into(),
                confirm_new_epoch: false,
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
                    response_open_timeout_ms: None,
                    chunk_idle_timeout_ms: None,
                    semantic_progress_timeout_ms: None,
                    models: vec!["frontier-a".into()],
                    model_details: vec![ModelDetailWire {
                        name: "frontier-a".into(),
                        display_name: None,
                        context_window: None,
                        supported_efforts: Vec::new(),
                        default_effort: None,
                        supported_speeds: Vec::new(),
                        supports_thinking_type: None,
                    }],
                    inventory_fetched_at_ms: None,
                    inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
                    auth_methods: vec![AuthMethod::ApiKey],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("frontier-a".into()),
                    enabled: true,
                    trust: haider_rpc::ProviderTrustWire::Full,
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
                response_open_timeout_ms: None,
                chunk_idle_timeout_ms: None,
                semantic_progress_timeout_ms: None,
                probe_vault_reference: None,
                trust: None,
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
                    response_open_timeout_ms: None,
                    chunk_idle_timeout_ms: None,
                    semantic_progress_timeout_ms: None,
                    models: vec!["local-frontier-a".into()],
                    model_details: vec![ModelDetailWire {
                        name: "local-frontier-a".into(),
                        display_name: None,
                        context_window: None,
                        supported_efforts: Vec::new(),
                        default_effort: None,
                        supported_speeds: Vec::new(),
                        supports_thinking_type: None,
                    }],
                    inventory_fetched_at_ms: None,
                    inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
                    auth_methods: Vec::new(),
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("local-frontier-a".into()),
                    enabled: true,
                    trust: haider_rpc::ProviderTrustWire::Full,
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
                    response_open_timeout_ms: None,
                    chunk_idle_timeout_ms: None,
                    semantic_progress_timeout_ms: None,
                    models: vec!["frontier-a".into(), "frontier-b".into()],
                    model_details: vec![
                        ModelDetailWire {
                            name: "frontier-a".into(),
                            display_name: None,
                            context_window: None,
                            supported_efforts: Vec::new(),
                            default_effort: None,
                            supported_speeds: Vec::new(),
                            supports_thinking_type: None,
                        },
                        ModelDetailWire {
                            name: "frontier-b".into(),
                            display_name: None,
                            context_window: None,
                            supported_efforts: Vec::new(),
                            default_effort: None,
                            supported_speeds: Vec::new(),
                            supports_thinking_type: None,
                        },
                    ],
                    inventory_fetched_at_ms: None,
                    inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
                    auth_methods: vec![AuthMethod::OAuth],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: Some("frontier-a".into()),
                    enabled: true,
                    trust: haider_rpc::ProviderTrustWire::Full,
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
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-shell-exec"),
            body: RequestBody::ShellExecScoped {
                command_id: CommandId::new("command-shell-1"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                branch_id: None,
                agent_id: None,
                command: "printf 'exact bytes\\n'".into(),
                cwd: Some("crates/haider-tools".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-shell-exec"),
            body: ResponseBody::ShellExec {
                session_id: SessionId::new("session-1"),
                run_id: Some(RunId::new("shell-run-1")),
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
                    allow_mobile: false,
                    auto_allow: false,
                }),
                cache_policy: None,
                interaction_mode: haider_protocol::session::SessionInteractionModeV1::Interactive,
                ssh_scope: None,
                account_alias: None,
                resolve_provider: false,
                resolve_model: false,
                effort: None,
                fast: None,
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
            user_command_withheld: false,
            encoding: None,
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
        // Session-level fork/metafork are distinct from branch.create. The
        // metafork review response has no child coordinates until the human
        // echoes the exact model-proposal digest.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-session-fork".into(),
            daemon_generation: 9,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.942".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([FEATURE_SESSION_FORK_V1.to_owned()]),
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-session-fork"),
            body: RequestBody::SessionFork {
                command_id: CommandId::new("command-session-fork"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                source_branch_id: Some(BranchId::new("branch-plan-b")),
                fork_node_id: Some(NodeId::new("node-fork-2")),
                fork_seq: Some(57),
                prompt: None,
                name: Some("Independent plan B".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-session-fork"),
            body: ResponseBody::SessionFork {
                session_id: SessionId::new("session-fork-child"),
                source_session_id: SessionId::new("session-1"),
                source_branch_id: Some(BranchId::new("branch-plan-b")),
                fork_node_id: NodeId::new("node-fork-2"),
                fork_seq: 57,
                created_seq: 61,
                worker_generation: 7,
                metadata: fork_metadata.clone(),
                forked_from: None,
                draft: None,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-session-metafork-review"),
            body: RequestBody::SessionMetafork {
                command_id: CommandId::new("command-session-metafork"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                source_branch_id: None,
                fork_node_id: NodeId::new("node-fork-3"),
                fork_seq: 60,
                name: Some("Chocolate-free child".into()),
                description: "remove parts about chocolate".into(),
                model_proposal: metafork_proposal.clone(),
                accepted_proposal_digest: None,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-session-metafork-review"),
            body: ResponseBody::SessionMetafork {
                committed: false,
                source_session_id: SessionId::new("session-1"),
                session_id: None,
                source_branch_id: None,
                fork_node_id: NodeId::new("node-fork-3"),
                fork_seq: 60,
                description: "remove parts about chocolate".into(),
                model_proposal: metafork_proposal.clone(),
                review_manifest: Some(metafork_review_manifest),
                proposal_digest: "reviewed-proposal-digest".into(),
                created_seq: None,
                worker_generation: None,
                metadata: None,
                omission_count: None,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-session-metafork-commit"),
            body: RequestBody::SessionMetafork {
                command_id: CommandId::new("command-session-metafork"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                source_branch_id: None,
                fork_node_id: NodeId::new("node-fork-3"),
                fork_seq: 60,
                name: Some("Chocolate-free child".into()),
                description: "remove parts about chocolate".into(),
                model_proposal: metafork_proposal.clone(),
                accepted_proposal_digest: Some("reviewed-proposal-digest".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-session-metafork-commit"),
            body: ResponseBody::SessionMetafork {
                committed: true,
                source_session_id: SessionId::new("session-1"),
                session_id: Some(SessionId::new("session-metafork-child")),
                source_branch_id: None,
                fork_node_id: NodeId::new("node-fork-3"),
                fork_seq: 60,
                description: "remove parts about chocolate".into(),
                model_proposal: metafork_proposal,
                review_manifest: None,
                proposal_digest: "reviewed-proposal-digest".into(),
                created_seq: Some(64),
                worker_generation: Some(7),
                metadata: Some(fork_metadata.clone()),
                omission_count: Some(6),
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
            user_command_withheld: false,
            encoding: None,
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
            user_command_withheld: false,
            encoding: None,
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
                user_code: None,
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
                    response_open_timeout_ms: None,
                    chunk_idle_timeout_ms: None,
                    semantic_progress_timeout_ms: None,
                    models: vec!["gemini-2.5-flash".into()],
                    model_details: vec![ModelDetailWire {
                        name: "gemini-2.5-flash".into(),
                        display_name: None,
                        context_window: Some(1_048_576),
                        supported_efforts: Vec::new(),
                        default_effort: None,
                        supported_speeds: Vec::new(),
                        supports_thinking_type: None,
                    }],
                    inventory_fetched_at_ms: None,
                    inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
                    auth_methods: vec![AuthMethod::ApiKey],
                    availability: ProviderAvailabilityWire::Available,
                    availability_reason: None,
                    default_model: None,
                    enabled: true,
                    trust: haider_rpc::ProviderTrustWire::Full,
                }],
                revision: 12,
                availability: None,
            },
        },
        // H1 append-only observation frames. Every preceding frame remains
        // byte-for-byte stable for older transcript consumers.
        WireFrame::Request {
            request_id: RequestId::new("request-observe"),
            body: RequestBody::SessionObserve {
                session_id: session_id.clone(),
                last_event_limit: 20,
                metadata_only: false,
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
                    run_id: None,
                    active_branch_id: None,
                    branches: Vec::new(),
                    main_head_node_id: None,
                    main_head_seq: 0,
                    latest_context_footprint: None,
                    pending_menus: Vec::new(),
                    subagents: Vec::new(),
                    lockdown: None,
                    updated_at_ms: 1_753_500_000_009,
                    last_event_kinds: vec!["run_state".into(), "menu_opened".into()],
                    turn_count: None,
                    agent_metrics: None,
                    needs_input: None,
                    workflow: None,
                },
            },
        },
        // S1 append-only direct-child message wire. Older clients retain the
        // preceding transcript and decode this method as unknown.
        WireFrame::Request {
            request_id: RequestId::new("request-agent-message"),
            body: RequestBody::AgentMessage {
                command_id: CommandId::new("command-agent-message"),
                session_id: SessionId::new("session-parent"),
                worker_generation: 7,
                agent: AgentId::new("agent-child-7"),
                text: "re-read the parser fixture".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-agent-message"),
            body: ResponseBody::AgentMessage {
                receipt: AgentMessageReceipt {
                    agent: AgentId::new("agent-child-7"),
                    delivery: AgentMessageDelivery::DeliveredSteer,
                    child_run_id: RunId::new("run-child-7"),
                    child_run_state: haider_protocol::state::RunState::Streaming,
                },
            },
        },
        // D1 append-only device credential discovery + import. Every earlier
        // frame stays byte-for-byte frozen. The v0.0.964 `account.refresh`
        // pair lives in the focused method fixture rather than rewriting this
        // historical transcript. Candidates carry metadata only.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-device-discovery".into(),
            daemon_generation: 12,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.13".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1.to_owned()]),
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-device-candidates"),
            body: RequestBody::AccountDeviceCandidates,
        },
        WireFrame::Response {
            request_id: RequestId::new("request-device-candidates"),
            body: ResponseBody::AccountDeviceCandidates {
                discovery_disabled: false,
                adoption_available: Vec::new(),
                candidates: vec![
                    DeviceCredentialCandidateWire {
                        candidate: format!("dc1_{}", "0123456789abcdef".repeat(4)),
                        source: "codex".into(),
                        provider: "openai-oauth".into(),
                        source_label: "Codex".into(),
                        account_label: Some("person@example.invalid".into()),
                        identity: None,
                        freshness: "fresh".into(),
                        expires_at_ms: Some(1_753_503_600_000),
                        path: "/home/golden/.codex/auth.json".into(),
                        import_supported: true,
                        unsupported_reason: None,
                    },
                    DeviceCredentialCandidateWire {
                        candidate: format!("dc1_{}", "fedcba9876543210".repeat(4)),
                        source: "gemini-cli".into(),
                        provider: "gemini".into(),
                        source_label: "Gemini CLI".into(),
                        account_label: None,
                        identity: None,
                        freshness: "unknown".into(),
                        expires_at_ms: None,
                        path: "/home/golden/.gemini/oauth_creds.json".into(),
                        import_supported: false,
                        unsupported_reason: Some(
                            "Gemini CLI OAuth credentials cannot be imported".into(),
                        ),
                    },
                ],
            },
        },
        // The honest configured-off state: disabled is a report, never an
        // empty-device claim.
        WireFrame::Response {
            request_id: RequestId::new("request-device-candidates-disabled"),
            body: ResponseBody::AccountDeviceCandidates {
                discovery_disabled: true,
                candidates: Vec::new(),
                adoption_available: Vec::new(),
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-import-device"),
            body: RequestBody::AccountImportDevice {
                command_id: CommandId::new("command-import-device"),
                candidate: format!("dc1_{}", "0123456789abcdef".repeat(4)),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-import-device"),
            body: ResponseBody::AccountImportDevice {
                descriptor: golden_oauth_descriptor(),
                revision: 13,
            },
        },
        // ── T1 additive transcription secret surface ─────────────────────
        // Existing entries above stay byte-identical; everything below is
        // append-only. The staged "secret" is a golden placeholder pinning
        // the WIRE shape — Debug redaction is pinned separately.
        WireFrame::Request {
            request_id: RequestId::new("request-transcription-set"),
            body: RequestBody::TranscriptionSecretSet {
                secret: SecretWire::new("golden-placeholder-deepgram-key"),
                clear: false,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-transcription-set"),
            body: ResponseBody::TranscriptionSecretSet { present: true },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-transcription-get"),
            body: RequestBody::TranscriptionSecretGet,
        },
        WireFrame::Response {
            request_id: RequestId::new("request-transcription-get"),
            body: ResponseBody::TranscriptionSecretGet {
                secret: Some(SecretWire::new("golden-placeholder-deepgram-key")),
            },
        },
        // The honest empty state: no key stored yet.
        WireFrame::Response {
            request_id: RequestId::new("request-transcription-get-empty"),
            body: ResponseBody::TranscriptionSecretGet { secret: None },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-transcription-clear"),
            body: RequestBody::TranscriptionSecretSet {
                secret: SecretWire::new(""),
                clear: true,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-transcription-clear"),
            body: ResponseBody::TranscriptionSecretSet { present: false },
        },
        // U1 append-only cross-provider usage report. Every earlier frame
        // stays byte-for-byte frozen. The request is parameterless; the
        // response carries only derived data — utilization is ALWAYS the
        // normalized 0–1 fraction on the wire (never a raw percentage), and
        // no field of the report can carry token/key bytes.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-usage-report".into(),
            daemon_generation: 13,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.70".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View]),
            features: BTreeSet::from([FEATURE_USAGE_REPORT_V1.to_owned()]),
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-usage-report"),
            body: RequestBody::UsageReport,
        },
        WireFrame::Response {
            request_id: RequestId::new("request-usage-report"),
            body: ResponseBody::UsageReport {
                report: UsageReportV1 {
                    generated_at_ms: 1_753_500_000_500,
                    accounts: vec![
                        AccountUsageReportV1 {
                            provider: "anthropic-oauth".into(),
                            alias: CredentialAlias::new("personal-max"),
                            identity: Some("person@example.invalid".into()),
                            plan: None,
                            auth_method: AuthMethod::OAuth,
                            meter: AccountMeterStateV1::Metered {
                                windows: vec![
                                    UsageWindowV1 {
                                        window: "five_hour".into(),
                                        utilization: 0.6,
                                        resets_at_ms: Some(1_753_507_200_000),
                                        label: None,
                                    },
                                    UsageWindowV1 {
                                        window: "seven_day".into(),
                                        utilization: 0.12,
                                        resets_at_ms: Some(1_753_900_000_000),
                                        label: None,
                                    },
                                ],
                            },
                            local: LocalUsageStatsV1 {
                                sessions: 2,
                                total_duration_ms: 3_600_000,
                                input_tokens: 90_000,
                                output_tokens: 7_000,
                                reasoning_tokens: 1_000,
                                cached_tokens: 60_000,
                                est_cost_usd: None,
                                api_equivalent_est_cost_usd: Some(0.42),
                                lines_added: 120,
                                lines_removed: 30,
                                cache: haider_protocol::usage::CacheUsageStatsV1::default(),
                            },
                        },
                        AccountUsageReportV1 {
                            provider: "openai-oauth".into(),
                            alias: CredentialAlias::new("work-chatgpt"),
                            identity: Some("person@example.invalid".into()),
                            plan: Some("plus".into()),
                            auth_method: AuthMethod::OAuth,
                            meter: AccountMeterStateV1::Unavailable {
                                reason: "http_status_429".into(),
                            },
                            local: LocalUsageStatsV1::default(),
                        },
                        AccountUsageReportV1 {
                            provider: "openai".into(),
                            alias: CredentialAlias::new("billing-key"),
                            identity: Some("work".into()),
                            plan: None,
                            auth_method: AuthMethod::ApiKey,
                            meter: AccountMeterStateV1::LocalOnly,
                            local: LocalUsageStatsV1 {
                                sessions: 1,
                                total_duration_ms: 600_000,
                                input_tokens: 40_000,
                                output_tokens: 3_000,
                                reasoning_tokens: 0,
                                cached_tokens: 0,
                                est_cost_usd: Some(0.08),
                                api_equivalent_est_cost_usd: Some(0.08),
                                lines_added: 12,
                                lines_removed: 4,
                                cache: haider_protocol::usage::CacheUsageStatsV1::default(),
                            },
                        },
                    ],
                },
                availability: None,
            },
        },
        // G2 append-only session-rename frames. Every earlier frame stays
        // byte-for-byte frozen; the request's absent title (a CLEAR) and
        // the response's normalized title pin both optional-field shapes.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-session-rename".into(),
            daemon_generation: 14,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.71".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([FEATURE_SESSION_RENAME_V1.to_owned()]),
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-session-rename"),
            body: RequestBody::SessionRename {
                command_id: CommandId::new("command-session-rename"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                title: Some("Parser rewrite".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-session-rename"),
            body: ResponseBody::SessionRename {
                session_id: SessionId::new("session-1"),
                title: Some("Parser rewrite".into()),
                renamed_seq: 61,
                worker_generation: 7,
            },
        },
        // ── G3: the two session-tuning pairs, appended at the transcript
        // END (additive-wire law — nothing before them can have moved). ──
        WireFrame::Request {
            request_id: RequestId::new("request-select-effort"),
            body: RequestBody::SessionSelectEffort {
                command_id: CommandId::new("command-select-effort"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                effort: Some("xhigh".into()),
                confirm_new_epoch: false,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-select-effort"),
            body: ResponseBody::SessionSelectEffort {
                session_id: SessionId::new("session-1"),
                effort: Some("xhigh".into()),
                selected_seq: 43,
                worker_generation: 7,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-select-fast"),
            body: RequestBody::SessionSelectFast {
                command_id: CommandId::new("command-select-fast"),
                session_id: SessionId::new("session-1"),
                worker_generation: 7,
                enabled: true,
                confirm_new_epoch: false,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-select-fast"),
            body: ResponseBody::SessionSelectFast {
                session_id: SessionId::new("session-1"),
                enabled: true,
                selected_seq: 44,
                worker_generation: 7,
            },
        },
        // Fleet snapshot frames are append-only: the additive method and
        // response shape do not alter any earlier protocol-v1 bytes.
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-session-fleet".into(),
            daemon_generation: 15,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.906".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View]),
            features: BTreeSet::from([FEATURE_SESSION_FLEET_V1.to_owned()]),
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-session-fleet"),
            body: RequestBody::SessionFleet {
                session_id: SessionId::new("session-1"),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-session-fleet"),
            body: ResponseBody::SessionFleet {
                snapshot: SessionFleetSnapshot {
                    session_id: SessionId::new("session-1"),
                    generated_at_ms: 1_753_500_050_000,
                    node_limit: 512,
                    depth_limit: 32,
                    roots: vec![FleetNodeWire {
                        agent_id: AgentId::new("agent-child-1"),
                        session_id: SessionId::new("session-child-1"),
                        callsign: Some("Ada".into()),
                        model: None,
                        provider: None,
                        task: "inspect parser".into(),
                        depth: 1,
                        parent_session_id: SessionId::new("session-1"),
                        parent_agent_id: None,
                        state: FleetAgentStateWire::Waiting,
                        metrics: Some(AgentMetricsSnapshot {
                            agent: Some(AgentId::new("agent-child-1")),
                            session_id: SessionId::new("session-child-1"),
                            head_seq: 9,
                            started_at_ms: 1_753_500_040_000,
                            terminal_at_ms: None,
                            live: true,
                            tool_attempts: 2,
                            usage: Some(AgentUsageMetrics {
                                logical_input_tokens: 800,
                                billed_output_tokens: 200,
                                additional_reasoning_tokens: 50,
                                cache_read_tokens: 300,
                                cache_write_tokens: 0,
                                cache_hit_basis_points: Some(3_750),
                                cache_reread_hit_basis_points: None,
                                metered_cost_microusd: Some(4_200),
                                api_equivalent_cost_microusd: Some(4_200),
                                all_lanes_priced: true,
                                has_metered_lanes: true,
                                has_oauth_lanes: false,
                                breakdowns: Vec::new(),
                            }),
                        }),
                        folded_children: 0,
                        children: Vec::new(),
                    }],
                    rollup: FleetRollupWire {
                        node_count: 1,
                        states: FleetStateCountsWire {
                            waiting: 1,
                            ..FleetStateCountsWire::default()
                        },
                        max_depth: 1,
                        metrics: FleetMetricsTotalsWire {
                            elapsed_ms: 10_000,
                            tool_attempts: 2,
                            usage: Some(AgentUsageMetrics {
                                logical_input_tokens: 800,
                                billed_output_tokens: 200,
                                additional_reasoning_tokens: 50,
                                cache_read_tokens: 300,
                                cache_write_tokens: 0,
                                cache_hit_basis_points: Some(3_750),
                                cache_reread_hit_basis_points: None,
                                metered_cost_microusd: Some(4_200),
                                api_equivalent_cost_microusd: Some(4_200),
                                all_lanes_priced: true,
                                has_metered_lanes: true,
                                has_oauth_lanes: false,
                                breakdowns: Vec::new(),
                            }),
                        },
                        metrics_complete: true,
                        complete: true,
                    },
                    truncated: false,
                },
            },
        },
        // WIRE-GAPS: current session/hook read shapes are appended after
        // every historical frame. Older transcript bytes stay frozen.
        WireFrame::Request {
            request_id: RequestId::new("request-list-workspace"),
            body: RequestBody::SessionList {
                cursor: None,
                limit: 64,
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-list-workspace"),
            body: ResponseBody::SessionList {
                sessions: vec![SessionSummary {
                    session_id: SessionId::new("session-workspace"),
                    head_seq: 17,
                    worker_generation: 15,
                    run_state: None,
                    run_id: None,
                    seen_at_ms: None,
                    last_activity_ms: None,
                    waiting_why: None,
                    needs_input: None,
                    metadata: None,
                    provider: None,
                    last_model: None,
                    cache_lifetime_hit_basis_points: None,
                    cache_reread_hit_basis_points: None,
                    workspace_cwd: Some("/work/original".into()),
                    turn_count: None,
                    footprint_tokens: None,
                    footprint_truth: None,
                    title: None,
                    agent_metrics: None,
                    parent_session_id: None,
                    kind: None,
                    agent_type: None,
                    effort: None,
                    fast: None,
                    account_alias: None,
                    forked_from: None,
                }],
                next_cursor: None,
            },
        },
        WireFrame::Request {
            request_id: RequestId::new("request-hooks-wire-gaps"),
            body: RequestBody::HooksList {
                cwd: "/work/original".into(),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-hooks-wire-gaps"),
            body: ResponseBody::HooksList {
                policy: "per_digest".into(),
                revision: 7,
                hooks: vec![HookSummaryWire {
                    name: "format".into(),
                    digest: "d".repeat(64),
                    source: "/work/original/hooks.json".into(),
                    kind: "exec".into(),
                    event: "run_finished".into(),
                    trusted: false,
                    trust_state: Some(HookTrustStateWire::RevokedByEdit),
                    decision: false,
                    timeout_ms: 30_000,
                }],
            },
        },
        // SLICE 2 appends the non-zero per-node fold witness after every
        // historical golden frame. The earlier zero-valued fleet node stays
        // byte-identical because zero is omitted on the wire.
        WireFrame::Response {
            request_id: RequestId::new("request-session-fleet-folded"),
            body: ResponseBody::SessionFleet {
                snapshot: SessionFleetSnapshot {
                    session_id: SessionId::new("session-folded"),
                    generated_at_ms: 1_753_500_060_000,
                    node_limit: 512,
                    depth_limit: 32,
                    roots: vec![FleetNodeWire {
                        agent_id: AgentId::new("agent-folded"),
                        session_id: SessionId::new("session-folded-child"),
                        callsign: Some("Fold".into()),
                        model: None,
                        provider: None,
                        task: "bounded branch".into(),
                        depth: 32,
                        parent_session_id: SessionId::new("session-folded"),
                        parent_agent_id: None,
                        state: FleetAgentStateWire::Done,
                        metrics: None,
                        folded_children: 3,
                        children: Vec::new(),
                    }],
                    rollup: FleetRollupWire {
                        node_count: 1,
                        states: FleetStateCountsWire {
                            done: 1,
                            ..FleetStateCountsWire::default()
                        },
                        max_depth: 32,
                        metrics: FleetMetricsTotalsWire::default(),
                        metrics_complete: false,
                        complete: false,
                    },
                    truncated: true,
                },
            },
        },
        // monitor_delivery_v1 appends a dedicated report and caught-up seal;
        // neither is a chat Event nor the private APK negative-id transport.
        WireFrame::MonitorDelivery {
            watch_id: "monitor-watch-1".into(),
            report: MonitorDeliveryReportWire {
                report_id: "monitor-report-1".into(),
                monitor_id: "monitor-1".into(),
                session_id: SessionId::new("session-1"),
                branch_id: Some(BranchId::new("branch-1")),
                agent_id: Some(AgentId::new("agent-1")),
                source: MonitorSourceKindWire::Sms,
                status: MonitorReportStatusWire::Matched,
                events: vec![MonitorEventWire {
                    sequence: 9,
                    observed_at_ms: 1_753_500_070_000,
                    payload: MonitorEventPayloadWire::Sms {
                        address: "+15550001".into(),
                        body: "ship it".into(),
                        received_at_ms: 1_753_500_069_000,
                    },
                }],
                coalesced_count: 3,
                omitted_count: 2,
                action: MonitorActionWire {
                    report: true,
                    follow_up: Some("summarize the alert".into()),
                },
                cursor: 71,
                dedupe: MonitorDeliveryDedupeWire {
                    delivery_key: "monitor-delivery-session-1-71".into(),
                    report_key: "monitor-report-1".into(),
                },
            },
        },
        WireFrame::MonitorDeliveryCaughtUp {
            watch_id: "monitor-watch-1".into(),
            session_id: SessionId::new("session-1"),
            high_water_cursor: 73,
        },
        // L4 appends the durable registry delta + seal frames after every
        // historical golden. The record remains fully typed: an agent
        // lineage fact is never flattened into workflow-DAG metadata.
        WireFrame::LoomRegistryDelta {
            watch_id: "loom-watch-1".into(),
            delta: haider_protocol::loom::LoomRegistryDelta {
                cursor: 44,
                change: haider_protocol::loom::LoomRegistryDeltaKind::Archived,
                entry: haider_protocol::loom::LoomRegistryEntryRef {
                    kind: haider_protocol::loom::LoomRegistryEntryKind::AgentType,
                    id: "reviewer".into(),
                    rev: 3,
                    digest: "digest-reviewer-3".into(),
                    archived: true,
                },
                record: haider_protocol::loom::LoomRegistryRecord::AgentType(
                    haider_protocol::loom::LoomAgentType {
                        id: "reviewer".into(),
                        name: "Reviewer".into(),
                        job: "Review changes".into(),
                        in_type: "Patch".into(),
                        out_type: "Verdict".into(),
                        clis: vec!["rg".into()],
                        apis: Vec::new(),
                        denials: Vec::new(),
                        skills: Vec::new(),
                        scripts: Vec::new(),
                        color: "#445566".into(),
                        glyph: "R".into(),
                        rev: 3,
                    },
                ),
            },
        },
        WireFrame::LoomRegistryCaughtUp {
            watch_id: "loom-watch-1".into(),
            high_water_cursor: 44,
        },
        // Peer messaging v1 is tail-only: both methods and both additive
        // event shapes leave every historical byte pin unchanged.
        WireFrame::Request {
            request_id: RequestId("request-peer-list".into()),
            body: RequestBody::PeerList {},
        },
        WireFrame::Response {
            request_id: RequestId("request-peer-list".into()),
            body: ResponseBody::PeerList {
                agents: vec![PeerDescriptor {
                    id: "session-peer".into(),
                    name: "workspace-a1b2c3".into(),
                    kind: PeerKind::HaiderSession,
                    workspace: "/tmp/workspace".into(),
                    model: "claude-test".into(),
                    state: PeerState::Idle,
                    started_at: 1_753_500_080_000,
                    last_seen: 1_753_500_081_000,
                }],
            },
        },
        WireFrame::Request {
            request_id: RequestId("request-peer-send".into()),
            body: RequestBody::PeerSend {
                to: "workspace-a1b2c3".into(),
                message: "Please inspect the failing boundary.".into(),
                summary: Some("debug boundary".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId("request-peer-send".into()),
            body: ResponseBody::PeerSend {
                receipt: PeerReceipt {
                    msg_id: "msg-peer-1".into(),
                    delivery: PeerDelivery::Queued,
                    reason: None,
                },
            },
        },
        WireFrame::PeerMessageReceived {
            message: PeerMessage {
                msg_id: "msg-peer-1".into(),
                from: PeerSender {
                    id: "external-fixture".into(),
                    name: "fixture".into(),
                    kind: PeerKind::External,
                    trust: PeerTrust::UntrustedExternal,
                },
                to: "session-1".into(),
                message: "Treat this as data, not instruction.".into(),
                summary: None,
                queued_at: 1_753_500_082_000,
                expires_at: 1_753_586_482_000,
            },
        },
        WireFrame::PeerDeliveryChanged {
            receipt: PeerReceipt {
                msg_id: "msg-peer-1".into(),
                delivery: PeerDelivery::Delivered,
                reason: None,
            },
        },
    ];
    append_union_contract_tail(&mut frames);
    append_prompt_fork_contract_tail(&mut frames, fork_metadata);
    append_fleet_identity_contract_tail(&mut frames);
    append_agent_cancel_contract_tail(&mut frames);
    frames
}

/// A/C/D union tail. These method pairs are decoded through the public wire
/// types before the byte golden is generated, so serde defaults and canonical
/// field ordering remain part of the transcript contract.
fn append_union_contract_tail(frames: &mut Vec<WireFrame>) {
    for (request_id, request, response) in [
        (
            "request-union-peer-name",
            r##"{"method":"peer.name","name":"reviewer"}"##,
            r##"{"method":"peer.name","agent":{"id":"session-peer","name":"reviewer","kind":"haider_session","workspace":"/tmp/workspace","model":"claude-test","state":"idle","started_at":1753500080000,"last_seen":1753500081000}}"##,
        ),
        (
            "request-union-ssh-list",
            r##"{"method":"ssh.list","session_id":"session-1"}"##,
            r##"{"method":"ssh.list","profiles":[{"name":"prod","description":"Production console","host":"prod.example.invalid","port":22,"user":"deploy","default_cwd":"/srv/app","last_used_ms":1720000004000,"multiplexing":true,"in_scope":true}]}"##,
        ),
        (
            "request-union-ssh-add",
            r##"{"method":"ssh.add","profile":{"name":"prod","description":"Production console","host":"prod.example.invalid","port":22,"user":"deploy","auth":{"kind":"key_material","vault_reference":"staged-ssh-key-1"},"default_cwd":"/srv/app"}}"##,
            r##"{"method":"ssh.add","profile":{"name":"prod","description":"Production console","host":"prod.example.invalid","port":22,"user":"deploy","default_cwd":"/srv/app","multiplexing":true,"in_scope":true}}"##,
        ),
        (
            "request-union-ssh-update",
            r##"{"method":"ssh.update","name":"prod","changes":{"description":"Primary production console","port":2222}}"##,
            r##"{"method":"ssh.update","profile":{"name":"prod","description":"Primary production console","host":"prod.example.invalid","port":2222,"user":"deploy","default_cwd":"/srv/app","multiplexing":true,"in_scope":true}}"##,
        ),
        (
            "request-union-ssh-remove",
            r##"{"method":"ssh.remove","name":"prod"}"##,
            r##"{"method":"ssh.remove","removed":"prod"}"##,
        ),
        (
            "request-union-ssh-test",
            r##"{"method":"ssh.test","name":"prod","timeout_s":15}"##,
            r##"{"method":"ssh.test","result":{"profile":{"name":"prod","description":"Production console","host":"prod.example.invalid","port":22,"user":"deploy","default_cwd":"/srv/app","host_key":{"algorithm":"ssh-ed25519","fingerprint":"SHA256:golden-fingerprint","pinned_at_ms":1720000005000},"last_used_ms":1720000005001,"multiplexing":true,"in_scope":true},"connected":true,"host_key_pinned":true}}"##,
        ),
        (
            "request-union-session-set-ssh-scope",
            r##"{"method":"session.set_ssh_scope","session_id":"session-1","scope":{"kind":"allow","names":["prod"]}}"##,
            r##"{"method":"session.set_ssh_scope","session_id":"session-1","scope":{"kind":"allow","names":["prod"]}}"##,
        ),
        (
            "request-union-ssh-shell",
            r##"{"method":"ssh.shell","name":"prod","command":"uname -a","cwd":"/srv/app","timeout_s":30}"##,
            r##"{"method":"ssh.shell","result":{"profile":"prod","stdout":"Linux prod\\n","stderr":"","exit_code":0,"timed_out":false}}"##,
        ),
        (
            "request-union-ssh-shell-open",
            r##"{"method":"ssh.shell_open","name":"prod","session_id":"session-1","term":"xterm-256color","size":{"cols":120,"rows":40,"pixel_width":960,"pixel_height":800}}"##,
            r##"{"method":"ssh.shell_open","shell":{"id":"sh-pty-0123456789","kind":{"kind":"ssh","profile":"prod"},"status":{"status":"running"},"title":"prod","cwd_or_host":"prod.example.invalid","created_at_ms":1720000006000,"last_activity_ms":1720000006001,"bytes_out":0}}"##,
        ),
        (
            "request-union-ssh-shell-input",
            r##"{"method":"ssh.shell_input","id":"sh-pty-0123456789","data_b64":"d2hvYW1pXG4="}"##,
            r##"{"method":"ssh.shell_input","shell":{"id":"sh-pty-0123456789","kind":{"kind":"ssh","profile":"prod"},"status":{"status":"running"},"title":"prod","cwd_or_host":"prod.example.invalid","created_at_ms":1720000006000,"last_activity_ms":1720000006001,"bytes_out":0}}"##,
        ),
        (
            "request-union-ssh-shell-resize",
            r##"{"method":"ssh.shell_resize","id":"sh-pty-0123456789","size":{"cols":132,"rows":43,"pixel_width":1056,"pixel_height":860}}"##,
            r##"{"method":"ssh.shell_resize","shell":{"id":"sh-pty-0123456789","kind":{"kind":"ssh","profile":"prod"},"status":{"status":"running"},"title":"prod","cwd_or_host":"prod.example.invalid","created_at_ms":1720000006000,"last_activity_ms":1720000006001,"bytes_out":0}}"##,
        ),
        (
            "request-union-ssh-shell-eof",
            r##"{"method":"ssh.shell_eof","id":"sh-pty-0123456789"}"##,
            r##"{"method":"ssh.shell_eof","shell":{"id":"sh-pty-0123456789","kind":{"kind":"ssh","profile":"prod"},"status":{"status":"running"},"title":"prod","cwd_or_host":"prod.example.invalid","created_at_ms":1720000006000,"last_activity_ms":1720000006001,"bytes_out":0}}"##,
        ),
        (
            "request-union-shell-list",
            r##"{"method":"shell.list"}"##,
            r##"{"method":"shell.list","shells":[{"id":"sh-0123456789abcdef0123","kind":{"kind":"ssh","profile":"prod"},"status":{"status":"running"},"title":"prod: uname -a","cwd_or_host":"prod.example.invalid","created_at_ms":1720000006000,"last_activity_ms":1720000006001,"bytes_out":11}]}"##,
        ),
        (
            "request-union-shell-close",
            r##"{"method":"shell.close","id":"sh-0123456789abcdef0123"}"##,
            r##"{"method":"shell.close","shell":{"id":"sh-0123456789abcdef0123","kind":{"kind":"ssh","profile":"prod"},"status":{"status":"closed"},"title":"prod: uname -a","cwd_or_host":"prod.example.invalid","created_at_ms":1720000006000,"last_activity_ms":1720000007000,"bytes_out":11}}"##,
        ),
        (
            "request-union-provider-set-trust",
            r##"{"method":"provider.set_trust","command_id":"command-provider-trust","name":"research","trust":"lockdown","expected_revision":12}"##,
            r##"{"method":"provider.set_trust","provider":{"provider":"research","api_family":"openai_chat_completions","models":["search-1"],"model_details":[],"auth_methods":[],"availability":"available","enabled":true,"trust":"lockdown"},"revision":13}"##,
        ),
        (
            "request-union-lockdown-status",
            r##"{"method":"lockdown.status","provider":"research"}"##,
            r##"{"method":"lockdown.status","status":{"provider":"research","tools_allowed":["fs_read","fs_search","web_search"],"quota_used":4096,"quota_limit":1073741824}}"##,
        ),
        (
            "request-union-lockdown-set-quota",
            r##"{"method":"lockdown.set_quota","command_id":"command-lockdown-quota","bytes":2147483648}"##,
            r##"{"method":"lockdown.set_quota","status":{"tools_allowed":["fs_read","fs_search","web_search"],"quota_used":4096,"quota_limit":2147483648}}"##,
        ),
    ] {
        frames.push(WireFrame::Request {
            request_id: RequestId(request_id.into()),
            body: serde_json::from_str(request).expect("decode union-tail golden request"),
        });
        frames.push(WireFrame::Response {
            request_id: RequestId(request_id.into()),
            body: serde_json::from_str(response).expect("decode union-tail golden response"),
        });
    }
}

/// Prompt-oriented session forking is a strict four-frame tail append. The
/// existing `session.fork` method remains transcript-covered exactly once as
/// a method even though both selector shapes now have golden witnesses.
fn append_prompt_fork_contract_tail(frames: &mut Vec<WireFrame>, metadata: SessionMetadataV1) {
    let source_session_id = SessionId::new("session-prompt-source");
    let child_session_id = SessionId::new("session-prompt-child");
    let provenance = SessionForkProvenance {
        session_id: source_session_id.clone(),
        seq: 58,
    };
    frames.extend([
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-session-prompt-fork".into(),
            daemon_generation: 10,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.966".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View, Capability::Control]),
            features: BTreeSet::from([
                FEATURE_SESSION_FORK_V1.to_owned(),
                FEATURE_SESSION_PROMPT_FORK_V1.to_owned(),
            ]),
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Request {
            request_id: RequestId::new("request-session-prompt-fork"),
            body: RequestBody::SessionFork {
                command_id: CommandId::new("command-session-prompt-fork"),
                session_id: source_session_id.clone(),
                worker_generation: 7,
                source_branch_id: Some(BranchId::new("branch-plan-b")),
                fork_node_id: None,
                fork_seq: None,
                prompt: Some(SessionForkPromptSelector { seq: 58 }),
                name: Some("Edit plan B".into()),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-session-prompt-fork"),
            body: ResponseBody::SessionFork {
                session_id: child_session_id.clone(),
                source_session_id: source_session_id.clone(),
                source_branch_id: Some(BranchId::new("branch-plan-b")),
                fork_node_id: NodeId::new("node-before-prompt-b"),
                fork_seq: 57,
                created_seq: 58,
                worker_generation: 7,
                metadata,
                forked_from: Some(provenance.clone()),
                draft: Some(SessionForkDraft {
                    text: "Revise the implementation plan using this file.".into(),
                    attachments: vec![AttachmentBlock::File {
                        artifact: ArtifactRef::new("blake3:prompt-b-file"),
                        name: "requirements.txt".into(),
                        lines: 12,
                    }],
                }),
            },
        },
        WireFrame::SessionRosterDelta {
            summaries: vec![SessionSummary {
                session_id: child_session_id,
                head_seq: 58,
                worker_generation: 7,
                run_state: Some(ObserveRunStateWire::Idle),
                run_id: None,
                seen_at_ms: None,
                last_activity_ms: Some(1_753_500_090_000),
                waiting_why: None,
                needs_input: None,
                metadata: None,
                provider: None,
                last_model: None,
                cache_lifetime_hit_basis_points: None,
                cache_reread_hit_basis_points: None,
                workspace_cwd: None,
                turn_count: Some(3),
                footprint_tokens: None,
                footprint_truth: None,
                title: Some("Edit plan B".into()),
                agent_metrics: None,
                parent_session_id: None,
                kind: Some(haider_rpc::SessionKindWire::Root),
                agent_type: None,
                effort: None,
                fast: None,
                account_alias: None,
                forked_from: Some(provenance),
            }],
        },
    ]);
}

/// X1 appends the advertised manifest-identity shape for both fleet delivery
/// paths. No request method is new: the existing snapshot and descendant
/// attach methods return richer optional node records.
fn append_fleet_identity_contract_tail(frames: &mut Vec<WireFrame>) {
    let root_session_id = SessionId::new("session-fleet-identity-root");
    let child_session_id = SessionId::new("session-fleet-identity-child");
    let agent_id = AgentId::new("agent-fleet-identity-child");
    frames.extend([
        WireFrame::Welcome(Welcome {
            protocol: 1,
            instance_id: "instance-session-fleet-identity".into(),
            daemon_generation: 16,
            frame_limit: TEST_FRAME_LIMIT as u32,
            profile_id: "profile-1".into(),
            daemon_version: "0.0.966".into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: capabilities([Capability::View]),
            features: BTreeSet::from([FEATURE_SESSION_FLEET_IDENTITY_V1.to_owned()]),
            user_command_withheld: false,
            encoding: None,
        }),
        WireFrame::Response {
            request_id: RequestId::new("request-session-fleet-identity"),
            body: ResponseBody::SessionFleet {
                snapshot: SessionFleetSnapshot {
                    session_id: root_session_id.clone(),
                    generated_at_ms: 1_753_500_091_000,
                    node_limit: 512,
                    depth_limit: 32,
                    roots: vec![FleetNodeWire {
                        agent_id: agent_id.clone(),
                        session_id: child_session_id.clone(),
                        callsign: Some("jade-fox-a1b2c3".into()),
                        model: Some("gpt-5.6".into()),
                        provider: Some("openai".into()),
                        task: "review data flow".into(),
                        depth: 1,
                        parent_session_id: root_session_id.clone(),
                        parent_agent_id: None,
                        state: FleetAgentStateWire::Live,
                        metrics: None,
                        folded_children: 0,
                        children: Vec::new(),
                    }],
                    rollup: FleetRollupWire {
                        node_count: 1,
                        states: FleetStateCountsWire {
                            live: 1,
                            ..FleetStateCountsWire::default()
                        },
                        max_depth: 1,
                        metrics: FleetMetricsTotalsWire::default(),
                        metrics_complete: false,
                        complete: true,
                    },
                    truncated: false,
                },
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-session-descendants-identity"),
            body: ResponseBody::SessionDescendantsAttach {
                attachment_id: AttachmentId::new("descendants-identity"),
                baseline: haider_rpc::SessionDescendantBaselineWire {
                    session_id: root_session_id.clone(),
                    generated_at_ms: 1_753_500_091_000,
                    fanout: haider_rpc::DescendantFanoutWire {
                        requested_children: 8,
                        accepted_children: 8,
                        hard_limit: haider_rpc::DESCENDANT_STREAM_MAX_CHILDREN,
                    },
                    truncation: haider_rpc::DescendantTruncationWire {
                        truncated: false,
                        streamed_children: 1,
                        omitted_children: 0,
                        count_complete: true,
                    },
                    roots: vec![haider_rpc::DescendantStreamNodeWire {
                        session_id: child_session_id,
                        agent_id,
                        child_run_id: RunId::new("run-fleet-identity-child"),
                        parent_session_id: root_session_id,
                        parent_run_id: RunId::new("run-fleet-identity-parent"),
                        parent_branch_id: None,
                        parent_agent_id: None,
                        depth: 1,
                        callsign: Some("jade-fox-a1b2c3".into()),
                        model: Some("gpt-5.6".into()),
                        provider: Some("openai".into()),
                        task: "review data flow".into(),
                        state: FleetAgentStateWire::Live,
                        requested_after_seq: 0,
                        replay_through_seq: 4,
                        parent_anchors: haider_rpc::DescendantParentAnchorsWire::default(),
                        children: Vec::new(),
                    }],
                },
            },
        },
    ]);
}

/// K1 appends one request/success pair for owned direct-child cancellation.
/// Every preceding v0.0.966 frame remains byte-for-byte frozen.
fn append_agent_cancel_contract_tail(frames: &mut Vec<WireFrame>) {
    frames.extend([
        WireFrame::Request {
            request_id: RequestId::new("request-agent-cancel"),
            body: RequestBody::AgentCancel {
                command_id: CommandId::new("command-agent-cancel"),
                session_id: SessionId::new("session-parent"),
                worker_generation: 7,
                agent: AgentId::new("agent-child-7"),
            },
        },
        WireFrame::Response {
            request_id: RequestId::new("request-agent-cancel"),
            body: ResponseBody::AgentCancel {
                agent: AgentId::new("agent-child-7"),
                child_session_id: SessionId::new("session-child-7"),
                child_run_id: RunId::new("run-child-7"),
                status: CancelStatus::Accepted,
                terminal_seq: None,
            },
        },
    ]);
    debug_assert_eq!(FEATURE_AGENT_CANCEL_V1, "agent_cancel_v1");
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
        label: None,
        account_identity: None,
        created_at_ms: None,
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
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}
