//! E5-E8 error-wave VISUAL pass: the new surfaces' designed treatment,
//! pinned in the e5 register (severity-railed cards, quiet in-flight
//! facts, plain parity).
//!
//! Laws under test:
//!
//! * BANNER GRAMMAR: the persistent diagnostic banner wears the error-card
//!   grammar — severity rail `▏` + glyph + BOLD tone-ink title, dim detail
//!   (title echo stripped), dim fact segments in the one error-fact
//!   vocabulary. Severity travels in TEXT (✗ failure / ⚠ action-needed),
//!   never ink alone. Store faults are failures; a degraded voice lane is
//!   calm warn. Width pressure sheds whole fact segments (the subcode
//!   never) and ellipsizes the detail — the title never yields;
//! * RECOVERY-UNKNOWN TONE: the E6 effect-reconciliation card is
//!   UNCERTAINTY, not failure — calm amber rail behind the ⌁ glyph, typed
//!   detail replacing the prose body, the server's first option (probe)
//!   wearing the primary gold affordance;
//! * EXHAUSTED-RETRY SEVERITY: the busy-retry-exhausted failure renders as
//!   the err-railed card in the transcript AND the err-toned banner —
//!   exhausted bounds are failures, unlike the quiet in-flight flashes;
//! * QUIET RETRY FACTS: bounded in-flight retries (`tool_json_repair`,
//!   `provider_tool_fallback`, the web_fetch retry note on a COMPLETED
//!   row) read as dim ⟳ fact lines — never warn/err ink;
//! * LOCAL-ONLY LINE: the `/peers` rejection is one matter-of-fact status
//!   flash — no banner row, no error card.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
use haider_protocol::ids::{DeviceId, EffectId, EventId, ItemId, MenuId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_rpc::ERROR_CODE_BUSY;
use haider_tui::app::{AppModel, AppRequest, RuntimeMode};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::plain::render_plain;
use haider_tui::projection::SessionProjection;
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier};

mod common;
use common::{key, launcher_model};

fn sid() -> SessionId {
    SessionId::new("e8-session")
}

