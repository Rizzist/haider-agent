//! U2 — the `/usage` screen: U1's `usage.report` snapshot rendered as
//! cross-provider limit bars, honest unavailability, API-key token/cost
//! counters, and local journal stats.
//!
//! Owner contract: OAuth accounts wear limit BARS with % + reset times per
//! window; API-key accounts show tokens + est cost and NEVER a 0-100
//! meter; unavailable meters render the typed reason honestly — never a
//! fabricated bar; identities render MASKED by default (streamer-friendly)
//! with a per-visit reveal; `/usage <provider>` filters; the screen rides
//! F2b's scroll discipline and F2a's key-ownership law (esc closes, ⏎ is
//! never hijacked).
#![allow(clippy::expect_used)]

use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1, UsageWindowV1,
};
use haider_tui::app::{AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::commands::{COMMANDS, has_arg_slots, offers_arg_completions, palette_items};
use haider_tui::format::{
    USAGE_BAR_CELLS, UsageTone, fmt_pct, fmt_reset, mask_identity, usage_bar, usage_tone,
};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

mod common;
use common::{key, launcher_model, run_slash};

const GENERATED_AT_MS: u64 = 1_000_000_000_000;

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, hits)
}

/// The BODY rows only — the status bar (whose token meter legitimately
/// wears `▰▱` + `%` on every screen) is excluded, so meter-glyph
/// assertions bind to the usage report alone.
fn body_text(rows: &[String]) -> String {
    rows[..rows.len().saturating_sub(1)].join("\n")
}

fn stats(sessions: u64, est_cost_usd: Option<f64>) -> LocalUsageStatsV1 {
    LocalUsageStatsV1 {
        sessions,
        total_duration_ms: 13_320_000, // 3h 42m 0s
        input_tokens: 1_200_000,
        output_tokens: 340_000,
        reasoning_tokens: 12_000,
        cached_tokens: 800_000,
        est_cost_usd,
        lines_added: 1240,
        lines_removed: 380,
    }
}

/// The U1 wire shapes CONSUMED verbatim: a two-account OAuth provider
/// (metered + unavailable), an API-key local-only provider.
fn report() -> UsageReportV1 {
    UsageReportV1 {
        generated_at_ms: GENERATED_AT_MS,
        accounts: vec![
            AccountUsageReportV1 {
                provider: "anthropic-oauth".to_owned(),
                alias: CredentialAlias::new("anthropic"),
                identity: Some("max@example.com".to_owned()),
                plan: Some("max_20x".to_owned()),
                auth_method: AuthMethod::OAuth,
                meter: AccountMeterStateV1::Metered {
                    windows: vec![
                        UsageWindowV1 {
                            window: "five_hour".to_owned(),
                            utilization: 0.83,
                            resets_at_ms: Some(GENERATED_AT_MS + 8_070_000), // 2h 14m 30s
                            label: None,
                        },
                        UsageWindowV1 {
                            window: "seven_day".to_owned(),
                            utilization: 0.17,
                            resets_at_ms: Some(GENERATED_AT_MS + 442_800_999), // 5d 3h
                            label: None,
                        },
                    ],
                },
                local: stats(12, Some(4.12)),
            },
            AccountUsageReportV1 {
                provider: "anthropic-oauth".to_owned(),
                alias: CredentialAlias::new("anthropic2"),
                identity: Some("work@corp.dev".to_owned()),
                plan: None,
                auth_method: AuthMethod::OAuth,
                meter: AccountMeterStateV1::Unavailable {
                    reason: "http 401 — token expired".to_owned(),
                },
                local: stats(1, None),
            },
            AccountUsageReportV1 {
                provider: "openai".to_owned(),
                alias: CredentialAlias::new("openai"),
                identity: Some("dev@corp.dev".to_owned()),
                plan: None,
                auth_method: AuthMethod::ApiKey,
                meter: AccountMeterStateV1::LocalOnly,
                local: stats(3, Some(0.57)),
            },
        ],
    }
}

