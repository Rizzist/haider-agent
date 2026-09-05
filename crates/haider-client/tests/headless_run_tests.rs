#![allow(clippy::expect_used)]
#![cfg(unix)]
//! Daemon-backed headless transaction laws over a real Unix socket.

use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use haider_client::{
    EnsureError, EnsureOptions, HeadlessBlockingReason, HeadlessEvent, HeadlessFailureCode,
    HeadlessImageAttachment, HeadlessOutcome, HeadlessRunError, HeadlessRunRequest,
    HeadlessSessionConfig, ProfileEnv, ResolvedProfile, resolve_profile, run_headless,
    run_headless_with_session_config,
};
use haider_rpc::haider_protocol::EventPayload;
use haider_rpc::haider_protocol::credential::AuthMethod;
use haider_rpc::haider_protocol::envelope::{
    PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_rpc::haider_protocol::error::ErrorCode;
use haider_rpc::haider_protocol::ids::{
    ArtifactRef, DeviceId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_rpc::haider_protocol::item::{ItemEvent, TurnItem};
use haider_rpc::haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_rpc::haider_protocol::session::{
    SessionInteractionModeV1, SessionMetadataV1, SessionPermissionOverridesV1,
};
use haider_rpc::haider_protocol::state::{RunState, WaitReason};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, CancelStatus, Capability, CapabilitySet,
    DEFAULT_FRAME_LIMIT, LifecyclePhase, ProviderApiFamilyWire, ProviderAvailabilityWire,
    ProviderSummaryWire, RequestBody, RequestId, ResponseBody, SubmitDisposition,
    WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

const LIMIT: usize = DEFAULT_FRAME_LIMIT;
const BOUND: Duration = Duration::from_secs(5);

struct Peer {
    stream: UnixStream,
    decoder: uds_codec::Decoder,
    queued: VecDeque<WireFrame>,
}

impl Peer {
    async fn next(&mut self) -> WireFrame {
        loop {
            if let Some(frame) = self.queued.pop_front() {
                if let WireFrame::Ping { nonce } = frame {
                    self.write(&WireFrame::Pong { nonce }).await;
                    continue;
                }
                return frame;
            }
            let mut bytes = [0_u8; 8192];
            let read = self.stream.read(&mut bytes).await.expect("peer read");
            assert!(read > 0, "client closed before expected frame");
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "peer decode: {:?}", batch.error);
            self.queued.extend(batch.frames);
        }
    }

    /// Like [`Self::next`], but a clean client close yields `None`
    /// instead of a panic — for "nothing more arrives" assertions where
    /// finishing (and closing) is legal.
    async fn try_next(&mut self) -> Option<WireFrame> {
        loop {
            if let Some(frame) = self.queued.pop_front() {
                if let WireFrame::Ping { nonce } = frame {
                    self.write(&WireFrame::Pong { nonce }).await;
                    continue;
                }
                return Some(frame);
            }
            let mut bytes = [0_u8; 8192];
            let read = self.stream.read(&mut bytes).await.expect("peer read");
            if read == 0 {
                return None;
            }
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "peer decode: {:?}", batch.error);
            self.queued.extend(batch.frames);
        }
    }

    async fn binary_next(&mut self) -> haider_rpc::binary_artifact::Frame {
        self.decoder.set_binary_artifacts(true);
        loop {
            let mut bytes = [0_u8; 8192];
            let read = self.stream.read(&mut bytes).await.expect("binary read");
            assert_ne!(read, 0);
            let batch = self.decoder.push(&bytes[..read]);
            assert!(batch.error.is_none(), "binary decode: {:?}", batch.error);
            for frame in batch.frames {
                if let WireFrame::Ping { nonce } = frame {
                    self.write(&WireFrame::Pong { nonce }).await;
                } else {
                    panic!("ordinary request overtook binary upload");
                }
            }
            if let Some(frame) = batch.binary_artifacts.into_iter().next() {
                return frame;
            }
        }
    }

    async fn request(&mut self) -> (RequestId, RequestBody) {
        match self.next().await {
            WireFrame::Request { request_id, body } => (request_id, body),
            other => panic!("expected request, got {other:?}"),
        }
    }

    async fn write(&mut self, frame: &WireFrame) {
        let bytes = uds_codec::encode(frame, LIMIT).expect("peer encode");
        self.stream.write_all(&bytes).await.expect("peer write");
    }

    async fn respond(&mut self, request_id: RequestId, body: ResponseBody) {
        self.write(&WireFrame::Response { request_id, body }).await;
    }
}

fn profile() -> (tempfile::TempDir, ResolvedProfile) {
    let root = tempfile::Builder::new()
        .prefix("hhead")
        .tempdir_in("/tmp")
        .expect("short profile root");
    let profile = resolve_profile(&ProfileEnv {
        profile_dir: Some(root.path().join("profile")),
        home: None,
        user_profile: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve profile");
    std::fs::create_dir_all(&profile.runtime_dir).expect("runtime dir");
    (root, profile)
}

fn welcome(profile: &ResolvedProfile) -> Welcome {
    let mut features = haider_client::required_live_features();
    features.insert(haider_rpc::FEATURE_AUTONOMOUS_INTERACTION_V1.to_owned());
    features.insert(haider_rpc::FEATURE_SESSION_CONFIG_V1.to_owned());
    features.insert(haider_rpc::FEATURE_SESSION_CREATE_ADMISSION_V1.to_owned());
    features.insert(haider_rpc::FEATURE_SESSION_ACCOUNT_SELECT_V1.to_owned());
    features.insert(haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1.to_owned());
    features.insert(haider_rpc::FEATURE_SESSION_FAST_SELECT_V1.to_owned());
    Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: "headless-test-peer".into(),
        daemon_generation: 1,
        frame_limit: LIMIT as u32,
        profile_id: profile.profile_id.clone(),
        daemon_version: "0.0.36-test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features,
        user_command_withheld: false,
        encoding: None,
    }
}

fn spawn_peer<F, Fut>(profile: &ResolvedProfile, scenario: F) -> tokio::task::JoinHandle<()>
where
    F: FnOnce(Peer) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind peer");
    let welcome = welcome(profile);
    tokio::spawn(async move {
        scenario(accept_peer(&listener, welcome).await).await;
    })
}

async fn accept_peer(listener: &UnixListener, welcome: Welcome) -> Peer {
    let (mut stream, _) = listener.accept().await.expect("accept client");
    let mut decoder = uds_codec::Decoder::new(LIMIT);
    let mut bytes = [0_u8; 8192];
    let read = stream.read(&mut bytes).await.expect("read hello");
    let batch = decoder.push(&bytes[..read]);
    assert!(batch.error.is_none());
    assert!(matches!(batch.frames.first(), Some(WireFrame::Hello(_))));
    let encoded = uds_codec::encode(&WireFrame::Welcome(welcome), LIMIT).expect("welcome");
    stream.write_all(&encoded).await.expect("write welcome");
    Peer {
        stream,
        decoder,
        queued: batch.frames.into_iter().skip(1).collect(),
    }
}

async fn accept_create_and_attach(peer: &mut Peer) -> (SessionId, AttachmentId) {
    let (create_request, create) = peer.request().await;
    let RequestBody::SessionCreateWithPermissionOverrides {
        permission_overrides,
        interaction_mode,
        ..
    } = create
    else {
        panic!("headless runner must use additive session.create shape");
    };
    assert_eq!(permission_overrides, None);
    assert_eq!(interaction_mode, SessionInteractionModeV1::Autonomous);
    respond_create_and_attach(peer, create_request, "fake", "fake-model").await
}

async fn respond_create_and_attach(
    peer: &mut Peer,
    create_request: RequestId,
    provider: &str,
    model: &str,
) -> (SessionId, AttachmentId) {
    respond_create_and_attach_with_account(peer, create_request, provider, model, None).await
}

async fn respond_create_and_attach_with_account(
    peer: &mut Peer,
    create_request: RequestId,
    provider: &str,
    model: &str,
    account_alias: Option<&str>,
) -> (SessionId, AttachmentId) {
    let session_id = SessionId::new("headless-session");
    peer.respond(
        create_request,
        ResponseBody::SessionCreate {
            session_id: session_id.clone(),
            created_seq: 0,
            worker_generation: 7,
            metadata: SessionMetadataV1 {
                provider_base_url: None,
                provider_rebind_id: None,
                cwd: "/tmp".into(),
                provider: provider.into(),
                account_alias: account_alias.map(Into::into),
                model: model.into(),
                max_tokens: 4096,
                permission_overrides: None,
                interaction_mode: SessionInteractionModeV1::Autonomous,
                system_prompt_version: Some("test".into()),
                title: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                context_economy: Default::default(),
                created_at_ms: 1,
                agent_type: None,
            },
        },
    )
    .await;

    let (attach_request, attach) = peer.request().await;
    let RequestBody::SessionAttach {
        session_id: attached,
        after_seq,
        mode,
        ..
    } = attach
    else {
        panic!("submit overtook Control attach");
    };
    assert_eq!(attached, session_id);
    assert_eq!(after_seq, 0);
    assert_eq!(mode, AttachMode::Control);
    let attachment_id = AttachmentId::new("headless-attachment");
    peer.respond(
        attach_request,
        ResponseBody::SessionAttach {
            attachment_id: attachment_id.clone(),
            attach_state: AttachState {
                session_id: session_id.clone(),
                requested_after_seq: 0,
                replay_through_seq: 0,
                worker_generation: 7,
                authority_epoch: 1,
            },
        },
    )
    .await;
    peer.write(&WireFrame::AttachCaughtUp {
        attachment_id: attachment_id.clone(),
        high_water_seq: 0,
    })
    .await;
    (session_id, attachment_id)
}

