#![allow(clippy::expect_used)]

use crate::accounts::ConnectionTransport;
use crate::loom_author::{draft_from_prose, validate};
use crate::session_hub::{FrameSendError, FrameSink, SessionHub, SessionHubConfig};
use crate::worker::{ProviderFactory, ResolvedTurnProvider};
use haider_core::{SessionCreateCommand, SqliteStoreHandle};
use haider_protocol::graph::graph_template_digest;
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::loom::{
    LoomAgentType, LoomAuthorAgentTypeSpec, LoomAuthorEvidenceContract, LoomAuthorKind,
    LoomAuthorNodeSpec, LoomAuthorSpec, LoomAuthorWorkflowSpec, ValidatedLoomAuthorSpec,
};
use haider_protocol::provider::FinishReason;
use haider_protocol::session::SessionMetadataV1;
use haider_provider::{FakeProvider, FakeStep};
use haider_rpc::{
    AttachMode, Capability, CommandId, RequestBody, RequestId, ResponseBody, WireFrame,
};
use haider_store::Store;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, timeout};

struct FixedProviderFactory {
    provider: Arc<FakeProvider>,
}

#[async_trait::async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider) as Arc<dyn haider_provider::Provider>,
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

#[derive(Default)]
struct CapturingSink(Mutex<Vec<WireFrame>>);

impl FrameSink for CapturingSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("sink lock").push(frame);
        Ok(())
    }
}

