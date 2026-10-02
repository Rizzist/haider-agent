#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::effect::{AuthorizationSource, AuthorizationVerdict, EffectIntent, EffectOutcome};
use crate::ids::EffectId;
use crate::ids::{AgentId, ItemId};
use crate::item::ToolStatus;
use crate::state::RunState;

fn class_of(name: &str) -> EffectClass {
    match name {
        "computer" => EffectClass::ScreenControl,
        "mobile" => EffectClass::MobileControl,
        "web_fetch" => EffectClass::Network {
            host: "example.test".into(),
        },
        "ssh_shell" => EffectClass::RemoteExecution,
        "fs_edit" | "fs_write" => EffectClass::FsWrite,
        "process_exec" | "test_run" => EffectClass::ProcessExec,
        "spawn_subagent" => EffectClass::AgentSpawn,
        _ => EffectClass::FsRead,
    }
}

fn intent(id: &str, name: &str) -> EventPayload {
    EventPayload::Effect(EffectPhase::Intent(EffectIntent {
        effect: EffectId::new(format!("effect-{id}")),
        class: class_of(name),
        summary: format!("{name} call"),
        args_digest: "digest".into(),
        workspace_revision: None,
    }))
}

fn authorized(id: &str, verdict: AuthorizationVerdict) -> EventPayload {
    EventPayload::Effect(EffectPhase::Authorized {
        effect: EffectId::new(format!("effect-{id}")),
        verdict,
    })
}

fn dispatched(id: &str) -> EventPayload {
    EventPayload::Effect(EffectPhase::Dispatched {
        effect: EffectId::new(format!("effect-{id}")),
    })
}

/// A real, authorized, dispatched call of `name`.
fn start(tracker: &mut ActivityTracker, id: &str, name: &str, at_ms: u64) {
    tracker.observe(&intent(id, name), at_ms);
    tracker.observe(&authorized(id, AuthorizationVerdict::Allow), at_ms);
    tracker.observe(&dispatched(id), at_ms);
}

/// Its outcome plus the successfully completed item.
fn finish(tracker: &mut ActivityTracker, id: &str, name: &str, at_ms: u64) {
    tracker.observe(
        &EventPayload::Effect(EffectPhase::Outcome {
            effect: EffectId::new(format!("effect-{id}")),
            outcome: EffectOutcome::Ok,
            freshness: None,
            workspace_mutation: None,
        }),
        at_ms,
    );
    let mut completed = tool_item(id, name, ToolStatus::Completed, true);
    if let EventPayload::Item(ItemEvent::Completed {
        item: TurnItem::ToolCall { activity, .. },
        ..
    }) = &mut completed
    {
        *activity = ActivityKind::for_effect(&class_of(name));
    }
    tracker.observe(&completed, at_ms);
}

fn tool_item(id: &str, name: &str, status: ToolStatus, completed: bool) -> EventPayload {
    let item = TurnItem::ToolCall {
        call_id: format!("call-{id}"),
        name: name.to_owned(),
        args: serde_json::json!({}),
        status,
        activity: None,
    };
    let item_id = ItemId::new(id);
    EventPayload::Item(if completed {
        ItemEvent::Completed { item_id, item }
    } else {
        ItemEvent::Started { item_id, item }
    })
}

#[test]
fn only_typed_tool_activity_counts() {
    use ActivityKind as A;
    for (name, typed) in [
        ("computer", A::Read),
        (" mobile ", A::Edit),
        ("BROWSER_open", A::Run),
        ("ssh_shell", A::Web),
        ("ＣＯＭＰＵＴＥＲ", A::Read),
        ("", A::Run),
        ("mcp__server__computer", A::Read),
    ] {
        let item = TurnItem::ToolCall {
            call_id: "typed".into(),
            name: name.into(),
            args: serde_json::Value::Null,
            status: ToolStatus::Completed,
            activity: Some(typed),
        };
        assert_eq!(A::for_item(&item), Some(typed), "{name}");
    }
}

#[test]
fn derivation_never_reads_free_text() {
    // A read tool whose ARGUMENTS talk about editing and computers stays read.
    let item = TurnItem::ToolCall {
        call_id: "c".into(),
        name: "fs_read".into(),
        args: serde_json::json!({"path": "computer_edit_ssh_web.md"}),
        status: ToolStatus::Completed,
        activity: None,
    };
    assert_eq!(ActivityKind::for_item(&item), None);
    // Prose items carry no activity at all.
    let prose = TurnItem::AgentMessage {
        text: "I will now use the computer and ssh".to_owned().into(),
    };
    assert_eq!(ActivityKind::for_item(&prose), None);
    assert_eq!(
        ActivityKind::for_item(&TurnItem::ChildSpawn {
            agent: AgentId::new("a")
        }),
        Some(ActivityKind::Orchestrate)
    );
}