async fn accept_submit(peer: &mut Peer, session_id: &SessionId) -> (RequestId, RunId) {
    let (request_id, submit) = peer.request().await;
    let RequestBody::TurnSubmitWithBranch {
        session_id: submitted,
        worker_generation,
        ..
    } = submit
    else {
        panic!("expected turn.submit after attach barrier, got {submit:?}");
    };
    assert_eq!(&submitted, session_id);
    assert_eq!(worker_generation, 7);
    (request_id, RunId::new("headless-run"))
}

fn envelope(
    session_id: &SessionId,
    run_id: &RunId,
    seq: u64,
    payload: EventPayload,
) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("headless-event-{seq}")),
        seq,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("headless-peer"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(payload).expect("payload").into(),
    }
}

async fn send_event(peer: &mut Peer, attachment_id: &AttachmentId, envelope: RawEnvelope) {
    peer.write(&WireFrame::Event {
        attachment_id: attachment_id.clone(),
        session_id: envelope.session_id.clone(),
        envelope,
    })
    .await;
}

fn request(timeout: Option<Duration>) -> HeadlessRunRequest {
    HeadlessRunRequest {
        cwd: "/tmp".into(),
        prompt: "hello".into(),
        attachments: Vec::new(),
        durable_attachments: Vec::new(),
        provider: Some("fake".into()),
        model: Some("fake-model".into()),
        max_tokens: 4096,
        budget: haider_rpc::haider_protocol::headless::RunBudgetV1::default(),
        seed: None,
        replay_of: None,
        journal_pin: false,
        detached: false,
        permission_overrides: SessionPermissionOverridesV1::default(),
        trust_hooks: false,
        timeout,
        terminal_grace: Duration::from_millis(250),
    }
}

async fn run_with_events(
    profile: ResolvedProfile,
    request: HeadlessRunRequest,
    capacity: usize,
    consumer_delay: Duration,
) -> (haider_client::HeadlessRunResult, Vec<HeadlessEvent>) {
    let (sender, mut receiver) = mpsc::channel(capacity);
    let task = tokio::spawn(async move {
        run_headless(&profile, EnsureOptions::default(), request, sender).await
    });
    tokio::time::sleep(consumer_delay).await;
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        events
    });
    let result = tokio::time::timeout(BOUND, task)
        .await
        .expect("runner bound")
        .expect("runner task")
        .expect("headless result");
    let events = collector.await.expect("collector");
    (result, events)
}

/// R2-05 hold-out pin: attach and headless.start remain separate correlated
/// requests. The start frame cannot be sent until both the attach receipt and
/// its caught-up barrier have arrived; the attempted pipeline regressed and
/// was reverted.
#[tokio::test]
async fn r2_05_attach_then_start_are_ordered_separate_requests_with_receipts() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind ordered peer");
    let mut advertised = welcome(&profile);
    advertised
        .features
        .insert(haider_rpc::FEATURE_HEADLESS_RUN_V1.to_owned());
    let peer = tokio::spawn(async move {
        let mut peer = accept_peer(&listener, advertised).await;
        let (create_request, create) = peer.request().await;
        assert!(matches!(
            create,
            RequestBody::SessionCreateWithPermissionOverrides { .. }
        ));
        let session_id = SessionId::new("r2-05-session");
        peer.respond(
            create_request,
            ResponseBody::SessionCreate {
                session_id: session_id.clone(),
                created_seq: 0,
                worker_generation: 7,
                metadata: SessionMetadataV1 {
                    provider_base_url: None,
                    provider_rebind_id: None,
                    cwd: "/tmp".into(),
                    provider: "fake".into(),
                    account_alias: None,
                    model: "fake-model".into(),
                    max_tokens: 4_096,
                    permission_overrides: None,
                    interaction_mode: SessionInteractionModeV1::Autonomous,
                    system_prompt_version: Some("test".into()),
                    title: None,
                    effort: None,
                    fast: false,
                    cache_policy: Default::default(),
                    context_economy: Default::default(),
                    created_at_ms: 1,
                    agent_type: None,
                },
            },
        )
        .await;

        let (attach_request, attach) = peer.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                mode: AttachMode::Control,
                ..
            }
        ));
        assert!(peer.queued.is_empty(), "start was coalesced with attach");
        // BOUND / 50 = 100 ms: a pipelined frame is already writable in the
        // same local-socket scheduling turn, while the full test owns 5 s.
        assert!(
            tokio::time::timeout(BOUND / 50, peer.next()).await.is_err(),
            "start arrived before the attach receipt"
        );
        let attachment_id = AttachmentId::new("r2-05-attachment");
        peer.respond(
            attach_request.clone(),
            ResponseBody::SessionAttach {
                attachment_id: attachment_id.clone(),
                attach_state: AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: 0,
                    replay_through_seq: 0,
                    worker_generation: 7,
                    authority_epoch: 1,
                },
            },
        )
        .await;
        assert!(
            tokio::time::timeout(BOUND / 50, peer.next()).await.is_err(),
            "start arrived before attach caught-up"
        );
        peer.write(&WireFrame::AttachCaughtUp {
            attachment_id: attachment_id.clone(),
            high_water_seq: 0,
        })
        .await;

        let (start_request, start) = peer.request().await;
        assert_ne!(
            start_request, attach_request,
            "requests need distinct receipts"
        );
        let RequestBody::HeadlessRunStart {
            session_id: started_session,
            worker_generation,
            ..
        } = start
        else {
            panic!("expected separate headless.run.start frame");
        };
        assert_eq!(started_session, session_id);
        assert_eq!(worker_generation, 7);
        let run_id = RunId::new("r2-05-run");
        peer.respond(
            start_request,
            ResponseBody::HeadlessRunStart {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
    });

    let mut run = request(None);
    run.journal_pin = true;
    let (result, _events) = run_with_events(profile, run, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert_eq!(result.terminal_seq, Some(1));
}

#[tokio::test]
async fn headless_pin_feature_is_required_before_session_mutation() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        assert!(peer.try_next().await.is_none());
    });
    let mut run = request(None);
    run.journal_pin = true;
    let (sender, _receiver) = mpsc::channel(1);
    let error = run_headless(&profile, EnsureOptions::default(), run, sender)
        .await
        .expect_err("missing headless feature");
    assert!(matches!(
        error,
        HeadlessRunError::Ensure(EnsureError::MissingFeatures { ref missing, .. })
            if missing.contains(haider_rpc::FEATURE_HEADLESS_RUN_V1)
    ));
    peer.await.expect("peer");
}

#[tokio::test]
async fn budget_feature_is_required_before_session_mutation() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind budget peer");
    let mut advertised = welcome(&profile);
    advertised
        .features
        .insert(haider_rpc::FEATURE_HEADLESS_RUN_V1.to_owned());
    let peer = tokio::spawn(async move {
        let mut peer = accept_peer(&listener, advertised).await;
        assert!(peer.try_next().await.is_none());
    });
    let mut run = request(None);
    run.journal_pin = true;
    run.budget.max_tokens = Some(1);
    let (sender, _receiver) = mpsc::channel(1);
    let error = run_headless(&profile, EnsureOptions::default(), run, sender)
        .await
        .expect_err("missing budget feature");
    assert!(matches!(
        error,
        HeadlessRunError::Ensure(EnsureError::MissingFeatures { ref missing, .. })
            if missing.contains(haider_rpc::FEATURE_RUN_BUDGET_V1)
    ));
    peer.await.expect("peer");
}

/// MUTATION CHECK: rely on the older, broader permission-overrides feature for
/// the additive read-only deny. Expected RUNTIME failure: the client sends a
/// session.create to a daemon that may ignore `read_only` while accepting the
/// legacy allow fields.
#[tokio::test]
async fn read_only_feature_is_required_before_session_mutation() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind read-only peer");
    let mut advertised = welcome(&profile);
    advertised
        .features
        .insert(haider_rpc::FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned());
    let peer = tokio::spawn(async move {
        let mut peer = accept_peer(&listener, advertised).await;
        assert!(peer.try_next().await.is_none());
    });
    let mut run = request(None);
    run.permission_overrides = SessionPermissionOverridesV1 {
        read_only: true,
        allow_writes: true,
        allow_exec: true,
        allow_mobile: false,
        auto_allow: true,
    };
    let (sender, _receiver) = mpsc::channel(1);
    let error = run_headless(&profile, EnsureOptions::default(), run, sender)
        .await
        .expect_err("missing read-only feature");
    assert!(matches!(
        error,
        HeadlessRunError::Ensure(EnsureError::MissingFeatures { ref missing, .. })
            if missing == &BTreeSet::from([haider_rpc::FEATURE_SESSION_READ_ONLY_V1.to_owned()])
    ));
    peer.await.expect("peer");
}

fn provider_summary_fixture(
    provider: &str,
    default_model: Option<&str>,
    models: &[&str],
) -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: provider.into(),
        api_family: ProviderApiFamilyWire::OpenAiResponses,
        endpoint: Some("https://example.test/v1/responses".into()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: models.iter().map(|model| (*model).into()).collect(),
        model_details: Vec::new(),
        inventory_fetched_at_ms: None,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: vec![AuthMethod::OAuth],
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: default_model.map(Into::into),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

async fn serve_flagless_done(peer: &mut Peer, expected_provider: &str, expected_model: &str) {
    let (create_request, create_body) = peer.request().await;
    let RequestBody::SessionCreateWithPermissionOverrides {
        provider: created_provider,
        model: created_model,
        permission_overrides,
        resolve_provider,
        resolve_model,
        effort,
        fast,
        ..
    } = create_body
    else {
        panic!("headless admission must begin with session.create");
    };
    assert!(created_provider.is_empty());
    assert!(created_model.is_empty());
    assert!(resolve_provider);
    assert!(resolve_model);
    assert_eq!(permission_overrides, None);
    assert_eq!(effort, None);
    assert_eq!(fast, None);
    let (session_id, attachment_id) =
        respond_create_and_attach(peer, create_request, expected_provider, expected_model).await;
    let (submit_request, run_id) = accept_submit(peer, &session_id).await;
    peer.respond(
        submit_request,
        ResponseBody::TurnSubmit {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            accepted_seq: 1,
            worker_generation: 7,
            disposition: SubmitDisposition::Started,
        },
    )
    .await;
    send_event(
        peer,
        &attachment_id,
        envelope(
            &session_id,
            &run_id,
            1,
            EventPayload::RunState(RunState::Done),
        ),
    )
    .await;
}

/// MUTATION CHECK: restore account.list/provider.list bootstrap or resolve
/// provider/model client-side. Expected runtime failure: the first request is
/// not the one atomic create body pinned below.
#[tokio::test]
async fn flagless_bootstrap_creates_on_active_provider_and_published_default_model() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        serve_flagless_done(&mut peer, "openai-oauth", "gpt-active-default").await;
    });
    let mut run = request(None);
    run.provider = None;
    run.model = None;

    let (result, _events) = run_with_events(profile, run, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.provider, "openai-oauth");
    assert_eq!(result.model, "gpt-active-default");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
}