/// A demo-mode model already ON the usage screen with the report
/// installed — rendering laws exercise the same reducer seams as live
/// (`apply_report` is the one writer in both modes).
fn usage_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/usage");
    assert_eq!(model.screen, Screen::Usage, "the screen opens");
    model.usage.apply_report(report());
    model
}

fn live_gated_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_USAGE_REPORT_V1.to_owned());
    model
}

// ---- bar math + formatting laws --------------------------------------

/// BAR-MATH LAW: 0–1 → cells with clamping and floor-fill, plus the two
/// honesty clamps (nonzero shows ≥ 1 cell, sub-1.0 never reads full).
///
/// MUTATION CHECK (executed): replace the floor fill with `.round()` in
/// `usage_bar` — 0.96 rounds up to a full 10-cell bar and the
/// `not-yet-exhausted` assertion fails.
#[test]
fn usage_bar_math_clamps_and_floors() {
    assert_eq!(usage_bar(0.83, 10), "▰▰▰▰▰▰▰▰▱▱", "0.83 floors to 8 cells");
    assert_eq!(usage_bar(0.0, 10), "▱▱▱▱▱▱▱▱▱▱", "untouched renders empty");
    assert_eq!(usage_bar(1.0, 10), "▰▰▰▰▰▰▰▰▰▰", "exhausted renders full");
    assert_eq!(usage_bar(2.5, 10), "▰▰▰▰▰▰▰▰▰▰", "over-range clamps to 1.0");
    assert_eq!(usage_bar(-0.5, 10), "▱▱▱▱▱▱▱▱▱▱", "negatives clamp to 0.0");
    assert_eq!(
        usage_bar(0.96, 10),
        "▰▰▰▰▰▰▰▰▰▱",
        "a not-yet-exhausted window never reads full"
    );
    assert_eq!(
        usage_bar(0.87, 10),
        "▰▰▰▰▰▰▰▰▱▱",
        "FLOOR, never round: 0.87 stays at 8 cells (rounding would claim 9)"
    );
    assert_eq!(
        usage_bar(0.001, 10),
        "▰▱▱▱▱▱▱▱▱▱",
        "any nonzero utilization shows at least one cell"
    );
    assert_eq!(USAGE_BAR_CELLS, 10, "the screen's bar width is pinned");
    assert_eq!(fmt_pct(0.83), "83%");
    assert_eq!(fmt_pct(0.005), "1%", "percent rounds half-up");
    assert_eq!(fmt_pct(7.5), "100%", "percent clamps to 100");
    assert_eq!(fmt_pct(-1.0), "0%", "percent clamps to 0");
}

/// RESET-TIME LAW: `resets soon` under a minute (or elapsed), `{m}m`
/// under an hour, `{h}h {m}m` under a day, `{d}d {h}h` from a day up —
/// minutes always floored.
///
/// MUTATION CHECK (executed): drop the `saturating_sub` (compute
/// `resets - generated` with wrapping semantics inverted) or round
/// minutes up — the exact-string asserts fail.
#[test]
fn reset_times_format_by_tier() {
    let now = GENERATED_AT_MS;
    assert_eq!(fmt_reset(now, now), "resets soon", "the elapsed instant");
    assert_eq!(fmt_reset(now, now - 5_000), "resets soon", "a past reset");
    assert_eq!(fmt_reset(now, now + 59_999), "resets soon", "sub-minute");
    assert_eq!(fmt_reset(now, now + 34 * 60_000), "resets in 34m");
    assert_eq!(
        fmt_reset(now, now + 8_070_000),
        "resets in 2h 14m",
        "2h 14m 30s floors the seconds"
    );
    assert_eq!(
        fmt_reset(now, now + 442_800_999),
        "resets in 5d 3h",
        "day tier drops minutes"
    );
}