fn raw(seq: u64, at_ms: u64, payload: &EventPayload) -> haider_protocol::envelope::RawEnvelope {
    haider_protocol::envelope::EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-e8-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("e8-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: at_ms,
        render: haider_protocol::envelope::RenderTargets {
            ui: true,
            durable: true,
            prompt: haider_protocol::envelope::PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.requests.clear();
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, terminal)
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    row_of_from(rows, needle, 0)
}

fn row_of_from(rows: &[String], needle: &str, from: usize) -> u16 {
    u16::try_from(
        rows.iter()
            .enumerate()
            .skip(from)
            .find(|(_, row)| row.contains(needle))
            .map(|(index, _)| index)
            .unwrap_or_else(|| panic!("row not found: {needle}\n{rows:#?}")),
    )
    .expect("row index")
}

fn col_of(row: &str, needle: &str) -> u16 {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {row:?}"));
    u16::try_from(row[..byte].chars().count()).expect("col index")
}

fn ink(rgb: haider_tui::theme::Rgb) -> Color {
    Color::Rgb(rgb.r, rgb.g, rgb.b)
}

fn store_fault() -> ErrorPresentation {
    ErrorPresentation::new(
        "store-full",
        "Store unwritable",
        "Store unwritable — profile disk is full. Not committed: event-5. \
         Free space or restore write access, then retry.",
        ErrorScope::Profile,
        [ErrorAction::Retry],
    )
}

/// BANNER GRAMMAR: a store fault is a FAILURE — err rail + ✗ + BOLD err
/// title, the detail dim with its title echo stripped, the subcode and
/// actions riding the dim fact slot. A mic fault is action-needed — the ⚠
/// warn register, and no err ink anywhere on its row. Width pressure
/// sheds the actions segment whole and ellipsizes the detail while the
/// subcode survives. MUTATION: restore the all-warn uppercase banner and
/// the glyph/ink/echo assertions fail.
#[test]
fn e8a_store_fault_banner_wears_the_err_rail_and_facts() {
    let mut model = live_session();
    model.profile_diagnostic = Some(store_fault());
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 160, 24);
    let buffer = terminal.backend().buffer();
    let banner = &rows[0];
    assert!(banner.contains("✗ Store unwritable"), "{banner:?}");
    assert!(
        banner.contains("— profile disk is full. Not committed: event-5."),
        "detail rides the banner without echoing its title: {banner:?}"
    );
    assert!(
        !banner.contains("Store unwritable — Store unwritable"),
        "the title echo is stripped"
    );
    assert!(banner.contains("· store-full"), "{banner:?}");
    assert!(banner.contains("actions: retry"), "{banner:?}");
    assert_eq!(
        buffer[(col_of(banner, "▏"), 0)].fg,
        ink(theme.err),
        "the rail is the err accent"
    );
    let title_x = col_of(banner, "Store unwritable");
    assert_eq!(buffer[(title_x, 0)].fg, ink(theme.err));
    assert!(
        buffer[(title_x, 0)].modifier.contains(Modifier::BOLD),
        "TITLE prominent"
    );
    assert_eq!(
        buffer[(col_of(banner, "profile disk"), 0)].fg,
        ink(theme.dim),
        "detail is dim prose"
    );
    assert_eq!(
        buffer[(col_of(banner, "store-full"), 0)].fg,
        ink(theme.dim),
        "facts are muted metadata"
    );

    // Width pressure: whole-segment fact shedding + detail ellipsis; the
    // identity subcode never sheds and no word is cut without the honest …
    let (rows, _) = draw(&model, 40, 24);
    let narrow = &rows[0];
    assert!(narrow.contains("✗ Store unwritable"), "{narrow:?}");
    assert!(
        narrow.contains("store-full"),
        "identity survives: {narrow:?}"
    );
    assert!(
        !narrow.contains("actions:"),
        "the actions segment sheds whole: {narrow:?}"
    );
    assert!(narrow.contains('…'), "the detail ellipsizes honestly");

    // The voice lane's fault is ACTION-NEEDED, not failure: ⚠ + warn, and
    // no cell of the banner row wears the err ink.
    model.profile_diagnostic = None;
    model.voice_diagnostic = Some(ErrorPresentation::new(
        "microphone-unavailable",
        "Microphone unavailable",
        "device vanished",
        ErrorScope::Profile,
        [ErrorAction::Retry],
    ));
    let (rows, terminal) = draw(&model, 160, 24);
    let buffer = terminal.backend().buffer();
    let banner = &rows[0];
    assert!(banner.contains("⚠ Microphone unavailable"), "{banner:?}");
    assert_eq!(buffer[(col_of(banner, "⚠"), 0)].fg, ink(theme.warn));
    for x in 0..buffer.area.width {
        assert_ne!(
            buffer[(x, 0)].fg,
            ink(theme.err),
            "a degraded voice lane is never alarm-red"
        );
    }
}