/// MUTATION CHECK: resolve the provider/model client-side, drop the exact
/// alias, or accept metadata for a different account. The first atomic create
/// body or the response validation then fails.
#[tokio::test]
async fn account_only_bootstrap_uses_that_accounts_daemon_default_model() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (create_request, create_body) = peer.request().await;
        let RequestBody::SessionCreateWithPermissionOverrides {
            provider,
            model,
            account_alias,
            resolve_provider,
            resolve_model,
            ..
        } = create_body
        else {
            panic!("account-only admission must begin with session.create");
        };
        assert!(provider.is_empty() && model.is_empty());
        assert!(resolve_provider && resolve_model);
        assert_eq!(
            account_alias.as_ref().map(|alias| alias.as_str()),
            Some("work")
        );
        let (session_id, attachment_id) = respond_create_and_attach_with_account(
            &mut peer,
            create_request,
            "openai-oauth",
            "gpt-work-default",
            Some("work"),
        )
        .await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
    });
    let mut run = request(None);
    run.provider = None;
    run.model = None;
    let (sender, mut receiver) = mpsc::channel(8);
    let result = run_headless_with_session_config(
        &profile,
        EnsureOptions::default(),
        run,
        HeadlessSessionConfig {
            account: Some("work".into()),
            ..HeadlessSessionConfig::default()
        },
        sender,
    )
    .await
    .expect("account-only run");
    while receiver.recv().await.is_some() {}
    peer.await.expect("peer");
    assert_eq!(result.provider, "openai-oauth");
    assert_eq!(result.model, "gpt-work-default");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
}

/// MUTATION CHECK: restore post-create select_effort/select_fast requests.
/// Expected runtime failure: the atomic create body omits either initial
/// tuning coordinate.
#[tokio::test]
async fn initial_headless_tuning_is_part_of_atomic_create() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (request_id, body) = peer.request().await;
        let RequestBody::SessionCreateWithPermissionOverrides {
            provider,
            model,
            resolve_provider,
            resolve_model,
            effort,
            fast,
            ..
        } = body
        else {
            panic!("headless admission must begin with session.create");
        };
        assert_eq!(provider, "fake");
        assert_eq!(model, "fake-model");
        assert!(!resolve_provider && !resolve_model);
        assert_eq!(effort.as_deref(), Some("high"));
        assert_eq!(fast, Some(true));
        peer.respond(
            request_id,
            ResponseBody::Error {
                code: "fixture_stop".into(),
                message: "coordinates captured".into(),
                retryable: false,
                data: None,
            },
        )
        .await;
    });
    let (sender, _receiver) = mpsc::channel(1);
    let error = run_headless_with_session_config(
        &profile,
        EnsureOptions::default(),
        request(None),
        HeadlessSessionConfig {
            effort: Some("high".into()),
            fast: Some(true),
            ..HeadlessSessionConfig::default()
        },
        sender,
    )
    .await
    .expect_err("fixture refuses after inspecting create");
    peer.await.expect("peer");
    assert!(matches!(
        error,
        HeadlessRunError::Rpc {
            stage: "session.create",
            ref code,
            ..
        } if code == "fixture_stop"
    ));
}

/// MUTATION CHECK: stop routing `HeadlessRunRequest.model` through the shared
/// selector or treat a slash as unconditionally literal. Expected runtime
/// failure: session.create receives the provider-prefixed selector instead of
/// the endpoint's bare wire model id.
#[tokio::test]
async fn configured_provider_model_selector_reaches_create_as_bare_wire_id() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (provider_request, provider_body) = peer.request().await;
        assert_eq!(
            provider_body,
            RequestBody::ProviderList {
                provider: Some("bench-proxy".into())
            }
        );
        peer.respond(
            provider_request,
            ResponseBody::ProviderList {
                providers: vec![provider_summary_fixture(
                    "bench-proxy",
                    Some("deepseek-v4-flash"),
                    &["canonical-other"],
                )],
                revision: 1,
                availability: None,
            },
        )
        .await;
        let (create_request, create_body) = peer.request().await;
        let RequestBody::SessionCreateWithPermissionOverrides {
            provider,
            model,
            permission_overrides,
            ..
        } = create_body
        else {
            panic!("selector bootstrap must be followed by session.create");
        };
        assert_eq!(provider, "bench-proxy");
        assert_eq!(model, "deepseek-v4-flash");
        assert_eq!(permission_overrides, None);
        let (session_id, attachment_id) = respond_create_and_attach(
            &mut peer,
            create_request,
            "bench-proxy",
            "deepseek-v4-flash",
        )
        .await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
    });
    let mut run = request(None);
    run.provider = Some("bench-proxy".into());
    run.model = Some("bench-proxy/deepseek-v4-flash".into());

    let (result, _events) = run_with_events(profile, run, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.provider, "bench-proxy");
    assert_eq!(result.model, "deepseek-v4-flash");
}

/// MUTATION CHECK: restore the client-side account/provider collection
/// bootstrap. Expected runtime failure: the first frame is not the atomic
/// create, or the daemon's typed refusal is not preserved.
#[tokio::test]
async fn daemon_resolved_default_without_published_model_is_typed() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (request_id, body) = peer.request().await;
        let RequestBody::SessionCreateWithPermissionOverrides {
            provider,
            model,
            resolve_provider,
            resolve_model,
            ..
        } = body
        else {
            panic!("headless admission must begin with session.create");
        };
        assert!(provider.is_empty() && model.is_empty());
        assert!(resolve_provider && resolve_model);
        peer.respond(
            request_id,
            ResponseBody::Error {
                code: haider_client::ERROR_CODE_NO_DEFAULT_MODEL.into(),
                message: "provider `openai-oauth` publishes no default model".into(),
                retryable: false,
                data: None,
            },
        )
        .await;
    });
    let mut run = request(None);
    run.provider = None;
    run.model = None;
    let (sender, _receiver) = mpsc::channel(1);
    let error = tokio::time::timeout(
        BOUND,
        run_headless(&profile, EnsureOptions::default(), run, sender),
    )
    .await
    .expect("runner bound")
    .expect_err("catalog without default must refuse");
    peer.await.expect("peer");
    assert!(matches!(
        error,
        HeadlessRunError::Bootstrap {
            stage: "session.create",
            code: haider_client::ERROR_CODE_NO_DEFAULT_MODEL,
            retryable: false,
            ..
        }
    ));
}

/// MUTATION CHECK: restore account.list bootstrap or lose the daemon's typed
/// no-active-account refusal. Expected runtime failure: the first request or
/// the preserved error coordinates differ.
#[tokio::test]
async fn flagless_bootstrap_without_active_account_is_typed() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (request_id, body) = peer.request().await;
        let RequestBody::SessionCreateWithPermissionOverrides {
            provider,
            model,
            resolve_provider,
            resolve_model,
            ..
        } = body
        else {
            panic!("headless admission must begin with session.create");
        };
        assert!(provider.is_empty() && model.is_empty());
        assert!(resolve_provider && resolve_model);
        peer.respond(
            request_id,
            ResponseBody::Error {
                code: haider_client::ERROR_CODE_NO_ACTIVE_ACCOUNT.into(),
                message: "no active daemon account is configured".into(),
                retryable: false,
                data: None,
            },
        )
        .await;
    });
    let mut run = request(None);
    run.provider = None;
    run.model = None;
    let (sender, _receiver) = mpsc::channel(1);
    let error = tokio::time::timeout(
        BOUND,
        run_headless(&profile, EnsureOptions::default(), run, sender),
    )
    .await
    .expect("runner bound")
    .expect_err("no active account must refuse");
    peer.await.expect("peer");
    assert!(matches!(
        error,
        HeadlessRunError::Bootstrap {
            stage: "session.create",
            code: haider_client::ERROR_CODE_NO_ACTIVE_ACCOUNT,
            retryable: false,
            ..
        }
    ));
}

/// MUTATION CHECK: submit before Control attach or discard frames received
/// before the submit response. Expected RUNTIME failure: the peer observes
/// the wrong method order or the early assistant/Done facts disappear.
#[tokio::test]
async fn control_attach_precedes_submit_and_pre_response_events_correlate() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Queued),
            ),
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("answer"),
                    item: TurnItem::AgentMessage {
                        text: "daemon answer".into(),
                    },
                }),
            ),
        )
        .await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                3,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
    });

    let (result, events) = run_with_events(profile, request(None), 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert_eq!(result.response, Some("daemon answer".into()));
    assert_eq!(result.terminal_seq, Some(3));
    // Two-phase announcement law: resolution-time first (created_seq from
    // session.create — the mock's 0), the acceptance-time refinement second.
    assert!(matches!(
        events.first(),
        Some(HeadlessEvent::Accepted {
            session_id,
            head_seq: 0,
        }) if session_id.as_str() == "headless-session"
    ));
    assert!(matches!(
        events.get(1),
        Some(HeadlessEvent::Accepted { head_seq: 1, .. })
    ));
    assert_eq!(events.len(), 5);
}