#[test]
fn priority_order_is_the_owner_order() {
    let ranks: Vec<u8> = ActivityKind::ALL.iter().map(|kind| kind.rank()).collect();
    assert_eq!(ranks, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(
        highest([ActivityKind::Read, ActivityKind::Edit, ActivityKind::Run]),
        Some(ActivityKind::Edit)
    );
    assert_eq!(
        highest([ActivityKind::Orchestrate, ActivityKind::Web]),
        Some(ActivityKind::Web)
    );
    assert_eq!(highest([ActivityKind::Unknown]), None);
}

#[test]
fn glyphs_are_two_column_emoji_with_vs16_where_needed() {
    for kind in ActivityKind::ALL {
        let emoji = kind.emoji();
        assert!(!emoji.is_empty());
        let base = emoji.chars().next().unwrap() as u32;
        // Text-default bases must carry VS16.
        if matches!(base, 0x1F5A5 | 0x270F | 0x2699) {
            assert!(emoji.ends_with('\u{FE0F}'), "{kind:?} lacks VS16");
        }
        assert!(kind.text_label().starts_with('['));
        assert!(kind.text_label().is_ascii());
    }
    assert_eq!(ActivityKind::Read.text_label(), "[read]");
    assert_eq!(ActivityKind::Orchestrate.text_label(), "[orch]");
    assert_eq!(ActivityKind::Computer.text_label(), "[screen]");
}

#[test]
fn live_updates_follow_calls_as_they_start() {
    let mut tracker = ActivityTracker::new();
    assert_eq!(tracker.current(0, false), None);
    start(&mut tracker, "1", "fs_read", 1_000);
    assert_eq!(tracker.current(1_000, false), Some(ActivityKind::Read));
    finish(&mut tracker, "1", "fs_read", 1_200);
    // Read → edit within the hot window: edit outranks read.
    start(&mut tracker, "2", "fs_edit", 2_000);
    assert_eq!(tracker.current(2_000, false), Some(ActivityKind::Edit));
    finish(&mut tracker, "2", "fs_edit", 2_100);
    // Edit → run: edit stays hot and outranks run until the window passes.
    start(&mut tracker, "3", "test_run", 3_000);
    assert_eq!(tracker.current(3_000, false), Some(ActivityKind::Edit));
    let after_edit_cools = 2_100 + ACTIVITY_HOT_WINDOW_MS + 1;
    assert_eq!(
        tracker.current(after_edit_cools, false),
        Some(ActivityKind::Run),
        "the still-open test run is the current activity"
    );
    // A long-running open call stays current indefinitely.
    assert_eq!(
        tracker.current(3_000 + 20 * 60_000, false),
        Some(ActivityKind::Run)
    );
    assert_eq!(
        tracker.used(),
        vec![ActivityKind::Edit, ActivityKind::Run, ActivityKind::Read]
    );
}

#[test]
fn quiet_falls_back_to_most_recent_then_nothing() {
    let mut tracker = ActivityTracker::new();
    start(&mut tracker, "1", "fs_edit", 1_000);
    finish(&mut tracker, "1", "fs_edit", 1_100);
    start(&mut tracker, "2", "fs_read", 1_500);
    finish(&mut tracker, "2", "fs_read", 1_600);
    assert_eq!(tracker.current(1_600, false), Some(ActivityKind::Edit));
    // Past the hot window: most recent activity (read), not the priority.
    let quiet = 1_600 + ACTIVITY_HOT_WINDOW_MS + 1;
    assert_eq!(tracker.current(quiet, false), Some(ActivityKind::Read));
    // Past the linger: no icon.
    let silent = 1_600 + ACTIVITY_LINGER_MS + 1;
    assert_eq!(tracker.current(silent, false), None);
    // Live children keep the orchestrating icon while quiet.
    assert_eq!(
        tracker.current(silent, true),
        Some(ActivityKind::Orchestrate)
    );
}

#[test]
fn terminal_runs_show_no_current_activity_but_keep_history() {
    let mut tracker = ActivityTracker::new();
    start(&mut tracker, "1", "computer", 10);
    tracker.observe(&EventPayload::RunState(RunState::Done), 20);
    assert_eq!(tracker.current(20, true), None);
    assert_eq!(tracker.used(), vec![ActivityKind::Computer]);
    // A later run (message_subagent / monitor wake) revives it.
    let later = 10 + ACTIVITY_HOT_WINDOW_MS + 1;
    tracker.observe(&EventPayload::RunState(RunState::Thinking), later);
    start(&mut tracker, "2", "mobile", later);
    assert_eq!(tracker.current(later, false), Some(ActivityKind::Mobile));
}

#[test]
fn replay_order_and_duplicate_starts_are_idempotent() {
    let mut once = ActivityTracker::new();
    let mut twice = ActivityTracker::new();
    for tracker in [&mut once, &mut twice] {
        start(tracker, "1", "web_fetch", 5);
    }
    start(&mut twice, "1", "web_fetch", 5);
    assert_eq!(once, twice);
    // An out-of-order older fact never rewinds `latest`.
    finish(&mut twice, "0", "fs_read", 1);
    assert_eq!(
        twice.current(5 + ACTIVITY_HOT_WINDOW_MS + 1, false),
        Some(ActivityKind::Web)
    );
}

#[test]
fn in_flight_is_bounded() {
    let mut tracker = ActivityTracker::new();
    for n in 0..(ACTIVITY_MAX_IN_FLIGHT + 10) {
        start(&mut tracker, &n.to_string(), "fs_read", 1);
    }
    assert_eq!(tracker.in_flight.len(), ACTIVITY_MAX_IN_FLIGHT);
}

#[test]
fn wire_names_round_trip_and_unknown_kinds_degrade() {
    for kind in ActivityKind::ALL {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, format!("\"{}\"", kind.name()));
        assert_eq!(serde_json::from_str::<ActivityKind>(&json).unwrap(), kind);
    }
    assert_eq!(
        serde_json::from_str::<ActivityKind>("\"teleport\"").unwrap(),
        ActivityKind::Unknown
    );
    assert_eq!(ActivityDisplay::Emoji.glyph(ActivityKind::Unknown), None);
}

