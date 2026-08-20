//! H4 — the `/hooks` screen: daemon-truth listing with trust states,
//! receipted trust/revoke that installs nothing locally, revoked-by-edit
//! derivation, bounded newest-first firings, honest feature/demo gates,
//! and the decision-hook status chip.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::hook::{HookDecisionKind, HookEventPayload, HookFired, HookOutput};
use haider_protocol::ids::{DeviceId, EventId, MenuId, RunId, SessionId};
use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
use haider_rpc::{AttachmentId, HookSummaryWire, HookTrustStateWire, ResponseBody, SessionSummary};
use haider_tui::app::{AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::hooks::HookRow;
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

mod common;
use common::{key, launcher_model, submit};

fn sid() -> SessionId {
    SessionId::new("s-hooks")
}

/// A live session with the hooks feature advertised and a literal cwd the
/// listing must capture at issuance.
fn live_hooks_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_HOOKS_V1.to_owned()]
        .into_iter()
        .collect();
    model.cwd = "/work/h4".to_owned();
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model
}

fn wire(
    name: &str,
    digest: &str,
    trusted: bool,
    kind: &str,
    event: &str,
    decision: bool,
) -> HookSummaryWire {
    HookSummaryWire {
        name: name.to_owned(),
        digest: digest.to_owned(),
        source: "/work/h4/hooks.json".to_owned(),
        kind: kind.to_owned(),
        event: event.to_owned(),
        trusted,
        trust_state: None,
        decision,
        timeout_ms: 30_000,
    }
}

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rows.push(line);
    }
    (rows, hits)
}

/// One committed envelope for the hooks session. `ui: false` for hook
/// facts — the engine journals them `render.ui == false`.
fn raw(seq: u64, run: Option<&str>, ui: bool, payload: serde_json::Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: run.map(RunId::new),
        agent_id: None,
        device_id: DeviceId::new("hooks-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

/// A journaled `hook_fired` fact (`render.ui == false`, like the engine's).
fn hook_fired_env(
    seq: u64,
    run: &str,
    hook: &str,
    decision: Option<(HookDecisionKind, bool)>,
) -> RawEnvelope {
    let output = HookOutput {
        preview: String::new(),
        bytes: 0,
        truncated: false,
        artifact: None,
    };
    let fired = HookFired {
        hook: hook.to_owned(),
        digest: "c".repeat(64),
        kind: if decision.is_some() {
            haider_protocol::hook::HookRuntimeKind::Decision
        } else {
            haider_protocol::hook::HookRuntimeKind::Exec
        },
        observed_seq: seq,
        exit_code: Some(0),
        timed_out: false,
        stdout: output.clone(),
        stderr: output,
        proposed_decision: decision.map(|(kind, _)| kind),
        menu_id: None,
        decision_applied: decision.is_some_and(|(_, applied)| applied),
    };
    raw(
        seq,
        Some(run),
        false,
        HookEventPayload::HookFired(fired)
            .to_payload_value()
            .expect("fact serializes"),
    )
}

/// Any run-scoped content envelope — what moves the session's current run.
fn run_env(seq: u64, run: &str) -> RawEnvelope {
    raw(
        seq,
        Some(run),
        true,
        serde_json::to_value(EventPayload::IdleDecayed).expect("payload serializes"),
    )
}

/// MUTATION CHECK (H4 law 1): fabricate rows while the read is in flight,
/// drop the snapshot apply, or paint every row trusted. Expected runtime
/// failure: the fetching state shows rows, or the listed names / ✓ ○
/// glyphs / policy below never appear.
#[test]
fn hooks_screen_lists_daemon_truth_with_trust_states() {
    let mut model = live_hooks_model();
    submit(&mut model, "/hooks");
    assert_eq!(model.screen, Screen::Hooks, "the screen opened");
    // In flight: the read is honest — nothing fabricated.
    let (rows, _) = draw(&model, 120, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("fetching the daemon's hook discovery…")),
        "in-flight state says so"
    );
    // The cwd was captured at issuance.
    let refresh = model
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::HooksRefresh { .. }))
        .expect("the refresh was requested")
        .clone();
    assert_eq!(
        refresh,
        AppRequest::HooksRefresh {
            cwd: "/work/h4".to_owned()
        }
    );
    // Driver → wire: a plain read, method `hooks.list`.
    let mut driver = LiveDriver::new("test");
    let commands = driver.handle_request(&mut model, refresh);
    assert_eq!(
        commands,
        vec![LiveCommand::HooksList {
            cwd: "/work/h4".to_owned()
        }]
    );
    let body = serde_json::to_value(request_body(commands[0].clone())).expect("body serializes");
    assert_eq!(body["method"], "hooks.list");
    assert_eq!(body["cwd"], "/work/h4");
    // The daemon's listing is the ONLY row writer.
    driver.apply(
        &mut model,
        LiveReply::Hooks {
            policy: "per_digest".to_owned(),
            revision: 0,
            hooks: vec![
                wire(
                    "fmt-on-finish",
                    &"a".repeat(64),
                    true,
                    "exec",
                    "run_finished",
                    false,
                ),
                wire(
                    "perm-gate",
                    &"b".repeat(64),
                    false,
                    "decision",
                    "run_parked",
                    true,
                ),
            ],
        },
    );
    let (rows, hits) = draw(&model, 120, 34);
    let listing = rows.join("\n");
    assert!(listing.contains("policy per_digest"), "policy is shown");
    assert!(
        listing.contains("1. ✓ fmt-on-finish"),
        "the trusted row wears ✓ and its number: {listing}"
    );
    assert!(
        listing.contains("exec:run_finished · aaaaaaaa · trusted"),
        "kind, matcher, short digest and trust label render"
    );
    assert!(
        listing.contains("2. ○ perm-gate"),
        "the untrusted row wears ○"
    );
    assert!(
        listing.contains("decision:run_parked · decision · bbbbbbbb · untrusted"),
        "the decision hook names itself"
    );
    // Rows are clickable by VALUE (digest).
    assert!(
        hits.iter()
            .any(|(_, hit)| hit == &Hit::HookRow("a".repeat(64))),
        "the row's hit carries its digest"
    );
}

