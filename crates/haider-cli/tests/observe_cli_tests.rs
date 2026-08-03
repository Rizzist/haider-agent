//! H1 CLI schema, parser, help, exit, and no-daemon laws.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use haider_protocol::agent::ChipState;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{AgentId, BranchId, DeviceId, EventId, NodeId, SessionId};
use haider_protocol::session::SessionMetadataV1;
use haider_rpc::{ObserveMenuWire, ObserveRunStateWire, ObserveSubagentWire, SessionObserveDigest};

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod cli_main;

use cli_main::observe::{
    AccountView, DaemonView, ObserveJson, Parsed, SessionDocument, SessionsDocument,
    SnapshotOptions, StatusDocument, UpdateView, depth_view, exit_code_for_observe_error,
    parse_events_options, parse_session_options, parse_snapshot_options, session_human_text,
    sessions_human_text, summary_view, write_raw_envelope_jsonl,
};
use cli_main::run::{
    EX_BLOCKED, EX_CANCELLED, EX_IOERR, EX_PROTOCOL, EX_PROVIDER, EX_SOFTWARE, EX_TIMEOUT,
    EX_UNAVAILABLE, EX_USAGE,
};
use haider_client::{
    ClientError, ConnectError, DisconnectReason, ObserveError, ProfileEnv, resolve_profile,
};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn golden(name: &str, actual: &str) {
    let path = fixture_path(name);
    if std::env::var("UPDATE_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
        std::fs::write(&path, actual).expect("write fixture");
    }
    assert_eq!(
        std::fs::read_to_string(&path).expect("read observe fixture"),
        actual
    );
}

fn digest(
    id: &str,
    run_state: ObserveRunStateWire,
    menu: Option<ObserveMenuWire>,
) -> SessionObserveDigest {
    SessionObserveDigest {
        session_id: SessionId::new(id),
        head_seq: 12,
        worker_generation: 7,
        metadata: Some(SessionMetadataV1 {
            cwd: format!("/tmp/{id}"),
            provider: "openai".into(),
            model: "gpt-observe".into(),
            max_tokens: 4096,
            system_prompt_version: Some("v1".into()),
            permission_overrides: None,
            created_at_ms: 1_800_000_000_000,
        }),
        title: "Inspect durable automation truth".into(),
        run_state,
        active_branch_id: Some(BranchId::new("branch-review")),
        branches: vec![BranchDescriptor {
            branch_id: BranchId::new("branch-review"),
            name: "review".into(),
            source_branch_id: None,
            fork_node_id: NodeId::new("node-fork"),
            fork_seq: 5,
            created_seq: 6,
            created_at_ms: 1_800_000_000_006,
            head_node_id: NodeId::new("node-head"),
            head_seq: 11,
        }],
        main_head_node_id: Some(NodeId::new("node-main")),
        main_head_seq: 4,
        latest_context_footprint: Some(ContextFootprint {
            input_tokens: 800,
            output_tokens: 150,
            cached_input_tokens: 50,
            used_tokens: 1_000,
            context_window: Some(128_000),
            reserved_output_tokens: 4_096,
            soft_threshold_tokens: Some(100_000),
            estimated_turns_to_threshold: Some(9),
            truth: ContextFootprintTruth::Exact,
        }),
        pending_menus: menu.into_iter().collect(),
        subagents: vec![
            ObserveSubagentWire {
                agent_id: AgentId::new("agent-daemon-name"),
                callsign: Some("Saffron".into()),
                task: "verify RPC".into(),
                state: serde_json::to_value(ChipState::Waiting)
                    .expect("chip serializes")
                    .as_str()
                    .expect("chip string")
                    .to_owned(),
            },
            ObserveSubagentWire {
                agent_id: AgentId::new("agent-without-callsign"),
                callsign: None,
                task: "inspect omitted identity".into(),
                state: serde_json::to_value(ChipState::Thinking)
                    .expect("chip serializes")
                    .as_str()
                    .expect("chip string")
                    .to_owned(),
            },
        ],
        updated_at_ms: 1_800_000_000_012,
        last_event_kinds: vec!["run_state".into(), "future_observe_kind_v9".into()],
    }
}