#[test]
fn display_resolution_precedence() {
    use ActivityDisplay as D;
    assert_eq!(
        D::resolve(None, None, false, Some("xterm-256color")),
        D::Emoji
    );
    assert_eq!(
        D::resolve(None, None, true, Some("xterm-256color")),
        D::Text
    );
    assert_eq!(D::resolve(None, None, false, Some("linux")), D::Text);
    assert_eq!(D::resolve(None, None, false, Some("dumb")), D::Text);
    assert_eq!(D::resolve(None, Some(D::Off), true, None), D::Off);
    assert_eq!(D::resolve(None, Some(D::Emoji), true, None), D::Emoji);
    assert_eq!(
        D::resolve(Some("text"), Some(D::Emoji), false, None),
        D::Text
    );
    assert_eq!(D::resolve(Some("off"), None, false, None), D::Off);
    // An unparseable override is ignored, never a silent disable.
    assert_eq!(D::resolve(Some("sparkles"), None, false, None), D::Emoji);
    assert_eq!(D::Text.glyph(ActivityKind::Run), Some("[run]"));
    assert_eq!(D::Off.glyph(ActivityKind::Run), None);
}

#[test]
fn wire_state_round_trips_and_evaluates_on_the_client_clock() {
    let mut tracker = ActivityTracker::new();
    start(&mut tracker, "1", "fs_read", 1_000);
    finish(&mut tracker, "1", "fs_read", 1_100);
    start(&mut tracker, "2", "ssh_shell", 2_000);
    let state = tracker.state();
    assert_eq!(state.open, vec![ActivityKind::Remote]);
    let json = serde_json::to_value(&state).unwrap();
    let decoded: ActivityStateV1 = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded, state);
    let rebuilt = ActivityTracker::from_state(&decoded);
    for now in [2_000, 2_000 + ACTIVITY_LINGER_MS * 3] {
        assert_eq!(rebuilt.current(now, false), tracker.current(now, false));
    }
    assert_eq!(rebuilt.used(), tracker.used());
    // Unknown future kinds on the wire are ignored, never coerced.
    let future: ActivityStateV1 = serde_json::from_value(serde_json::json!({
        "open": ["teleport"],
        "touched": [{"kind": "teleport", "at_ms": 5}],
        "latest": {"kind": "teleport", "at_ms": 5}
    }))
    .unwrap();
    assert_eq!(ActivityTracker::from_state(&future).current(5, false), None);
    // An empty state serializes to an empty object.
    assert_eq!(
        serde_json::to_string(&ActivityStateV1::default()).unwrap(),
        "{}"
    );
}

#[test]
fn a_requested_tool_name_alone_never_sets_an_icon_or_history() {
    // F1: a child WITHOUT computer use names `computer`: the model-requested
    // ToolCall item starts and then fails/is refused. No effect was
    // dispatched, so there is no activity and no "used" history.
    let mut tracker = ActivityTracker::new();
    tracker.observe(
        &tool_item("1", "computer", ToolStatus::InProgress, false),
        10,
    );
    assert_eq!(tracker.current(10, false), None);
    for status in [
        ToolStatus::Failed,
        ToolStatus::Rejected,
        ToolStatus::Cancelled,
        ToolStatus::Unknown,
        ToolStatus::Conflict,
    ] {
        tracker.observe(&tool_item("1", "computer", status, true), 20);
    }
    assert_eq!(tracker.current(20, false), None);
    assert!(tracker.used().is_empty());
    assert!(tracker.is_empty());
    // A journaled intent the policy DENIED never becomes activity, and a
    // dispatch for an intent that was denied is ignored too.
    tracker.observe(&intent("2", "mobile"), 30);
    tracker.observe(
        &authorized(
            "2",
            AuthorizationVerdict::Deny {
                reason: "not granted".into(),
            },
        ),
        30,
    );
    tracker.observe(&dispatched("2"), 31);
    assert_eq!(tracker.current(31, false), None);
    // An intent parked on a permission menu is not activity yet.
    tracker.observe(&intent("3", "ssh_shell"), 40);
    tracker.observe(
        &authorized(
            "3",
            AuthorizationVerdict::Ask {
                menu: crate::ids::MenuId::new("m"),
            },
        ),
        40,
    );
    assert_eq!(tracker.current(40, false), None);
    assert!(tracker.used().is_empty());
    // Once approved and dispatched it is.
    tracker.observe(&authorized("3", AuthorizationVerdict::Allow), 50);
    tracker.observe(&dispatched("3"), 50);
    assert_eq!(tracker.current(50, false), Some(ActivityKind::Remote));
    // A dispatch without any journaled intent carries no class: ignored.
    let mut orphan = ActivityTracker::new();
    orphan.observe(&dispatched("9"), 1);
    assert!(orphan.is_empty());
}