/// THRESHOLD LAW: the bar's ink tone flips calm→warn at 0.70 and
/// warn→err at 0.90 (theme slots only — the renderer maps tones).
#[test]
fn usage_tone_thresholds_are_pinned() {
    assert_eq!(usage_tone(0.0), UsageTone::Ok);
    assert_eq!(usage_tone(0.6999), UsageTone::Ok);
    assert_eq!(usage_tone(0.70), UsageTone::Warn);
    assert_eq!(usage_tone(0.8999), UsageTone::Warn);
    assert_eq!(usage_tone(0.90), UsageTone::Err);
    assert_eq!(usage_tone(5.0), UsageTone::Err, "clamped before comparing");
}

/// MASK LAW (owner addendum): first character of the local part and of
/// the domain survive, every other character masks length-preserving, the
/// final `.tld` stays readable — and the masked form never contains the
/// full local part.
///
/// MUTATION CHECK (executed): make `mask_identity` return its input
/// verbatim — the masked-form asserts and the never-leaks assert fail.
#[test]
fn identity_masking_keeps_first_chars_and_tld_only() {
    assert_eq!(
        mask_identity("support@diffforge.ai"),
        "s******@d********.ai"
    );
    assert_eq!(mask_identity("max@example.com"), "m**@e******.com");
    assert!(
        !mask_identity("max@example.com").contains("max"),
        "the full local part never survives the mask"
    );
    assert_eq!(
        mask_identity("handle"),
        "h*****",
        "non-emails mask as one part"
    );
    assert_eq!(
        mask_identity("a@b"),
        "a@b",
        "single-char parts have nothing to hide"
    );
    assert_eq!(mask_identity(""), "", "empty stays empty");
}

// ---- per-state rendering ---------------------------------------------

/// A `metered` OAuth account renders one bar per window with % and reset
/// time, plus the (masked) identity · plan · auth-flavor line.
#[test]
fn metered_accounts_render_bars_percent_and_resets() {
    let model = usage_model();
    let (rows, _) = draw(&model, 110, 30);
    let text = rows.join("\n");
    assert!(
        text.contains("anthropic-oauth"),
        "the provider header renders"
    );
    assert!(text.contains("five_hour"), "window names render");
    assert!(
        text.contains("▰▰▰▰▰▰▰▰▱▱"),
        "0.83 renders the floor-filled 8/10 bar"
    );
    assert!(text.contains("83%"), "the percent label renders");
    assert!(text.contains("resets in 2h 14m"), "the reset time renders");
    assert!(text.contains("seven_day"), "every window renders");
    assert!(text.contains("resets in 5d 3h"), "the weekly reset renders");
    assert!(
        text.contains("max_20x"),
        "the plan renders (unmasked — not sensitive)"
    );
    assert!(text.contains("oauth"), "the auth flavor renders");
    assert!(
        text.contains("est $4.12"),
        "the local stats carry the cost estimate"
    );
    assert!(text.contains("3h 42m"), "the local duration renders");
    assert!(text.contains("+1240 −380 lines"), "LOC renders");
    assert!(text.contains("in 1.2M"), "token splits render");
}

/// An `unavailable` meter renders its typed reason and NEVER a bar — no
/// meter glyph may appear anywhere in that account's block.
///
/// MUTATION CHECK (executed): make the renderer's `Unavailable` arm fall
/// through to a zeroed bar — the no-glyph assert fails.
#[test]
fn unavailable_meters_render_the_typed_reason_never_a_bar() {
    let mut model = usage_model();
    // ← the second anthropic account (the unavailable one) via →.
    model.handle(key(KeyCode::Right));
    let (rows, _) = draw(&model, 110, 30);
    let text = body_text(&rows);
    assert!(
        text.contains("meter unavailable — http 401 — token expired"),
        "the typed reason renders honestly"
    );
    // The anthropic block now shows NO bar; openai is local-only; so the
    // whole report body must carry no meter glyph at all.
    assert!(
        !text.contains('▰') && !text.contains('▱'),
        "an unavailable meter never fabricates a bar"
    );
}

