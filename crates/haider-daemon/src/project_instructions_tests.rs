#![allow(clippy::expect_used)]

use crate::project_instructions::{
    MAX_PROJECT_INSTRUCTION_FILE_BYTES, MAX_PROJECT_INSTRUCTION_TOTAL_BYTES, TRUNCATION_MARKER,
    load,
};
use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, SystemPromptBuilder,
    WorkerDependencies, WorkerManager, WorkerManagerHandle, cache_grant_scope_digest,
};
use async_trait::async_trait;
use haider_core::{
    BranchCreateCommand, SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
    estimate_provider_request_input_tokens,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::Grant;
use haider_protocol::context::ContextFootprint;
use haider_protocol::effect::EffectClass;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{BranchId, DeviceId, EventId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::project_instructions::ProjectInstructionsLoaded;
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::state::SessionState;
use haider_provider::{
    FakeProvider, FakeStep, Message, Provider, ProviderError, ProviderStream, ToolDefinition,
    TurnRequest,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::time::{Duration, timeout};

fn daemon_session_context(request: &TurnRequest) -> &str {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message.blocks.as_slice() {
            [Block::Text { text }] if text.starts_with("[DAEMON-BOUND SESSION CONTEXT]") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .expect("daemon-bound session context")
}

// Turns that execute the production Windows PowerShell pay a cold-start cost
// that can exceed five seconds under the concurrent crate gate. Keep Unix's
// existing budget byte-for-byte and widen only the Windows test ceiling.
#[cfg(windows)]
const PROCESS_TURN_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(not(windows))]
const PROCESS_TURN_DEADLINE: Duration = Duration::from_secs(5);

struct FixedProviderFactory {
    provider: Arc<dyn Provider>,
    context_window: Option<u64>,
}

#[async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: self.context_window,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

struct TestWorker {
    _profile: TempDir,
    store: SqliteStoreHandle,
    hub: SessionHub,
    manager: WorkerManager,
    handle: WorkerManagerHandle,
    session_id: SessionId,
    device_id: DeviceId,
}

impl TestWorker {
    async fn start(
        workspace: &Path,
        provider: Arc<dyn Provider>,
        session_suffix: &str,
        max_tokens: u64,
        context_window: Option<u64>,
    ) -> Self {
        let profile = tempfile::tempdir().expect("profile");
        let store = SqliteStoreHandle::open(profile.path())
            .await
            .expect("store");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                diagnostics: None,
                provider_factory: Arc::new(FixedProviderFactory {
                    provider,
                    context_window,
                }),
                tool_factory: Arc::new(BrokerToolFactory),
                delegation: None,
                web_search: None,
            },
            false,
        );
        let handle = manager.handle();
        hub.install_worker_manager(handle.clone())
            .expect("install worker manager");
        let session_id = SessionId::new(format!("instructions-{session_suffix}"));
        let device_id = DeviceId::new(format!("instructions-device-{session_suffix}"));
        let cwd = canonical_utf8(workspace);
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("create-instructions-{session_suffix}"),
            request_digest: format!("create-instructions-digest-{session_suffix}"),
            request_json: format!(r#"{{"session":"{session_suffix}"}}"#),
            session_id: session_id.clone(),
            cwd,
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens,
            permission_overrides: Some(SessionPermissionOverridesV1 {
                allow_writes: false,
                allow_exec: true,
                allow_mobile: false,
                auto_allow: false,
            }),
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("created-instructions-{session_suffix}")),
            device_id: device_id.clone(),
        })
        .await
        .expect("create session");
        Self {
            _profile: profile,
            store,
            hub,
            manager,
            handle,
            session_id,
            device_id,
        }
    }

    async fn submit(&self, suffix: &str) -> RunId {
        self.submit_on_branch_with_deadline(suffix, None, Duration::from_secs(5))
            .await
    }

    async fn submit_on_branch(&self, suffix: &str, branch_id: Option<BranchId>) -> RunId {
        self.submit_on_branch_with_deadline(suffix, branch_id, Duration::from_secs(5))
            .await
    }

    async fn submit_on_branch_with_deadline(
        &self,
        suffix: &str,
        branch_id: Option<BranchId>,
        deadline: Duration,
    ) -> RunId {
        let run_id = RunId::new(format!("instructions-run-{suffix}"));
        let accepted = self
            .hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("submit-instructions-{suffix}"),
                request_digest: format!("submit-instructions-digest-{suffix}"),
                request_json: format!(r#"{{"turn":"{suffix}"}}"#),
                session_id: self.session_id.clone(),
                worker_generation: self.store.worker_generation(),
                run_id: run_id.clone(),
                agent_id: None,
                branch_id,
                text: format!("turn {suffix}"),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
                queued_event_id: EventId::new(format!("queued-instructions-{suffix}")),
                user_event_id: EventId::new(format!("user-instructions-{suffix}")),
                active_event_id: EventId::new(format!("active-instructions-{suffix}")),
                device_id: self.device_id.clone(),
            })
            .await
            .expect("accept turn");
        self.handle.submit(accepted).await.expect("submit turn");
        wait_for_terminal(&self.store, &self.session_id, &run_id, deadline).await;
        run_id
    }

    async fn close(self) {
        self.manager.shutdown().await.expect("manager shutdown");
        self.hub.shutdown().await.expect("hub shutdown");
        self.store.close().await.expect("store close");
    }
}