#[test]
fn successful_server_tool_completion_counts_without_an_effect() {
    // Provider-executed tools (web_search) have no local effect; their only
    // fact is the completed item, which counts only when it succeeded.
    let mut tracker = ActivityTracker::new();
    let mut completed = tool_item("s", "web_search", ToolStatus::Completed, true);
    if let EventPayload::Item(ItemEvent::Completed {
        item: TurnItem::ToolCall { activity, .. },
        ..
    }) = &mut completed
    {
        *activity = Some(ActivityKind::Web);
    }
    tracker.observe(&completed, 5);
    assert_eq!(tracker.current(5, false), Some(ActivityKind::Web));
    let mut failed = ActivityTracker::new();
    failed.observe(&tool_item("s", "web_search", ToolStatus::Failed, true), 5);
    assert!(failed.is_empty());
}

#[test]
fn gate_v1_completed_untyped_server_names_never_set_activity() {
    for name in [
        "computer",
        "mobile",
        "browser_open the release page",
        "ssh_shell",
        " COMPUTER ",
        "ｃｏｍｐｕｔｅｒ",
        "",
        "mcp__server__computer",
    ] {
        let mut tracker = ActivityTracker::new();
        tracker.observe(&tool_item("srv", name, ToolStatus::Completed, true), 1_000);
        assert_eq!(tracker.current(1_000, false), None, "{name}");
        assert!(tracker.used().is_empty(), "{name}");
    }
}

#[test]
fn failed_typed_rows_do_not_enter_history() {
    for status in [
        ToolStatus::Failed,
        ToolStatus::Rejected,
        ToolStatus::Cancelled,
        ToolStatus::Unknown,
        ToolStatus::Conflict,
    ] {
        let mut event = tool_item("typed", "Run tests", status, true);
        if let EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::ToolCall { activity, .. },
            ..
        }) = &mut event
        {
            *activity = Some(ActivityKind::Run);
        }
        let mut tracker = ActivityTracker::new();
        tracker.observe(&event, 50);
        assert_eq!(tracker.current(50, false), None, "{status:?}");
        assert!(tracker.used().is_empty(), "{status:?}");
    }
}