/// An API-key account renders tokens + est cost and NEVER a 0–100 meter:
/// no bar glyphs, no percent, an explicit no-server-meter note.
#[test]
fn api_key_accounts_render_tokens_and_cost_never_a_meter() {
    let mut model = usage_model();
    model.usage.filter = Some("openai".to_owned());
    let (rows, _) = draw(&model, 110, 30);
    let text = body_text(&rows);
    assert!(
        text.contains("api key — no provider meter"),
        "the local-only state says so"
    );
    assert!(text.contains("est $0.57"), "est cost renders");
    assert!(text.contains("in 1.2M"), "token splits render");
    assert!(
        !text.contains('▰') && !text.contains('▱') && !text.contains('%'),
        "an api-key account never wears a 0-100 meter"
    );
}

/// Absent cost estimates render an honest `est —`, never $0.00.
#[test]
fn missing_cost_estimates_render_a_dash_never_zero() {
    let mut model = usage_model();
    model.handle(key(KeyCode::Right));
    let (rows, _) = draw(&model, 110, 30);
    let text = rows.join("\n");
    assert!(
        text.contains("est —"),
        "the unpriced account's stats say est — honestly"
    );
}

// ---- masking ----------------------------------------------------------

/// Identities are MASKED by default on open, `r` reveals for the current
/// visit only, and closing the screen restores the mask.
///
/// MUTATION CHECK (executed): drop the `revealed = false` reset from
/// `enter_usage` — the reopened screen still shows the raw email and the
/// final assert fails.
#[test]
fn identities_render_masked_by_default_and_reveal_is_per_visit() {
    let mut model = usage_model();
    let (rows, _) = draw(&model, 110, 30);
    let text = rows.join("\n");
    assert!(
        !text.contains("max@example.com"),
        "the raw email never renders on open"
    );
    assert!(text.contains("m**@e******.com"), "the masked form renders");
    // r reveals for THIS visit.
    model.handle(key(KeyCode::Char('r')));
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("max@example.com"),
        "r reveals the identity"
    );
    // esc closes; reopening starts masked again.
    model.handle(key(KeyCode::Esc));
    assert_ne!(model.screen, Screen::Usage, "esc closes the screen");
    run_slash(&mut model, "/usage");
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        !rows.join("\n").contains("max@example.com"),
        "a new visit always opens masked"
    );
    // Sub-Escape-Lane (survivor closed): a reveal must not survive an
    // exit that BYPASSES `exit_usage` either — ⌃C walks straight to the
    // launcher, so only the one-door reset in `enter_usage` can restore
    // the mask for the next visit.
    model.handle(key(KeyCode::Char('r')));
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("max@example.com"),
        "revealed again (precondition)"
    );
    model.handle(haider_tui::app::AppEvent::Key(
        ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        ),
    ));
    assert_eq!(
        model.screen,
        Screen::Launcher,
        "⌃C leaves without exit_usage"
    );
    run_slash(&mut model, "/usage");
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        !rows.join("\n").contains("max@example.com"),
        "the visit after a ⌃C exit STILL opens masked"
    );
}

// ---- filter -----------------------------------------------------------

/// `/usage <provider>` prefix-filters the groups; an unknown filter says
/// so honestly; bare `/usage` clears.
#[test]
fn usage_filter_shows_only_the_named_provider() {
    // The usage screen owns its keys, so each re-run walks back to the
    // launcher first (the report survives the round trip — display
    // state, not a fetch).
    let mut model = usage_model();
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/usage anthropic");
    assert_eq!(model.usage.filter.as_deref(), Some("anthropic"));
    let (rows, _) = draw(&model, 110, 30);
    let text = body_text(&rows);
    assert!(text.contains("anthropic-oauth"), "the prefix match shows");
    assert!(
        !text.contains("openai"),
        "the filtered-out provider is absent"
    );
    // Unknown filter: an honest empty note, never an invented group.
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/usage nonesuch");
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("no accounts match \"nonesuch\""),
        "an unknown filter says so"
    );
    // PREFIX law: a mid-string fragment never matches (`oauth` is inside
    // `anthropic-oauth` but names no provider).
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/usage oauth");
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("no accounts match \"oauth\""),
        "the filter is a PREFIX, never a substring"
    );
    // Bare /usage clears the filter.
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/usage");
    assert_eq!(model.usage.filter, None, "bare /usage clears the filter");
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        body_text(&rows).contains("openai"),
        "everything shows again"
    );
}