/// RECOVERY-UNKNOWN TONE: the E6 four-choice card wears the calm amber
/// rail and ⌁ glyph — uncertainty, not failure — with its typed detail
/// replacing the "Dispatched effect:" prose, the subcode fact line, the
/// selected option explained, and the server's first option (probe)
/// keeping the gold primary affordance once unselected. Plain mode
/// carries the same detail, facts, and all four options. MUTATION: drop
/// the `MenuKind::Recovery` arms in `menu_block`/`wrapped_menu_body`/plain
/// and the accent/typed-detail/parity assertions fail.
#[test]
fn e8b_effect_recovery_card_is_calm_amber_with_typed_detail() {
    let menu = haider_protocol::menu::effect_recovery_menu(
        MenuId::new("e8-recovery"),
        EffectId::new("e-4411"),
        "process_exec (git push)",
    );
    let mut model = live_session();
    model.route_raw(&raw(1, 1_000, &EventPayload::MenuOpened(menu.clone())));
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 110, 32);
    let buffer = terminal.backend().buffer();
    let title_y = row_of(&rows, "⌁ Effect outcome unknown");
    let title_row = &rows[title_y as usize];
    assert_eq!(
        buffer[(col_of(title_row, "▏"), title_y)].fg,
        ink(theme.warn),
        "uncertainty wears the calm warn accent, never err"
    );
    let title_x = col_of(title_row, "Effect outcome unknown");
    assert_eq!(buffer[(title_x, title_y)].fg, ink(theme.warn));
    assert!(buffer[(title_x, title_y)].modifier.contains(Modifier::BOLD));
    let detail_y = row_of(&rows, "Haider lost contact after dispatching");
    assert_eq!(
        buffer[(col_of(&rows[detail_y as usize], "Haider lost"), detail_y)].fg,
        ink(theme.dim),
        "typed detail is dim prose"
    );
    let fact_y = row_of(&rows, "effect-outcome-unknown");
    assert_eq!(
        buffer[(col_of(&rows[fact_y as usize], "effect-outcome"), fact_y)].fg,
        ink(theme.dim)
    );
    assert!(
        !rows.iter().any(|row| row.contains("Dispatched effect:")),
        "the typed detail replaces the prose body"
    );
    // The selected primary explains itself with its typed option detail.
    row_of(&rows, "1. Probe — Re-check whether the effect completed.");
    // Move off the primary: probe keeps the gold affordance.
    model.handle(key(KeyCode::Down));
    let (rows, terminal) = draw(&model, 110, 32);
    let buffer = terminal.backend().buffer();
    let probe_y = row_of(&rows, "1. Probe");
    assert_eq!(
        buffer[(col_of(&rows[probe_y as usize], "1. Probe"), probe_y)].fg,
        ink(theme.gold),
        "the unselected primary wears the gold affordance"
    );
    let done_y = row_of(&rows, "2. Mark done");
    assert_eq!(
        buffer[(col_of(&rows[done_y as usize], "2. Mark done"), done_y)].fg,
        ink(theme.bright),
        "the selected row keeps selection styling"
    );

    // Plain parity: detail + fact line + every choice, no prose body.
    let mut projection = SessionProjection::default();
    projection.apply(&EventPayload::MenuOpened(menu));
    let rendered = render_plain(&projection, 0, None);
    assert!(rendered.contains("? Effect outcome unknown"));
    assert!(rendered.contains(
        "  Haider lost contact after dispatching process_exec (git push). \
         Reconcile it before continuing."
    ));
    assert!(rendered.contains("  effect-outcome-unknown"));
    for option in ["1. Probe", "2. Mark done", "3. Retry", "4. Abandon"] {
        assert!(rendered.contains(option), "{rendered}");
    }
    assert!(!rendered.contains("Dispatched effect:"), "{rendered}");
}

/// EXHAUSTED-RETRY SEVERITY: when the bounded busy retry exhausts, the
/// transcript gets the TYPED card-shaped err block (bold title, railed dim
/// detail, fact line with the actions hint) and the persistent banner
/// wears the err register — an exhausted bound is a failure, unlike the
/// quiet in-flight flashes. MUTATION: route the exhaustion through the
/// plain one-line `record_session_error` and the card assertions fail.
#[test]
fn e8c_busy_exhausted_renders_the_err_card_and_banner() {
    let mut model = live_session();
    let mut driver = LiveDriver::new("e8-busy-visual");
    let initial = driver.handle_request(
        &mut model,
        AppRequest::Rename {
            session: sid(),
            title: "bounded".into(),
        },
    );
    let command_id = initial[0].command_id().expect("rename command id").clone();
    let base = std::time::Instant::now();
    let busy = |command_id: &haider_rpc::CommandId, message: &str| LiveReply::Failed {
        command_id: Some(command_id.clone()),
        code: ERROR_CODE_BUSY.into(),
        message: message.into(),
        retryable: true,
        presentation: None,
    };
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(busy(&command_id, "busy")),
        base,
    );
    let _ = live_pass(
        &mut driver,
        &mut model,
        None,
        base + std::time::Duration::from_millis(250),
    );
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(busy(&command_id, "busy")),
        base + std::time::Duration::from_millis(250),
    );
    let _ = live_pass(
        &mut driver,
        &mut model,
        None,
        base + std::time::Duration::from_millis(500),
    );
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(busy(&command_id, "still busy")),
        base + std::time::Duration::from_millis(500),
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text.contains("· busy retry bound exhausted")),
        "{:?}",
        model.flash
    );

    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 130, 32);
    let buffer = terminal.backend().buffer();
    // The banner (row 0) wears the err register.
    let banner = &rows[0];
    assert!(banner.contains("✗ Command still busy"), "{banner:?}");
    assert_eq!(buffer[(col_of(banner, "▏"), 0)].fg, ink(theme.err));
    // The transcript card below: bold err title, err rail, dim detail,
    // fact line carrying the subcode and the actions hint.
    let title_y = row_of_from(&rows, "✗ Command still busy", 1);
    let title_x = col_of(&rows[title_y as usize], "Command still busy");
    assert_eq!(buffer[(title_x, title_y)].fg, ink(theme.err));
    assert!(buffer[(title_x, title_y)].modifier.contains(Modifier::BOLD));
    let detail_y = row_of_from(&rows, "busy retry bound exhausted after 3 attempts", 1);
    let detail_row = &rows[detail_y as usize];
    assert_eq!(
        buffer[(col_of(detail_row, "▏"), detail_y)].fg,
        ink(theme.err),
        "the severity rail is the err accent"
    );
    assert_eq!(
        buffer[(col_of(detail_row, "busy retry bound"), detail_y)].fg,
        ink(theme.dim),
        "detail is dim"
    );
    let fact_y = row_of_from(&rows, "busy-retry-exhausted · actions: retry", 1);
    assert_eq!(
        buffer[(col_of(&rows[fact_y as usize], "busy-retry"), fact_y)].fg,
        ink(theme.dim)
    );
}

