//! G2 wire laws for `session.rename` over a real UnixStream: one receipted
//! command updates `sessions.meta_json`, journals the `session_renamed`
//! config fact atomically with the receipt, and surfaces the title through
//! `session.list` — plus the daemon-side auto-title on a session's FIRST
//! accepted turn.

#![allow(clippy::expect_used)]

mod support;

use haider_daemon::DaemonConfig;
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::provider::FinishReason;
use haider_protocol::session::SessionConfigEventPayload;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use haider_rpc::{
    AttachMode, ClientKind, CommandId, RequestBody, RequestId, ResponseBody, SeqRange, WireFrame,
};
use std::fs;
use std::sync::Arc;
use support::{UdsClient, ready_with_dependencies, test_root};

fn text_turn(text: &str) -> Vec<FakeStep> {
    vec![
        FakeStep::EmitText { text: text.into() },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]
}

fn fake_dependencies(
    script: Vec<FakeStep>,
) -> (haider_daemon::DaemonDependencies, Arc<FakeProvider>) {
    let fake = Arc::new(FakeProvider::new(script));
    let factory = fake.clone();
    let dependencies = haider_daemon::DaemonDependencies {
        provider_factory: haider_daemon::ProviderFactoryConfig::Injected {
            factory: Arc::new(SingleFactory { fake: factory }),
            providers: std::collections::BTreeSet::from(["fake".to_owned()]),
        },
        ..haider_daemon::DaemonDependencies::default()
    };
    (dependencies, fake)
}

#[derive(Clone)]
struct SingleFactory {
    fake: Arc<FakeProvider>,
}

#[async_trait::async_trait]
impl haider_daemon::ProviderFactory for SingleFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<haider_daemon::ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(haider_daemon::ResolvedTurnProvider {
            provider: self.fake.clone(),
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

async fn send(client: &mut UdsClient, config: &DaemonConfig, request_id: &str, body: RequestBody) {
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new(request_id),
                body,
            },
            config.frame_limit,
        )
        .await;
}

async fn next_response(client: &mut UdsClient) -> ResponseBody {
    loop {
        if let WireFrame::Response { body, .. } = client.next().await {
            return body;
        }
    }
}

async fn create_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &std::path::Path,
    create_command: &str,
) -> (haider_protocol::ids::SessionId, u64) {
    send(
        client,
        config,
        create_command,
        RequestBody::SessionCreate {
            command_id: CommandId::new(format!("{create_command}-command")),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
        },
    )
    .await;
    let ResponseBody::SessionCreate {
        session_id,
        worker_generation,
        ..
    } = next_response(client).await
    else {
        panic!("expected session.create response");
    };
    send(
        client,
        config,
        &format!("{create_command}-attach"),
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    loop {
        if matches!(client.next().await, WireFrame::AttachCaughtUp { .. }) {
            break;
        }
    }
    (session_id, worker_generation)
}

async fn run_turn(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &haider_protocol::ids::SessionId,
    generation: u64,
    label: &str,
    text: &str,
) {
    send(
        client,
        config,
        label,
        RequestBody::TurnSubmit {
            command_id: CommandId::new(format!("{label}-command")),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: text.into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && serde_json::from_value::<EventPayload>(envelope.payload.into())
                .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
        {
            return;
        }
    }
}

fn rename_body(
    command_id: &str,
    session_id: &haider_protocol::ids::SessionId,
    generation: u64,
    title: Option<&str>,
) -> RequestBody {
    RequestBody::SessionRename {
        command_id: CommandId::new(command_id),
        session_id: session_id.clone(),
        worker_generation: generation,
        title: title.map(str::to_owned),
    }
}

async fn list_title(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &haider_protocol::ids::SessionId,
    request_id: &str,
) -> (Option<String>, Option<String>) {
    send(
        client,
        config,
        request_id,
        RequestBody::SessionList {
            cursor: None,
            limit: 50,
        },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } = client.next().await
        {
            let summary = sessions
                .into_iter()
                .find(|summary| &summary.session_id == session_id)
                .expect("listed session");
            let metadata_title = summary.metadata.and_then(|metadata| metadata.title);
            return (summary.title, metadata_title);
        }
    }
}

async fn read_session(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &haider_protocol::ids::SessionId,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send(
        client,
        config,
        request_id,
        RequestBody::SessionRead {
            session_id: session_id.clone(),
            range: SeqRange {
                start_seq: 1,
                end_seq: 1_024,
            },
        },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionRead { result },
            ..
        } = client.next().await
        {
            return result.envelopes;
        }
    }
}

