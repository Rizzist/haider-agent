//! E5 — the E2-E4 error waves' VISUAL pass: the typed presentation's
//! designed treatment, pinned in the u2/s4 register.
//!
//! Laws under test:
//!
//! * FACT-LINE LAW: the compact fact line composes display-ordered
//!   (`subcode · HTTP 429 · req 8f3a2c1d… · resets in 2m 14s`), the reset
//!   human-readable in the ONE h/m/s vocabulary; under width pressure it
//!   sheds WHOLE segments by pinned rank (req → HTTP → actions → resets;
//!   the subcode never sheds) — never a mid-word truncation;
//! * INCOMPLETE-MARKER TONE: `⚠ incomplete — stream interrupted (…)` is a
//!   dim-warning marker (warn glyph, dim text) — interruption is not
//!   failure, so NO cell of the marker row wears the err ink, and the
//!   partial body above keeps its normal text ink;
//! * RECOVERY-CARD CHROME: the card wears a severity-toned `▏` accent and
//!   a BOLD tone-ink title (calm warn for a rate limit — never scary
//!   red), its fact line counts down LIVE on the daemon clock, and the
//!   server's FIRST option is the visually distinct primary affordance
//!   (gold when unselected); the daemon's baseline prose body ("Provider
//!   HTTP status: …") is never double-rendered beside the typed facts;
//! * PLAIN PARITY: plain mode shows the card's detail and fact line as
//!   honest text (static reset — plain has no clock to count against).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
use haider_protocol::ids::{DeviceId, EventId, ItemId, MenuId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::{ErrorRecoveryCardKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_tui::app::{AppModel, RuntimeMode};
use haider_tui::plain::render_plain;
use haider_tui::projection::{
    SessionProjection, error_fact_segments, error_fact_segments_with_actions,
};
use haider_tui::render::{render, shed_fact_line};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier};

mod common;
use common::{key, launcher_model};

fn sid() -> SessionId {
    SessionId::new("e5-session")
}

fn raw(seq: u64, at_ms: u64, payload: &EventPayload) -> haider_protocol::envelope::RawEnvelope {
    haider_protocol::envelope::EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-e5-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("e5-device"),
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
        payload: serde_json::to_value(payload)
            .expect("payload serializes")
            .into(),
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

/// The full-fact presentation every law here exercises: HTTP 429,
/// a 16-char request id, and a 2m 14s provider delay stamped at daemon
/// time 1 000 000 ms (so `reset_at_ms` = 1 134 000).
fn rate_limit_presentation() -> ErrorPresentation {
    ErrorPresentation::new(
        "rate-limited",
        "Provider rate limit reached",
        "Wait for the provider limit to reset, then retry.",
        ErrorScope::Account,
        [ErrorAction::Wait, ErrorAction::Retry],
    )
    .with_http_status(429)
    .with_request_id(Some("8f3a2c1d9b7e5a42"))
    .with_retry_after(Some(134_000), 1_000_000)
}

/// The daemon's rate-limit card shape (actor.rs `recovery_menu`),
/// baseline prose body included — the TUI must render the TYPED facts
/// and never double-render the prose.
fn rate_limit_menu() -> Menu {
    let presentation = rate_limit_presentation();
    Menu {
        id: MenuId::new("e5-rate-limit"),
        kind: MenuKind::ErrorRecovery {
            card: ErrorRecoveryCardKind::RateLimit,
            presentation: presentation.clone(),
            option_actions: vec![ErrorAction::Wait, ErrorAction::Retry],
            provider: Some("anthropic".into()),
            account: None,
            source_run: None,
            source_item: None,
        },
        title: presentation.title.clone(),
        body: vec![
            presentation.detail.clone(),
            "Provider HTTP status: 429".into(),
            "Request ID: 8f3a2c1d9b7e5a42".into(),
            "Retry countdown: 134s (reset at Unix time 1134000 ms).".into(),
        ],
        options: vec![
            MenuOption {
                key: "wait".into(),
                label: "Wait".into(),
                detail: Some("Wait until the displayed reset time before retrying.".into()),
                decision: None,
            },
            MenuOption {
                key: "retry".into(),
                label: "Retry".into(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "error-recovery".into(),
        ttl_ms: None,
        timeout_option: None,
    }
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
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
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

/// FACT-LINE LAW: composition is display-ordered with the human-readable
/// reset, and width pressure sheds WHOLE segments in the pinned rank
/// order — req id first, then HTTP, then the actions hint, then the
/// reset; the subcode survives any budget, and no budget ever yields a
/// mid-segment cut. MUTATION: truncate instead of shed (or reorder the
/// ranks) and the subset/order assertions below fail.
#[test]
fn e5a_fact_line_composes_and_sheds_whole_segments() {
    let presentation = rate_limit_presentation();
    let segments = error_fact_segments_with_actions(&presentation, None);
    let full = shed_fact_line(&segments, 500);
    assert_eq!(
        full, "rate-limited · HTTP 429 · req 8f3a2c1d… · resets in 2m 14s · actions: wait, retry",
        "display order + short req + h/m/s reset"
    );
    // The LIVE form counts against the supplied daemon clock.
    let live = error_fact_segments(&presentation, Some(1_060_000));
    assert_eq!(
        shed_fact_line(&live, 500),
        "rate-limited · HTTP 429 · req 8f3a2c1d… · resets in 1m 14s"
    );
    // Shedding: every narrower budget drops WHOLE segments, rank order.
    let originals: Vec<&str> = segments.iter().map(|(s, _)| s.as_str()).collect();
    let mut seen = Vec::new();
    for budget in (0..=full.chars().count()).rev() {
        let shed = shed_fact_line(&segments, budget);
        for piece in shed.split(" · ") {
            assert!(
                originals.contains(&piece),
                "budget {budget} produced a cut segment: {piece:?}"
            );
        }
        if !seen.contains(&shed) {
            seen.push(shed);
        }
    }
    assert_eq!(
        seen,
        vec![
            "rate-limited · HTTP 429 · req 8f3a2c1d… · resets in 2m 14s · actions: wait, retry"
                .to_owned(),
            // req (rank 4) sheds first…
            "rate-limited · HTTP 429 · resets in 2m 14s · actions: wait, retry".to_owned(),
            // …then HTTP (rank 3)…
            "rate-limited · resets in 2m 14s · actions: wait, retry".to_owned(),
            // …then the actions hint (rank 2)…
            "rate-limited · resets in 2m 14s".to_owned(),
            // …then the reset (rank 1); the subcode NEVER sheds.
            "rate-limited".to_owned(),
        ],
        "whole segments, pinned rank order, subcode floor"
    );
}

/// INCOMPLETE-MARKER TONE: warn glyph, dim text, no err ink anywhere on
/// the marker row; the partial body keeps the normal text ink.
/// MUTATION: restore the all-warn (or any err) marker and the per-cell
/// ink assertions fail.
#[test]
fn e5b_incomplete_marker_wears_dim_warning_never_err() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        0,
        &EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("partial"),
            item: TurnItem::IncompleteAgentMessage {
                text: "partial answer".into(),
                interruption: ErrorPresentation::new(
                    "stream-interrupted",
                    "Response stream interrupted",
                    "The provider connection ended after content.",
                    ErrorScope::Turn,
                    [ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
                ),
            },
        }),
    ));
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 110, 32);
    let marker = "⚠ incomplete — stream interrupted (stream-interrupted)";
    let y = row_of(&rows, marker);
    let row = &rows[y as usize];
    let buffer = terminal.backend().buffer();
    let glyph_x = col_of(row, "⚠");
    assert_eq!(
        buffer[(glyph_x, y)].fg,
        ink(theme.warn),
        "the ⚠ accent is the warn ink"
    );
    let text_x = col_of(row, "incomplete —");
    assert_eq!(
        buffer[(text_x, y)].fg,
        ink(theme.dim),
        "the marker text is dim metadata"
    );
    for x in 0..buffer.area.width {
        assert_ne!(
            buffer[(x, y)].fg,
            ink(theme.err),
            "interruption is not failure — no err ink on the marker row"
        );
    }
    let body_y = row_of(&rows, "partial answer");
    let body_x = col_of(&rows[body_y as usize], "partial answer");
    assert_eq!(
        buffer[(body_x, body_y)].fg,
        ink(theme.text),
        "the partial body keeps normal styling"
    );
}

/// RECOVERY-CARD CHROME: ▏ warn accent + BOLD warn title behind the ⟳
/// renewal glyph, typed fact line counting down on the daemon clock, the
/// baseline prose body absent, the selected option explained by its typed
/// detail, and the FIRST (primary) option gold once unselected.
/// MUTATION: drop the recovery branch in `menu_block`/`wrapped_menu_body`
/// and the accent/fact/countdown assertions fail against the baseline
/// dim-prose card.
#[test]
fn e5c_recovery_card_accent_facts_countdown_and_primary_affordance() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        1_000_000,
        &EventPayload::MenuOpened(rate_limit_menu()),
    ));
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 110, 32);
    let buffer = terminal.backend().buffer();
    // Title: ⟳ glyph (a limit renews — not a warning triangle), warn ink,
    // BOLD, behind the ▏ severity accent.
    let title_y = row_of(&rows, "⟳ Provider rate limit reached");
    let title_row = &rows[title_y as usize];
    let accent_x = col_of(title_row, "▏");
    assert_eq!(buffer[(accent_x, title_y)].fg, ink(theme.warn));
    let title_x = col_of(title_row, "Provider rate limit");
    assert_eq!(buffer[(title_x, title_y)].fg, ink(theme.warn));
    assert!(
        buffer[(title_x, title_y)].modifier.contains(Modifier::BOLD),
        "TITLE prominent"
    );
    // The typed fact line, LIVE against committed_at_ms 1 000 000.
    let fact = "rate-limited · HTTP 429 · req 8f3a2c1d… · resets in 2m 14s";
    let fact_y = row_of(&rows, fact);
    let fact_x = col_of(&rows[fact_y as usize], "rate-limited");
    assert_eq!(
        buffer[(fact_x, fact_y)].fg,
        ink(theme.dim),
        "facts are muted metadata"
    );
    // The daemon's baseline prose body is not double-rendered.
    assert!(
        !rows.iter().any(|row| row.contains("Provider HTTP status")),
        "typed facts replace the baseline prose"
    );
    // The selected primary option explains itself with its typed detail.
    row_of(
        &rows,
        "1. Wait — Wait until the displayed reset time before retrying.",
    );
    // A later envelope ticks the countdown — same card, fresher clock.
    model.route_raw(&raw(
        2,
        1_060_000,
        &EventPayload::RunState(haider_protocol::state::RunState::Thinking),
    ));
    let (rows, _) = draw(&model, 110, 32);
    row_of(&rows, "resets in 1m 14s");
    // Affordance ordering: move the cursor off the primary — the FIRST
    // (server-recommended) option keeps a distinct gold affordance.
    model.handle(key(KeyCode::Down));
    let (rows, terminal) = draw(&model, 110, 32);
    let buffer = terminal.backend().buffer();
    let wait_y = row_of(&rows, "1. Wait");
    let wait_x = col_of(&rows[wait_y as usize], "1. Wait");
    assert_eq!(
        buffer[(wait_x, wait_y)].fg,
        ink(theme.gold),
        "the unselected primary wears the gold affordance"
    );
    let retry_y = row_of(&rows, "2. Retry");
    let retry_x = col_of(&rows[retry_y as usize], "2. Retry");
    assert_eq!(
        buffer[(retry_x, retry_y)].fg,
        ink(theme.bright),
        "the selected row keeps selection styling"
    );
}