/// MUTATION CHECK: omit the `--trust-hooks` feature requirement or require it
/// for ordinary runs. Expected RUNTIME failure: the exact set difference is
/// empty or contains something other than the additive hooks feature.
#[test]
fn run_scoped_hook_trust_requires_only_the_additive_hooks_feature() {
    let ordinary =
        haider_client::required_headless_features(SessionPermissionOverridesV1::default());
    let trusted = haider_client::required_headless_features_with_hook_trust(
        SessionPermissionOverridesV1::default(),
    );
    assert_eq!(
        trusted
            .difference(&ordinary)
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([haider_rpc::FEATURE_HOOKS_V1.to_owned()])
    );
}

/// MUTATION CHECK: reduce replayed run facts before the idempotent retry
/// recovers its run_id, mint a new submit command, or turn disconnect into
/// cancellation/success. Expected RUNTIME failure: retry identity changes or
/// the replayed Done is not correlated after the response-losing reconnect.
#[tokio::test]
async fn submit_response_loss_reconnects_buffers_replay_and_retries_same_command() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let handshake = welcome(&profile);
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
        let (session_id, _) = accept_create_and_attach(&mut first).await;
        let (_, submit) = first.request().await;
        let RequestBody::TurnSubmitWithBranch {
            command_id: original_command,
            ..
        } = submit
        else {
            panic!("first connection expected submit");
        };
        drop(first);

        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let attachment_id = AttachmentId::new("retry-attachment");
        second
            .respond(
                attach_request,
                ResponseBody::SessionAttach {
                    attachment_id: attachment_id.clone(),
                    attach_state: AttachState {
                        session_id: session_id.clone(),
                        requested_after_seq: 0,
                        replay_through_seq: 2,
                        worker_generation: 8,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        let run_id = RunId::new("response-lost-run");
        send_event(
            &mut second,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Queued),
            ),
        )
        .await;
        send_event(
            &mut second,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id,
                high_water_seq: 2,
            })
            .await;

        let (retry_request, retry) = second.request().await;
        let RequestBody::TurnSubmitWithBranch {
            command_id: retry_command,
            ..
        } = retry
        else {
            panic!("second connection expected submit retry");
        };
        assert_eq!(retry_command, original_command);
        second
            .respond(
                retry_request,
                ResponseBody::TurnSubmit {
                    session_id,
                    run_id,
                    accepted_seq: 1,
                    worker_generation: 8,
                    disposition: SubmitDisposition::Started,
                },
            )
            .await;
    });

    let (result, events) = run_with_events(profile, request(None), 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert_eq!(result.terminal_seq, Some(2));
    assert_eq!(events.len(), 4);
}

/// MUTATION CHECK: create the session before uploading, omit uploaded refs
/// from submit, or mint a replacement submit after response loss. Expected
/// RUNTIME failure: the peer observes the wrong ordering, attachment blocks,
/// or durable command identity across the reconnect.
#[tokio::test]
async fn headless_attach_uploads_then_submits_with_durable_identity() {
    attach_upload_resume_case(false).await;
}

#[tokio::test]
async fn headless_binary_upload_disconnect_retries_snapshot_then_resumes_durable_submit() {
    attach_upload_resume_case(true).await;
}

async fn attach_upload_resume_case(binary: bool) {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let mut handshake = welcome(&profile);
    handshake
        .features
        .insert(haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned());
    if binary {
        handshake
            .features
            .insert(haider_rpc::binary_artifact::FEATURE.to_owned());
    }
    let image_bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let expected_ref = ArtifactRef::new(format!("blake3:{}", blake3::hash(&image_bytes).to_hex()));
    let peer_ref = expected_ref.clone();
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
        if binary {
            use haider_rpc::binary_artifact::Frame;
            let Frame::Begin {
                request_id,
                bytes,
                digest,
            } = first.binary_next().await
            else {
                panic!("begin")
            };
            assert_eq!(bytes, 8);
            assert_eq!(digest, peer_ref);
            first
                .respond(request_id, ResponseBody::ArtifactPutProgress { bytes: 0 })
                .await;
            let Frame::Chunk { bytes, offset, .. } = first.binary_next().await else {
                panic!("chunk")
            };
            assert_eq!(offset, 0);
            assert_eq!(&*bytes, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
            // Lose the chunk ACK. The public headless reconnect seam must
            // restart upload using the immutable original bytes.
            drop(first);
            first = accept_peer(&listener, handshake.clone()).await;
            let Frame::Begin {
                request_id,
                bytes,
                digest,
            } = first.binary_next().await
            else {
                panic!("retry begin")
            };
            assert_eq!(bytes, 8);
            assert_eq!(digest, peer_ref);
            first
                .respond(request_id, ResponseBody::ArtifactPutProgress { bytes: 0 })
                .await;
            let Frame::Chunk {
                request_id,
                bytes,
                offset,
            } = first.binary_next().await
            else {
                panic!("retry chunk")
            };
            assert_eq!(offset, 0);
            assert_eq!(&*bytes, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
            first
                .respond(request_id, ResponseBody::ArtifactPutProgress { bytes: 8 })
                .await;
            let Frame::Finish { request_id } = first.binary_next().await else {
                panic!("finish")
            };
            first
                .respond(
                    request_id,
                    ResponseBody::ArtifactPut {
                        artifact: peer_ref.clone(),
                        bytes: 8,
                    },
                )
                .await;
        } else {
            let (put_request, put) = first.request().await;
            let RequestBody::ArtifactPut { data_base64 } = put else {
                panic!("artifact.put must precede session.create, got {put:?}");
            };
            assert_eq!(data_base64, "iVBORw0KGgo=");
            first
                .respond(
                    put_request,
                    ResponseBody::ArtifactPut {
                        artifact: peer_ref.clone(),
                        bytes: 8,
                    },
                )
                .await;
        }

        let (session_id, _) = accept_create_and_attach(&mut first).await;
        let (_, submit) = first.request().await;
        let original_submit = submit.clone();
        let RequestBody::TurnSubmitWithBranch {
            command_id: original_command,
            attachments: original_attachments,
            ..
        } = submit
        else {
            panic!("first connection expected submit");
        };
        assert_eq!(original_attachments.len(), 1);
        assert_eq!(
            original_attachments[0],
            haider_rpc::haider_protocol::tool::AttachmentBlock::Image {
                artifact: peer_ref.clone(),
                mime: "image/png".into(),
                width: None,
                height: None,
            }
        );
        drop(first);

        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let attachment_id = AttachmentId::new("image-retry-attachment");
        second
            .respond(
                attach_request,
                ResponseBody::SessionAttach {
                    attachment_id: attachment_id.clone(),
                    attach_state: AttachState {
                        session_id: session_id.clone(),
                        requested_after_seq: 0,
                        replay_through_seq: 2,
                        worker_generation: 8,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        let run_id = RunId::new("image-response-lost-run");
        send_event(
            &mut second,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Queued),
            ),
        )
        .await;
        send_event(
            &mut second,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id,
                high_water_seq: 2,
            })
            .await;

        let (retry_request, retry) = second.request().await;
        assert_eq!(retry, original_submit, "retry must resend the whole body");
        let RequestBody::TurnSubmitWithBranch {
            command_id: retry_command,
            attachments: retry_attachments,
            ..
        } = retry
        else {
            panic!("second connection expected submit retry");
        };
        assert_eq!(retry_command, original_command);
        assert_eq!(retry_attachments, original_attachments);
        second
            .respond(
                retry_request,
                ResponseBody::TurnSubmit {
                    session_id,
                    run_id,
                    accepted_seq: 1,
                    worker_generation: 8,
                    disposition: SubmitDisposition::Started,
                },
            )
            .await;
    });

    let mut with_image = request(None);
    with_image
        .attachments
        .push(haider_client::HeadlessAttachment::Image(
            HeadlessImageAttachment {
                bytes: image_bytes,
                mime: "image/png".into(),
            },
        ));
    let (result, events) = run_with_events(profile, with_image, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert_eq!(result.attachments, vec![expected_ref]);
    assert_eq!(events.len(), 4);
}

/// MUTATION CHECK: begin the wall clock only after submit correlation or wait
/// forever for a live peer that withholds the create response. Expected
/// RUNTIME failure: this exceeds the bound or does not return the typed
/// timeout_before_acceptance error.
#[tokio::test]
async fn preacceptance_response_wait_obeys_wall_clock_timeout() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (_, create) = peer.request().await;
        assert!(matches!(
            create,
            RequestBody::SessionCreateWithPermissionOverrides { .. }
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let (sender, _receiver) = mpsc::channel(4);
    let error = tokio::time::timeout(
        BOUND,
        run_headless(
            &profile,
            EnsureOptions::default(),
            request(Some(Duration::from_millis(20))),
            sender,
        ),
    )
    .await
    .expect("runner bound")
    .expect_err("preacceptance timeout");
    assert!(matches!(
        error,
        HeadlessRunError::Rpc {
            ref code,
            stage: "session.create",
            ..
        } if code == "timeout_before_acceptance"
    ));
    peer.await.expect("peer");
}

/// MUTATION CHECK: let a healthy socket withhold the submit response past the
/// wall clock, or return timeout without resolving the stable submit/cancel
/// commands. Expected RUNTIME failure: no reconnect occurs, command identity
/// changes, cancellation is absent, or the final forced outcome is not Timeout.
#[tokio::test]
async fn withheld_submit_response_is_recovered_and_durably_cancelled() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let handshake = welcome(&profile);
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
        let (session_id, _) = accept_create_and_attach(&mut first).await;
        let (_, submit) = first.request().await;
        let RequestBody::TurnSubmitWithBranch {
            command_id: original_command,
            ..
        } = submit
        else {
            panic!("expected first submit");
        };
        // Keep the transport healthy but deliberately never answer submit.
        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let attachment_id = AttachmentId::new("timed-submit-recovery");
        second
            .respond(
                attach_request,
                ResponseBody::SessionAttach {
                    attachment_id: attachment_id.clone(),
                    attach_state: AttachState {
                        session_id: session_id.clone(),
                        requested_after_seq: 0,
                        replay_through_seq: 0,
                        worker_generation: 7,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id: attachment_id.clone(),
                high_water_seq: 0,
            })
            .await;
        let (retry_request, retry) = second.request().await;
        let RequestBody::TurnSubmitWithBranch {
            command_id: retry_command,
            ..
        } = retry
        else {
            panic!("expected submit recovery");
        };
        assert_eq!(retry_command, original_command);
        let run_id = RunId::new("timed-submit-run");
        second
            .respond(
                retry_request,
                ResponseBody::TurnSubmit {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    accepted_seq: 1,
                    worker_generation: 7,
                    disposition: SubmitDisposition::Started,
                },
            )
            .await;
        let (cancel_request, cancel) = second.request().await;
        assert!(matches!(cancel, RequestBody::TurnCancel { .. }));
        second
            .respond(
                cancel_request,
                ResponseBody::TurnCancel {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    status: CancelStatus::Accepted,
                    terminal_seq: None,
                },
            )
            .await;
        send_event(
            &mut second,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Cancelled),
            ),
        )
        .await;
        drop(first);
    });

    let mut timed = request(Some(Duration::from_millis(20)));
    timed.terminal_grace = Duration::from_millis(250);
    let (result, _) = run_with_events(profile, timed, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Timeout);
    assert_eq!(result.terminal_seq, Some(1));
}