/// QUIET RETRY FACTS (markers): `tool_json_repair` composes its human
/// sentence from the typed data and `provider_tool_fallback` keeps the
/// daemon's label — both as dim ⟳ rows with no warn/err ink; an unknown
/// extension kind keeps the generic faint ⋯. Plain mode speaks the same
/// sentences. MUTATION: drop `retry_marker_label` (or route it back to
/// the faint ⋯ arm) and the glyph/ink/parity assertions fail.
#[test]
fn e8d_retry_markers_read_as_quiet_dim_fact_lines() {
    let repair = TurnItem::Extension {
        kind: "tool_json_repair".into(),
        data: serde_json::json!({
            "attempt": 1,
            "max_attempts": 1,
            "call_id": "c-1",
            "tool": "fs_edit",
        }),
    };
    let fallback = TurnItem::Extension {
        kind: "provider_tool_fallback".into(),
        data: serde_json::json!({
            "label": "provider hosted web tool rejected — using local web_fetch",
            "attempt": 1,
            "max_attempts": 1,
        }),
    };
    let mystery = TurnItem::Extension {
        kind: "mystery_marker".into(),
        data: serde_json::json!({}),
    };
    let mut model = live_session();
    for (seq, (id, item)) in [
        ("x-repair", repair.clone()),
        ("x-fallback", fallback.clone()),
        ("x-mystery", mystery.clone()),
    ]
    .into_iter()
    .enumerate()
    {
        model.route_raw(&raw(
            seq as u64 + 1,
            1_000,
            &EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(id),
                item,
            }),
        ));
    }
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 130, 32);
    let buffer = terminal.backend().buffer();
    let repair_line = "⟳ malformed fs_edit arguments — model asked to reissue (attempt 1/1)";
    let fallback_line = "⟳ provider hosted web tool rejected — using local web_fetch";
    for needle in [repair_line, fallback_line] {
        let y = row_of(&rows, needle);
        let row = &rows[y as usize];
        assert_eq!(
            buffer[(col_of(row, "⟳"), y)].fg,
            ink(theme.dim),
            "a bounded retry is a quiet dim fact"
        );
        for x in 0..buffer.area.width {
            assert_ne!(buffer[(x, y)].fg, ink(theme.err), "never alarming: {row:?}");
            assert_ne!(
                buffer[(x, y)].fg,
                ink(theme.warn),
                "never alarming: {row:?}"
            );
        }
    }
    // Unknown kinds keep the generic faint ⋯ treatment.
    let mystery_y = row_of(&rows, "⋯ mystery_marker");
    assert_eq!(
        buffer[(
            col_of(&rows[mystery_y as usize], "⋯ mystery_marker"),
            mystery_y
        )]
            .fg,
        ink(theme.faint)
    );

    // Plain parity: the same sentences behind the same glyphs.
    let mut projection = SessionProjection::default();
    for (id, item) in [
        ("x-repair", repair),
        ("x-fallback", fallback),
        ("x-mystery", mystery),
    ] {
        projection.apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(id),
            item,
        }));
    }
    let rendered = render_plain(&projection, 0, None);
    assert!(
        rendered.contains("⟳ malformed fs_edit arguments — model asked to reissue (attempt 1/1)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("⟳ provider hosted web tool rejected — using local web_fetch"),
        "{rendered}"
    );
    assert!(rendered.contains("⋯ mystery_marker"), "{rendered}");
}