async fn response(sink: &CapturingSink, request_id: &str) -> ResponseBody {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Some(body) =
                sink.0
                    .lock()
                    .expect("sink lock")
                    .iter()
                    .find_map(|frame| match frame {
                        WireFrame::Response {
                            request_id: found,
                            body,
                        } if found.as_str() == request_id => Some(body.clone()),
                        _ => None,
                    })
            {
                return body;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("correlated authoring response")
}

fn metadata() -> SessionMetadataV1 {
    SessionMetadataV1 {
        cwd: "/workspace".into(),
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        max_tokens: 4_096,
        system_prompt_version: Some("test-system".into()),
        permission_overrides: None,
        interaction_mode: Default::default(),
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
        agent_type: None,
    }
}

fn workflow_spec(task: &str) -> LoomAuthorSpec {
    LoomAuthorSpec::Workflow(LoomAuthorWorkflowSpec {
        id: "review-flow".into(),
        in_type: "Brief".into(),
        out_type: "Brief".into(),
        nodes: vec![LoomAuthorNodeSpec {
            id: "execute".into(),
            agent_type: None,
            task: task.into(),
            in_type: "Brief".into(),
            out_type: "Brief".into(),
            gate: "command".into(),
            depends_on: Vec::new(),
            back_edge: None,
            evidence: Some(LoomAuthorEvidenceContract {
                protocol: "instruct_pipe_v1".into(),
                tool: "graph_evidence".into(),
                required_green: 1,
            }),
        }],
    })
}

fn typed_agent(id: &str, input: &str, output: &str) -> LoomAgentType {
    LoomAgentType {
        id: id.into(),
        name: id.into(),
        job: format!("Perform the {id} stage."),
        in_type: input.into(),
        out_type: output.into(),
        clis: Vec::new(),
        apis: Vec::new(),
        denials: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: String::new(),
        glyph: String::new(),
        rev: 1,
    }
}

fn evidence(required_green: u32) -> Option<LoomAuthorEvidenceContract> {
    Some(LoomAuthorEvidenceContract {
        protocol: "instruct_pipe_v1".into(),
        tool: "graph_evidence".into(),
        required_green,
    })
}

#[tokio::test]
async fn authoring_rpc_registers_and_executes_each_confirmed_hash() {
    let ai_text = serde_json::to_string_pretty(&workflow_spec("package the AI draft"))
        .expect("encode AI fixture");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText { text: ai_text },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    hub.install_loom_author_provider(Arc::new(FixedProviderFactory { provider }))
        .expect("install author provider");
    let session_id = SessionId::new("loom-authoring-session");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-loom-authoring-session".into(),
        request_digest: "create-loom-authoring-session-digest".into(),
        request_json: r#"{"session":"loom-authoring-session"}"#.into(),
        session_id: session_id.clone(),
        cwd: std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .into_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4_096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("loom-authoring-created"),
        device_id: DeviceId::new("loom-authoring-device"),
    })
    .await
    .expect("create authoring session");
    let sink = Arc::new(CapturingSink::default());
    let connection = hub
        .open_connection(
            BTreeSet::from([Capability::View, Capability::Control]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("open control connection");
    connection
        .request(
            RequestId::new("author-control-attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach authoring control");
    assert!(matches!(
        response(&sink, "author-control-attach").await,
        ResponseBody::SessionAttach { .. }
    ));

    connection
        .request(
            RequestId::new("author-draft"),
            RequestBody::LoomAuthorDraft {
                session_id: session_id.clone(),
                kind: LoomAuthorKind::Workflow,
                prose: "Package a research result for review".into(),
            },
        )
        .await
        .expect("route draft");
    let ResponseBody::LoomAuthorDraft { draft } = response(&sink, "author-draft").await else {
        panic!("draft response");
    };
    assert_eq!(draft.revision, 1);
    assert!(draft.errors.is_empty());

    let mut invalid_spec: LoomAuthorSpec =
        serde_json::from_str(&draft.text).expect("draft JSON for invalid edit");
    let LoomAuthorSpec::Workflow(workflow) = &mut invalid_spec else {
        panic!("workflow draft");
    };
    workflow.nodes[0]
        .evidence
        .as_mut()
        .expect("evidence")
        .required_green = 0;
    let invalid_text = serde_json::to_string_pretty(&invalid_spec).expect("invalid edit");
    let invalid_line = invalid_text
        .lines()
        .position(|line| line.contains("\"required_green\""))
        .map(|line| u32::try_from(line + 1).expect("bounded line"))
        .expect("required_green line");
    connection
        .request(
            RequestId::new("author-revise-invalid"),
            RequestBody::LoomAuthorRevise {
                authoring_id: draft.authoring_id.clone(),
                expected_revision: draft.revision,
                kind: draft.kind,
                text: invalid_text,
            },
        )
        .await
        .expect("route invalid revision");
    let ResponseBody::LoomAuthorRevise { draft: invalid } =
        response(&sink, "author-revise-invalid").await
    else {
        panic!("invalid revision response");
    };
    assert!(invalid.errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::InvalidField
            && error.location.line == invalid_line
            && error.location.field == "nodes[0].evidence.required_green"
    }));
    connection
        .request(
            RequestId::new("author-confirm-invalid"),
            RequestBody::LoomAuthorConfirm {
                authoring_id: invalid.authoring_id.clone(),
                expected_revision: invalid.revision,
                kind: invalid.kind,
                text: invalid.text.clone(),
                expected_rev: Some(0),
                expected_digest: None,
            },
        )
        .await
        .expect("route invalid confirmation");
    let ResponseBody::LoomAuthorConfirm {
        confirmed: None,
        errors,
    } = response(&sink, "author-confirm-invalid").await
    else {
        panic!("invalid confirmation response");
    };
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::InvalidField
            && error.location.line == invalid_line
            && error.location.field == "nodes[0].evidence.required_green"
    }));
    assert!(
        store
            .loom_workflow("review-flow".into())
            .await
            .expect("workflow registry after rejection")
            .is_none()
    );

    let mut first_spec: LoomAuthorSpec = serde_json::from_str(&draft.text).expect("draft JSON");
    let LoomAuthorSpec::Workflow(workflow) = &mut first_spec else {
        panic!("workflow draft");
    };
    workflow.nodes[0].task = "package the first RPC revision".into();
    let first_text = serde_json::to_string(&first_spec).expect("noncanonical first edit");
    connection
        .request(
            RequestId::new("author-revise-1"),
            RequestBody::LoomAuthorRevise {
                authoring_id: draft.authoring_id.clone(),
                expected_revision: invalid.revision,
                kind: LoomAuthorKind::Workflow,
                text: first_text,
            },
        )
        .await
        .expect("route first revision");
    let ResponseBody::LoomAuthorRevise { draft: first } = response(&sink, "author-revise-1").await
    else {
        panic!("first revision response");
    };
    assert!(first.errors.is_empty());
    connection
        .request(
            RequestId::new("author-confirm-1"),
            RequestBody::LoomAuthorConfirm {
                authoring_id: first.authoring_id.clone(),
                expected_revision: first.revision,
                kind: first.kind,
                text: first.text.clone(),
                expected_rev: Some(0),
                expected_digest: None,
            },
        )
        .await
        .expect("route first confirmation");
    let ResponseBody::LoomAuthorConfirm {
        confirmed: Some(first_confirmed),
        errors,
    } = response(&sink, "author-confirm-1").await
    else {
        panic!("first confirmation response");
    };
    assert!(errors.is_empty());
    assert_ne!(first_confirmed.canonical_text, first.text);
    connection
        .request(
            RequestId::new("author-confirm-1-retry"),
            RequestBody::LoomAuthorConfirm {
                authoring_id: first.authoring_id.clone(),
                expected_revision: first.revision,
                kind: first.kind,
                text: first.text.clone(),
                expected_rev: Some(0),
                expected_digest: None,
            },
        )
        .await
        .expect("retry first confirmation");
    let ResponseBody::LoomAuthorConfirm {
        confirmed: Some(replayed_confirmation),
        errors,
    } = response(&sink, "author-confirm-1-retry").await
    else {
        panic!("retried confirmation response");
    };
    assert!(errors.is_empty());
    assert_eq!(replayed_confirmation, first_confirmed);

    // The mutation fence selects the current registry row by name, so prove
    // revision 1 executable while it is current. The pinned graph must then
    // remain on those exact immutable bytes when revision 2 is registered.
    connection
        .request(
            RequestId::new("execute-confirmed-first"),
            RequestBody::GraphPin {
                command_id: CommandId::new("execute-confirmed-first-command"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                template: "review-flow".into(),
                expected_digest: Some(first_confirmed.execution_digest.clone()),
            },
        )
        .await
        .expect("route confirmed workflow execution pin");
    let ResponseBody::GraphPin { digest, .. } = response(&sink, "execute-confirmed-first").await
    else {
        panic!("confirmed hash graph pin response");
    };
    assert_eq!(digest, first_confirmed.execution_digest);

    let mut second_spec: LoomAuthorSpec =
        serde_json::from_str(&first_confirmed.canonical_text).expect("canonical first JSON");
    let LoomAuthorSpec::Workflow(workflow) = &mut second_spec else {
        panic!("workflow confirmation");
    };
    workflow.nodes[0].task = "package the second RPC revision".into();
    let second_text = serde_json::to_string_pretty(&second_spec).expect("second edit");
    connection
        .request(
            RequestId::new("author-revise-2"),
            RequestBody::LoomAuthorRevise {
                authoring_id: first.authoring_id,
                expected_revision: first.revision,
                kind: first.kind,
                text: second_text,
            },
        )
        .await
        .expect("route second revision");
    let ResponseBody::LoomAuthorRevise { draft: second } = response(&sink, "author-revise-2").await
    else {
        panic!("second revision response");
    };
    assert!(second.errors.is_empty());
    connection
        .request(
            RequestId::new("author-confirm-2"),
            RequestBody::LoomAuthorConfirm {
                authoring_id: second.authoring_id,
                expected_revision: second.revision,
                kind: second.kind,
                text: second.text,
                expected_rev: Some(first_confirmed.registration.rev),
                expected_digest: Some(first_confirmed.registration.digest.clone()),
            },
        )
        .await
        .expect("route second confirmation");
    let ResponseBody::LoomAuthorConfirm {
        confirmed: Some(second_confirmed),
        errors,
    } = response(&sink, "author-confirm-2").await
    else {
        panic!("second confirmation response");
    };
    assert!(errors.is_empty());
    assert_eq!(
        (
            first_confirmed.registration.rev,
            second_confirmed.registration.rev
        ),
        (1, 2)
    );
    assert_ne!(
        first_confirmed.execution_digest,
        second_confirmed.execution_digest
    );

    connection
        .request(
            RequestId::new("confirmed-first-stays-pinned"),
            RequestBody::GraphStatus {
                session_id: session_id.clone(),
            },
        )
        .await
        .expect("read pinned first revision after registry advance");
    let ResponseBody::GraphStatus {
        status: Some(status),
    } = response(&sink, "confirmed-first-stays-pinned").await
    else {
        panic!("pinned graph status response");
    };
    assert_eq!(status.digest, first_confirmed.execution_digest);

    for (request_id, digest) in [
        ("execute-first", first_confirmed.execution_digest),
        ("execute-second", second_confirmed.execution_digest),
    ] {
        connection
            .request(
                RequestId::new(request_id),
                RequestBody::WorkflowInstance {
                    workflow_id: "review-flow".into(),
                    template_digest: Some(digest.clone()),
                },
            )
            .await
            .expect("route exact workflow instance");
        let ResponseBody::WorkflowInstance {
            instance: Some(instance),
        } = response(&sink, request_id).await
        else {
            panic!("retained workflow instance response");
        };
        assert_eq!(instance.template_digest, digest);
    }

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn prose_draft_edit_confirm_and_reedit_keep_both_executable_hashes() {
    let ai_text = serde_json::to_string_pretty(&workflow_spec("package the AI draft"))
        .expect("encode AI fixture");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: format!("```json\n{ai_text}\n```"),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let factory = FixedProviderFactory { provider };
    let draft = draft_from_prose(
        "author-1".into(),
        LoomAuthorKind::Workflow,
        "Package a research result for review",
        &[],
        &metadata(),
        &factory,
    )
    .await
    .expect("AI draft");
    assert_eq!(draft.revision, 1);
    assert!(draft.errors.is_empty(), "{:?}", draft.errors);

    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let mut spec: LoomAuthorSpec = serde_json::from_str(&draft.text).expect("typed draft");
    let LoomAuthorSpec::Workflow(workflow) = &mut spec else {
        panic!("workflow draft kind");
    };
    workflow.nodes[0].task = "package the first revision".into();
    let edited = serde_json::to_string_pretty(&spec).expect("edited text");
    let ValidatedLoomAuthorSpec::Workflow {
        source,
        canonical_text,
    } = validate(&edited, LoomAuthorKind::Workflow, &[]).expect("valid edit")
    else {
        panic!("workflow validation");
    };
    assert_eq!(
        serde_json::from_str::<LoomAuthorSpec>(&canonical_text).expect("canonical typed text"),
        spec
    );
    let first_registration = store
        .loom_register_workflow(&source)
        .expect("confirm first revision");
    let first = store
        .loom_workflow(&first_registration.id)
        .expect("read first")
        .expect("first exists");
    let first_execution_digest = graph_template_digest(&first.template);
    let retained_first = store
        .loom_workflow_registered_revision(
            &first_registration.id,
            first_registration.rev,
            &first_registration.digest,
        )
        .expect("read retained first")
        .expect("retained first exists");
    assert_eq!(
        graph_template_digest(&retained_first.template),
        first_execution_digest,
        "confirmation resolves the exact retained execution fence"
    );

    let LoomAuthorSpec::Workflow(workflow) = &mut spec else {
        panic!("workflow draft kind");
    };
    workflow.nodes[0].task = "package the second revision".into();
    let revised_text = serde_json::to_string_pretty(&spec).expect("revised text");
    let ValidatedLoomAuthorSpec::Workflow { source, .. } =
        validate(&revised_text, LoomAuthorKind::Workflow, &[]).expect("valid revision")
    else {
        panic!("workflow validation");
    };
    let second_registration = store
        .loom_register_workflow_cas(
            &source,
            &haider_protocol::loom::LoomRevisionExpectation {
                rev: first_registration.rev,
                digest: Some(first_registration.digest.clone()),
            },
        )
        .expect("confirm second revision");
    let haider_store::LoomRegistryMutation::Applied {
        value: second_registration,
        ..
    } = second_registration
    else {
        panic!("current workflow expectation cannot conflict");
    };
    let second = store
        .loom_workflow(&second_registration.id)
        .expect("read second")
        .expect("second exists");
    let second_execution_digest = graph_template_digest(&second.template);

    assert_eq!((first_registration.rev, second_registration.rev), (1, 2));
    assert_ne!(first_registration.digest, second_registration.digest);
    assert_ne!(first_execution_digest, second_execution_digest);
    let retained = store
        .loom_workflow_revision(&first.id, &first_execution_digest)
        .expect("retained lookup")
        .expect("first hash still executes");
    assert_eq!(retained, first, "confirmation is immutable by hash");
}

#[test]
fn typed_author_document_lowers_fork_join_back_edge_and_evidence_contracts() {
    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let agents = vec![
        typed_agent("research", "Brief", "Research"),
        typed_agent("draft", "Research", "Draft"),
        typed_agent("review", "Research", "Review"),
        typed_agent("release", "Draft + Review", "Release"),
    ];
    for agent in &agents {
        store
            .loom_register_agent_type(agent)
            .expect("register typed fixture");
    }
    let registered = store.loom_agent_types().expect("registered signatures");
    let spec = LoomAuthorSpec::Workflow(LoomAuthorWorkflowSpec {
        id: "typed-release".into(),
        in_type: "Brief".into(),
        out_type: "Release".into(),
        nodes: vec![
            LoomAuthorNodeSpec {
                id: "research".into(),
                agent_type: Some("research".into()),
                task: "collect evidence".into(),
                in_type: "Brief".into(),
                out_type: "Research".into(),
                gate: "command".into(),
                depends_on: Vec::new(),
                back_edge: None,
                evidence: evidence(1),
            },
            LoomAuthorNodeSpec {
                id: "draft".into(),
                agent_type: Some("draft".into()),
                task: "draft release".into(),
                in_type: "Research".into(),
                out_type: "Draft".into(),
                gate: "command".into(),
                depends_on: vec!["research".into()],
                back_edge: None,
                evidence: evidence(1),
            },
            LoomAuthorNodeSpec {
                id: "review".into(),
                agent_type: Some("review".into()),
                task: "review evidence".into(),
                in_type: "Research".into(),
                out_type: "Review".into(),
                gate: "command".into(),
                depends_on: vec!["research".into()],
                back_edge: None,
                evidence: evidence(1),
            },
            LoomAuthorNodeSpec {
                id: "release".into(),
                agent_type: Some("release".into()),
                task: "join and release".into(),
                in_type: "Draft + Review".into(),
                out_type: "Release".into(),
                gate: "all_of".into(),
                depends_on: vec!["draft".into(), "review".into()],
                back_edge: Some("research".into()),
                evidence: evidence(2),
            },
        ],
    });
    let text = serde_json::to_string_pretty(&spec).expect("typed author document");
    let ValidatedLoomAuthorSpec::Workflow { source, .. } =
        validate(&text, LoomAuthorKind::Workflow, &registered).expect("typed author validation")
    else {
        panic!("workflow validation");
    };
    let registration = store
        .loom_register_workflow(&source)
        .expect("register lowered workflow");
    let workflow = store
        .loom_workflow(&registration.id)
        .expect("read workflow")
        .expect("registered workflow");
    let release = workflow
        .template
        .nodes
        .iter()
        .find(|node| node.name.as_str() == "RELEASE")
        .expect("release node");
    assert_eq!(
        release
            .depends_on
            .iter()
            .map(|node| node.as_str())
            .collect::<Vec<_>>(),
        ["DRAFT", "REVIEW"]
    );
    assert_eq!(
        release.red_target.as_ref().map(|node| node.as_str()),
        Some("RESEARCH")
    );
    assert_eq!(
        release.gate,
        haider_protocol::graph::GraphGateKind::AllOfN { n: 2 }
    );
    assert!(workflow.meta.iter().all(|meta| {
        meta.agent_type.is_some()
            && meta.agent_type_rev == Some(1)
            && meta.agent_type_digest.is_some()
    }));
}

#[test]
fn graph_and_capability_errors_point_at_the_offending_values() {
    let mut spec = workflow_spec("bad back edge");
    let LoomAuthorSpec::Workflow(workflow) = &mut spec else {
        panic!("workflow fixture");
    };
    let mut later = workflow.nodes[0].clone();
    later.id = "later".into();
    later.depends_on = vec!["execute".into()];
    workflow.nodes[0].back_edge = Some("later".into());
    workflow.nodes.push(later);
    let text = serde_json::to_string_pretty(&spec).expect("invalid graph text");
    let back_edge_line = text
        .lines()
        .position(|line| line.contains("\"back_edge\": \"later\""))
        .map(|line| u32::try_from(line + 1).expect("bounded line"))
        .expect("back-edge line");
    let errors = validate(&text, LoomAuthorKind::Workflow, &[]).expect_err("forward red edge");
    assert!(errors.iter().any(|error| {
        error.location.field == "nodes[0].back_edge" && error.location.line == back_edge_line
    }));
}

#[tokio::test]
async fn ai_draft_requires_a_terminal_finish_event() {
    let text = serde_json::to_string(&workflow_spec("truncated")).expect("fixture");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText { text },
        FakeStep::PrematureEof,
    ]));
    let error = draft_from_prose(
        "author-truncated".into(),
        LoomAuthorKind::Workflow,
        "draft a workflow",
        &[],
        &metadata(),
        &FixedProviderFactory { provider },
    )
    .await
    .expect_err("premature EOF must reject");
    assert_eq!(error.code, haider_protocol::error::ErrorCode::ProviderError);
}

