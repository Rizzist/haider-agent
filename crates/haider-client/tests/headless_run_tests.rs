#![cfg(unix)]
//! Daemon-backed headless transaction laws over a real Unix socket.
#![allow(clippy::expect_used)]

use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use haider_client::{
    EnsureOptions, HeadlessBlockingReason, HeadlessEvent, HeadlessFailureCode,
    HeadlessImageAttachment, HeadlessOutcome, HeadlessRunError, HeadlessRunRequest, ProfileEnv,
    ResolvedProfile, resolve_profile, run_headless,
};
use haider_rpc::haider_protocol::EventPayload;
use haider_rpc::haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_rpc::haider_protocol::envelope::{
    PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_rpc::haider_protocol::error::ErrorCode;
use haider_rpc::haider_protocol::ids::{
    ArtifactRef, CredentialAlias, DeviceId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_rpc::haider_protocol::item::{ItemEvent, TurnItem};
use haider_rpc::haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_rpc::haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_rpc::haider_protocol::state::{RunState, WaitReason};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, CancelStatus, Capability, CapabilitySet,
    DEFAULT_FRAME_LIMIT, LifecyclePhase, ProviderApiFamilyWire, ProviderAvailabilityWire,
    ProviderDefaultWire, ProviderSummaryWire, RequestBody, RequestId, ResponseBody,
    SubmitDisposition, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
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
        model: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve profile");
    std::fs::create_dir_all(&profile.runtime_dir).expect("runtime dir");
    (root, profile)
}

fn welcome(profile: &ResolvedProfile) -> Welcome {
    Welcome {
        protocol: WIRE_PROTOCOL_VERSION,
        instance_id: "headless-test-peer".into(),
        daemon_generation: 1,
        frame_limit: LIMIT as u32,
        profile_id: profile.profile_id.clone(),
        daemon_version: "0.0.36-test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features: haider_client::required_live_features(),
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
        ..
    } = create
    else {
        panic!("headless runner must use additive session.create shape");
    };
    assert_eq!(permission_overrides, None);
    respond_create_and_attach(peer, create_request, "fake", "fake-model").await
}

async fn respond_create_and_attach(
    peer: &mut Peer,
    create_request: RequestId,
    provider: &str,
    model: &str,
) -> (SessionId, AttachmentId) {
    let session_id = SessionId::new("headless-session");
    peer.respond(
        create_request,
        ResponseBody::SessionCreate {
            session_id: session_id.clone(),
            created_seq: 0,
            worker_generation: 7,
            metadata: SessionMetadataV1 {
                cwd: "/tmp".into(),
                provider: provider.into(),
                model: model.into(),
                max_tokens: 4096,
                permission_overrides: None,
                system_prompt_version: Some("test".into()),
                title: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                created_at_ms: 1,
            },
        },
    )
    .await;

    let (attach_request, attach) = peer.request().await;
    let RequestBody::SessionAttach {
        session_id: attached,
        after_seq,
        mode,
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
        payload: serde_json::to_value(payload).expect("payload"),
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
        provider: Some("fake".into()),
        model: Some("fake-model".into()),
        max_tokens: 4096,
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

fn active_account(provider: &str) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new("active-account"),
        provider: provider.into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "active@example.com".into(),
        status: CredentialStatus::Ok,
        active: true,
    }
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
        models: models.iter().map(|model| (*model).into()).collect(),
        model_details: Vec::new(),
        auth_methods: vec![AuthMethod::OAuth],
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: default_model.map(Into::into),
        enabled: true,
    }
}

async fn serve_flagless_done(
    peer: &mut Peer,
    default_model: Option<&str>,
    models: &[&str],
    expected_model: &str,
) {
    let provider = "openai-oauth";
    let (account_request, account_body) = peer.request().await;
    assert_eq!(account_body, RequestBody::AccountList { provider: None });
    peer.respond(
        account_request,
        ResponseBody::AccountList {
            descriptors: vec![active_account(provider)],
            revision: Some(4),
            provider_active: Vec::new(),
            // Deliberately misleading: bootstrap must consult provider.list's
            // coherent provider summary, like the TUI, not this additive seam.
            provider_defaults: vec![ProviderDefaultWire {
                provider: provider.into(),
                model: "wrong-account-list-default".into(),
            }],
        },
    )
    .await;

    let (provider_request, provider_body) = peer.request().await;
    assert_eq!(
        provider_body,
        RequestBody::ProviderList {
            provider: Some(provider.into())
        }
    );
    peer.respond(
        provider_request,
        ResponseBody::ProviderList {
            providers: vec![provider_summary_fixture(provider, default_model, models)],
            revision: 4,
        },
    )
    .await;

    let (create_request, create_body) = peer.request().await;
    let RequestBody::SessionCreateWithPermissionOverrides {
        provider: created_provider,
        model: created_model,
        permission_overrides,
        ..
    } = create_body
    else {
        panic!("bootstrap must be followed by session.create");
    };
    assert_eq!(created_provider, provider);
    assert_eq!(created_model, expected_model);
    assert_eq!(permission_overrides, None);
    let (session_id, attachment_id) =
        respond_create_and_attach(peer, create_request, provider, expected_model).await;
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

/// MUTATION CHECK: drop account.list, hardcode anthropic, consume the
/// account.list default seam, or skip provider default resolution. Expected
/// RUNTIME failure: peer request order or the pinned create body differs.
#[tokio::test]
async fn flagless_bootstrap_creates_on_active_provider_and_published_default_model() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        serve_flagless_done(
            &mut peer,
            Some("gpt-active-default"),
            &["gpt-first", "gpt-active-default"],
            "gpt-active-default",
        )
        .await;
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

/// MUTATION CHECK: require default_model instead of falling back to the first
/// published slug. Expected RUNTIME failure: no create arrives with
/// `gpt-first`, or the runner returns a typed bootstrap failure.
#[tokio::test]
async fn flagless_bootstrap_falls_back_to_first_published_model() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        serve_flagless_done(&mut peer, None, &["gpt-first", "gpt-second"], "gpt-first").await;
    });
    let mut run = request(None);
    run.provider = None;
    run.model = None;

    let (result, _events) = run_with_events(profile, run, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.provider, "openai-oauth");
    assert_eq!(result.model, "gpt-first");
}

