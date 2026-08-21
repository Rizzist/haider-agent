//! H1 CLI schema, parser, help, exit, and no-daemon laws.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use haider_protocol::agent::{AgentMetricsSnapshot, AgentUsageMetrics, ChipState};
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{AgentId, BranchId, DeviceId, EventId, NodeId, SessionId};
use haider_protocol::session::SessionMetadataV1;
use haider_rpc::{
    FleetAgentStateWire, FleetMetricsTotalsWire, FleetNodeWire, FleetRollupWire,
    FleetStateCountsWire, ObserveMenuWire, ObserveRunStateWire, ObserveSubagentWire,
    SessionFleetSnapshot, SessionObserveDigest,
};

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod cli_main;

use cli_main::observe::{
    AccountView, DaemonView, FleetListEntry, FleetOptions, ObserveJson, Parsed, SessionDocument,
    SessionsDocument, SnapshotOptions, StatusDocument, UpdateView, depth_view,
    exit_code_for_observe_error, fleet_candidates, fleet_human_text, fleet_list_human_text,
    parse_events_options, parse_fleet_options, parse_session_options, parse_snapshot_options,
    session_human_text, sessions_human_text, stamp_update_view, summary_view,
    write_raw_envelope_jsonl,
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
    let expected = std::fs::read_to_string(&path).expect("read observe fixture");
    #[cfg(windows)]
    assert_eq!(expected.replace("\r\n", "\n"), actual.replace("\r\n", "\n"));
    #[cfg(not(windows))]
    assert_eq!(expected, actual);
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
            title: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            created_at_ms: 1_800_000_000_000,
            agent_type: None,
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
        turn_count: None,
        agent_metrics: None,
        needs_input: None,
    }
}

fn fleet_node(
    id: &str,
    callsign: &str,
    task: &str,
    depth: u32,
    state: FleetAgentStateWire,
    folded_children: u32,
    children: Vec<FleetNodeWire>,
) -> FleetNodeWire {
    FleetNodeWire {
        agent_id: AgentId::new(id),
        session_id: SessionId::new(format!("session-{id}")),
        callsign: Some(callsign.into()),
        task: task.into(),
        depth,
        parent_session_id: SessionId::new("fleet-cli-session"),
        parent_agent_id: None,
        state,
        metrics: Some(AgentMetricsSnapshot {
            agent: Some(AgentId::new(id)),
            session_id: SessionId::new(format!("session-{id}")),
            head_seq: 9,
            started_at_ms: 1_800_000_000_000,
            terminal_at_ms: Some(1_800_000_042_000),
            live: state == FleetAgentStateWire::Live,
            tool_attempts: 3,
            usage: Some(AgentUsageMetrics {
                logical_input_tokens: 1_200,
                billed_output_tokens: 300,
                api_equivalent_cost_microusd: Some(420_000),
                all_lanes_priced: true,
                has_oauth_lanes: true,
                ..AgentUsageMetrics::default()
            }),
        }),
        folded_children,
        children,
    }
}