/// MUTATION CHECK: advance the cursor across a gap, emit duplicates, or use
/// best-effort output sends. Expected RUNTIME failure: replay does not start
/// at seq 1 or the saturated one-slot consumer observes missing/duplicate
/// durable sequences.
#[tokio::test]
async fn duplicate_and_gap_replay_is_lossless_under_output_backpressure() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        let first = envelope(
            &session_id,
            &run_id,
            1,
            EventPayload::RunState(RunState::Queued),
        );
        send_event(&mut peer, &attachment_id, first.clone()).await;
        send_event(&mut peer, &attachment_id, first).await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                3,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;

        let (detach_request, detach) = peer.request().await;
        assert!(matches!(detach, RequestBody::SessionDetach { .. }));
        peer.respond(
            detach_request,
            ResponseBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        )
        .await;
        let (attach_request, attach) = peer.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 1,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let replay_attachment = AttachmentId::new("headless-replay");
        peer.respond(
            attach_request,
            ResponseBody::SessionAttach {
                attachment_id: replay_attachment.clone(),
                attach_state: AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: 1,
                    replay_through_seq: 3,
                    worker_generation: 7,
                    authority_epoch: 1,
                },
            },
        )
        .await;
        send_event(
            &mut peer,
            &replay_attachment,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("replayed-answer"),
                    item: TurnItem::AgentMessage {
                        text: "replayed".into(),
                    },
                }),
            ),
        )
        .await;
        send_event(
            &mut peer,
            &replay_attachment,
            envelope(
                &session_id,
                &run_id,
                3,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
        peer.write(&WireFrame::AttachCaughtUp {
            attachment_id: replay_attachment,
            high_water_seq: 3,
        })
        .await;
    });

    let (result, events) =
        run_with_events(profile, request(None), 1, Duration::from_millis(40)).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert_eq!(result.response, Some("replayed".into()));
    let seqs = events
        .into_iter()
        .filter_map(|event| match event {
            HeadlessEvent::Envelope(envelope) => Some(envelope.seq),
            HeadlessEvent::Terminal(terminal) => Some(terminal.envelope.seq),
            HeadlessEvent::Accepted { .. } | HeadlessEvent::PermissionDenied(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(seqs, vec![1, 2, 3]);
}

/// MUTATION CHECK: ignore the daemon's `Lagged` pressure signal, reattach
/// from anything but the fully-applied cursor, or trust queued live frames
/// across the recovery. Expected RUNTIME failure: the runner never
/// reattaches, replays from the wrong sequence, or omits a durable
/// sequence from the presentation stream.
#[tokio::test]
async fn lagged_pressure_recovers_every_durable_sequence() {
    // The runner's internal forwarding keeps pace with any externally
    // paced blast (bounded backpressure, never silent loss), so channel
    // saturation cannot be forced from outside — the daemon's OWN
    // `Lagged` frame is the protocol's pressure signal and drives the
    // same cursor recovery deterministically.
    const TERMINAL_SEQ: u64 = 10;

    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;

        for seq in 1..=5_u64 {
            send_event(
                &mut peer,
                &attachment_id,
                envelope(
                    &session_id,
                    &run_id,
                    seq,
                    EventPayload::RunState(RunState::Thinking),
                ),
            )
            .await;
        }
        peer.write(&WireFrame::Lagged {
            attachment_id: attachment_id.clone(),
            last_queued_seq: TERMINAL_SEQ,
        })
        .await;

        let (detach_request, detach) = peer.request().await;
        assert!(matches!(detach, RequestBody::SessionDetach { .. }));
        peer.respond(
            detach_request,
            ResponseBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        )
        .await;
        let (attach_request, attach) = peer.request().await;
        let RequestBody::SessionAttach {
            after_seq,
            mode: AttachMode::Control,
            sealed_replay: false,
            ..
        } = attach
        else {
            panic!("expected cursor recovery attach");
        };
        assert_eq!(after_seq, 5, "reattach resumes at the fully-applied cursor");
        let replay_attachment = AttachmentId::new("lagged-replay");
        peer.respond(
            attach_request,
            ResponseBody::SessionAttach {
                attachment_id: replay_attachment.clone(),
                attach_state: AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: after_seq,
                    replay_through_seq: TERMINAL_SEQ,
                    worker_generation: 7,
                    authority_epoch: 1,
                },
            },
        )
        .await;
        for seq in (after_seq + 1)..=TERMINAL_SEQ {
            let state = if seq == TERMINAL_SEQ {
                RunState::Done
            } else {
                RunState::Thinking
            };
            send_event(
                &mut peer,
                &replay_attachment,
                envelope(&session_id, &run_id, seq, EventPayload::RunState(state)),
            )
            .await;
        }
        peer.write(&WireFrame::AttachCaughtUp {
            attachment_id: replay_attachment,
            high_water_seq: TERMINAL_SEQ,
        })
        .await;
    });

    let (result, events) = run_with_events(profile, request(None), 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    let seqs = events
        .into_iter()
        .filter_map(|event| match event {
            HeadlessEvent::Envelope(envelope) => Some(envelope.seq),
            HeadlessEvent::Terminal(terminal) => Some(terminal.envelope.seq),
            HeadlessEvent::Accepted { .. } | HeadlessEvent::PermissionDenied(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(seqs, (1..=TERMINAL_SEQ).collect::<Vec<_>>());
}

/// MUTATION CHECK: omit the active run/grace deadline from cursor recovery.
/// Expected RUNTIME failure: a peer that answers attach but withholds
/// `AttachCaughtUp` hangs forever instead of ending with typed unconfirmed
/// cancellation after the bounded recovery attempt.
#[tokio::test]
async fn withheld_recovery_barrier_cannot_defeat_run_and_grace_deadlines() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::RunState(RunState::Thinking),
            ),
        )
        .await;
        let (detach_request, detach) = peer.request().await;
        assert!(matches!(detach, RequestBody::SessionDetach { .. }));
        peer.respond(
            detach_request,
            ResponseBody::SessionDetach { attachment_id },
        )
        .await;
        let (attach_request, attach) = peer.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        peer.respond(
            attach_request,
            ResponseBody::SessionAttach {
                attachment_id: AttachmentId::new("withheld-caught-up"),
                attach_state: AttachState {
                    session_id,
                    requested_after_seq: 0,
                    replay_through_seq: 2,
                    worker_generation: 7,
                    authority_epoch: 1,
                },
            },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    });

    let mut timed = request(Some(Duration::from_millis(30)));
    timed.terminal_grace = Duration::from_millis(40);
    let (sender, _receiver) = mpsc::channel(4);
    let error = tokio::time::timeout(
        BOUND,
        run_headless(&profile, EnsureOptions::default(), timed, sender),
    )
    .await
    .expect("runner bound")
    .expect_err("unconfirmed recovery cancellation");
    assert!(matches!(
        error,
        HeadlessRunError::Rpc { ref code, .. } if code == "cancellation_unconfirmed"
    ));
    peer.await.expect("peer");
}

/// MUTATION CHECK: choose a permission option by label/index or treat the
/// PermissionRequired parked state as terminal. Expected RUNTIME failure:
/// the peer does not receive the enumerated AllowOnce key at index 0, or
/// the eventual Done result is not observed.
#[tokio::test]
async fn permission_menu_selects_typed_allow_once_and_continues_to_done() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        let menu_id = MenuId::new("permission-menu");
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::PermissionRequired {
                    menu: menu_id.clone(),
                }),
            ),
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::MenuOpened(Menu {
                    id: menu_id.clone(),
                    kind: MenuKind::Permission {
                        effect_summary: "write /tmp/output".into(),
                    },
                    title: "Allow write?".into(),
                    body: Vec::new(),
                    options: vec![
                        MenuOption {
                            key: "allow".into(),
                            label: "Allow once".into(),
                            detail: None,
                            decision: Some(DecisionKind::AllowOnce),
                        },
                        MenuOption {
                            key: "typed-reject".into(),
                            label: "Not the selector".into(),
                            detail: None,
                            decision: Some(DecisionKind::RejectOnce),
                        },
                    ],
                    blocking: true,
                    scope: MenuScope::Session,
                    origin: "fs_write".into(),
                    ttl_ms: None,
                    timeout_option: None,
                }),
            ),
        )
        .await;
        let answer_request = match peer.next().await {
            WireFrame::MenuAnswer {
                request_id: Some(request_id),
                menu_id: answered,
                option_key,
                option_index,
                ..
            } => {
                assert_eq!(answered, menu_id);
                assert_eq!(option_key, "allow");
                assert_eq!(option_index, 0);
                request_id
            }
            other => panic!("expected typed menu answer, got {other:?}"),
        };
        peer.respond(
            answer_request,
            ResponseBody::MenuAnswer { resolution_seq: 3 },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                3,
                EventPayload::MenuAnswered(MenuAnswer {
                    menu: menu_id,
                    option_key: Some("allow".into()),
                    option_index: 0,
                    value: None,
                    via: AnswerVia::Rpc,
                }),
            ),
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                4,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
    });

    let (result, events) = run_with_events(profile, request(None), 8, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert!(result.permission_denials.is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HeadlessEvent::PermissionDenied(_)))
    );
}