#[test]
fn registered_effectless_dispatch_is_live_until_settlement() {
    use crate::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use crate::ids::{DeviceId, EventId, SessionId};
    let mut payload = serde_json::to_value(EventPayload::RunState(RunState::RunningTool))
        .expect("running state serializes");
    payload["activity_kind"] = serde_json::json!("web");
    payload["activity_call_id"] = serde_json::json!("registered-call");
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("registered-live"),
        seq: 1,
        session_id: SessionId::new("s"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("d"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 100,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    };
    let mut tracker = ActivityTracker::new();
    tracker.set_terminal(true);
    tracker.observe_raw(&envelope);
    assert_eq!(tracker.current(101, false), Some(ActivityKind::Web));
    assert!(
        tracker.used().is_empty(),
        "a running call is not successful history"
    );
    let mut failed = tool_item("failed", "web_search", ToolStatus::Rejected, true);
    if let EventPayload::Item(ItemEvent::Completed {
        item: TurnItem::ToolCall { call_id, .. },
        ..
    }) = &mut failed
    {
        *call_id = "registered-call".into();
    }
    tracker.observe(&failed, 102);
    assert_eq!(tracker.current(102, false), None);
    assert!(tracker.used().is_empty());

    let mut successful = ActivityTracker::new();
    successful.observe_raw(&envelope);
    let mut completed = tool_item("ok", "web_search", ToolStatus::Completed, true);
    if let EventPayload::Item(ItemEvent::Completed {
        item: TurnItem::ToolCall {
            call_id, activity, ..
        },
        ..
    }) = &mut completed
    {
        *call_id = "registered-call".into();
        *activity = Some(ActivityKind::Web);
    }
    successful.observe(&completed, 102);
    assert_eq!(successful.current(102, false), Some(ActivityKind::Web));
    assert_eq!(successful.used(), vec![ActivityKind::Web]);

    // Two accepted calls can overlap. Closing one must not erase the other;
    // a duplicate start replaces only its own typed fact during replay.
    let mut concurrent = ActivityTracker::new();
    concurrent.observe_raw(&envelope);
    let mut second = envelope.clone();
    second.seq = 2;
    second.payload.insert_metadata(
        "activity_call_id",
        serde_json::json!("second-registered-call"),
    );
    second
        .payload
        .insert_metadata("activity_kind", serde_json::json!("read"));
    concurrent.observe_raw(&second);
    concurrent.observe_raw(&second);
    assert_eq!(
        concurrent.state().open,
        vec![ActivityKind::Web, ActivityKind::Read]
    );
    let mut rejected = tool_item("one", "web_search", ToolStatus::Rejected, true);
    if let EventPayload::Item(ItemEvent::Completed {
        item: TurnItem::ToolCall { call_id, .. },
        ..
    }) = &mut rejected
    {
        *call_id = "registered-call".into();
    }
    concurrent.observe(&rejected, 103);
    assert_eq!(
        concurrent.current(100 + ACTIVITY_LINGER_MS * 3, false),
        Some(ActivityKind::Read),
        "an accepted live call survives both time decay and another call's rejection"
    );
    assert!(concurrent.used().is_empty());
    concurrent.observe(&EventPayload::RunState(RunState::Done), 104);
    assert_eq!(concurrent.current(104, false), None);
    assert!(concurrent.state().open.is_empty());

    for (call_id, kind) in [("", "web"), ("unknown", "teleport")] {
        let mut invalid = envelope.clone();
        invalid
            .payload
            .insert_metadata("activity_call_id", serde_json::json!(call_id));
        invalid
            .payload
            .insert_metadata("activity_kind", serde_json::json!(kind));
        let mut replay = ActivityTracker::new();
        replay.observe_raw(&invalid);
        assert!(replay.is_empty(), "invalid live metadata: {call_id}/{kind}");
    }
    let mut old_journal = envelope.clone();
    old_journal.payload = serde_json::to_value(EventPayload::RunState(RunState::RunningTool))
        .expect("old run state serializes")
        .into();
    let mut replay = ActivityTracker::new();
    replay.set_terminal(true);
    replay.observe_raw(&old_journal);
    assert!(
        replay.state().terminal,
        "untyped RunningTool keeps a terminal icon closed"
    );
    assert!(
        replay.is_empty(),
        "old journals cannot invent live activity"
    );
}

#[test]
fn gate_v4_provider_transport_recovery_has_no_agent_activity() {
    let effect = EffectId::new("provider-admission-run-1");
    let mut tracker = ActivityTracker::new();
    for phase in [
        EffectPhase::Intent(EffectIntent {
            effect: effect.clone(),
            class: EffectClass::ProviderTransport,
            summary: "provider request admitted before restart".into(),
            args_digest: "sha256:0".into(),
            workspace_revision: None,
        }),
        EffectPhase::Authorized {
            effect: effect.clone(),
            verdict: AuthorizationVerdict::Allow,
        },
        EffectPhase::Dispatched {
            effect: effect.clone(),
        },
        EffectPhase::Outcome {
            effect,
            outcome: EffectOutcome::Unknown,
            freshness: None,
            workspace_mutation: None,
        },
    ] {
        tracker.observe(&EventPayload::Effect(phase), 5_000);
    }
    assert_eq!(tracker.current(5_000, false), None);
    assert!(tracker.used().is_empty());
}

#[test]
fn operator_shell_is_never_agent_activity_even_after_replay() {
    use crate::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use crate::ids::{DeviceId, EventId, SessionId};
    let item = TurnItem::CommandExecution {
        call_id: "user-shell".into(),
        command: "cargo test".into(),
        status: ToolStatus::Completed,
        exit_code: Some(0),
    };
    let event = EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("user-shell"),
        item,
    });
    let mut tracker = ActivityTracker::new();
    tracker.observe(&event, 1_000);
    assert!(tracker.used().is_empty());
    assert_eq!(tracker.current(1_000, false), None);
    // Both child targeting modes have the same old journal item shape:
    // the shell can be addressed through the child session or its agent id.
    for agent_id in [None, Some(AgentId::new("child-agent"))] {
        let raw = EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("old-shell"),
            seq: 1,
            session_id: SessionId::new("child-session"),
            branch_id: None,
            run_id: None,
            agent_id,
            device_id: DeviceId::new("device"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 1_000,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(&event).expect("old item").into(),
        };
        let mut replay = ActivityTracker::new();
        replay.observe_raw(&raw);
        assert!(replay.used().is_empty());
        assert_eq!(replay.current(1_000, false), None);
    }
    for (index, name) in ["process_exec", "test_run", "tool_script"]
        .into_iter()
        .enumerate()
    {
        let agent = TurnItem::ToolCall {
            call_id: format!("agent-{name}"),
            name: name.into(),
            args: serde_json::Value::Null,
            status: ToolStatus::Completed,
            activity: Some(ActivityKind::Run),
        };
        tracker.observe(
            &EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("agent-{name}")),
                item: agent,
            }),
            1_001 + index as u64,
        );
    }
    assert_eq!(tracker.used(), vec![ActivityKind::Run]);
}