/// MUTATION CHECK: rename the schema/kind tags, remove a required overview
/// field, or expose menu bodies/options. Expected RUNTIME failure: the exact
/// compact golden differs, or one of the literal secret sentinels appears.
#[test]
fn observe_json_schemas_are_goldened_and_secret_free() {
    const VAULT_SENTINEL: &str = "sk-schema-vault-sentinel";
    const OAUTH_SENTINEL: &str = "oauth-schema-refresh-sentinel";

    let status = StatusDocument {
        schema: "haider.observe.v1",
        kind: "status",
        daemon: DaemonView {
            version: "0.0.57".into(),
            generation: 9,
        },
        update: UpdateView {
            status: "available",
            current_version: "0.0.57".into(),
            latest_version: Some("0.0.58".into()),
            error: None,
        },
        features: vec!["session_observe_v1".into(), "tool_inventory_v1".into()],
        account: Some(AccountView {
            provider: "openai".into(),
            alias: "work".into(),
        }),
        session_count: 2,
        profile_path: "/tmp/haider-profile".into(),
    };
    let permission = digest(
        "session-permission",
        ObserveRunStateWire::ParkedPermission,
        Some(ObserveMenuWire {
            kind: "permission".into(),
            title: "Allow write?".into(),
            permission_description: Some("write src/lib.rs".into()),
        }),
    );
    let input = digest(
        "session-input",
        ObserveRunStateWire::ParkedInput,
        Some(ObserveMenuWire {
            kind: "secret".into(),
            title: "Credential required".into(),
            permission_description: None,
        }),
    );
    let sessions = SessionsDocument {
        schema: "haider.observe.v1",
        kind: "sessions",
        sessions: vec![summary_view(permission.clone()), summary_view(input)],
    };
    let session = SessionDocument {
        schema: "haider.observe.v1",
        kind: "session",
        session: depth_view(permission),
    };

    let status = serde_json::to_string(&status.json()).expect("status serializes") + "\n";
    let sessions = serde_json::to_string(&sessions.json()).expect("sessions serialize") + "\n";
    let session = serde_json::to_string(&session.json()).expect("session serializes") + "\n";
    golden("observe_status.json", &status);
    golden("observe_sessions.json", &sessions);
    golden("observe_session.json", &session);
    for output in [&status, &sessions, &session] {
        assert!(output.ends_with('\n'));
        assert!(!output.contains('\r'));
        assert!(!output.contains(VAULT_SENTINEL));
        assert!(!output.contains(OAUTH_SENTINEL));
    }
    assert!(sessions.contains("parked_permission"));
    assert!(sessions.contains("parked_input"));
    let session_json: serde_json::Value =
        serde_json::from_str(&session).expect("session golden is JSON");
    let subagents = session_json["session"]["subagents"]
        .as_array()
        .expect("depth subagents");
    let unnamed = subagents
        .iter()
        .find(|subagent| subagent["id"] == "agent-without-callsign")
        .expect("unnamed daemon subagent");
    assert!(unnamed["callsign"].is_null());
}

/// MUTATION CHECK: accept ambiguous snapshot/watch flags or remove the
/// explicit forward-compatibility help. Expected RUNTIME failure: a parser
/// assertion changes or the literal compatibility sentence is absent.
#[test]
fn observe_parsers_and_stream_help_are_explicit() {
    assert!(matches!(
        parse_snapshot_options(&["--json".into(), "--no-spawn".into()], "status"),
        Ok(Parsed::Run(SnapshotOptions {
            json: true,
            no_spawn: true
        }))
    ));
    assert!(parse_session_options(&["s-1".into(), "--json".into(), "--watch".into()]).is_err());
    assert!(parse_events_options(&["--follow".into(), "--follow".into()]).is_err());

    let output = Command::new(env!("CARGO_BIN_EXE_haider"))
        .args(["events", "--help"])
        .output()
        .expect("run events help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LF-framed raw event envelopes"));
    assert!(stdout.contains("tolerate unknown kinds and fields"));
}

/// MUTATION CHECK: route no-spawn through ensure_daemon, panic on a missing
/// socket, or return generic failure. Expected RUNTIME failure: either command
/// creates daemon state or exits with a code other than literal 69.
#[test]
fn no_daemon_no_spawn_paths_are_typed_69_and_do_not_start_a_daemon() {
    let root = tempfile::Builder::new()
        .prefix("hobs-cli")
        .tempdir_in("/tmp")
        .expect("short temp profile");
    let profile_dir = root.path().join("profile");
    for command in [
        vec!["status", "--json", "--no-spawn"],
        vec!["events", "--no-spawn"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_haider"))
            .args(command)
            .env("HAIDER_PROFILE_DIR", &profile_dir)
            .env_remove("XDG_RUNTIME_DIR")
            .output()
            .expect("run no-spawn observe command");
        assert_eq!(output.status.code(), Some(i32::from(EX_UNAVAILABLE)));
        assert!(output.stdout.is_empty());
    }
    let resolved = resolve_profile(&ProfileEnv {
        profile_dir: Some(profile_dir.clone()),
        home: None,
        model: None,
        xdg_runtime_dir: None,
    })
    .expect("resolve inspected profile");
    assert!(!resolved.endpoint_path.exists());
    assert!(!profile_dir.join("store.sqlite").exists());
    assert!(!profile_dir.join("daemon.log").exists());

    let error = ObserveError::NoDaemon(ConnectError::NotFound(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "literal missing daemon",
    )));
    assert_eq!(exit_code_for_observe_error(&error), EX_UNAVAILABLE);
    assert_eq!(EX_USAGE, 2);
}