#[tokio::test]
async fn pinned_agent_resolution_uses_current_then_retained_revision() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let first = store
        .loom_register_agent_type_with_install(typed_agent("writer", "Brief", "Draft"))
        .await
        .expect("register first agent")
        .registration;
    let current = hub
        .pinned_loom_agent_type("writer", Some(first.rev), Some(&first.digest))
        .await
        .expect("exact-current retained lookup")
        .expect("current agent");
    assert_eq!(current.rev, 1);

    let mut changed = typed_agent("writer", "Brief", "Draft");
    changed.job = "A newer writer role.".into();
    let second = store
        .loom_register_agent_type_with_install_cas(
            changed,
            haider_protocol::loom::LoomRevisionExpectation {
                rev: first.rev,
                digest: Some(first.digest.clone()),
            },
        )
        .await
        .expect("register changed agent");
    let haider_core::LoomRegistryMutation::Applied { value: second, .. } = second else {
        panic!("current agent expectation cannot conflict");
    };
    let second = second.registration;
    assert_eq!(second.rev, 2);
    let retained = hub
        .pinned_loom_agent_type("writer", Some(first.rev), Some(&first.digest))
        .await
        .expect("retained lookup")
        .expect("first revision retained on advance");
    assert_eq!(retained.rev, 1);
    assert_eq!(retained.job, "Perform the writer stage.");

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[test]
fn invalid_edit_is_typed_rejected_at_the_offending_field_and_line() {
    let mut spec = workflow_spec("validate one result");
    let LoomAuthorSpec::Workflow(workflow) = &mut spec else {
        panic!("workflow draft kind");
    };
    let mut invalid_node = workflow.nodes[0].clone();
    invalid_node.id = "verify".into();
    invalid_node.depends_on = vec!["execute".into()];
    invalid_node
        .evidence
        .as_mut()
        .expect("evidence")
        .required_green = 0;
    workflow.nodes.push(invalid_node);
    let invalid = serde_json::to_string_pretty(&spec).expect("invalid edit");
    let expected_line = invalid
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("\"required_green\""))
        .map(|(line, _)| u32::try_from(line + 1).expect("bounded test line"))
        .last()
        .expect("required_green line");
    let errors = validate(&invalid, LoomAuthorKind::Workflow, &[]).expect_err("zero green rejects");
    assert!(errors.iter().any(|error| {
        error.location.line == expected_line
            && error.location.field == "nodes[1].evidence.required_green"
            && error.message.contains("required_green")
    }));
}