#[test]
fn user_typed_process_effect_never_opens_activity_live_or_on_replay() {
    use crate::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use crate::ids::{DeviceId, EventId, RunId, SessionId};
    let phases = [
        intent("shell", "process_exec"),
        authorized(
            "shell",
            AuthorizationVerdict::PreAuthorized {
                source: AuthorizationSource::UserTyped,
            },
        ),
        dispatched("shell"),
        EventPayload::Effect(EffectPhase::Outcome {
            effect: EffectId::new("effect-shell"),
            outcome: EffectOutcome::Ok,
            freshness: None,
            workspace_mutation: None,
        }),
    ];
    for agent_id in [None, Some(AgentId::new("child-agent"))] {
        let mut live = ActivityTracker::new();
        let mut replay = ActivityTracker::new();
        for (index, phase) in phases.iter().enumerate() {
            let at_ms = 1_000 + index as u64;
            live.observe(phase, at_ms);
            let raw = EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("old-shell-effect-{index}")),
                seq: index as u64 + 1,
                session_id: SessionId::new("child-session"),
                branch_id: None,
                run_id: Some(RunId::new("shell-run")),
                agent_id: agent_id.clone(),
                device_id: DeviceId::new("device"),
                authority_epoch: 0,
                worker_generation: 1,
                causation_id: None,
                correlation_id: None,
                committed_at_ms: at_ms,
                render: RenderTargets {
                    ui: false,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: serde_json::to_value(phase)
                    .expect("old journal phase")
                    .into(),
            };
            replay.observe_raw(&raw);
            assert_eq!(live.current(at_ms, false), None);
            assert_eq!(replay.current(at_ms, false), None);
            assert!(live.used().is_empty());
            assert!(replay.used().is_empty());
        }
    }
    let mut agent_tool = ActivityTracker::new();
    start(&mut agent_tool, "agent", "process_exec", 2_000);
    assert_eq!(agent_tool.current(2_000, false), Some(ActivityKind::Run));
    finish(&mut agent_tool, "agent", "process_exec", 2_001);
    assert_eq!(agent_tool.used(), vec![ActivityKind::Run]);
}

#[test]
fn operator_checkpoint_effects_do_not_count_as_agent_edits() {
    for class in [EffectClass::CheckpointRestore, EffectClass::FsWrite] {
        let mut tracker = ActivityTracker::new();
        let effect = EffectId::new("operator-restore");
        tracker.observe(
            &EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect.clone(),
                class: class.clone(),
                summary: "workspace checkpoint".into(),
                args_digest: "digest".into(),
                workspace_revision: None,
            })),
            1_000,
        );
        tracker.observe(
            &EventPayload::Effect(EffectPhase::Authorized {
                effect: effect.clone(),
                verdict: AuthorizationVerdict::Allow,
            }),
            1_000,
        );
        tracker.observe(
            &EventPayload::Effect(EffectPhase::Dispatched {
                effect: effect.clone(),
            }),
            1_000,
        );
        if class == EffectClass::CheckpointRestore {
            assert!(tracker.used().is_empty());
            assert_eq!(tracker.current(1_000, false), None);
        } else {
            assert_eq!(
                tracker.current(1_000, false),
                Some(ActivityKind::Edit),
                "agent FsWrite remains edit"
            );
        }
    }
    for name in ["fs_write", "fs_edit", "fs_path"] {
        let mut tracker = ActivityTracker::new();
        tracker.observe(
            &EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("agent-{name}")),
                item: TurnItem::ToolCall {
                    call_id: format!("agent-{name}"),
                    name: name.into(),
                    args: serde_json::Value::Null,
                    status: ToolStatus::Completed,
                    activity: Some(ActivityKind::Edit),
                },
            }),
            2_000,
        );
        assert_eq!(
            tracker.current(2_000, false),
            Some(ActivityKind::Edit),
            "{name}"
        );
        assert_eq!(tracker.used(), vec![ActivityKind::Edit], "{name}");
    }
}