/// MUTATION CHECK: invent observe-specific exit values or collapse provider,
/// blocked, protocol, I/O, and unavailable failures. Expected RUNTIME failure:
/// one literal differs from the `haider run` table or a typed case maps wrong.
#[test]
fn exit_codes_match_the_headless_table() {
    assert_eq!(
        [
            EX_USAGE,
            EX_PROVIDER,
            EX_UNAVAILABLE,
            EX_SOFTWARE,
            EX_IOERR,
            EX_PROTOCOL,
            EX_BLOCKED,
            EX_TIMEOUT,
            EX_CANCELLED,
        ],
        [2, 65, 69, 70, 74, 76, 77, 124, 130]
    );
    let cases = [
        (
            ObserveError::Rpc {
                code: "provider_error".into(),
                message: "literal provider".into(),
                retryable: true,
            },
            EX_PROVIDER,
        ),
        (
            ObserveError::Rpc {
                code: "permission_denied".into(),
                message: "literal blocked".into(),
                retryable: false,
            },
            EX_BLOCKED,
        ),
        (
            ObserveError::Rpc {
                code: "invalid_argument".into(),
                message: "literal protocol".into(),
                retryable: false,
            },
            EX_PROTOCOL,
        ),
        (
            ObserveError::Rpc {
                code: "credential_limited".into(),
                message: "literal provider credential".into(),
                retryable: false,
            },
            EX_PROVIDER,
        ),
        (
            ObserveError::Rpc {
                code: "timeout_before_acceptance".into(),
                message: "literal timeout".into(),
                retryable: true,
            },
            EX_TIMEOUT,
        ),
        (
            ObserveError::Rpc {
                code: "unknown_method".into(),
                message: "literal protocol method".into(),
                retryable: false,
            },
            EX_PROTOCOL,
        ),
        (
            ObserveError::Client(ClientError::Disconnected(DisconnectReason::PeerClosed)),
            EX_IOERR,
        ),
        (ObserveError::StreamTask("literal task".into()), EX_SOFTWARE),
    ];
    for (error, expected) in cases {
        assert_eq!(exit_code_for_observe_error(&error), expected, "{error}");
    }
}

/// MUTATION CHECK: omit overview/depth facts from the human formatters.
/// Expected RUNTIME failure: one of the daemon-known branch, footprint,
/// timestamp, permission, or subagent literals disappears.
#[test]
fn human_views_include_the_scoped_observation_facts() {
    let permission = digest(
        "human-session",
        ObserveRunStateWire::ParkedPermission,
        Some(ObserveMenuWire {
            kind: "permission".into(),
            title: "Allow write?".into(),
            permission_description: Some("write src/lib.rs".into()),
        }),
    );
    let sessions = SessionsDocument {
        schema: "haider.observe.v1",
        kind: "sessions",
        sessions: vec![summary_view(permission.clone())],
    };
    let overview = sessions_human_text(&sessions);
    for expected in [
        "branch=branch-review [main,branch-review]",
        "footprint=exact:1000",
        "subagents=2",
        "updated_at=1800000000012",
    ] {
        assert!(
            overview.contains(expected),
            "missing `{expected}`: {overview}"
        );
    }

    let session = SessionDocument {
        schema: "haider.observe.v1",
        kind: "session",
        session: depth_view(permission),
    };
    let depth = session_human_text(&session);
    for expected in [
        "write src/lib.rs",
        "Saffron (agent-daemon-name) — waiting — verify RPC",
        "agent-without-callsign (agent-without-callsign) — thinking",
        "branch: review @ 11",
    ] {
        assert!(depth.contains(expected), "missing `{expected}`: {depth}");
    }
}

/// MUTATION CHECK: wrap stream values in `haider.observe.v1`, omit the final
/// LF, emit CRLF, or reject a future payload kind. Expected RUNTIME failure:
/// exact byte framing or RawEnvelope round-trip differs.
#[test]
fn watch_streams_are_lf_framed_raw_envelopes_and_tolerate_additive_kinds() {
    let envelope = RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("future-event"),
        seq: 41,
        session_id: SessionId::new("future-session"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("future-device"),
        authority_epoch: 3,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1_800_000_000_041,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({
            "type": "future_observe_kind_v99",
            "additive": {"field": true}
        }),
    };
    let mut output = Vec::new();
    write_raw_envelope_jsonl(&mut output, &envelope).expect("write raw JSONL");
    assert_eq!(output.last(), Some(&b'\n'));
    assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert!(!output.contains(&b'\r'));
    let decoded: RawEnvelope =
        serde_json::from_slice(&output[..output.len() - 1]).expect("decode raw envelope line");
    assert_eq!(decoded, envelope);
    assert!(
        !String::from_utf8(output)
            .expect("JSON UTF-8")
            .contains("haider.observe.v1")
    );
}

#[test]
fn fixture_paths_remain_inside_the_cli_test_tree() {
    assert!(fixture_path("observe_status.json").starts_with(Path::new(env!("CARGO_MANIFEST_DIR"))));
}