// ---- keys: tabs, ownership, scroll ------------------------------------

/// ←/→ cycle the cursor group's accounts and WRAP; the selection only
/// moves within the same provider (the openai group has one account and
/// never cycles).
#[test]
fn left_right_tabs_cycle_the_cursor_groups_accounts_and_wrap() {
    let mut model = usage_model();
    let groups = model.usage.groups();
    assert_eq!(groups.len(), 2, "two provider groups");
    assert_eq!(model.usage.selected_tab(&groups[0]), 0);
    model.handle(key(KeyCode::Right));
    assert_eq!(
        model.usage.selected_tab(&model.usage.groups()[0]),
        1,
        "→ selects the second account"
    );
    model.handle(key(KeyCode::Right));
    assert_eq!(
        model.usage.selected_tab(&model.usage.groups()[0]),
        0,
        "→ wraps back to the first"
    );
    model.handle(key(KeyCode::Left));
    assert_eq!(
        model.usage.selected_tab(&model.usage.groups()[0]),
        1,
        "← wraps the other way"
    );
    // The single-account group never cycles.
    model.handle(key(KeyCode::Down));
    assert_eq!(model.usage.cursor, 1, "↓ moves the group cursor");
    model.handle(key(KeyCode::Right));
    assert_eq!(
        model.usage.selected_tab(&model.usage.groups()[1]),
        0,
        "a one-account group has nothing to cycle"
    );
    // A tab chip click selects exactly the account the rect carried.
    model.handle_hit(Hit::UsageAccountTab {
        provider: "anthropic-oauth".to_owned(),
        index: 0,
    });
    assert_eq!(
        model.usage.cursor, 0,
        "the click moves the cursor to its group"
    );
    assert_eq!(model.usage.selected_tab(&model.usage.groups()[0]), 0);
}

/// KEY-OWNERSHIP LAW: while `/usage` shows, esc closes back to the prior
/// surface and ⏎ does NOTHING (no hijack — the screen is read-only);
/// printable keys are swallowed, never a composer echo.
#[test]
fn usage_owns_its_keys_esc_closes_and_enter_never_hijacks() {
    let mut model = usage_model();
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Usage, "⏎ neither closes nor acts");
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.screen, Screen::Usage, "stray keys are swallowed");
    assert!(
        model.composer.text().is_empty(),
        "swallowed keys never reach a composer"
    );
    model.handle(key(KeyCode::Esc));
    assert_eq!(
        model.screen,
        Screen::Launcher,
        "esc walks back (no session)"
    );
}