#[test]
fn old_checkpoint_journals_replay_without_operator_edit_history() {
    use crate::checkpoint::{CheckpointKind, CheckpointOrigin, CheckpointRecorded};
    use crate::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use crate::ids::{CheckpointId, DeviceId, EventId, RunId, SessionId};
    for agent_id in [None, Some(AgentId::new("child-agent"))] {
        for origin in [
            CheckpointOrigin::Undo,
            CheckpointOrigin::Redo,
            CheckpointOrigin::RollbackTurn,
            CheckpointOrigin::Tool,
        ] {
            let run = RunId::new("old-operator-run");
            let effect = EffectId::new("old-operator-effect");
            let envelope = |seq: u64, payload: EventPayload| EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("old-checkpoint-{seq}")),
                seq,
                session_id: SessionId::new("child"),
                branch_id: None,
                run_id: Some(run.clone()),
                agent_id: agent_id.clone(),
                device_id: DeviceId::new("device"),
                authority_epoch: 0,
                worker_generation: 1,
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 1_000 + seq,
                render: RenderTargets {
                    ui: false,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: serde_json::to_value(payload).expect("payload").into(),
            };
            for (prior_edit, intervening_work) in [(false, false), (true, false), (false, true)] {
                let mut tracker = ActivityTracker::new();
                if prior_edit {
                    start(&mut tracker, "prior-agent", "fs_write", 500);
                    finish(&mut tracker, "prior-agent", "fs_write", 501);
                    tracker.observe(&EventPayload::RunState(RunState::Done), 502);
                }
                for (seq, payload) in [
                    EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                        effect: effect.clone(),
                        class: EffectClass::FsWrite,
                        summary: "old checkpoint".into(),
                        args_digest: "digest".into(),
                        workspace_revision: None,
                    })),
                    EventPayload::Effect(EffectPhase::Authorized {
                        effect: effect.clone(),
                        verdict: AuthorizationVerdict::Allow,
                    }),
                    EventPayload::Effect(EffectPhase::Dispatched {
                        effect: effect.clone(),
                    }),
                    EventPayload::Effect(EffectPhase::Outcome {
                        effect: effect.clone(),
                        outcome: EffectOutcome::Ok,
                        freshness: None,
                        workspace_mutation: None,
                    }),
                    EventPayload::CheckpointRecorded(CheckpointRecorded {
                        checkpoint_id: CheckpointId::new("old-checkpoint"),
                        session_id: SessionId::new("child"),
                        branch_id: None,
                        run_id: run.clone(),
                        effect_id: effect.clone(),
                        call_id: "operator".into(),
                        seq: 5,
                        workspace_revision: None,
                        kind: CheckpointKind::Write,
                        origin,
                        source_checkpoint_id: None,
                        paths: Vec::new(),
                        post_digest: "digest".into(),
                        recorded_at_ms: 1_005,
                        redacted_content: false,
                    }),
                ]
                .into_iter()
                .enumerate()
                {
                    if intervening_work && seq == 4 {
                        let mut agent =
                            tool_item("new-agent", "process_exec", ToolStatus::Completed, true);
                        if let EventPayload::Item(ItemEvent::Completed {
                            item: TurnItem::ToolCall { activity, .. },
                            ..
                        }) = &mut agent
                        {
                            *activity = Some(ActivityKind::Run);
                        }
                        tracker.observe_raw(&envelope(5, agent));
                    }
                    tracker.observe_raw(&envelope(
                        seq as u64 + 1 + u64::from(intervening_work && seq == 4),
                        payload,
                    ));
                }
                if intervening_work {
                    assert_eq!(tracker.used(), vec![ActivityKind::Edit, ActivityKind::Run]);
                    assert_eq!(
                        tracker.state().latest.map(|touch| touch.kind),
                        Some(ActivityKind::Run)
                    );
                } else if origin == CheckpointOrigin::Tool {
                    assert_eq!(tracker.used(), vec![ActivityKind::Edit]);
                    assert_eq!(tracker.current(1_005, false), Some(ActivityKind::Edit));
                } else if prior_edit {
                    assert_eq!(tracker.used(), vec![ActivityKind::Edit]);
                    assert_eq!(
                        tracker.state().touched[0].at_ms,
                        501,
                        "operator restore cannot refresh prior agent edits"
                    );
                    assert_eq!(
                        tracker.current(1_005, false),
                        None,
                        "operator restore cannot revive a terminal agent's icon"
                    );
                } else {
                    assert!(tracker.used().is_empty(), "{origin:?}");
                    assert_eq!(tracker.current(1_005, false), None, "{origin:?}");
                }
            }
        }
    }
}

#[test]
fn effect_classes_map_to_the_documented_kinds() {
    use ActivityKind as A;
    for (class, kind) in [
        (EffectClass::ScreenObserve, Some(A::Computer)),
        (EffectClass::ScreenControl, Some(A::Computer)),
        (EffectClass::GuiAct, Some(A::Computer)),
        (EffectClass::MobileObserve, Some(A::Mobile)),
        (EffectClass::MobileControl, Some(A::Mobile)),
        (EffectClass::ReadSms, Some(A::Mobile)),
        (EffectClass::Network { host: "h".into() }, Some(A::Web)),
        (EffectClass::RemoteExecution, Some(A::Remote)),
        (EffectClass::FsWrite, Some(A::Edit)),
        (EffectClass::GitOp, Some(A::Edit)),
        (EffectClass::ProcessExec, Some(A::Run)),
        (EffectClass::FsRead, Some(A::Read)),
        (EffectClass::AgentSpawn, Some(A::Orchestrate)),
        (EffectClass::PeerMessage, Some(A::Orchestrate)),
        (EffectClass::CredentialAccess, None),
        (EffectClass::ProviderTransport, None),
    ] {
        assert_eq!(A::for_effect(&class), kind, "{class:?}");
    }
}