#[test]
fn structural_edits_report_stable_nested_paths() {
    let errors =
        validate("", LoomAuthorKind::Workflow, &[]).expect_err("empty authoring text rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::Syntax
            && error.location.field == "$"
            && error.location.line == 1
            && error.location.column == 1
    }));

    let valid =
        serde_json::to_string_pretty(&workflow_spec("validate structure")).expect("workflow JSON");
    let wrong_type = valid.replace("\"required_green\": 1", "\"required_green\": \"one\"");
    let errors = validate(&wrong_type, LoomAuthorKind::Workflow, &[])
        .expect_err("nested wrong type rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::InvalidField
            && error.location.field == "nodes[0].evidence.required_green"
    }));

    let missing = valid.replace(",\n        \"required_green\": 1", "");
    let errors = validate(&missing, LoomAuthorKind::Workflow, &[])
        .expect_err("nested missing field rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::MissingField
            && error.location.field == "nodes[0].evidence.required_green"
    }));

    let unknown = valid.replace(
        "        \"required_green\": 1",
        "        \"required_green\": 1,\n        \"unexpected\": true",
    );
    let errors = validate(&unknown, LoomAuthorKind::Workflow, &[])
        .expect_err("nested unknown field rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::InvalidField
            && error.location.field == "nodes[0].evidence.unexpected"
    }));
}