fn rename_facts(journal: &[RawEnvelope]) -> Vec<Option<String>> {
    journal
        .iter()
        .filter_map(|envelope| {
            SessionConfigEventPayload::session_renamed_from_value(&envelope.payload)
        })
        .collect()
}

/// LAW (LB1): `session.rename` commits with a receipt — `meta_json.title`
/// updated, the `session_renamed` fact journaled and PUBLISHED to the live
/// attachment, `session.list` carrying the title — and a duplicate
/// command id replays the exact committed response without a second fact.
/// The title is NORMALIZED (trimmed, control characters stripped, ≤ 80
/// chars), never an echo of the request.
///
/// MUTATION CHECK: skip the `UPDATE sessions SET meta_json` in
/// `rename_session`, drop the receipt claim, or append the fact outside
/// the transaction. Expected RUNTIME failure: the list/metadata assertion,
/// the replay equality, or the single-fact count below.
#[tokio::test]
async fn rename_is_receipted_published_listed_and_replayed() {
    let root = test_root("g2-rename-wire-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "g2-rename-wire",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, _fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "g2-rename-client",
        "g2-rename-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) =
        create_and_attach(&mut client, &config, &workspace, "create-rename").await;

    // The raw title carries surrounding whitespace and a control byte —
    // the committed truth is the normalized form.
    send(
        &mut client,
        &config,
        "rename-1",
        rename_body(
            "rename-1-command",
            &session_id,
            generation,
            Some("  Parser\u{7} rewrite  "),
        ),
    )
    .await;
    let mut first = None;
    let mut fact = None;
    while first.is_none() || fact.is_none() {
        match client.next().await {
            WireFrame::Response { body, .. } => first = Some(body),
            WireFrame::Event { envelope, .. } => {
                if let Some(title) =
                    SessionConfigEventPayload::session_renamed_from_value(&envelope.payload)
                {
                    fact = Some(title);
                }
            }
            _ => {}
        }
    }
    let first = first.expect("rename response");
    assert_eq!(
        fact.expect("published session_renamed fact").as_deref(),
        Some("Parser rewrite")
    );
    let ResponseBody::SessionRename {
        session_id: responded_session,
        title,
        renamed_seq,
        worker_generation,
    } = first.clone()
    else {
        panic!("expected session.rename response, got {first:?}");
    };
    assert_eq!(responded_session, session_id);
    assert_eq!(title.as_deref(), Some("Parser rewrite"));
    assert_eq!(worker_generation, generation);
    assert!(renamed_seq > 0);

    // R2: the same command replays the exact committed coordinates.
    send(
        &mut client,
        &config,
        "rename-1-retry",
        rename_body(
            "rename-1-command",
            &session_id,
            generation,
            Some("  Parser\u{7} rewrite  "),
        ),
    )
    .await;
    assert_eq!(next_response(&mut client).await, first);

    // session.list carries the title, top-level AND inside metadata.
    let (summary_title, metadata_title) =
        list_title(&mut client, &config, &session_id, "list-after-rename").await;
    assert_eq!(summary_title.as_deref(), Some("Parser rewrite"));
    assert_eq!(metadata_title.as_deref(), Some("Parser rewrite"));

    // Exactly ONE durable fact — the replay journaled nothing.
    let journal = read_session(&mut client, &config, &session_id, "read-after-rename").await;
    assert_eq!(
        rename_facts(&journal),
        vec![Some("Parser rewrite".to_owned())]
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// LAW (LB2): a stale `worker_generation` is refused with the stable typed
/// code and mutates NOTHING — no metadata title, no journaled fact, no
/// receipt (a later rename under the same command id would be a fresh
/// command, not a replay).
///
/// MUTATION CHECK: drop the generation fence from `rename_session`.
/// Expected RUNTIME failure: the stale request commits and the no-mutation
/// assertions below fail.
#[tokio::test]
async fn stale_generation_rename_is_refused_and_mutates_nothing() {
    let root = test_root("g2-rename-stale-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "g2-rename-stale",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, _fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "g2-rename-stale-client",
        "g2-rename-stale-instance",
        ClientKind::Cli,
    )
    .await;
    let (session_id, generation) =
        create_and_attach(&mut client, &config, &workspace, "create-stale").await;

    send(
        &mut client,
        &config,
        "rename-stale",
        rename_body(
            "rename-stale-command",
            &session_id,
            generation + 1,
            Some("Never lands"),
        ),
    )
    .await;
    let ResponseBody::Error { code, .. } = next_response(&mut client).await else {
        panic!("expected typed stale-generation refusal");
    };
    assert_eq!(code, "stale_generation");

    let (summary_title, metadata_title) =
        list_title(&mut client, &config, &session_id, "list-after-stale").await;
    assert_eq!(summary_title, None);
    assert_eq!(metadata_title, None);
    let journal = read_session(&mut client, &config, &session_id, "read-after-stale").await;
    assert!(rename_facts(&journal).is_empty());

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// LAW (LB3): the daemon auto-titles a session on its FIRST accepted turn
/// — slug of the first user message, journaled as the same
/// `session_renamed` fact — and never again: a second turn does not
/// re-title, an explicit rename wins over the auto-title, and the
/// auto-title never overwrites a title that already exists.
///
/// MUTATION CHECK: drop the `only_if_untitled` guard (or the
/// `metadata.title.is_some()` early return) from the auto-title path.
/// Expected RUNTIME failure: the pre-named session below is overwritten by
/// its first turn's slug.
#[tokio::test]
async fn auto_title_fires_once_on_first_accept_and_never_overwrites() {
    let root = test_root("g2-auto-title-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "g2-auto-title",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, _fake) = fake_dependencies(
        [
            text_turn("first answer"),
            text_turn("second answer"),
            text_turn("named-first answer"),
        ]
        .concat(),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "g2-auto-title-client",
        "g2-auto-title-instance",
        ClientKind::Cli,
    )
    .await;

    // Session one: the first accept titles it from the first message.
    let (session_id, generation) =
        create_and_attach(&mut client, &config, &workspace, "create-auto").await;
    run_turn(
        &mut client,
        &config,
        &session_id,
        generation,
        "turn-one",
        "Fix the parser bug in lexer.rs",
    )
    .await;
    let (summary_title, metadata_title) =
        list_title(&mut client, &config, &session_id, "list-after-first").await;
    assert_eq!(summary_title.as_deref(), Some("fix-the-parser"));
    assert_eq!(metadata_title.as_deref(), Some("fix-the-parser"));

    // The second turn does NOT re-title.
    run_turn(
        &mut client,
        &config,
        &session_id,
        generation,
        "turn-two",
        "Completely different words now",
    )
    .await;
    let (summary_title, _) =
        list_title(&mut client, &config, &session_id, "list-after-second").await;
    assert_eq!(summary_title.as_deref(), Some("fix-the-parser"));
    let journal = read_session(&mut client, &config, &session_id, "read-after-two").await;
    assert_eq!(
        rename_facts(&journal),
        vec![Some("fix-the-parser".to_owned())],
        "exactly one auto-title fact across two turns"
    );

    // An explicit rename WINS over the auto-title.
    send(
        &mut client,
        &config,
        "rename-explicit",
        rename_body(
            "rename-explicit-command",
            &session_id,
            generation,
            Some("My plan"),
        ),
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionRename { .. },
            ..
        } = client.next().await
        {
            break;
        }
    }
    let (summary_title, _) =
        list_title(&mut client, &config, &session_id, "list-after-explicit").await;
    assert_eq!(summary_title.as_deref(), Some("My plan"));

    // Session two: named BEFORE its first turn — the auto-title must not
    // overwrite the explicit name.
    let (named_id, named_generation) =
        create_and_attach(&mut client, &config, &workspace, "create-named").await;
    send(
        &mut client,
        &config,
        "rename-named-first",
        rename_body(
            "rename-named-first-command",
            &named_id,
            named_generation,
            Some("Named first"),
        ),
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionRename { .. },
            ..
        } = client.next().await
        {
            break;
        }
    }
    run_turn(
        &mut client,
        &config,
        &named_id,
        named_generation,
        "turn-named",
        "Totally unrelated first message",
    )
    .await;
    let (summary_title, metadata_title) =
        list_title(&mut client, &config, &named_id, "list-named").await;
    assert_eq!(summary_title.as_deref(), Some("Named first"));
    assert_eq!(metadata_title.as_deref(), Some("Named first"));
    let journal = read_session(&mut client, &config, &named_id, "read-named").await;
    assert_eq!(
        rename_facts(&journal),
        vec![Some("Named first".to_owned())],
        "the explicit rename is the ONLY fact — auto-title yielded"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