/// MUTATION CHECK (H4 law 2): flip the row's trust locally on ⏎, skip the
/// receipted command, reuse `hooks.trust` for revoke, or install from the
/// receipt instead of the chained listing. Expected runtime failure: the
/// no-local-fabrication assertions below, the wire-method assertions, or
/// the receipt-installs-nothing assertion.
#[test]
fn trust_and_revoke_dispatch_receipted_commands_and_install_nothing_locally() {
    let mut model = live_hooks_model();
    submit(&mut model, "/hooks");
    let mut driver = LiveDriver::new("test");
    // Seed the driver's cwd through the real refresh drain.
    let refresh = model
        .requests
        .drain(..)
        .find(|request| matches!(request, AppRequest::HooksRefresh { .. }))
        .expect("the refresh was requested");
    driver.handle_request(&mut model, refresh);
    driver.apply(
        &mut model,
        LiveReply::Hooks {
            policy: "per_digest".to_owned(),
            revision: 0,
            hooks: vec![wire(
                "guard",
                &"b".repeat(64),
                false,
                "exec",
                "run_started",
                false,
            )],
        },
    );
    // ⏎ opens the confirmation card for the highlighted row.
    model.handle(key(KeyCode::Enter));
    let (rows, _) = draw(&model, 120, 34);
    let card = rows.join("\n");
    assert!(card.contains("trust hook `guard`?"), "the card names it");
    assert!(card.contains("⏎ confirm · esc cancel"), "the card's keys");
    // Esc cancels the CARD, never the screen (session-scoped esc law).
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Hooks, "esc closed only the card");
    let (rows, _) = draw(&model, 120, 34);
    assert!(
        !rows.join("\n").contains("trust hook `guard`?"),
        "the card is gone"
    );
    // Re-open and confirm: ONE receipted request, nothing local moves.
    model.handle(key(KeyCode::Enter));
    model.handle(key(KeyCode::Enter));
    let trust = model
        .requests
        .drain(..)
        .find(|request| matches!(request, AppRequest::HooksTrust { .. }))
        .expect("the trust request was pushed");
    assert_eq!(
        trust,
        AppRequest::HooksTrust {
            digest: "b".repeat(64),
            trusted: true
        }
    );
    assert!(
        model
            .hooks
            .rows
            .as_ref()
            .is_some_and(|rows| !rows[0].trusted),
        "dispatch installed NOTHING locally"
    );
    assert_eq!(
        model.hooks.pending.as_deref(),
        Some("b".repeat(64).as_str()),
        "the one-at-a-time gate armed"
    );
    // The driver mints a durable receipt and the wire method is
    // `hooks.trust`.
    let commands = driver.handle_request(&mut model, trust);
    assert_eq!(commands.len(), 1);
    let LiveCommand::HooksTrust {
        command_id,
        digest,
        trusted,
    } = commands[0].clone()
    else {
        panic!("a receipted hooks trust command: {commands:?}");
    };
    assert_eq!(digest, "b".repeat(64));
    assert!(trusted);
    assert_eq!(driver.outbox_len(), 1, "the mutation is outboxed");
    let body = serde_json::to_value(request_body(commands[0].clone())).expect("body serializes");
    assert_eq!(body["method"], "hooks.trust");
    assert_eq!(body["digest"], "b".repeat(64).as_str());
    assert_eq!(body["command_id"], command_id.0.as_str());
    // The receipt retires the gate, chains a fresh listing at the SAME
    // cwd — and still installs nothing.
    let context = CommandContext::of(&commands[0]);
    let replies = map_response(
        &context,
        ResponseBody::HooksTrust {
            digest: "b".repeat(64),
            trusted: true,
        },
    );
    assert_eq!(
        replies,
        vec![LiveReply::HookTrustChanged {
            command_id,
            digest: "b".repeat(64),
            trusted: true,
        }]
    );
    let chained = driver.apply(&mut model, replies[0].clone());
    assert_eq!(
        chained,
        vec![LiveCommand::HooksList {
            cwd: "/work/h4".to_owned()
        }],
        "the receipt chains daemon truth"
    );
    assert_eq!(driver.outbox_len(), 0, "the receipt retired the outbox");
    assert_eq!(model.hooks.pending, None, "the gate released");
    assert!(
        model
            .hooks
            .rows
            .as_ref()
            .is_some_and(|rows| !rows[0].trusted),
        "the receipt itself installed NOTHING"
    );
    // Only the daemon's next listing moves the trust column.
    driver.apply(
        &mut model,
        LiveReply::Hooks {
            policy: "per_digest".to_owned(),
            revision: 0,
            hooks: vec![wire(
                "guard",
                &"b".repeat(64),
                true,
                "exec",
                "run_started",
                false,
            )],
        },
    );
    assert!(
        model
            .hooks
            .rows
            .as_ref()
            .is_some_and(|rows| rows[0].trusted),
        "daemon truth flipped the column"
    );
    // REVOKE: a trusted row's card offers revoke and rides `hooks.revoke`.
    model.handle(key(KeyCode::Enter));
    let (rows, _) = draw(&model, 120, 34);
    assert!(
        rows.join("\n").contains("revoke hook `guard`?"),
        "a trusted row's card revokes"
    );
    model.handle(key(KeyCode::Enter));
    let revoke = model
        .requests
        .drain(..)
        .find(|request| matches!(request, AppRequest::HooksTrust { .. }))
        .expect("the revoke request was pushed");
    assert_eq!(
        revoke,
        AppRequest::HooksTrust {
            digest: "b".repeat(64),
            trusted: false
        }
    );
    let commands = driver.handle_request(&mut model, revoke);
    let body = serde_json::to_value(request_body(commands[0].clone())).expect("body serializes");
    assert_eq!(body["method"], "hooks.revoke");
    assert_eq!(body["digest"], "b".repeat(64).as_str());
}