/// F2b SCROLL-REACHABILITY: a report taller than the frame reaches its
/// last line via End, pages by 8, wheels by 3, and clamps at the
/// frame-written max.
///
/// MUTATION CHECK (executed): pin the render's scroll application to zero
/// (drop the `.scroll` on the Paragraph) — the End assertion fails.
#[test]
fn long_reports_scroll_to_reach_every_line() {
    let mut model = usage_model();
    let mut long = report();
    let template = long.accounts[2].clone();
    for index in 0..10 {
        let mut synthetic = template.clone();
        synthetic.provider = format!("synth-{index:02}");
        synthetic.alias = CredentialAlias::new(format!("synth{index}"));
        long.accounts.push(synthetic);
    }
    model.usage.apply_report(long);
    let (rows, _) = draw(&model, 100, 24);
    let text = rows.join("\n");
    assert!(text.contains("USAGE"), "the head renders first");
    assert!(
        !text.contains("synth-09"),
        "the tail overflows a 24-row frame (precondition)"
    );
    model.handle(key(KeyCode::End));
    let (rows, _) = draw(&model, 100, 24);
    assert!(
        rows.join("\n").contains("synth-09"),
        "End reaches the last provider block"
    );
    model.handle(key(KeyCode::Home));
    let (rows, _) = draw(&model, 100, 24);
    assert!(rows.join("\n").contains("USAGE"), "Home restores the head");
    model.handle(key(KeyCode::PageDown));
    assert_eq!(model.usage.scroll.get(), 8, "PageDown steps by 8");
    model.handle_wheel(false);
    assert_eq!(model.usage.scroll.get(), 11, "wheel steps by 3");
    model.handle_wheel(true);
    assert_eq!(model.usage.scroll.get(), 8, "wheel up steps back");
    for _ in 0..50 {
        model.handle(key(KeyCode::PageDown));
    }
    draw(&model, 100, 24);
    let max = model.usage.scroll_max.get();
    assert!(max > 0, "the long report really overflows");
    assert_eq!(
        model.usage.scroll.get(),
        max,
        "the offset clamps at the frame-written max"
    );
    // ↑↓ cursor moves follow into view (the F2b latch).
    model.handle(key(KeyCode::Up));
    assert!(
        model.usage.follow_cursor.get(),
        "a cursor move arms the latch"
    );
    draw(&model, 100, 24);
    assert!(
        !model.usage.follow_cursor.get(),
        "the frame consumes the latch"
    );
}

// ---- registry ---------------------------------------------------------

/// `/usage` is REGISTERED, ⏎ on its palette row RUNS it (never an
/// arg-slot lead jump — the F2a law), and Tab still offers the provider
/// filter completions.
#[test]
fn usage_is_registered_and_enter_runs_it_without_arg_hijack() {
    assert!(
        COMMANDS.iter().any(|spec| spec.name == "usage"),
        "/usage is in the registry"
    );
    assert!(
        !has_arg_slots("usage"),
        "no ⏎ hijack: the exact-match lead jump must never own /usage"
    );
    assert!(
        offers_arg_completions("usage"),
        "tab still completes the provider filter"
    );
    let slots = haider_tui::commands::DynamicSlots {
        providers: vec![("anthropic-oauth".to_owned(), "Anthropic".to_owned())],
        ..Default::default()
    };
    let items = palette_items("usage", false, &slots);
    assert_eq!(items.len(), 1, "a fully-typed /usage matches its own row");
    assert!(
        matches!(items[0], haider_tui::commands::PaletteItem::Cmd(spec) if spec.name == "usage"),
        "…and the row is the COMMAND, never an arg row"
    );
    let args = palette_items("usage anthro", false, &slots);
    assert!(
        args.iter().any(|item| matches!(
            item,
            haider_tui::commands::PaletteItem::Arg { cmd: "usage", value, .. } if value == "anthropic-oauth"
        )),
        "the argument slot completes from the discovered providers"
    );
}

// ---- demo + live wiring ----------------------------------------------

/// Demo mode opens an HONEST empty state: no fabricated report, no
/// request pushed, the note names live mode as the source of truth.
#[test]
fn demo_usage_opens_an_honest_empty_state() {
    let mut model = launcher_model();
    run_slash(&mut model, "/usage");
    assert_eq!(model.screen, Screen::Usage);
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::UsageRefresh)),
        "demo never pushes the read"
    );
    assert!(model.usage.report.is_none(), "demo fabricates no report");
    let (rows, _) = draw(&model, 100, 24);
    assert!(
        rows.join("\n")
            .contains("demo — usage is live daemon truth"),
        "the demo state says so"
    );
}