/// MUTATION CHECK: discard a permission action after socket enqueue, reuse
/// the menu-opening generation after a restart, or mint a new durable answer
/// identity. Expected RUNTIME failure: the reattached client hangs, retries
/// generation 7, or changes the command id instead of retrying at generation
/// 8 after replay proves the first answer did not commit.
#[tokio::test]
async fn permission_answer_response_loss_replays_then_retries_current_generation() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let handshake = welcome(&profile);
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
        let (session_id, attachment_id) = accept_create_and_attach(&mut first).await;
        let (submit_request, run_id) = accept_submit(&mut first, &session_id).await;
        first
            .respond(
                submit_request,
                ResponseBody::TurnSubmit {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    accepted_seq: 1,
                    worker_generation: 7,
                    disposition: SubmitDisposition::Started,
                },
            )
            .await;
        let menu_id = MenuId::new("permission-retry");
        send_event(
            &mut first,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::PermissionRequired {
                    menu: menu_id.clone(),
                }),
            ),
        )
        .await;
        send_event(
            &mut first,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::MenuOpened(Menu {
                    id: menu_id.clone(),
                    kind: MenuKind::Permission {
                        effect_summary: "execute command".into(),
                    },
                    title: "Allow execution?".into(),
                    body: Vec::new(),
                    options: vec![MenuOption {
                        key: "allow".into(),
                        label: "Allow".into(),
                        detail: None,
                        decision: Some(DecisionKind::AllowOnce),
                    }],
                    blocking: true,
                    scope: MenuScope::Session,
                    origin: "process_exec".into(),
                    ttl_ms: None,
                    timeout_option: None,
                }),
            ),
        )
        .await;
        let original_command = match first.next().await {
            WireFrame::MenuAnswer {
                command_id,
                worker_generation: 7,
                request_id: Some(_),
                ..
            } => command_id,
            other => panic!("expected first correlated menu answer, got {other:?}"),
        };
        drop(first);

        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 2,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let retry_attachment = AttachmentId::new("permission-retry-attachment");
        second
            .respond(
                attach_request,
                ResponseBody::SessionAttach {
                    attachment_id: retry_attachment.clone(),
                    attach_state: AttachState {
                        session_id: session_id.clone(),
                        requested_after_seq: 2,
                        replay_through_seq: 2,
                        worker_generation: 8,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id: retry_attachment.clone(),
                high_water_seq: 2,
            })
            .await;
        let (retry_request, retry_command) = match second.next().await {
            WireFrame::MenuAnswer {
                request_id: Some(request_id),
                command_id,
                worker_generation: 8,
                ..
            } => (request_id, command_id),
            other => panic!("expected current-generation menu retry, got {other:?}"),
        };
        assert_eq!(retry_command, original_command);
        second
            .respond(
                retry_request,
                ResponseBody::MenuAnswer { resolution_seq: 3 },
            )
            .await;
        send_event(
            &mut second,
            &retry_attachment,
            envelope(
                &session_id,
                &run_id,
                3,
                EventPayload::MenuAnswered(MenuAnswer {
                    menu: menu_id,
                    option_key: Some("allow".into()),
                    option_index: 0,
                    value: None,
                    via: AnswerVia::Rpc,
                }),
            ),
        )
        .await;
        send_event(
            &mut second,
            &retry_attachment,
            envelope(
                &session_id,
                &run_id,
                4,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
    });

    let (result, _) = run_with_events(profile, request(None), 8, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert!(result.permission_denials.is_empty());
}

/// MUTATION CHECK: treat any replayed menu answer as proof that headless's
/// selected AllowOnce won. Expected RUNTIME failure: a competing RejectOnce
/// resolution is accepted as success instead of producing the typed blocked
/// reason and one durable cancellation.
#[tokio::test]
async fn competing_permission_resolution_is_fail_closed_and_cancelled() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let handshake = welcome(&profile);
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
        let (session_id, attachment_id) = accept_create_and_attach(&mut first).await;
        let (submit_request, run_id) = accept_submit(&mut first, &session_id).await;
        first
            .respond(
                submit_request,
                ResponseBody::TurnSubmit {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    accepted_seq: 1,
                    worker_generation: 7,
                    disposition: SubmitDisposition::Started,
                },
            )
            .await;
        let menu_id = MenuId::new("permission-competing-answer");
        send_event(
            &mut first,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::PermissionRequired {
                    menu: menu_id.clone(),
                }),
            ),
        )
        .await;
        send_event(
            &mut first,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::MenuOpened(Menu {
                    id: menu_id.clone(),
                    kind: MenuKind::Permission {
                        effect_summary: "write protected file".into(),
                    },
                    title: "Allow write?".into(),
                    body: Vec::new(),
                    options: vec![
                        MenuOption {
                            key: "allow".into(),
                            label: "Allow once".into(),
                            detail: None,
                            decision: Some(DecisionKind::AllowOnce),
                        },
                        MenuOption {
                            key: "reject".into(),
                            label: "Reject once".into(),
                            detail: None,
                            decision: Some(DecisionKind::RejectOnce),
                        },
                    ],
                    blocking: true,
                    scope: MenuScope::Session,
                    origin: "fs_write".into(),
                    ttl_ms: None,
                    timeout_option: None,
                }),
            ),
        )
        .await;
        assert!(matches!(
            first.next().await,
            WireFrame::MenuAnswer {
                request_id: Some(_),
                ref option_key,
                option_index: 0,
                ..
            } if option_key == "allow"
        ));
        drop(first);

        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 2,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let replay_attachment = AttachmentId::new("competing-answer-replay");
        second
            .respond(
                attach_request,
                ResponseBody::SessionAttach {
                    attachment_id: replay_attachment.clone(),
                    attach_state: AttachState {
                        session_id: session_id.clone(),
                        requested_after_seq: 2,
                        replay_through_seq: 3,
                        worker_generation: 8,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        send_event(
            &mut second,
            &replay_attachment,
            envelope(
                &session_id,
                &run_id,
                3,
                EventPayload::MenuAnswered(MenuAnswer {
                    menu: menu_id,
                    option_key: Some("reject".into()),
                    option_index: 1,
                    value: None,
                    via: AnswerVia::Rpc,
                }),
            ),
        )
        .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id: replay_attachment.clone(),
                high_water_seq: 3,
            })
            .await;
        let (cancel_request, cancel) = second.request().await;
        assert!(matches!(
            cancel,
            RequestBody::TurnCancel {
                worker_generation: 8,
                ..
            }
        ));
        second
            .respond(
                cancel_request,
                ResponseBody::TurnCancel {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    status: CancelStatus::Accepted,
                    terminal_seq: None,
                },
            )
            .await;
        send_event(
            &mut second,
            &replay_attachment,
            envelope(
                &session_id,
                &run_id,
                4,
                EventPayload::RunState(RunState::Cancelled),
            ),
        )
        .await;
    });

    let (result, _) = run_with_events(profile, request(None), 8, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::InputRequired);
    assert!(matches!(
        result.failure.map(|failure| failure.code),
        Some(HeadlessFailureCode::Blocked(
            HeadlessBlockingReason::PermissionResolutionConflict
        ))
    ));
}

/// MIGRATION ORACLE: the old CLI injected a failure into the terminal store
/// append and required adjacent StoreCorrupt/Errored JSONL plus a bounded
/// nonzero return. The daemon owns that append now; this wire-level runner
/// test preserves the non-hanging reduction half while the CLI exit/output
/// table preserves StoreCorrupt → 70.
///
/// MUTATION CHECK: end on RunFailed alone, ignore the adjacent Errored fact,
/// or wait for another frame after the terminal. Expected RUNTIME failure:
/// the five-second bound fires or the typed StoreCorrupt result/terminal line
/// is absent.
#[tokio::test]
async fn adjacent_store_failure_and_errored_terminal_return_without_hanging() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunFailed {
                    code: ErrorCode::StoreCorrupt,
                    message: "injected append failure".into(),
                    retryable: false,
                    presentation: None,
                },
            ),
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::RunState(RunState::Errored),
            ),
        )
        .await;
    });

    let (result, events) = run_with_events(profile, request(None), 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Errored);
    assert!(matches!(
        result.failure.map(|failure| failure.code),
        Some(HeadlessFailureCode::Run(ErrorCode::StoreCorrupt))
    ));
    assert!(matches!(
        events.last(),
        Some(HeadlessEvent::Terminal(terminal))
            if serde_json::from_value::<EventPayload>(terminal.envelope.payload.clone().into())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Errored))
    ));
}

#[tokio::test]
async fn workflow_unfinished_survives_the_durable_headless_projection() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunFailed {
                    code: ErrorCode::WorkflowUnfinished,
                    message: "workflow remains unfinished".into(),
                    retryable: false,
                    presentation: None,
                },
            ),
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::RunState(RunState::Errored),
            ),
        )
        .await;
    });

    let (result, _) = run_with_events(profile, request(None), 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Errored);
    assert!(matches!(
        result.failure.map(|failure| failure.code),
        Some(HeadlessFailureCode::Run(ErrorCode::WorkflowUnfinished))
    ));
}