#[test]
fn semantic_graph_edits_report_the_edited_field_or_containing_node() {
    let mut invalid_node_id = workflow_spec("validate node id");
    let LoomAuthorSpec::Workflow(workflow) = &mut invalid_node_id else {
        panic!("workflow draft kind");
    };
    workflow.nodes[0].id = "2nd".into();
    let invalid_id_text = serde_json::to_string_pretty(&invalid_node_id).expect("node id edit");
    let errors = validate(&invalid_id_text, LoomAuthorKind::Workflow, &[])
        .expect_err("graph-invalid node id rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::InvalidField
            && error.location.field == "nodes[0].id"
    }));

    let mut invalid_node_output = workflow_spec("validate node output");
    let LoomAuthorSpec::Workflow(workflow) = &mut invalid_node_output else {
        panic!("workflow draft kind");
    };
    workflow.nodes[0].out_type = "not a type".into();
    let invalid_output_text =
        serde_json::to_string_pretty(&invalid_node_output).expect("node output edit");
    let errors = validate(&invalid_output_text, LoomAuthorKind::Workflow, &[])
        .expect_err("malformed node out_type rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::InvalidField
            && error.location.field == "nodes[0].out_type"
    }));

    let mut output_mismatch = workflow_spec("validate output");
    let LoomAuthorSpec::Workflow(workflow) = &mut output_mismatch else {
        panic!("workflow draft kind");
    };
    workflow.out_type = "Report".into();
    let output_text = serde_json::to_string_pretty(&output_mismatch).expect("output edit");
    let errors = validate(&output_text, LoomAuthorKind::Workflow, &[])
        .expect_err("workflow output mismatch rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::TypeMismatch
            && error.location.field == "out_type"
            && error.location.line > 1
    }));

    let mut control_widening = workflow_spec("misleading control type");
    let LoomAuthorSpec::Workflow(workflow) = &mut control_widening else {
        panic!("workflow draft kind");
    };
    workflow.nodes[0].in_type = "Brief + Report".into();
    let widening_text = serde_json::to_string_pretty(&control_widening).expect("widening edit");
    let errors = validate(&widening_text, LoomAuthorKind::Workflow, &[])
        .expect_err("control-node widening rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::TypeMismatch
            && error.location.field == "nodes[0].in_type"
    }));

    let mut missing_evidence = workflow_spec("validate evidence");
    let LoomAuthorSpec::Workflow(workflow) = &mut missing_evidence else {
        panic!("workflow draft kind");
    };
    workflow.nodes[0].evidence = None;
    let missing_text = serde_json::to_string_pretty(&missing_evidence).expect("missing evidence");
    let node_line = missing_text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == "{")
        .map(|(line, _)| u32::try_from(line + 1).expect("bounded node line"))
        .last()
        .expect("node object line");
    let errors = validate(&missing_text, LoomAuthorKind::Workflow, &[])
        .expect_err("missing semantic evidence rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::MissingField
            && error.location.field == "nodes[0].evidence"
            && error.location.line == node_line
    }));

    let mut case_collision = workflow_spec("first node");
    let LoomAuthorSpec::Workflow(workflow) = &mut case_collision else {
        panic!("workflow draft kind");
    };
    workflow.nodes[0].id = "step".into();
    let mut second = workflow.nodes[0].clone();
    second.id = "STEP".into();
    second.depends_on = vec!["step".into()];
    workflow.nodes.push(second);
    let collision_text = serde_json::to_string_pretty(&case_collision).expect("collision edit");
    let errors = validate(&collision_text, LoomAuthorKind::Workflow, &[])
        .expect_err("case-folded node collision rejects");
    assert!(errors.iter().any(|error| {
        error.code == haider_protocol::loom::LoomAuthorValidationCode::DuplicateValue
            && error.location.field == "nodes[1].id"
    }));
}