fn canonical_utf8(path: &Path) -> String {
    std::fs::canonicalize(path)
        .expect("canonical path")
        .to_str()
        .expect("UTF-8 path")
        .to_owned()
}

async fn wait_for_terminal(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    deadline: Duration,
) {
    timeout(deadline, async {
        loop {
            let events = store.read(session_id, 0, 512).await.expect("read events");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(run_id)
                    && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                        |payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()),
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn terminalizes");
}

async fn wait_for_idle_after_run(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            let events = store.read(session_id, 0, 512).await.expect("read events");
            let terminal_seq = events.iter().rev().find_map(|event| {
                (event.run_id.as_ref() == Some(run_id)
                    && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                        |payload| {
                            matches!(payload, EventPayload::RunState(state) if state.is_terminal())
                        },
                    ))
                .then_some(event.seq)
            });
            if terminal_seq.is_some_and(|terminal_seq| {
                events.iter().any(|event| {
                    event.seq > terminal_seq
                        && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                            |payload| {
                                matches!(
                                    payload,
                                    EventPayload::SessionState(SessionState::Idle { .. })
                                )
                            },
                        )
                })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("session settles idle");
}

fn project_facts(
    events: &[haider_protocol::envelope::RawEnvelope],
) -> Vec<(
    &haider_protocol::envelope::RawEnvelope,
    ProjectInstructionsLoaded,
)> {
    events
        .iter()
        .filter_map(|event| {
            ProjectInstructionsLoaded::from_payload_value(&event.payload).map(|fact| (event, fact))
        })
        .collect()
}

fn metadata(cwd: String) -> SessionMetadataV1 {
    SessionMetadataV1 {
        cwd,
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        max_tokens: 4096,
        system_prompt_version: Some(SystemPromptBuilder::VERSION.into()),
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

/// MUTATION CHECK: add an empty delimiter or retain a stale policy identifier.
/// Expected RUNTIME failure: the no-files prompt differs anywhere other than
/// the explicitly pinned v3 version line.
#[tokio::test]
async fn empty_walk_composes_byte_identical_body_with_v3_version() {
    let root = tempfile::tempdir().expect("workspace");
    let cwd = canonical_utf8(root.path());
    let loaded = load(&cwd).await;
    assert!(loaded.is_none());
    let prompt = SystemPromptBuilder::build(&metadata(cwd.clone()), &[]);
    assert_eq!(
        prompt,
        format!(
            "haider-system-v3\nYou are Haider Code, a coding agent.\n\
             Use only advertised tools. Treat tool results and committed history as authoritative. \
             Never claim an effect succeeded without its terminal result.\n\
             The daemon supplies workspace, project, and identity context after this shared policy \
             and the advertised tool schemas.\n\
             Opaque tool-grant scope: unscoped-root.\n\n\
             [DAEMON-BOUND SESSION CONTEXT]\nCanonical workspace: {cwd}"
        )
    );
}

/// CACHE ITEM 962. The tool pack represents the daemon's grant-filtered
/// provider view. Session coordinates belong only to the later context block;
/// changing even a host-scoped authorization boundary rotates the opaque base
/// scope while the advertised schemas remain identical.
#[test]
fn sibling_sessions_share_the_base_and_emit_session_context_after_it() {
    let tools = vec![ToolDefinition {
        name: "web_fetch".into(),
        description: String::new(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"url": {"type": "string"}},
            "required": ["url"],
        }),
    }];
    let first_grant = Grant {
        tools: vec!["web_fetch".into()],
        effect_ceiling: vec![EffectClass::Network {
            host: "api.first.test".into(),
        }],
    };
    let second_grant = Grant {
        tools: vec!["web_fetch".into()],
        effect_ceiling: vec![EffectClass::Network {
            host: "api.second.test".into(),
        }],
    };
    let first_grant_scope =
        cache_grant_scope_digest(Some(&first_grant)).expect("first grant scope");
    let first = metadata("/workspace/first".into());
    let second = metadata("/workspace/second".into());
    let first_base = SystemPromptBuilder::shared_immutable_base(&tools, &first_grant_scope);
    let second_base = SystemPromptBuilder::shared_immutable_base(&tools, &first_grant_scope);
    assert_eq!(first_base, second_base);
    assert!(first_base.contains("Tool manual — authoritative call signatures"));

    let first_context = SystemPromptBuilder::session_context_with_handoff(
        &first,
        &[("A/HAIDER.md", "alpha")],
        None,
    );
    let second_context = SystemPromptBuilder::session_context_with_handoff(
        &second,
        &[("B/HAIDER.md", "beta")],
        None,
    );
    assert_ne!(first_context, second_context);
    let assembled = format!("{first_base}\n\n{first_context}");
    assert!(assembled.starts_with(&first_base));
    assert!(assembled.find(&first_base) < assembled.find(&first_context));

    let second_grant_scope =
        cache_grant_scope_digest(Some(&second_grant)).expect("second grant scope");
    let different_grant_base =
        SystemPromptBuilder::shared_immutable_base(&tools, &second_grant_scope);
    assert_ne!(
        first_base, different_grant_base,
        "a different effect ceiling must create another base with the same schemas"
    );
}

/// MUTATION CHECK: compose cwd-first, prefer AGENTS.md over HAIDER.md, or
/// retain both same-directory files. Expected RUNTIME failure: the ordered
/// file paths or composed precedence assertions change.
#[tokio::test]
async fn nearest_instructions_compose_last_and_haider_wins_within_directory() {
    let root = tempfile::tempdir().expect("workspace");
    let project = root.path().join("project");
    let child = project.join("child");
    std::fs::create_dir_all(&child).expect("nested workspace");
    std::fs::write(project.join("HAIDER.md"), "parent-haider").expect("parent HAIDER");
    std::fs::write(project.join("AGENTS.md"), "shadowed-parent-agents").expect("parent AGENTS");
    std::fs::write(child.join("AGENTS.md"), "nearest-agents").expect("child AGENTS");
    let canonical_project = std::fs::canonicalize(&project).expect("canonical project");
    let canonical_child = canonical_utf8(&child);

    let loaded = load(&canonical_child).await.expect("loaded instructions");
    let relevant = loaded
        .files()
        .iter()
        .filter(|file| Path::new(&file.path).starts_with(&canonical_project))
        .collect::<Vec<_>>();
    assert_eq!(relevant.len(), 2);
    assert!(Path::new(&relevant[0].path).ends_with(Path::new("project").join("HAIDER.md")));
    assert_eq!(relevant[0].text, "parent-haider");
    assert!(
        Path::new(&relevant[1].path)
            .ends_with(Path::new("project").join("child").join("AGENTS.md"))
    );
    assert_eq!(relevant[1].text, "nearest-agents");
    assert!(
        loaded
            .files()
            .iter()
            .all(|file| file.text != "shadowed-parent-agents")
    );

    let prompt = SystemPromptBuilder::build(&metadata(canonical_child), &loaded.prompt_entries());
    for file in relevant {
        assert!(prompt.contains(&format!("Project instructions ({}):", file.path)));
    }
    assert!(prompt.find("parent-haider") < prompt.find("nearest-agents"));
}

/// MUTATION CHECK: truncate bytes without reserving the marker or split a
/// multibyte scalar. Expected RUNTIME failure: the loaded body is invalid,
/// exceeds 48 KiB, lacks the marker, or reports a mismatched digest/length.
#[tokio::test]
async fn per_file_cap_truncates_at_utf8_boundary_with_marker() {
    let root = tempfile::tempdir().expect("workspace");
    let prefix_budget = MAX_PROJECT_INSTRUCTION_FILE_BYTES - TRUNCATION_MARKER.len();
    let mut source = "a".repeat(prefix_budget.saturating_sub(1));
    source.push('€');
    source.push_str(&"z".repeat(128));
    std::fs::write(root.path().join("HAIDER.md"), source).expect("oversized instructions");
    let loaded = load(&canonical_utf8(root.path()))
        .await
        .expect("loaded instructions");
    let file = loaded
        .files()
        .iter()
        .find(|file| file.path.ends_with("HAIDER.md"))
        .expect("loaded HAIDER");
    assert!(file.truncated);
    assert!(file.text.ends_with(TRUNCATION_MARKER));
    assert!(file.text.len() <= MAX_PROJECT_INSTRUCTION_FILE_BYTES);
    assert_eq!(
        file.digest,
        blake3::hash(file.text.as_bytes()).to_hex().to_string()
    );
    assert_eq!(
        std::str::from_utf8(file.text.as_bytes()).expect("valid UTF-8"),
        file.text
    );
}

/// MUTATION CHECK: spend the aggregate budget root-first or omit the 96 KiB
/// cap. Expected RUNTIME failure: the nearest files are truncated first, the
/// composition order changes, or contributed bytes exceed the total limit.
#[tokio::test]
async fn total_cap_preserves_nearest_files_and_composes_them_last() {
    let root = tempfile::tempdir().expect("workspace");
    let grand = root.path().join("grand");
    let parent = grand.join("parent");
    let child = parent.join("child");
    std::fs::create_dir_all(&child).expect("nested workspace");
    for (directory, byte) in [(&grand, b'g'), (&parent, b'p'), (&child, b'c')] {
        std::fs::write(directory.join("HAIDER.md"), vec![byte; 40 * 1024])
            .expect("instruction file");
    }
    let canonical_grand = std::fs::canonicalize(&grand).expect("canonical grand");
    let loaded = load(&canonical_utf8(&child))
        .await
        .expect("loaded instructions");
    let relevant = loaded
        .files()
        .iter()
        .filter(|file| Path::new(&file.path).starts_with(&canonical_grand))
        .collect::<Vec<_>>();
    assert_eq!(relevant.len(), 3);
    assert!(Path::new(&relevant[0].path).ends_with(Path::new("grand").join("HAIDER.md")));
    assert!(relevant[0].truncated);
    assert!(
        Path::new(&relevant[1].path).ends_with(Path::new("grand").join("parent").join("HAIDER.md"))
    );
    assert!(!relevant[1].truncated);
    assert!(
        Path::new(&relevant[2].path).ends_with(
            Path::new("grand")
                .join("parent")
                .join("child")
                .join("HAIDER.md")
        )
    );
    assert!(!relevant[2].truncated);
    assert!(
        loaded
            .files()
            .iter()
            .map(|file| file.text.len())
            .sum::<usize>()
            <= MAX_PROJECT_INSTRUCTION_TOTAL_BYTES
    );
}

/// MUTATION CHECK: canonicalize through a symlinked parent and continue the
/// walk. Expected RUNTIME failure: the noncanonical workspace contributes a
/// file instead of stopping before any symlink traversal.
#[cfg(unix)]
#[tokio::test]
async fn upward_walk_refuses_symlinked_parents_and_stops_at_root() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("workspace");
    let real = root.path().join("real");
    let child = real.join("child");
    std::fs::create_dir_all(&child).expect("real workspace");
    std::fs::write(real.join("HAIDER.md"), "must-not-load-through-link").expect("instructions");
    let link = root.path().join("linked");
    symlink(&real, &link).expect("parent symlink");
    let noncanonical = link.join("child");
    let loaded = load(noncanonical.to_str().expect("UTF-8 link path")).await;
    assert!(loaded.is_none());

    let root_loaded = load("/").await;
    if let Some(root_loaded) = root_loaded {
        assert!(root_loaded.files().iter().all(|file| {
            Path::new(&file.path)
                .parent()
                .is_some_and(|parent| parent == Path::new("/"))
        }));
        assert!(root_loaded.files().len() <= 1);
    }
}

#[derive(Clone)]
struct EditingProvider {
    inner: FakeProvider,
    instruction_path: PathBuf,
    calls: Arc<AtomicUsize>,
}

#[cfg(unix)]
const PINNED_EXEC_COMMAND: &str = "printf pinned";
#[cfg(windows)]
const PINNED_EXEC_COMMAND: &str = "[Console]::Out.Write('pinned')";

#[async_trait]
impl Provider for EditingProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.inner.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            std::fs::write(&self.instruction_path, "edited-for-next-logical-turn")
                .expect("edit instructions between rounds");
        }
        self.inner.stream_turn(request).await
    }
}