/// MUTATION CHECK: treat a parked Waiting state as terminal or coalesce the
/// natural Cancelled terminal with Errored/Done. Expected RUNTIME failure:
/// the runner returns before the delayed terminal or reports the wrong typed
/// outcome/sequence.
#[tokio::test]
async fn parked_waiting_does_not_end_run_and_natural_cancelled_is_distinct() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::ProviderBackoff,
                }),
            ),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::RunState(RunState::Cancelled),
            ),
        )
        .await;
    });

    let (result, events) = run_with_events(profile, request(None), 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Cancelled);
    assert_eq!(result.terminal_seq, Some(2));
    assert_eq!(events.len(), 4);
}

/// MUTATION CHECK: report the cancellation terminal instead of retaining the
/// wall-clock timeout, or mint a second cancel command. Expected RUNTIME
/// failure: outcome is not Timeout or the peer observes more than one
/// durable turn.cancel request.
#[tokio::test]
async fn timeout_sends_one_cancel_and_remains_timeout_after_cancelled_terminal() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        let (cancel_request, cancel) = peer.request().await;
        assert!(matches!(cancel, RequestBody::TurnCancel { .. }));
        peer.respond(
            cancel_request,
            ResponseBody::TurnCancel {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                status: CancelStatus::Accepted,
                terminal_seq: None,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Cancelled),
            ),
        )
        .await;
        // A clean client close is legal here (the runner is done); only an
        // actual second frame violates the one-cancel law.
        match tokio::time::timeout(Duration::from_millis(50), peer.try_next()).await {
            Err(_) | Ok(None) => {}
            Ok(Some(frame)) => panic!("timeout emitted a second command: {frame:?}"),
        }
    });

    let (result, _) = run_with_events(
        profile,
        request(Some(Duration::from_millis(20))),
        4,
        Duration::ZERO,
    )
    .await;
    assert_eq!(result.outcome, HeadlessOutcome::Timeout);
    assert_eq!(result.terminal_seq, Some(1));
    peer.await.expect("peer");
}

/// MUTATION CHECK: await the bounded presentation sink from durable
/// reduction. Expected RUNTIME failure: with the one-slot output left full,
/// the peer never observes timeout cancellation before the test begins
/// draining presentation events.
#[tokio::test]
async fn blocked_output_does_not_delay_wall_clock_cancellation() {
    let (_root, profile) = profile();
    let (cancel_seen, cancellation) = oneshot::channel();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        for seq in 1..=2 {
            send_event(
                &mut peer,
                &attachment_id,
                envelope(
                    &session_id,
                    &run_id,
                    seq,
                    EventPayload::RunState(RunState::Thinking),
                ),
            )
            .await;
        }
        let (cancel_request, cancel) = peer.request().await;
        assert!(matches!(cancel, RequestBody::TurnCancel { .. }));
        let _ = cancel_seen.send(());
        peer.respond(
            cancel_request,
            ResponseBody::TurnCancel {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                status: CancelStatus::Accepted,
                terminal_seq: None,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                3,
                EventPayload::RunState(RunState::Cancelled),
            ),
        )
        .await;
    });

    // 250ms trigger (was 20): generous enough to survive loaded-gate setup,
    // still expires (the mock never completes) to drive the cancel law.
    let mut timed = request(Some(Duration::from_millis(250)));
    timed.terminal_grace = Duration::from_millis(250);
    let (sender, mut receiver) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        run_headless(&profile, EnsureOptions::default(), timed, sender).await
    });
    // Observation bound scales with the 250ms trigger: the LAW is that the
    // cancel reaches the peer promptly after expiry despite a blocked output
    // channel (capacity 1, consumer parked) — not an absolute latency.
    tokio::time::timeout(Duration::from_millis(1_500), cancellation)
        .await
        .expect("cancel must not wait for output drain")
        .expect("peer reports cancel");

    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        events
    });
    let result = tokio::time::timeout(BOUND, task)
        .await
        .expect("runner bound")
        .expect("runner task")
        .expect("headless result");
    let events = collector.await.expect("collector");
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Timeout);
    assert_eq!(result.terminal_seq, Some(3));
    assert_eq!(events.len(), 5);
}

/// MUTATION CHECK: treat disconnect during forced-outcome grace as immediate
/// completion. Expected RUNTIME failure: the runner omits replayed Cancelled,
/// loses terminal_seq, or reports transport failure instead of draining the
/// remaining grace after its durable cancel was confirmed.
#[tokio::test]
async fn confirmed_cancel_disconnect_replays_terminal_within_grace() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let handshake = welcome(&profile);
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
        let (session_id, _) = accept_create_and_attach(&mut first).await;
        let (submit_request, run_id) = accept_submit(&mut first, &session_id).await;
        first
            .respond(
                submit_request,
                ResponseBody::TurnSubmit {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    accepted_seq: 1,
                    worker_generation: 7,
                    disposition: SubmitDisposition::Started,
                },
            )
            .await;
        let (cancel_request, cancel) = first.request().await;
        assert!(matches!(cancel, RequestBody::TurnCancel { .. }));
        first
            .respond(
                cancel_request,
                ResponseBody::TurnCancel {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    status: CancelStatus::Accepted,
                    terminal_seq: None,
                },
            )
            .await;
        drop(first);

        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let attachment_id = AttachmentId::new("cancel-terminal-replay");
        second
            .respond(
                attach_request,
                ResponseBody::SessionAttach {
                    attachment_id: attachment_id.clone(),
                    attach_state: AttachState {
                        session_id: session_id.clone(),
                        requested_after_seq: 0,
                        replay_through_seq: 1,
                        worker_generation: 7,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        send_event(
            &mut second,
            &attachment_id,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Cancelled),
            ),
        )
        .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id,
                high_water_seq: 1,
            })
            .await;
    });

    // 250ms trigger (was 20): generous enough to survive loaded-gate setup,
    // still expires (the mock never completes) to drive the cancel law.
    let mut timed = request(Some(Duration::from_millis(250)));
    timed.terminal_grace = Duration::from_millis(250);
    let (result, events) = run_with_events(profile, timed, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Timeout);
    assert_eq!(result.terminal_seq, Some(1));
    assert_eq!(events.len(), 3);
}

/// MUTATION CHECK: freeze the first cancel generation, discard cancellation
/// delivery errors, or mint a second logical cancel after reconnect. Expected
/// RUNTIME failure: the retry uses generation 7/a different command id, the
/// runner reports timeout before durable cancellation, or it hangs.
#[tokio::test]
async fn cancel_response_loss_replays_then_retries_same_command_at_current_generation() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let handshake = welcome(&profile);
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
        let (session_id, _) = accept_create_and_attach(&mut first).await;
        let (submit_request, run_id) = accept_submit(&mut first, &session_id).await;
        first
            .respond(
                submit_request,
                ResponseBody::TurnSubmit {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    accepted_seq: 1,
                    worker_generation: 7,
                    disposition: SubmitDisposition::Started,
                },
            )
            .await;
        let original_command = match first.request().await.1 {
            RequestBody::TurnCancel {
                command_id,
                worker_generation: 7,
                ..
            } => command_id,
            other => panic!("expected first cancel, got {other:?}"),
        };
        drop(first);

        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
                ..
            }
        ));
        let retry_attachment = AttachmentId::new("cancel-retry-attachment");
        second
            .respond(
                attach_request,
                ResponseBody::SessionAttach {
                    attachment_id: retry_attachment.clone(),
                    attach_state: AttachState {
                        session_id: session_id.clone(),
                        requested_after_seq: 0,
                        replay_through_seq: 0,
                        worker_generation: 8,
                        authority_epoch: 1,
                    },
                },
            )
            .await;
        second
            .write(&WireFrame::AttachCaughtUp {
                attachment_id: retry_attachment.clone(),
                high_water_seq: 0,
            })
            .await;
        let (retry_request, retry) = second.request().await;
        let RequestBody::TurnCancel {
            command_id: retry_command,
            worker_generation: 8,
            ..
        } = retry
        else {
            panic!("expected current-generation cancel retry");
        };
        assert_eq!(retry_command, original_command);
        second
            .respond(
                retry_request,
                ResponseBody::TurnCancel {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    status: CancelStatus::Accepted,
                    terminal_seq: None,
                },
            )
            .await;
        send_event(
            &mut second,
            &retry_attachment,
            envelope(
                &session_id,
                &run_id,
                1,
                EventPayload::RunState(RunState::Cancelling),
            ),
        )
        .await;
        send_event(
            &mut second,
            &retry_attachment,
            envelope(
                &session_id,
                &run_id,
                2,
                EventPayload::RunState(RunState::Cancelled),
            ),
        )
        .await;
    });

    let (result, _) = run_with_events(
        profile,
        // 250ms trigger (was 20): generous enough to survive loaded-gate setup,
        // still expires (the mock never completes) to drive the cancel law.
        request(Some(Duration::from_millis(250))),
        4,
        Duration::ZERO,
    )
    .await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Timeout);
    assert_eq!(result.terminal_seq, Some(2));
}

/// MUTATION CHECK: ignore expiry while durable cancellation is still
/// unconfirmed and return a forced timeout anyway. Expected RUNTIME failure:
/// the runner produces a `HeadlessRunResult` or hangs instead of surfacing the
/// typed cancellation_unconfirmed RPC failure within the bounded grace.
#[tokio::test]
async fn unconfirmed_cancellation_cannot_be_reported_as_timeout() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (session_id, _) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id,
                run_id,
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        let (_, cancel) = peer.request().await;
        assert!(matches!(cancel, RequestBody::TurnCancel { .. }));
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let mut timed = request(Some(Duration::from_millis(10)));
    timed.terminal_grace = Duration::from_millis(30);
    let (sender, _receiver) = mpsc::channel(4);
    let error = tokio::time::timeout(
        BOUND,
        run_headless(&profile, EnsureOptions::default(), timed, sender),
    )
    .await
    .expect("runner bound")
    .expect_err("unconfirmed cancellation must not become timeout");
    assert!(matches!(
        error,
        HeadlessRunError::Rpc {
            stage: "turn.cancel",
            ref code,
            ..
        } if code == "cancellation_unconfirmed"
    ));
    peer.await.expect("peer");
}