/// MUTATION CHECK: fall back to profile defaults or continue to provider.list
/// when account.list has nothing active. Expected RUNTIME failure: the exact
/// typed no_active_account bootstrap error is not returned before create.
#[tokio::test]
async fn flagless_bootstrap_without_active_account_is_typed() {
    let (_root, profile) = profile();
    let peer = spawn_peer(&profile, |mut peer| async move {
        let (request_id, body) = peer.request().await;
        assert_eq!(body, RequestBody::AccountList { provider: None });
        peer.respond(
            request_id,
            ResponseBody::AccountList {
                descriptors: Vec::new(),
                revision: Some(1),
                provider_active: Vec::new(),
                provider_defaults: Vec::new(),
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
            stage: "account.list",
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
    assert_eq!(result.response.as_deref(), Some("daemon answer"));
    assert_eq!(result.terminal_seq, Some(3));
    assert_eq!(events.len(), 3);
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
    assert_eq!(events.len(), 2);
}

/// MUTATION CHECK: create the session before uploading, omit uploaded refs
/// from submit, or mint a replacement submit after response loss. Expected
/// RUNTIME failure: the peer observes the wrong ordering, attachment blocks,
/// or durable command identity across the reconnect.
#[tokio::test]
async fn headless_attach_uploads_then_submits_with_durable_identity() {
    let (_root, profile) = profile();
    let listener = UnixListener::bind(&profile.endpoint_path).expect("bind reconnect peer");
    let mut handshake = welcome(&profile);
    handshake
        .features
        .insert(haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned());
    let image_bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let expected_ref = ArtifactRef::new(format!("blake3:{}", blake3::hash(&image_bytes).to_hex()));
    let peer_ref = expected_ref.clone();
    let peer = tokio::spawn(async move {
        let mut first = accept_peer(&listener, handshake.clone()).await;
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
    assert_eq!(events.len(), 2);
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
    assert_eq!(result.response.as_deref(), Some("replayed"));
    let seqs = events
        .into_iter()
        .filter_map(|event| match event {
            HeadlessEvent::Envelope(envelope) => Some(envelope.seq),
            HeadlessEvent::PermissionDenied(_) => None,
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
            HeadlessEvent::PermissionDenied(_) => None,
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
/// the peer does not receive the enumerated RejectOnce key at index 1, or
/// the eventual Done/denial result is not observed.
#[tokio::test]
async fn permission_menu_selects_typed_reject_once_and_continues_to_done() {
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
                            label: "Reject once".into(),
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
                assert_eq!(option_key, "typed-reject");
                assert_eq!(option_index, 1);
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
                    option_key: Some("typed-reject".into()),
                    option_index: 1,
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
    assert_eq!(result.permission_denials.len(), 1);
    assert!(events.iter().any(|event| matches!(
        event,
        HeadlessEvent::PermissionDenied(denial)
            if denial.effect_summary == "write /tmp/output"
    )));
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
                        key: "reject".into(),
                        label: "Reject".into(),
                        detail: None,
                        decision: Some(DecisionKind::RejectOnce),
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
                    option_key: Some("reject".into()),
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
    assert_eq!(result.permission_denials.len(), 1);
}

/// MUTATION CHECK: treat any replayed menu answer as proof that headless's
/// selected RejectOnce won. Expected RUNTIME failure: a competing AllowOnce
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
                option_index: 1,
                ..
            } if option_key == "reject"
        ));
        drop(first);

        let mut second = accept_peer(&listener, handshake).await;
        let (attach_request, attach) = second.request().await;
        assert!(matches!(
            attach,
            RequestBody::SessionAttach {
                after_seq: 2,
                mode: AttachMode::Control,
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
                    option_key: Some("allow".into()),
                    option_index: 0,
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
        Some(HeadlessEvent::Envelope(envelope))
            if serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Errored))
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
    assert_eq!(events.len(), 2);
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

    let mut timed = request(Some(Duration::from_millis(20)));
    timed.terminal_grace = Duration::from_millis(250);
    let (sender, mut receiver) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        run_headless(&profile, EnsureOptions::default(), timed, sender).await
    });
    tokio::time::timeout(Duration::from_millis(150), cancellation)
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
    assert_eq!(events.len(), 3);
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

    let mut timed = request(Some(Duration::from_millis(20)));
    timed.terminal_grace = Duration::from_millis(250);
    let (result, events) = run_with_events(profile, timed, 4, Duration::ZERO).await;
    peer.await.expect("peer");
    assert_eq!(result.outcome, HeadlessOutcome::Timeout);
    assert_eq!(result.terminal_seq, Some(1));
    assert_eq!(events.len(), 1);
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
        request(Some(Duration::from_millis(20))),
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
fn override_feature_is_required_only_when_a_flag_is_present() {
    let no_flags =
        haider_client::required_headless_features(SessionPermissionOverridesV1::default());
    let flags = haider_client::required_headless_features(SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: false,
        auto_allow: false,
    });
    assert_eq!(no_flags, haider_client::required_live_features());
    assert_eq!(
        flags
            .difference(&no_flags)
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([haider_rpc::FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned()])
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