#[test]
fn an_outstanding_child_keeps_the_parent_orchestrating_until_it_reports() {
    use crate::agent::{ChildReport, ReportVerification};
    let mut tracker = ActivityTracker::new();
    let spawn = EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("cs"),
        item: TurnItem::ChildSpawn {
            agent: crate::ids::AgentId::new("kid"),
        },
    });
    tracker.observe(&spawn, 10);
    let much_later = 10 + ACTIVITY_LINGER_MS * 5;
    assert_eq!(
        tracker.current(much_later, false),
        Some(ActivityKind::Orchestrate),
        "waiting on a child is orchestrating however long it takes"
    );
    assert_eq!(tracker.state().open, vec![ActivityKind::Orchestrate]);
    let report = EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("cr"),
        item: TurnItem::ChildResult {
            report: ChildReport {
                agent: crate::ids::AgentId::new("kid"),
                summary: "done".into(),
                verified: ReportVerification::Unverified,
                workspace_revision: None,
            },
        },
    });
    tracker.observe(&report, much_later);
    assert!(tracker.state().open.is_empty());
    assert_eq!(
        tracker.current(much_later + ACTIVITY_LINGER_MS + 1, false),
        None
    );
}

#[test]
fn operator_shell_run_states_preserve_prior_agent_activity_live_and_after_restart() {
    use crate::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use crate::ids::{DeviceId, EventId, RunId, SessionId};
    for agent in [None, Some(AgentId::new("child-agent"))] {
        for terminal in [false, true] {
            let mut tracker = ActivityTracker::new();
            start(&mut tracker, "prior-agent", "fs_write", 500);
            finish(&mut tracker, "prior-agent", "fs_write", 501);
            if terminal {
                tracker.observe(&EventPayload::RunState(RunState::Done), 502);
            }
            let before = tracker.state();
            let mut replay = tracker.clone();
            let raw = |seq, payload: EventPayload| EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("shell-origin-{seq}")),
                seq,
                session_id: SessionId::new("child-session"),
                branch_id: None,
                run_id: Some(RunId::new("operator-shell-run")),
                agent_id: agent.clone(),
                device_id: DeviceId::new("device"),
                authority_epoch: 0,
                worker_generation: 1,
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 1_000 + seq,
                render: RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: serde_json::to_value(payload).expect("shell fact").into(),
            };
            // The real admission is one atomic store transaction: RunningTool,
            // then the typed CommandExecution started row and origin extension.
            let admission = [
                raw(1, EventPayload::RunState(RunState::RunningTool)),
                raw(
                    2,
                    EventPayload::Item(ItemEvent::Started {
                        item_id: ItemId::new("operator-shell"),
                        item: TurnItem::CommandExecution {
                            call_id: "operator-shell".into(),
                            command: "echo operator".into(),
                            status: ToolStatus::InProgress,
                            exit_code: None,
                        },
                    }),
                ),
            ];
            for envelope in &admission {
                tracker.observe_raw(envelope);
                let persisted = serde_json::to_vec(envelope).expect("admission journal bytes");
                replay
                    .observe_raw(&serde_json::from_slice(&persisted).expect("replayed admission"));
                assert_eq!(tracker.state(), before, "each operator admission frame");
                assert_eq!(replay.state(), before, "each replayed admission frame");
            }
            assert_eq!(
                tracker.state(),
                before,
                "typed operator admission cannot revive prior activity"
            );
            assert_eq!(replay.state(), before);
            for (index, payload) in [
                intent("shell", "process_exec"),
                authorized(
                    "shell",
                    AuthorizationVerdict::PreAuthorized {
                        source: AuthorizationSource::UserTyped,
                    },
                ),
                dispatched("shell"),
                EventPayload::RunState(RunState::EffectOutcomeUnknown),
                EventPayload::RunState(RunState::Cancelling),
                EventPayload::RunState(RunState::Cancelled),
                EventPayload::RunState(RunState::Errored),
                EventPayload::RunState(RunState::Done),
            ]
            .into_iter()
            .enumerate()
            {
                let envelope = raw(index as u64 + 3, payload);
                tracker.observe_raw(&envelope);
                let persisted = serde_json::to_vec(&envelope).expect("journal bytes");
                replay.observe_raw(&serde_json::from_slice(&persisted).expect("restart journal"));
                assert_eq!(tracker.state(), before);
                assert_eq!(replay.state(), before);
            }
            let mut unscoped = raw(19, EventPayload::RunState(RunState::Done));
            unscoped.run_id = None;
            let mut unscoped_tracker = tracker.clone();
            unscoped_tracker.observe_raw(&unscoped);
            assert!(
                unscoped_tracker.state().terminal,
                "unscoped agent facts cannot inherit a prior operator origin"
            );
            // A later agent run retains the normal lifecycle behavior.
            let mut next = raw(20, EventPayload::RunState(RunState::Thinking));
            next.run_id = Some(RunId::new("next-agent-run"));
            tracker.observe_raw(&next);
            assert!(!tracker.state().terminal);
            assert_eq!(tracker.current(1_020, false), Some(ActivityKind::Edit));
        }
    }
}