#[test]
fn agent_capability_denials_are_content_bearing_and_never_grants() {
    let spec = LoomAuthorSpec::AgentType(LoomAuthorAgentTypeSpec {
        id: "release-inspector".into(),
        name: "Release inspector".into(),
        job: "Inspect release artifacts".into(),
        in_type: "Artifact".into(),
        out_type: "Report".into(),
        capability_keys: vec!["cli:rg".into(), "api:example.invalid".into()],
        grants: vec!["cli:rg".into()],
        denials: vec!["api:example.invalid".into()],
        skills: Vec::new(),
        scripts: Vec::new(),
        color: String::new(),
        glyph: String::new(),
    });
    let edited = serde_json::to_string_pretty(&spec).expect("edited text");
    let ValidatedLoomAuthorSpec::AgentType { record, .. } =
        validate(&edited, LoomAuthorKind::AgentType, &[]).expect("valid agent")
    else {
        panic!("agent validation");
    };
    assert_eq!(record.clis, vec!["rg"]);
    assert!(record.apis.is_empty());
    assert_eq!(record.denials, vec!["api:example.invalid"]);
    let denied_digest = record.digest();
    let mut without_denial = record;
    without_denial.denials.clear();
    assert_ne!(denied_digest, without_denial.digest());

    let LoomAuthorSpec::AgentType(mut invalid) = spec else {
        panic!("agent draft kind");
    };
    invalid.capability_keys = vec!["api:example.invalid:443".into()];
    invalid.grants = invalid.capability_keys.clone();
    invalid.denials.clear();
    let invalid_text =
        serde_json::to_string_pretty(&LoomAuthorSpec::AgentType(invalid)).expect("invalid edit");
    let errors = validate(&invalid_text, LoomAuthorKind::AgentType, &[])
        .expect_err("API ports reject before registry mutation");
    assert!(errors.iter().any(|error| {
        error.location.field == "grants"
            && error.location.line
                == invalid_text
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| line.contains("api:example.invalid:443"))
                    .map(|(line, _)| u32::try_from(line + 1).expect("bounded test line"))
                    .last()
                    .expect("offending grant line")
    }));
}