async fn assert_blocked(payload: EventPayload, reason: HeadlessBlockingReason) {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, move |mut peer| async move {
        let (session_id, attachment_id) = accept_create_and_attach(&mut peer).await;
        let (submit_request, run_id) = accept_submit(&mut peer, &session_id).await;
        peer.respond(
            submit_request,
            ResponseBody::TurnSubmit {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                accepted_seq: 1,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(&session_id, &run_id, 1, payload),
        )
        .await;
        let (cancel_request, cancel) = peer.request().await;
        assert!(matches!(cancel, RequestBody::TurnCancel { .. }));
        peer.respond(
            cancel_request,
            ResponseBody::TurnCancel {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                status: CancelStatus::Accepted,
                terminal_seq: None,
            },
        )
        .await;
        let mut terminal = envelope(
            &session_id,
            &run_id,
            2,
            EventPayload::RunState(RunState::Cancelled),
        );
        let payload = terminal.payload.as_object_mut().expect("terminal payload");
        payload.insert("terminal_kind".into(), serde_json::json!("failure"));
        payload.insert("error_code".into(), serde_json::json!(reason.code()));
        send_event(&mut peer, &attachment_id, terminal).await;
    });
    let (result, _) = run_with_events(profile, request(None), 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::InputRequired);
    assert!(matches!(
        result.failure.map(|failure| failure.code),
        Some(HeadlessFailureCode::Blocked(actual)) if actual == reason
    ));
}

/// MUTATION CHECK: guess a response for non-permission input or resume an
/// unknown effect. Expected RUNTIME failure: either case does not send cancel
/// and return its typed blocking reason.
#[tokio::test]
async fn nonpermission_input_and_unknown_effect_cancel_with_typed_reasons() {
    assert_blocked(
        EventPayload::RunState(RunState::InputRequired {
            menu: MenuId::new("question"),
        }),
        HeadlessBlockingReason::InputRequired,
    )
    .await;
    assert_blocked(
        EventPayload::RunState(RunState::EffectOutcomeUnknown),
        HeadlessBlockingReason::EffectOutcomeUnknown,
    )
    .await;
}

#[test]
fn override_and_read_only_features_are_required_only_when_used() {
    let no_flags =
        haider_client::required_headless_features(SessionPermissionOverridesV1::default());
    let flags = haider_client::required_headless_features(SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: true,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    });
    let mut expected = haider_client::required_live_features();
    expected.insert(haider_rpc::FEATURE_AUTONOMOUS_INTERACTION_V1.to_owned());
    expected.insert(haider_rpc::FEATURE_SESSION_CREATE_ADMISSION_V1.to_owned());
    assert_eq!(no_flags, expected);
    assert_eq!(
        flags
            .difference(&no_flags)
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([haider_rpc::FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned()])
    );
    let read_only = haider_client::required_headless_features(SessionPermissionOverridesV1 {
        read_only: true,
        allow_writes: false,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    });
    assert_eq!(
        read_only
            .difference(&no_flags)
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            haider_rpc::FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_READ_ONLY_V1.to_owned(),
        ])
    );
    let attachments = haider_client::required_headless_features_with_attachments(
        SessionPermissionOverridesV1::default(),
    );
    assert_eq!(
        attachments
            .difference(&no_flags)
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned()])
    );
}

/// The continuation handle selects the original session; it must never enter
/// session.create or run.retry, which would respectively lose or omit its tools.
#[tokio::test]
async fn resume_budget_checkpoint_submits_new_turn_in_original_session() {
    use haider_rpc::haider_protocol::headless::{HeadlessRunSpecV1, RunBudgetV1};
    use haider_rpc::haider_protocol::request_budget::{
        RequestBudgetContinuationV1, RequestBudgetPhaseV1, RequestBudgetStatusV1, RequestBudgetV1,
    };
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind resume peer");
    let mut advertised = welcome(&profile);
    advertised
        .features
        .insert(haider_rpc::FEATURE_HEADLESS_RUN_V1.into());
    advertised
        .features
        .insert(haider_rpc::FEATURE_RUN_BUDGET_V1.into());
    advertised
        .features
        .insert(haider_rpc::FEATURE_REQUEST_BUDGET_V1.into());
    let peer = tokio::spawn(async move {
        let mut peer = accept_peer(&listener, advertised).await;
        let session_id = SessionId::new("resume-session");
        let source_id = RunId::new("resume-source");
        let (status_request, status) = peer.request().await;
        assert!(matches!(status, RequestBody::HeadlessRunStatus { run_id } if run_id == source_id));
        let checkpoint = RequestBudgetStatusV1 {
            used: 64,
            budget: RequestBudgetV1::default(),
            phase: RequestBudgetPhaseV1::HardBound,
            continuation: RequestBudgetContinuationV1 {
                session_id: session_id.clone(),
                run_id: source_id.clone(),
                branch_id: None,
                agent_id: None,
            },
        };
        let source_events = vec![
            envelope(
                &session_id,
                &source_id,
                1,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("budget-checkpoint"),
                    item: checkpoint.to_extension_item().expect("typed checkpoint"),
                }),
            ),
            envelope(
                &session_id,
                &source_id,
                2,
                EventPayload::RunState(RunState::Errored),
            ),
        ];
        peer.respond(
            status_request,
            ResponseBody::HeadlessRunStatus {
                session_id: session_id.clone(),
                run_id: source_id.clone(),
                worker_generation: 7,
                state: RunState::Errored,
                head_seq: 2,
                terminal_seq: Some(2),
                budget_exhausted: None,
                spec: HeadlessRunSpecV1 {
                    cwd: "/original-workspace".into(),
                    provider: "fake".into(),
                    model: "original-model".into(),
                    max_output_tokens: 1024,
                    effort: None,
                    fast: false,
                    seed: Some(12),
                    permission_overrides: SessionPermissionOverridesV1::default(),
                    trust_hooks: false,
                    budget: RunBudgetV1 {
                        max_time_ms: Some(30_000),
                        ..RunBudgetV1::default()
                    },
                    request_deadline_unix_ms: Some(1),
                    replay_of: None,
                    continuation_of: None,
                },
            },
        )
        .await;
        let (read_request, read) = peer.request().await;
        let RequestBody::SessionRead {
            session_id: read_session,
            range,
        } = read
        else {
            panic!("read durable checkpoint");
        };
        assert_eq!(read_session, session_id);
        peer.respond(
            read_request,
            ResponseBody::SessionRead {
                result: haider_rpc::SessionReadResult {
                    session_id: session_id.clone(),
                    range,
                    head_seq: 2,
                    metadata: None,
                    latest_context_footprint: None,
                    envelopes: source_events.clone(),
                },
            },
        )
        .await;
        let (attach_request, attach) = peer.request().await;
        assert!(
            matches!(attach, RequestBody::SessionAttach { session_id: attached, after_seq: 0, mode: AttachMode::Control, .. } if attached == session_id)
        );
        let attachment_id = AttachmentId::new("resume-attachment");
        peer.respond(
            attach_request,
            ResponseBody::SessionAttach {
                attachment_id: attachment_id.clone(),
                attach_state: AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: 0,
                    replay_through_seq: 2,
                    worker_generation: 7,
                    authority_epoch: 1,
                },
            },
        )
        .await;
        for event in source_events {
            send_event(&mut peer, &attachment_id, event).await;
        }
        peer.write(&WireFrame::AttachCaughtUp {
            attachment_id: attachment_id.clone(),
            high_water_seq: 2,
        })
        .await;
        let (submit_request, submit) = peer.request().await;
        let RequestBody::HeadlessRunStart {
            session_id: submitted,
            text,
            spec,
            ..
        } = submit
        else {
            panic!("new same-session turn, not retry");
        };
        assert_eq!(submitted, session_id);
        assert_eq!(spec.continuation_of, Some(source_id.clone()));
        assert_eq!(spec.cwd, "/original-workspace");
        assert_eq!(spec.model, "original-model");
        assert_eq!(spec.max_output_tokens, 1024);
        assert_eq!(spec.seed, Some(12));
        assert_eq!(
            spec.request_deadline_unix_ms, None,
            "old absolute deadline must not poison continuation"
        );
        assert_eq!(spec.budget.max_time_ms, Some(30_000));
        assert_eq!(
            spec.budget.request_budget,
            Some(RequestBudgetV1 {
                tranche: 40,
                hard_cap: 80
            })
        );
        assert!(text.contains("committed work and tool results"));
        assert!(text.contains("finish the remaining test"));
        let resumed = RunId::new("resumed-run");
        peer.respond(
            submit_request,
            ResponseBody::HeadlessRunStart {
                session_id: session_id.clone(),
                run_id: resumed.clone(),
                accepted_seq: 3,
                worker_generation: 7,
                disposition: SubmitDisposition::Started,
            },
        )
        .await;
        send_event(
            &mut peer,
            &attachment_id,
            envelope(
                &session_id,
                &resumed,
                3,
                EventPayload::RunState(RunState::Done),
            ),
        )
        .await;
    });
    let (sender, mut receiver) = mpsc::channel(8);
    let collector = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let mut resumed = request(None);
    resumed.prompt = "finish the remaining test".into();
    resumed.budget.request_budget = Some(RequestBudgetV1 {
        tranche: 40,
        hard_cap: 80,
    });
    let result = tokio::time::timeout(
        BOUND,
        haider_client::resume_headless_with_event_mode_and_interrupts(
            &profile,
            EnsureOptions::default(),
            resumed,
            RunId::new("resume-source"),
            sender,
            haider_client::HeadlessEventMode::FullRecordSet,
            None,
        ),
    )
    .await
    .expect("resume bound")
    .expect("resume result");
    peer.await.expect("peer");
    collector.await.expect("collector");
    assert_eq!(result.session_id, SessionId::new("resume-session"));
    assert_eq!(result.run_id, RunId::new("resumed-run"));
    assert_eq!(result.outcome, HeadlessOutcome::Done);
    assert_eq!(result.provider, "fake");
    assert_eq!(result.model, "original-model");
}