/// MUTATION CHECK: reload instructions for a retry/tool round or freeze them
/// across accepted turns. Expected RUNTIME failure: the first two provider
/// prompts differ or the next logical turn still contains the old bytes.
#[tokio::test]
async fn one_pinned_logical_turn_sees_one_snapshot_and_edits_apply_next_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let instruction_path = workspace.path().join("HAIDER.md");
    std::fs::write(&instruction_path, "original-pinned-policy").expect("instructions");
    let inner = FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "pinning-exec".into(),
            name: "process_exec".into(),
            args: serde_json::json!({"command": PINNED_EXEC_COMMAND}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "pinning-exec".into(),
        },
        FakeStep::EmitText {
            text: "first done".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "second done".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(EditingProvider {
        inner: inner.clone(),
        instruction_path,
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let worker = TestWorker::start(workspace.path(), provider, "pinning", 4096, Some(64_000)).await;
    worker
        .submit_on_branch_with_deadline("pinning-one", None, PROCESS_TURN_DEADLINE)
        .await;
    worker.submit("pinning-two").await;

    let requests = inner.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert!(daemon_session_context(&requests[0]).contains("original-pinned-policy"));
    assert!(daemon_session_context(&requests[2]).contains("edited-for-next-logical-turn"));
    assert_eq!(
        requests[1].system_prompt, requests[2].system_prompt,
        "task-specific instruction edits must not rotate the shared base"
    );
    worker.close().await;
}

/// MUTATION CHECK: omit the fact, render it into replay, emit it every turn,
/// digest different bytes, retain stale nonempty state after removal, or send
/// the loader through the effect broker. Expected RUNTIME failure: the fact
/// sequence/coordinates change, prompt render is not Omit, digest/bytes do not
/// prove the provider body, or an Effect row appears.
#[tokio::test]
async fn loaded_fact_is_durable_omitted_change_only_and_not_a_broker_effect() {
    let workspace = tempfile::tempdir().expect("workspace");
    let instruction_path = workspace.path().join("HAIDER.md");
    std::fs::write(&instruction_path, "fact-alpha").expect("instructions");
    let fake = Arc::new(FakeProvider::new(
        (0..5)
            .flat_map(|index| {
                [
                    FakeStep::EmitText {
                        text: format!("done-{index}"),
                    },
                    FakeStep::Finish {
                        reason: FinishReason::EndTurn,
                    },
                ]
            })
            .collect(),
    ));
    let worker =
        TestWorker::start(workspace.path(), fake.clone(), "facts", 4096, Some(64_000)).await;
    let first = worker.submit("fact-1").await;
    worker.submit("fact-2-unchanged").await;
    std::fs::write(&instruction_path, "fact-beta").expect("edit instructions");
    let changed = worker.submit("fact-3-changed").await;
    std::fs::remove_file(&instruction_path).expect("remove instructions");
    let removed = worker.submit("fact-4-removed").await;
    worker.submit("fact-5-empty-unchanged").await;

    let events = worker
        .store
        .read(&worker.session_id, 0, 512)
        .await
        .expect("journal");
    let facts = project_facts(&events);
    assert_eq!(facts.len(), 3);
    assert_eq!(facts[0].0.run_id.as_ref(), Some(&first));
    assert_eq!(facts[1].0.run_id.as_ref(), Some(&changed));
    assert_eq!(facts[2].0.run_id.as_ref(), Some(&removed));
    assert!(facts.iter().all(|(event, _)| {
        event.branch_id.is_none()
            && !event.render.ui
            && event.render.durable
            && event.render.prompt == PromptRender::Omit
    }));
    assert_eq!(facts[0].1.files.len(), 1);
    assert_eq!(facts[0].1.files[0].bytes, 10);
    assert_eq!(
        facts[0].1.files[0].digest,
        blake3::hash(b"fact-alpha").to_hex().to_string()
    );
    assert_eq!(facts[1].1.files[0].bytes, 9);
    assert!(facts[2].1.files.is_empty());
    assert!(events.iter().all(|event| {
        !serde_json::from_value::<EventPayload>(event.payload.clone())
            .is_ok_and(|payload| matches!(payload, EventPayload::Effect(_)))
    }));

    let requests = fake.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert!(daemon_session_context(&requests[2]).contains("fact-beta"));
    assert!(!daemon_session_context(&requests[3]).contains("Project instructions ("));
    assert_eq!(requests[3].system_prompt, requests[4].system_prompt);
    worker.close().await;
}

/// MUTATION CHECK: stamp the supplemental fact on main or re-read a mutable
/// active branch instead of the accepted branch coordinate. Expected RUNTIME
/// failure: the changed branch turn's fact is absent or does not carry the
/// exact named branch and run identifiers.
#[tokio::test]
async fn loaded_fact_keeps_the_accepted_named_branch_coordinate() {
    let workspace = tempfile::tempdir().expect("workspace");
    let instruction_path = workspace.path().join("HAIDER.md");
    std::fs::write(&instruction_path, "main-policy").expect("instructions");
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "main answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "branch answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let worker = TestWorker::start(workspace.path(), fake, "fact-branch", 4096, Some(64_000)).await;
    let main_run = worker.submit("fact-branch-main").await;
    let main_events = worker
        .store
        .read(&worker.session_id, 0, 256)
        .await
        .expect("main journal");
    let (fork_node_id, fork_seq) = main_events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(&main_run))
        .filter_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, event.seq))
        })
        .next_back()
        .expect("main head");
    let branch_id = BranchId::new("instructions-branch-a");
    let request_json = r#"{"branch":"instructions-branch-a"}"#.to_owned();
    worker
        .store
        .create_branch(BranchCreateCommand {
            command_id: "create-instructions-branch-a".into(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: worker.session_id.clone(),
            worker_generation: worker.store.worker_generation(),
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id,
            fork_seq,
            name: Some("Instruction branch A".into()),
            event_id: EventId::new("created-instructions-branch-a"),
            device_id: worker.device_id.clone(),
        })
        .await
        .expect("create branch");
    std::fs::write(&instruction_path, "branch-policy").expect("change branch policy");
    let branch_run = worker
        .submit_on_branch("fact-branch-turn", Some(branch_id.clone()))
        .await;
    let events = worker
        .store
        .read(&worker.session_id, 0, 512)
        .await
        .expect("branch journal");
    let branch_fact = project_facts(&events)
        .into_iter()
        .find(|(event, _)| event.run_id.as_ref() == Some(&branch_run))
        .expect("branch instruction fact");
    assert_eq!(branch_fact.0.branch_id.as_ref(), Some(&branch_id));
    assert_eq!(
        branch_fact.1.files[0].digest,
        blake3::hash(b"branch-policy").to_hex().to_string()
    );
    worker.close().await;
}