/// MUTATION CHECK (H4 law 3): infer trust from client snapshot history or
/// ignore the wire classification. Expected runtime failure: this one-shot
/// daemon snapshot cannot render ✗, or its trust card offers revoke.
#[test]
fn edited_hook_renders_revoked_state() {
    let mut model = live_hooks_model();
    submit(&mut model, "/hooks");
    let mut edited = wire("fmt", &"d".repeat(64), true, "exec", "run_finished", false);
    edited.trust_state = Some(HookTrustStateWire::RevokedByEdit);
    let mut never = wire(
        "never",
        &"e".repeat(64),
        false,
        "exec",
        "run_started",
        false,
    );
    never.trust_state = Some(HookTrustStateWire::Untrusted);
    // One daemon snapshot is sufficient; the TUI has no prior digest from
    // which it could derive revocation. The legacy boolean is deliberately
    // contradictory so both the glyph and confirmation-card assertions pin
    // the new classification as the authority.
    model
        .hooks
        .apply_snapshot("per_digest".to_owned(), 4, vec![edited, never]);
    let (rows, _) = draw(&model, 120, 34);
    let listing = rows.join("\n");
    assert!(
        listing.contains("✗ fmt") && listing.contains("revoked by edit"),
        "the edited hook renders revoked-by-edit: {listing}"
    );
    // Non-degenerate contrast: a hook NEVER trusted stays plain ○.
    assert!(
        listing.contains("○ never") && listing.contains("· untrusted"),
        "a never-trusted hook stays untrusted, not revoked"
    );
    model.handle(key(KeyCode::Enter));
    assert!(
        model
            .hooks
            .confirm
            .as_ref()
            .is_some_and(|confirm| confirm.grant),
        "a revoked-by-edit card offers trust from wire state"
    );
}