fn fleet_snapshot() -> SessionFleetSnapshot {
    SessionFleetSnapshot {
        session_id: SessionId::new("fleet-cli-session"),
        generated_at_ms: 1_800_000_060_000,
        node_limit: 512,
        depth_limit: 32,
        roots: vec![
            fleet_node(
                "agent-alpha",
                "alpha",
                "coordinate the sweep",
                1,
                FleetAgentStateWire::Live,
                0,
                vec![
                    fleet_node(
                        "agent-done",
                        "done",
                        "verify output",
                        2,
                        FleetAgentStateWire::Done,
                        0,
                        vec![],
                    ),
                    fleet_node(
                        "agent-fold",
                        "fold",
                        "deep branch",
                        2,
                        FleetAgentStateWire::Failed,
                        2,
                        vec![],
                    ),
                ],
            ),
            fleet_node(
                "agent-queued",
                "queued",
                "await capacity",
                1,
                FleetAgentStateWire::Queued,
                0,
                vec![],
            ),
        ],
        rollup: FleetRollupWire {
            node_count: 4,
            states: FleetStateCountsWire {
                queued: 1,
                live: 1,
                waiting: 0,
                done: 1,
                failed: 1,
                cancelled: 0,
            },
            max_depth: 2,
            metrics: FleetMetricsTotalsWire::default(),
            metrics_complete: false,
            complete: false,
        },
        truncated: true,
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
            menu_id: None,
            request_seq: None,
            worker_generation: None,
            opened_at_ms: None,
            body: Vec::new(),
            options: Vec::new(),
            permission_description: Some("write src/lib.rs".into()),
            presentation: None,
        }),
    );
    let input = digest(
        "session-input",
        ObserveRunStateWire::ParkedInput,
        Some(ObserveMenuWire {
            kind: "secret".into(),
            title: "Credential required".into(),
            menu_id: None,
            request_seq: None,
            worker_generation: None,
            opened_at_ms: None,
            body: Vec::new(),
            options: Vec::new(),
            permission_description: None,
            presentation: None,
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
    assert!(matches!(
        parse_fleet_options(&["fleet-session".into(), "--json".into(), "--no-spawn".into()]),
        Ok(Parsed::Run(FleetOptions {
            session_id: Some(ref id),
            json: true,
            no_spawn: true,
        })) if id == "fleet-session"
    ));
    assert!(matches!(
        parse_fleet_options(&[]),
        Ok(Parsed::Run(FleetOptions {
            session_id: None,
            json: false,
            no_spawn: false,
        }))
    ));
    assert!(parse_fleet_options(&["--json".into()]).is_err());
    assert!(parse_fleet_options(&["a".into(), "b".into()]).is_err());

    let output = Command::new(env!("CARGO_BIN_EXE_haider"))
        .args(["events", "--help"])
        .output()
        .expect("run events help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LF-framed raw event envelopes"));
    assert!(stdout.contains("tolerate unknown kinds and fields"));

    let output = Command::new(env!("CARGO_BIN_EXE_haider"))
        .args(["fleet", "--help"])
        .output()
        .expect("run fleet help");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("fleet [<session-id>] [--json] [--no-spawn]")
    );
}