/// PLAIN PARITY: the recovery card's detail and fact line reach plain
/// mode as honest text (static reset — no clock in a pipe), above the
/// numbered options.
#[test]
fn e5d_plain_recovery_card_carries_detail_and_facts() {
    let mut projection = SessionProjection::default();
    projection.apply(&EventPayload::MenuOpened(rate_limit_menu()));
    let rendered = render_plain(&projection, 0, None);
    assert!(rendered.contains("? Provider rate limit reached"));
    assert!(rendered.contains("  Wait for the provider limit to reset, then retry."));
    assert!(
        rendered.contains("  rate-limited · HTTP 429 · req 8f3a2c1d… · resets in 2m 14s"),
        "the fact line in plain: {rendered}"
    );
    assert!(rendered.contains("  1. Wait"));
    assert!(rendered.contains("  2. Retry"));
}

/// The typed run failure's transcript block: err BOLD title, dim detail
/// behind the err rail, muted fact line with the actions hint — and the
/// flattened plain string carries the same facts (full request id).
#[test]
fn e5e_typed_run_failure_renders_the_card_shaped_block() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        1_000_000,
        &EventPayload::RunFailed {
            code: haider_protocol::error::ErrorCode::ProviderError,
            message: "raw body must not render".into(),
            retryable: true,
            presentation: Some(rate_limit_presentation()),
        },
    ));
    let theme = model.theme.theme();
    let (rows, terminal) = draw(&model, 110, 32);
    let buffer = terminal.backend().buffer();
    let title_y = row_of(&rows, "✗ Provider rate limit reached");
    let title_x = col_of(&rows[title_y as usize], "Provider rate limit");
    assert_eq!(buffer[(title_x, title_y)].fg, ink(theme.err));
    assert!(buffer[(title_x, title_y)].modifier.contains(Modifier::BOLD));
    let detail_y = row_of(&rows, "Wait for the provider limit to reset");
    let detail_row = &rows[detail_y as usize];
    assert_eq!(
        buffer[(col_of(detail_row, "▏"), detail_y)].fg,
        ink(theme.err),
        "the severity rail is the err accent"
    );
    assert_eq!(
        buffer[(col_of(detail_row, "Wait for"), detail_y)].fg,
        ink(theme.dim),
        "detail is dim"
    );
    let fact_y = row_of(
        &rows,
        "rate-limited · HTTP 429 · req 8f3a2c1d… · resets in 2m 14s · actions: wait, retry",
    );
    assert_eq!(
        buffer[(col_of(&rows[fact_y as usize], "rate-limited"), fact_y)].fg,
        ink(theme.dim)
    );
    // Plain parity: the flattened string keeps the FULL request id.
    let rendered = render_plain(&model.projection, 0, None);
    assert!(
        rendered.contains(
            "✗ Provider rate limit reached — Wait for the provider limit to reset, then retry. \
             [rate-limited] · HTTP 429 · req 8f3a2c1d9b7e5a42 · resets in 2m 14s · actions: wait, retry"
        ),
        "{rendered}"
    );
}