/// WIRE-GAPS items 3-4: a trust-change fact on the existing attached event
/// cadence chases `hooks.list` only when its revision advances, and uses the
/// active session's summary workspace rather than this process's cwd.
#[test]
fn trust_revision_event_refreshes_the_summary_workspace() {
    let mut model = live_hooks_model();
    model.cwd = "/opened/elsewhere".to_owned();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![SessionSummary {
                session_id: sid(),
                head_seq: 0,
                worker_generation: 7,
                metadata: None,
                workspace_cwd: Some("/work/original".into()),
                turn_count: None,
                footprint_tokens: None,
                footprint_truth: None,
                title: None,
                agent_metrics: None,
                last_model: None,
                parent_session_id: None,
                kind: None,
                agent_type: None,
            }],
            next_cursor: None,
        },
    );
    submit(&mut model, "/hooks");
    let refresh = model
        .requests
        .drain(..)
        .find(|request| matches!(request, AppRequest::HooksRefresh { .. }))
        .expect("summary-scoped refresh");
    assert_eq!(
        refresh,
        AppRequest::HooksRefresh {
            cwd: "/work/original".into()
        }
    );
    assert_eq!(
        driver.handle_request(&mut model, refresh),
        vec![LiveCommand::HooksList {
            cwd: "/work/original".into()
        }]
    );
    driver.apply(
        &mut model,
        LiveReply::Hooks {
            policy: "per_digest".into(),
            revision: 4,
            hooks: Vec::new(),
        },
    );
    let attachment = AttachmentId::new("hooks-revision-attachment");
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(),
            attachment: attachment.clone(),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    let changed = raw(
        1,
        None,
        false,
        HookEventPayload::HookTrustChanged {
            digest: "f".repeat(64),
            trusted: true,
            revision: 5,
        }
        .to_payload_value()
        .expect("trust fact"),
    );
    assert_eq!(
        driver.apply(
            &mut model,
            LiveReply::Event {
                attachment: attachment.clone(),
                session: sid(),
                envelope: Box::new(changed.clone()),
            },
        ),
        vec![LiveCommand::HooksList {
            cwd: "/work/original".into()
        }]
    );
    assert!(
        driver
            .apply(
                &mut model,
                LiveReply::Event {
                    attachment,
                    session: sid(),
                    envelope: Box::new(changed),
                },
            )
            .is_empty(),
        "a duplicate fact cannot trigger another refresh"
    );
}