/// MUTATION CHECK: trust the journaled pre-crash digest instead of re-reading,
/// duplicate a matching same-run fact, or fail to append a corrected fact.
/// Expected RUNTIME failure: recovery sends the old policy or the same run's
/// ordered facts do not end with the re-read digest semantics.
#[tokio::test]
async fn recovery_rereads_and_journals_a_fresh_same_run_fact_on_digest_change() {
    let profile = tempfile::tempdir().expect("profile");
    let workspace = tempfile::tempdir().expect("workspace");
    let instruction_path = workspace.path().join("HAIDER.md");
    std::fs::write(&instruction_path, "before-crash").expect("instructions");
    let session_id = SessionId::new("instructions-recovery");
    let run_id = RunId::new("instructions-recovered-run");
    let device_id = DeviceId::new("instructions-recovery-device");
    let first = SqliteStoreHandle::open(profile.path())
        .await
        .expect("first store");
    first
        .create_session(SessionCreateCommand {
            command_id: "create-instructions-recovery".into(),
            request_digest: "create-instructions-recovery-digest".into(),
            request_json: r#"{"session":"instructions-recovery"}"#.into(),
            session_id: session_id.clone(),
            cwd: canonical_utf8(workspace.path()),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new("created-instructions-recovery"),
            device_id: device_id.clone(),
        })
        .await
        .expect("create session");
    first
        .accept_turn(TurnAcceptCommand {
            command_id: "accept-instructions-recovery".into(),
            request_digest: "accept-instructions-recovery-digest".into(),
            request_json: r#"{"turn":"recover"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: first.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "recover me".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("queued-instructions-recovery"),
            user_event_id: EventId::new("user-instructions-recovery"),
            active_event_id: EventId::new("active-instructions-recovery"),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept recovery turn");
    let old_loaded = load(&canonical_utf8(workspace.path()))
        .await
        .expect("old instructions");
    let mut old_fact = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("old-instructions-fact"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: device_id.clone(),
        authority_epoch: 0,
        worker_generation: first.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: old_loaded.fact().to_payload_value().expect("old fact"),
    }];
    StoreHandle::append(&first, &mut old_fact)
        .await
        .expect("append old fact");
    first.close().await.expect("close first store");
    std::fs::write(&instruction_path, "after-crash-wins").expect("edit after crash");

    let recovered = SqliteStoreHandle::open(profile.path())
        .await
        .expect("recovered store");
    let mut work = recover_interrupted_turns(&recovered, &DeviceId::new("recovery-scan"))
        .await
        .expect("recover work");
    assert_eq!(work.len(), 1);
    let accepted = match work.pop().expect("queued work") {
        RecoveredWork::Queued(accepted) => accepted,
        RecoveredWork::Retry(_)
        | RecoveredWork::Checkpoint(_)
        | RecoveredWork::PartialStream(_)
        | RecoveredWork::ChildWait(_) => {
            panic!("expected queued recovery")
        }
    };
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "recovered".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(recovered.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: fake.clone(),
                context_window: Some(64_000),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let handle = manager.handle();
    hub.install_worker_manager(handle.clone())
        .expect("install manager");
    handle
        .recover_queued(accepted)
        .await
        .expect("resume recovery");
    wait_for_terminal(&recovered, &session_id, &run_id, Duration::from_secs(5)).await;

    let recovered_requests = fake.requests();
    let recovered_context = daemon_session_context(&recovered_requests[0]);
    assert!(
        recovered_context.contains("after-crash-wins")
            && !recovered_context.contains("before-crash")
    );
    let events = recovered
        .read(&session_id, 0, 512)
        .await
        .expect("recovered journal");
    let same_run = project_facts(&events)
        .into_iter()
        .filter(|(event, _)| event.run_id.as_ref() == Some(&run_id))
        .collect::<Vec<_>>();
    assert_eq!(same_run.len(), 2);
    assert_eq!(
        same_run.last().expect("fresh fact").1.files[0].digest,
        blake3::hash(b"after-crash-wins").to_hex().to_string()
    );
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    recovered.close().await.expect("close recovered store");
}

/// MUTATION CHECK: omit project instructions from request estimation or
/// rebuild manual post-compaction policy without the pinned block. Expected
/// RUNTIME failure: direct estimation does not grow or the durable manual
/// reset footprint differs from the exact composed system-prompt estimate.
#[tokio::test]
async fn footprint_and_manual_compaction_fit_include_instruction_bytes() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("HAIDER.md"),
        "manual-compaction-policy ".repeat(128),
    )
    .expect("instructions");
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "durable answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "compacted summary".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let worker = TestWorker::start(
        workspace.path(),
        fake.clone(),
        "compaction",
        4096,
        Some(64_000),
    )
    .await;
    let first_run = worker.submit("before-compaction").await;
    // A terminal RunState is committed by the harness before the worker
    // supervisor clears its active slot and commits aggregate Idle. Manual
    // compaction is intentionally idle-only, so synchronize on that durable
    // fence rather than racing the supervisor tail.
    wait_for_idle_after_run(&worker.store, &worker.session_id, &first_run).await;
    let first_request = fake.requests().into_iter().next().expect("seed request");
    let prompt = first_request
        .system_prompt
        .clone()
        .expect("composed prompt");
    assert_eq!(
        prompt,
        SystemPromptBuilder::shared_immutable_base(
            &first_request.tools,
            SystemPromptBuilder::UNSCOPED_GRANT_SCOPE,
        )
    );
    let session_context = daemon_session_context(&first_request).to_owned();
    assert!(session_context.contains("manual-compaction-policy"));

    worker
        .handle
        .compact(
            worker.session_id.clone(),
            "instructions-manual-compact".into(),
            worker.store.worker_generation(),
            None,
        )
        .await
        .expect("manual compaction");
    let expected = estimate_provider_request_input_tokens(
        &[
            Message::user_text("compacted summary"),
            Message::user_text(session_context),
        ],
        &Some(prompt),
        &first_request.tools,
        &[],
    );
    let events = worker
        .store
        .read(&worker.session_id, 0, 512)
        .await
        .expect("journal");
    let footprint = events
        .iter()
        .filter_map(|event| {
            let EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            }) = serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (kind == haider_protocol::context::CONTEXT_FOOTPRINT_EXTENSION_KIND)
                .then(|| serde_json::from_value::<ContextFootprint>(data).ok())
                .flatten()
        })
        .next_back()
        .expect("manual compaction footprint");
    assert_eq!(footprint.input_tokens, expected);
    worker.close().await;
}