/// MUTATION CHECK: route no-spawn through ensure_daemon, panic on a missing
/// socket, or return generic failure. Expected RUNTIME failure: either command
/// creates daemon state or exits with a code other than literal 69.
#[test]
fn no_daemon_no_spawn_paths_are_typed_69_and_do_not_start_a_daemon() {
    #[cfg(unix)]
    let temporary_base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let temporary_base =
        std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary base");
    let root = tempfile::Builder::new()
        .prefix("hobs-cli")
        .tempdir_in(temporary_base)
        .expect("short temp profile");
    let profile_dir = root.path().join("profile");
    for command in [
        vec!["status", "--json", "--no-spawn"],
        vec!["events", "--no-spawn"],
        vec!["fleet", "--no-spawn"],
        vec!["fleet", "session-missing", "--no-spawn"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_haider"))
            .args(command)
            .env("HAIDER_PROFILE_DIR", &profile_dir)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
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

/// MUTATION CHECK (v0.0.935): reintroduce a discovery call in `haider
/// status`'s update view, or stop consulting the six-hour stamp policy.
/// Expected RUNTIME failure: a bare profile with only a stamp FILE cannot
/// produce `checked_recently` (there is no network and no daemon here), or
/// the stale/missing-stamp cases stop reading `check_due`.
#[test]
fn status_update_view_reads_the_stamp_cache_without_network() {
    let profile = tempfile::tempdir().expect("profile");
    let fresh = stamp_update_view(profile.path());
    assert_eq!(fresh.status, "check_due", "no stamp means a check is due");
    assert_eq!(fresh.current_version, env!("CARGO_PKG_VERSION"));
    assert!(fresh.latest_version.is_none());
    assert!(fresh.error.is_none());

    let now = cli_main::update::check_policy::unix_timestamp_now();
    let stamp = profile.path().join("update-check.timestamp");
    std::fs::write(&stamp, format!("{now}\n")).expect("write fresh stamp");
    assert_eq!(stamp_update_view(profile.path()).status, "checked_recently");

    let stale = now.saturating_sub(7 * 60 * 60);
    std::fs::write(&stamp, format!("{stale}\n")).expect("write stale stamp");
    assert_eq!(stamp_update_view(profile.path()).status, "check_due");
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
        (
            ObserveError::UnknownSession(SessionId::new("missing-fleet")),
            EX_SOFTWARE,
        ),
        (
            ObserveError::MissingFeature(haider_rpc::FEATURE_SESSION_FLEET_V1),
            EX_PROTOCOL,
        ),
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
            menu_id: None,
            request_seq: None,
            worker_generation: None,
            opened_at_ms: None,
            body: Vec::new(),
            options: Vec::new(),
            permission_description: Some("write src/lib.rs".into()),
            presentation: None,
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

#[test]
fn fleet_snapshot_rendering_and_raw_json_are_append_only_goldens() {
    let snapshot = fleet_snapshot();
    let human = fleet_human_text(&snapshot);
    golden("observe_fleet.txt", &human);
    for expected in [
        "fleet of 4 · ✓1 ◉1 ✗1 ◌1 · depth 2",
        "◉ alpha ▸2 — coordinate the sweep · 3t · 1.5k · ≈$0.42",
        "│ ✓ done — verify output · 3t · 1.5k · ≈$0.42",
        "│ ✗ fold ⊞2 — deep branch · 3t · 1.5k · ≈$0.42",
        "◌ queued — await capacity · queued",
        "512-node view cap reached — deepest branches folded",
    ] {
        assert!(human.contains(expected), "missing `{expected}`: {human}");
    }

    let raw = serde_json::to_string(&snapshot).expect("fleet snapshot serializes") + "\n";
    golden("observe_fleet.json", &raw);
    assert!(!raw.contains("haider.observe.v1"));
    assert!(raw.contains(r#""folded_children":2"#));
    assert_eq!(raw.matches("folded_children").count(), 1);
}

#[test]
fn bare_fleet_lists_only_subagent_sessions_most_recent_first() {
    let mut excluded = digest("session-newest-empty", ObserveRunStateWire::Idle, None);
    excluded.updated_at_ms = 400;
    excluded.subagents.clear();
    let mut later_id = digest("session-z", ObserveRunStateWire::Running, None);
    later_id.updated_at_ms = 300;
    later_id.title = "new fleet".into();
    let mut tied_a = digest("session-a", ObserveRunStateWire::Running, None);
    tied_a.updated_at_ms = 200;
    tied_a.title = "tie a".into();
    let mut tied_b = digest("session-b", ObserveRunStateWire::Running, None);
    tied_b.updated_at_ms = 200;
    tied_b.title = "tie b".into();

    let candidates = fleet_candidates(vec![tied_b, excluded, later_id, tied_a]);
    assert_eq!(
        candidates
            .iter()
            .map(|digest| digest.session_id.as_str())
            .collect::<Vec<_>>(),
        ["session-z", "session-a", "session-b"]
    );
    let entries = candidates
        .into_iter()
        .map(|digest| FleetListEntry {
            id: digest.session_id.as_str().to_owned(),
            title: digest.title,
            snapshot: fleet_snapshot(),
        })
        .collect::<Vec<_>>();
    let list = fleet_list_human_text(&entries);
    golden("observe_fleet_list.txt", &list);
    assert!(!list.contains("session-newest-empty"));
    assert!(list.starts_with("session-z · new fleet · fleet of 4"));
    assert!(list.find("session-a").unwrap() < list.find("session-b").unwrap());
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