/// MUTATION CHECK (H4 law 4): record only `Apply` envelopes (hook facts
/// are `render.ui == false`), drop the store bound, append oldest-first,
/// or lift the render cap. Expected runtime failure: the store holds 0 or
/// 53 entries instead of literal 48, the newest fact is not first, or
/// more than literal 8 firing rows paint.
#[test]
fn firings_render_bounded_newest_first() {
    let mut model = live_hooks_model();
    for seq in 1..=53 {
        assert_eq!(
            model.route_raw(&hook_fired_env(seq, "run-1", &format!("hk-{seq}"), None)),
            haider_tui::projection::RawOutcome::Applied
        );
    }
    assert_eq!(model.hook_facts.len(), 48, "the store bound holds");
    let newest = model
        .hook_facts
        .recent()
        .next()
        .expect("facts recorded")
        .line();
    assert!(newest.contains("hk-53"), "newest first: {newest}");
    // The screen paints a BOUNDED newest-first window.
    submit(&mut model, "/hooks");
    model
        .hooks
        .apply_snapshot("per_digest".to_owned(), 0, vec![]);
    let (rows, _) = draw(&model, 120, 40);
    // (matched WITHOUT the ⚡ glyph — a double-width symbol leaves a filler
    // cell in the test buffer's per-cell reconstruction)
    let firing_rows: Vec<&String> = rows.iter().filter(|row| row.contains(" hk-")).collect();
    assert_eq!(firing_rows.len(), 8, "the render bound holds");
    assert!(
        firing_rows[0].contains("hk-53 ·"),
        "the newest firing leads: {}",
        firing_rows[0]
    );
    assert!(
        firing_rows[7].contains("hk-46 ·"),
        "the window is the newest eight: {}",
        firing_rows[7]
    );
    assert!(
        !rows.iter().any(|row| row.contains("hk-45 ·")),
        "older firings stay out of the window"
    );
}