/// QUIET RETRY FACTS (tool rows): a reason on a COMPLETED tool row is a
/// recovered in-flight retry — dim metadata beside the green ✓, never the
/// warn or err register; a FAILED row's reason keeps the err ink.
/// MUTATION: restore the warn styling on completed reasons and the
/// per-cell ink assertions fail.
#[test]
fn e8e_web_fetch_retry_note_on_completed_row_is_dim() {
    let mut model = live_session();
    let apply_tool = |model: &mut AppModel,
                      seq: u64,
                      id: &str,
                      call: &str,
                      status: ToolResultStatus,
                      reason: &str| {
        model.route_raw(&raw(
            seq,
            1_000,
            &EventPayload::Item(ItemEvent::Started {
                item_id: ItemId::new(id),
                item: TurnItem::ToolCall {
                    call_id: call.into(),
                    name: "web_fetch".into(),
                    args: serde_json::json!({}),
                    status: haider_protocol::item::ToolStatus::InProgress,
                },
            }),
        ));
        model.route_raw(&raw(
            seq + 1,
            1_000,
            &EventPayload::ToolResult {
                call_id: call.into(),
                result: BoundedResult {
                    preview: "{}".into(),
                    truncated: false,
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status,
                    reason: Some(reason.into()),
                    presentation: None,
                },
            },
        ));
        model.route_raw(&raw(
            seq + 2,
            1_000,
            &EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(id),
                item: TurnItem::ToolCall {
                    call_id: call.into(),
                    name: "web_fetch".into(),
                    args: serde_json::json!({}),
                    status: status.item_status(),
                },
            }),
        ));
    };
    apply_tool(
        &mut model,
        1,
        "i-recovered",
        "c-recovered",
        ToolResultStatus::Completed,
        "transient web_fetch failure — retry 2/2 succeeded",
    );
    apply_tool(
        &mut model,
        4,
        "i-exhausted",
        "c-exhausted",
        ToolResultStatus::Failed,
        "retry 2/2 exhausted — web_fetch returned HTTP 503",
    );
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 130, 32);
    let buffer = terminal.backend().buffer();
    let ok_y = row_of(&rows, "transient web_fetch failure — retry 2/2 succeeded");
    let ok_row = &rows[ok_y as usize];
    assert_eq!(
        buffer[(col_of(ok_row, "transient web_fetch"), ok_y)].fg,
        ink(theme.dim),
        "a recovered retry is quiet metadata"
    );
    for x in 0..buffer.area.width {
        assert_ne!(buffer[(x, ok_y)].fg, ink(theme.warn), "{ok_row:?}");
        assert_ne!(buffer[(x, ok_y)].fg, ink(theme.err), "{ok_row:?}");
    }
    let err_y = row_of(&rows, "retry 2/2 exhausted");
    assert_eq!(
        buffer[(col_of(&rows[err_y as usize], "retry 2/2 exhausted"), err_y)].fg,
        ink(theme.err),
        "an exhausted retry keeps the err register"
    );
}

/// LOCAL-ONLY LINE: `/peers` produces one dim matter-of-fact status flash
/// — the typed admission message — and NO banner row above the frame.
/// MUTATION: latch the rejection into `command_diagnostic` again and the
/// no-banner assertion fails.
#[test]
fn e8f_local_only_rejection_is_one_quiet_status_line() {
    let mut model = launcher_model();
    common::submit(&mut model, "/peers");
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 110, 32);
    let buffer = terminal.backend().buffer();
    assert!(
        !rows[0].contains('▏'),
        "no persistent banner for a matter-of-fact rejection: {:?}",
        rows[0]
    );
    let flash = "· /peers — not supported — Haider runs local-only";
    let y = row_of(&rows, flash);
    assert_eq!(
        buffer[(col_of(&rows[y as usize], flash), y)].fg,
        ink(theme.dim),
        "the flash is the quiet status register"
    );
    for x in 0..buffer.area.width {
        assert_ne!(
            buffer[(x, y)].fg,
            ink(theme.err),
            "matter-of-fact, never an error tone"
        );
    }
}