/// LIVE: entry is feature-gated BEFORE the screen opens (the B2b lesson)
/// — an ungated daemon flashes the stale note and nothing opens; a gated
/// daemon opens fetching and pushes the read.
#[test]
fn live_usage_entry_is_feature_gated_then_fetches() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    run_slash(&mut model, "/usage");
    assert_ne!(model.screen, Screen::Usage, "ungated: nothing opens");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("usage report")),
        "the stale-daemon note names the missing feature"
    );
    let mut model = live_gated_model();
    run_slash(&mut model, "/usage");
    assert_eq!(model.screen, Screen::Usage, "gated: the screen opens");
    assert!(model.usage.fetching, "…fetching");
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::UsageRefresh)),
        "…and the read is pushed"
    );
    // `f` re-reads; `r` is the reveal, never a fetch.
    model.requests.clear();
    model.handle(key(KeyCode::Char('f')));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::UsageRefresh)),
        "f fetches a fresh snapshot"
    );
    model.requests.clear();
    model.handle(key(KeyCode::Char('r')));
    assert!(model.requests.is_empty(), "r reveals — it never fetches");
}

/// The driver maps the read onto U1's wire and installs ONLY committed
/// replies; a typed failure lands on the screen and never erases held
/// truth.
///
/// MUTATION CHECK (executed): make `apply_report` keep `fetching = true`
/// — the installed-snapshot assert on `fetching` fails.
#[test]
fn live_replies_install_the_report_and_failures_land_typed() {
    let mut model = live_gated_model();
    run_slash(&mut model, "/usage");
    let mut driver = LiveDriver::new("test");
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    let commands: Vec<LiveCommand> = requests
        .into_iter()
        .flat_map(|request| driver.handle_request(&mut model, request))
        .collect();
    assert!(
        commands.contains(&LiveCommand::UsageReport),
        "the refresh becomes the usage.report read"
    );
    assert!(
        LiveCommand::UsageReport.command_id().is_none(),
        "a read carries no durable identity"
    );
    driver.apply(
        &mut model,
        LiveReply::UsageReport {
            report: Box::new(report()),
        },
    );
    assert!(
        model.usage.report.is_some(),
        "the committed snapshot installs"
    );
    assert!(!model.usage.fetching, "…and the in-flight mark clears");
    assert_eq!(model.usage.error, None);
    driver.apply(
        &mut model,
        LiveReply::UsageReportFailed {
            message: "daemon overloaded".to_owned(),
        },
    );
    assert_eq!(
        model.usage.error.as_deref(),
        Some("daemon overloaded"),
        "the typed failure lands on the screen"
    );
    assert!(
        model.usage.report.is_some(),
        "a failure never erases held truth"
    );
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n")
            .contains("usage read failed — daemon overloaded"),
        "…and renders there"
    );
}

/// The link encodes the read as U1's exact wire method and decodes both
/// the snapshot and the identity-tagged error (the read has no durable
/// command id — the hooks.list precedent).
#[test]
fn usage_wire_bodies_and_replies_map_onto_u1s_shapes() {
    let body = request_body(LiveCommand::UsageReport);
    assert_eq!(
        serde_json::to_value(&body).expect("encode"),
        serde_json::json!({"method": "usage.report"}),
        "the request is U1's parameterless usage.report"
    );
    let context = CommandContext::of(&LiveCommand::UsageReport);
    let replies = map_response(
        &context,
        haider_rpc::ResponseBody::UsageReport { report: report() },
    );
    assert!(
        matches!(
            replies.as_slice(),
            [LiveReply::UsageReport { report }] if report.accounts.len() == 3
        ),
        "the snapshot decodes whole"
    );
    let replies = map_response(
        &context,
        haider_rpc::ResponseBody::Error {
            code: "overloaded".to_owned(),
            message: "try later".to_owned(),
            retryable: true,
            data: None,
        },
    );
    assert_eq!(
        replies,
        vec![LiveReply::UsageReportFailed {
            message: "try later".to_owned()
        }],
        "the no-id error is identity-tagged onto the usage screen"
    );
}