/// WIRE-GAPS item 5: a decision firing retains its menu coordinate, paints
/// a value-carrying hit, and opens the committed menu as a read-only
/// drill-down rather than fabricating a new decision.
#[test]
fn decision_firing_jumps_to_its_committed_menu() {
    let mut model = live_hooks_model();
    let menu = Menu {
        id: MenuId::new("hook-menu-drilldown"),
        kind: MenuKind::Permission {
            effect_summary: "run formatter".into(),
        },
        title: "Allow formatter?".into(),
        body: vec!["The decision hook inspected this menu.".into()],
        options: vec![MenuOption {
            key: "allow_once".into(),
            label: "Allow once".into(),
            detail: None,
            decision: Some(haider_protocol::menu::DecisionKind::AllowOnce),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "process_exec".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    assert_eq!(
        model.route_raw(&raw(
            1,
            Some("run-menu"),
            true,
            serde_json::to_value(EventPayload::MenuOpened(menu.clone())).expect("menu payload"),
        )),
        haider_tui::projection::RawOutcome::Applied
    );
    let mut fired = hook_fired_env(
        2,
        "run-menu",
        "permission-guard",
        Some((HookDecisionKind::Allow, true)),
    );
    let HookEventPayload::HookFired(mut payload) =
        HookEventPayload::from_payload_value(fired.payload.clone()).expect("fired payload")
    else {
        panic!("expected hook fired")
    };
    payload.menu_id = Some(menu.id.clone());
    fired.payload = HookEventPayload::HookFired(payload)
        .to_payload_value()
        .expect("linked fired payload");
    assert_eq!(
        model.route_raw(&fired),
        haider_tui::projection::RawOutcome::Applied
    );
    model.screen = Screen::Hooks;
    model
        .hooks
        .apply_snapshot("per_digest".into(), 1, Vec::new());
    let (rows, hits) = draw(&model, 120, 40);
    assert!(
        rows.iter().any(|row| row.contains("[decision menu]")),
        "linked firing is visibly jump-capable"
    );
    let hit = hits
        .into_iter()
        .find_map(|(_, hit)| (hit == Hit::HookFiring(menu.id.clone())).then_some(hit))
        .expect("decision firing hit");
    model.handle_hit(hit);
    assert_eq!(
        model.hooks.drilldown.as_ref().map(|menu| &menu.id),
        Some(&menu.id)
    );
    let (rows, _) = draw(&model, 120, 40);
    let card = rows.join("\n");
    assert!(card.contains("decision menu · read-only"));
    assert!(card.contains("Allow formatter?"));
    assert!(card.contains("Allow once"));
}

/// MUTATION CHECK (H4 law 5): open the screen without the daemon feature,
/// fabricate demo rows, or let the demo dispatch a trust request. Expected
/// runtime failure: the stale-daemon note is missing / the screen moved,
/// the demo empty state lies, or a `HooksTrust` request escapes the demo.
#[test]
fn ungated_and_demo_are_honest() {
    // A live daemon WITHOUT the hooks feature: honest note, nothing opens,
    // nothing is requested.
    let mut ungated = launcher_model();
    ungated.mode = RuntimeMode::Live;
    ungated.daemon_version = Some("0.0.42".to_owned());
    ungated.sessions.clear();
    ungated.upsert_live_session(&sid());
    ungated.open_session(&sid());
    submit(&mut ungated, "/hooks");
    assert_eq!(ungated.screen, Screen::Session, "the screen never moved");
    let flash = ungated.flash.clone().unwrap_or_default();
    assert!(
        flash.contains("needs a newer daemon (running v0.0.42)"),
        "the stale-daemon note names the fix: {flash}"
    );
    assert!(
        ungated.requests.iter().all(|request| !matches!(
            request,
            AppRequest::HooksRefresh { .. } | AppRequest::HooksTrust { .. }
        )),
        "an ungated daemon is asked for nothing"
    );
    // DEMO: a sim-honest empty state that refuses trust actions.
    let mut demo = launcher_model();
    submit(&mut demo, "wire the billing hooks");
    assert_eq!(demo.screen, Screen::Session);
    submit(&mut demo, "/hooks");
    assert_eq!(demo.screen, Screen::Hooks, "demo opens the screen");
    let (rows, _) = draw(&demo, 120, 34);
    assert!(
        rows.iter().any(|row| row.contains("no hooks in the demo")),
        "the demo empty state says what it is"
    );
    assert!(
        demo.requests
            .iter()
            .all(|request| !matches!(request, AppRequest::HooksRefresh { .. })),
        "the demo never asks a daemon"
    );
    // Fixtures construct states: seed a row and try to trust it.
    demo.hooks.rows = Some(vec![HookRow::from_wire(wire(
        "guard",
        &"b".repeat(64),
        false,
        "exec",
        "run_started",
        false,
    ))]);
    demo.handle(key(KeyCode::Enter));
    demo.handle(key(KeyCode::Enter));
    assert!(
        demo.requests
            .iter()
            .all(|request| !matches!(request, AppRequest::HooksTrust { .. })),
        "demo trust dispatches NOTHING"
    );
    assert_eq!(demo.hooks.pending, None, "no gate armed");
    let message = demo.hooks.message.clone().unwrap_or_default();
    assert!(
        message.contains("live-only"),
        "the refusal is honest: {message}"
    );
}

/// MUTATION CHECK (H4 law 6): light the chip on any proposal (ignore
/// `decision_applied`), never clear it on a new run, or derive it from
/// display state. Expected runtime failure: the applied-fact chip is
/// missing, the lost-proposal fixture lights it, or a new run keeps it.
#[test]
fn decision_chip_follows_the_journaled_fact() {
    let status_row = |model: &AppModel| {
        let (rows, _) = draw(model, 140, 30);
        rows.join("\n")
    };
    let mut model = live_hooks_model();
    // Run 1 begins; no fact yet — no chip.
    assert_eq!(
        model.route_raw(&run_env(1, "run-1")),
        haider_tui::projection::RawOutcome::Applied
    );
    assert!(!model.hook_facts.decision_chip());
    assert!(!status_row(&model).contains("hook·decided"));
    // The journaled fact: a decision hook's answer was APPLIED this run.
    assert_eq!(
        model.route_raw(&hook_fired_env(
            2,
            "run-1",
            "perm-gate",
            Some((HookDecisionKind::Allow, true))
        )),
        haider_tui::projection::RawOutcome::Applied
    );
    assert!(model.hook_facts.decision_chip(), "the fact lights the chip");
    assert!(
        status_row(&model).contains("hook·decided"),
        "the status bar wears the chip"
    );
    // A NEW run's first envelope drops it — the chip is per-run truth.
    assert_eq!(
        model.route_raw(&run_env(3, "run-2")),
        haider_tui::projection::RawOutcome::Applied
    );
    assert!(!model.hook_facts.decision_chip(), "a new run clears it");
    assert!(!status_row(&model).contains("hook·decided"));
    // NON-DEGENERATE: a proposal the menu CAS did NOT apply is journaled
    // too (`decision_applied == false`) and must never light the chip — a
    // lost proposal is not authority.
    assert_eq!(
        model.route_raw(&hook_fired_env(
            4,
            "run-2",
            "perm-gate",
            Some((HookDecisionKind::Allow, false))
        )),
        haider_tui::projection::RawOutcome::Applied
    );
    assert!(
        !model.hook_facts.decision_chip(),
        "a lost proposal never lights the chip"
    );
    assert!(!status_row(&model).contains("hook·decided"));
}
