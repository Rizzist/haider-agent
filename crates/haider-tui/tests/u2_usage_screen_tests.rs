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
use haider_tui::app::{UsageModelRange, UsageScope};
use haider_tui::commands::{COMMANDS, has_arg_slots, offers_arg_completions, palette_items};
use haider_tui::format::{
    USAGE_BAR_CELLS, UsageTone, fmt_meter_reason, fmt_remaining, fmt_reset, mask_identity,
    remaining_bar, usage_tone,
};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

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

fn scope_strip_modifiers(model: &AppModel) -> Vec<(UsageScope, Modifier)> {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    hits.into_iter()
        .filter_map(|(rect, hit)| match hit {
            Hit::UsageScope(scope) => Some((scope, buffer[(rect.x, rect.y)].modifier)),
            _ => None,
        })
        .collect()
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
        api_equivalent_est_cost_usd: est_cost_usd,
        lines_added: 1240,
        lines_removed: 380,
        cache: haider_protocol::usage::CacheUsageStatsV1::default(),
    }
}

fn oauth_stats() -> LocalUsageStatsV1 {
    let mut local = stats(12, Some(4.12));
    local.est_cost_usd = None;
    local.api_equivalent_est_cost_usd = Some(4.12);
    local.cache.logical_input_tokens = 1_000_000;
    local.cache.uncached_input_tokens = 200_000;
    local.cache.cache_read_tokens = 800_000;
    local.cache.telemetry_covered_input_tokens = 1_000_000;
    local.cache.api_equivalent_input_with_cache_usd = Some(0.12);
    local.cache.api_equivalent_input_without_cache_usd = Some(1.20);
    local.cache.api_equivalent_estimated_savings_usd = Some(1.08);
    local
}

fn idle_oauth_stats() -> LocalUsageStatsV1 {
    LocalUsageStatsV1::default()
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
                local: oauth_stats(),
            },
            AccountUsageReportV1 {
                provider: "anthropic-oauth".to_owned(),
                alias: CredentialAlias::new("anthropic2"),
                identity: Some("work@corp.dev".to_owned()),
                plan: None,
                auth_method: AuthMethod::OAuth,
                meter: AccountMeterStateV1::Unavailable {
                    reason: "http_status_401".to_owned(),
                },
                local: idle_oauth_stats(),
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

/// BAR-MATH LAW, remaining semantics (owner 2026-08-24: the bar DRAINS —
/// runway going down, not consumption going up): 0–1 utilization → cells
/// filled by the REMAINING fraction, clamped and floor-filled, with the
/// mirrored honesty clamps (nonzero remaining shows ≥ 1 cell, nonzero
/// consumption never reads full).
///
/// MUTATION CHECK (executed): replace the floor fill with `.round()` in
/// `remaining_bar` — 0.83 used rounds its 1.7-cell runway up to 2 and the
/// `floors-runway` assertion fails; restore, green.
#[test]
fn remaining_bar_math_clamps_and_floors() {
    assert_eq!(
        remaining_bar(0.83, 10),
        "▰▱▱▱▱▱▱▱▱▱",
        "0.83 used floors its 17% runway to 1 cell"
    );
    assert_eq!(
        remaining_bar(0.0, 10),
        "▰▰▰▰▰▰▰▰▰▰",
        "untouched renders full runway"
    );
    assert_eq!(
        remaining_bar(1.0, 10),
        "▱▱▱▱▱▱▱▱▱▱",
        "exhausted renders empty"
    );
    assert_eq!(
        remaining_bar(2.5, 10),
        "▱▱▱▱▱▱▱▱▱▱",
        "over-range clamps to exhausted"
    );
    assert_eq!(
        remaining_bar(-0.5, 10),
        "▰▰▰▰▰▰▰▰▰▰",
        "negatives clamp to untouched"
    );
    assert_eq!(
        remaining_bar(0.04, 10),
        "▰▰▰▰▰▰▰▰▰▱",
        "any nonzero consumption never reads full"
    );
    assert_eq!(
        remaining_bar(0.13, 10),
        "▰▰▰▰▰▰▰▰▱▱",
        "FLOOR the runway, never round: 87% left stays at 8 cells"
    );
    assert_eq!(
        remaining_bar(0.999, 10),
        "▰▱▱▱▱▱▱▱▱▱",
        "any nonzero remaining shows at least one cell"
    );
    assert_eq!(USAGE_BAR_CELLS, 10, "the screen's bar width is pinned");
    assert_eq!(fmt_remaining(0.83), "17% left");
    assert_eq!(
        fmt_remaining(0.005),
        "99% left",
        "remaining floors — never overstate runway"
    );
    assert_eq!(
        fmt_remaining(7.5),
        "0% left",
        "over-consumed clamps to none"
    );
    assert_eq!(fmt_remaining(-1.0), "100% left", "negatives clamp to all");
}

/// 954 date-math pins: the heatmap's civil-date algebra is pure and must
/// hold at the boundaries clocks get wrong — leap days, year edges, and
/// the epoch itself. `civil_from_days`/`days_from_civil` are exact
/// inverses; weekday 0 is Sunday (1970-01-01 was a Thursday = 4).
///
/// MUTATION CHECK (executed): drop the `+ 1` in `civil_from_days`'s day
/// term — every named date shifts a day and the epoch pin fails.
#[test]
fn heatmap_civil_date_algebra_round_trips_and_knows_its_weekdays() {
    use haider_tui::format::{
        civil_from_days, days_from_civil, days_from_iso_date, weekday_from_days,
    };
    assert_eq!(civil_from_days(0), (1970, 1, 1), "the epoch");
    assert_eq!(weekday_from_days(0), 4, "1970-01-01 was a Thursday");
    for (y, m, d) in [
        (1970, 1, 1),
        (2000, 2, 29),
        (2024, 2, 29),
        (2025, 12, 31),
        (2026, 1, 1),
        (2026, 8, 24),
    ] {
        let z = days_from_civil(y, m, d);
        assert_eq!(
            civil_from_days(z),
            (y, m, d),
            "round trip through days-since-epoch"
        );
    }
    assert_eq!(
        days_from_iso_date("2026-08-24"),
        Some(days_from_civil(2026, 8, 24)),
        "ISO parse agrees with the algebra"
    );
    assert_eq!(days_from_iso_date("2026-13-01"), None, "bad month refused");
    assert_eq!(days_from_iso_date("garbage"), None, "garbage refused");
    // 2026-08-23 was a Sunday: row zero of the grid.
    assert_eq!(weekday_from_days(days_from_civil(2026, 8, 23)), 0);
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

#[test]
fn meter_reason_compaction_preserves_the_typed_mapping() {
    assert_eq!(fmt_meter_reason("http_status_401"), "http 401");
    assert_eq!(fmt_meter_reason("transport_timeout"), "transport timeout");
    assert_eq!(fmt_meter_reason("malformed_response"), "malformed response");
    assert_eq!(
        fmt_meter_reason("credential_unavailable"),
        "credential unavailable"
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
/// the domain survive, every hidden run is capped at eight stars, the final
/// `.tld` stays readable — and the masked form never contains the full local
/// part.
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

/// P1 length-hiding law: long runs cap at exactly eight stars while short
/// runs retain their natural size. Removing the cap fails the first assert;
/// padding every run to eight fails the second.
#[test]
fn masked_runs_cap_at_eight_without_padding_short_runs() {
    assert_eq!(mask_identity(&format!("C{}", "x".repeat(40))), "C********");
    assert_eq!(mask_identity("Cxyz"), "C***");
    assert_eq!(
        mask_identity("somethinglong@gmail.com"),
        "s********@g****.com"
    );
}

// ---- per-state rendering ---------------------------------------------

/// 970 scope law: one `s` from accounts opens the reset calendar, then the
/// established global → history → models sequence continues. Global renders ONE line
/// per account (every tab, not just selected), headlining the window with
/// the LEAST runway, and keeps the shared THIS DEVICE totals footer; the
/// per-provider detail (window names like `five_hour` on their own lines)
/// does not render. Re-entering the screen resets to the accounts detail
/// (the reveal-mask precedent: a screen opens predictably).
///
/// MUTATION CHECK (executed): make `UsageScope::next` return `self` — the
/// `global breadcrumb renders` assertion fails; restore, green.
#[test]
fn s_cycles_scope_and_global_renders_compact_overview() {
    let mut model = usage_model();
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("· accounts"),
        "the screen opens on the accounts scope"
    );

    model.handle(key(KeyCode::Char('s')));
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("· calendar"),
        "one s from accounts opens the reset calendar"
    );

    model.handle(key(KeyCode::Char('s')));
    let (rows, _) = draw(&model, 110, 30);
    let text = rows.join("\n");
    assert!(text.contains("· global"), "global breadcrumb renders");
    assert!(
        text.contains("(five_hour)"),
        "global headlines the least-runway window by name"
    );
    assert!(
        !text.contains("resets in"),
        "per-window reset detail does not render in global"
    );
    assert!(
        text.contains("THIS DEVICE"),
        "the totals footer serves both scopes"
    );

    model.handle(key(KeyCode::Char('s')));
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("· history"),
        "history keeps its place after calendar and global"
    );

    model.handle(key(KeyCode::Char('s')));
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("· models"),
        "models keeps its place after history"
    );

    model.handle(key(KeyCode::Char('s')));
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        rows.join("\n").contains("resets in"),
        "s cycles back to the accounts detail (reset lines return)"
    );

    model.handle(key(KeyCode::Char('s')));
    assert_eq!(model.usage.scope, UsageScope::Calendar);
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/usage");
    assert_eq!(
        model.usage.scope,
        UsageScope::Accounts,
        "re-entry resets to the accounts scope"
    );
}

#[test]
fn scope_strip_pins_every_active_scope_and_clicks_by_value() {
    let mut model = usage_model();
    for active in [
        UsageScope::Accounts,
        UsageScope::Calendar,
        UsageScope::Global,
        UsageScope::History,
        UsageScope::Models,
    ] {
        model.usage.scope = active;
        let (rows, hits) = draw(&model, 120, 30);
        assert!(
            rows.join("\n").contains(
                "accounts · calendar · global · history · models    s next scope · ← → account"
            ),
            "the shared strip renders on {active:?}"
        );
        let styles = scope_strip_modifiers(&model);
        assert_eq!(styles.len(), 5, "five scope labels are clickable");
        for (scope, modifier) in styles {
            assert_eq!(
                modifier.contains(Modifier::BOLD),
                scope == active,
                "only {active:?} is highlighted; examined {scope:?}"
            );
        }
        let history_hit = hits
            .into_iter()
            .find_map(|(_, hit)| matches!(hit, Hit::UsageScope(UsageScope::History)).then_some(hit))
            .expect("history scope hit");
        model.handle_hit(history_hit);
        assert_eq!(model.usage.scope, UsageScope::History);
    }
}

#[test]
fn direct_usage_scope_arguments_land_and_keep_the_provider_filter() {
    let mut model = usage_model();
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/usage history anthropic");
    assert_eq!(model.usage.scope, UsageScope::History);
    assert_eq!(model.usage.filter.as_deref(), Some("anthropic"));

    for (command, expected) in [
        ("/usage models", UsageScope::Models),
        ("/usage calendar", UsageScope::Calendar),
        ("/usage global", UsageScope::Global),
        ("/usage accounts", UsageScope::Accounts),
    ] {
        model.handle(key(KeyCode::Esc));
        run_slash(&mut model, command);
        assert_eq!(model.usage.scope, expected, "{command}");
        assert_eq!(model.usage.filter, None, "{command} has no filter");
    }
}

/// Models folds attributed daily rows over the selected range, keeps provider
/// coordinates, sorts most-to-least, and cycles today/7d/30d/all-time with r.
///
/// MUTATION CHECK: reverse the token comparator; the first-row ordering pin
/// fails. Drop the range suffix; the 7d aggregate remains 50k and fails.
#[test]
fn models_scope_orders_and_folds_selectable_ledger_ranges() {
    use haider_protocol::usage::{
        UsageHistoryDailyTotalV1, UsageHistoryModelTotalV1, UsageHistoryRangeDayV1,
    };
    let mut model = usage_model();
    for _ in 0..4 {
        model.handle(key(KeyCode::Char('s')));
    }
    assert_eq!(model.usage.scope, UsageScope::Models);
    let daily_total = UsageHistoryDailyTotalV1 {
        sampled_slots: 1,
        requests: 1,
        input_tokens: 1,
        ..UsageHistoryDailyTotalV1::default()
    };
    let attributed =
        |date: &str, model: &str, provider: &str, input_tokens: u64, cost: Option<u64>| {
            UsageHistoryRangeDayV1 {
                date: date.into(),
                total: Some(daily_total.clone()),
                models: vec![UsageHistoryModelTotalV1 {
                    model: model.into(),
                    provider: provider.into(),
                    requests: 2,
                    input_tokens,
                    output_tokens: input_tokens / 10,
                    cache_read_tokens: input_tokens / 2,
                    reasoning_tokens: 0,
                    est_cost_microusd: cost,
                }],
            }
        };
    model.usage.apply_history(vec![
        UsageHistoryRangeDayV1 {
            date: "2026-08-20".into(),
            total: Some(daily_total.clone()),
            models: Vec::new(),
        },
        attributed(
            "2026-08-21",
            "gpt-5.6-sol",
            "openai-oauth",
            30_000,
            Some(120_000),
        ),
        attributed(
            "2026-08-27",
            "claude-sonnet-5",
            "anthropic-oauth",
            10_000,
            Some(30_000),
        ),
        attributed(
            "2026-08-28",
            "gpt-5.6-sol",
            "openai-oauth",
            50_000,
            Some(200_000),
        ),
    ]);

    let (rows, _) = draw(&model, 120, 34);
    let text = rows.join("\n");
    assert!(text.contains("today (UTC)"));
    assert!(text.contains("model                    provider"));
    assert!(text.contains("gpt-5.6-sol"));
    assert!(
        !text.contains("claude-sonnet-5"),
        "today excludes older rows"
    );

    model.handle(key(KeyCode::Char('r')));
    assert_eq!(model.usage.model_range, UsageModelRange::SevenDays);
    let (rows, _) = draw(&model, 120, 34);
    let text = rows.join("\n");
    assert!(text.contains("7d"));
    let gpt = text.find("gpt-5.6-sol").expect("largest row");
    let claude = text.find("claude-sonnet-5").expect("second row");
    assert!(gpt < claude, "80k-ish GPT total sorts before Claude");
    assert!(
        text.contains("80k"),
        "same model/provider folds across days"
    );
    assert!(text.contains("$0.32"), "daily known prices add exactly");

    model.handle(key(KeyCode::Char('r')));
    assert_eq!(model.usage.model_range, UsageModelRange::ThirtyDays);
    model.handle(key(KeyCode::Char('r')));
    assert_eq!(model.usage.model_range, UsageModelRange::AllTime);
    model.handle(key(KeyCode::Char('r')));
    assert_eq!(model.usage.model_range, UsageModelRange::Today);

    // A typed failure never flattens the held range.
    model.usage.history_failed("socket lost");
    let (rows, _) = draw(&model, 110, 34);
    let text = rows.join("\n");
    assert!(
        text.contains("model history read failed"),
        "the error is typed"
    );
    assert!(
        text.contains("gpt-5.6-sol"),
        "the held range stays visible under the error"
    );
}

#[test]
fn models_scope_names_empty_attribution_and_scrolls_long_tables() {
    use haider_protocol::usage::{
        UsageHistoryDailyTotalV1, UsageHistoryModelTotalV1, UsageHistoryRangeDayV1,
    };
    let mut model = usage_model();
    model.usage.scope = UsageScope::Models;
    let total = UsageHistoryDailyTotalV1 {
        sampled_slots: 1,
        requests: 1,
        ..UsageHistoryDailyTotalV1::default()
    };
    model.usage.apply_history(vec![
        UsageHistoryRangeDayV1 {
            date: "2026-08-27".into(),
            total: Some(total.clone()),
            models: vec![UsageHistoryModelTotalV1 {
                model: "first-attributed".into(),
                provider: "openai-oauth".into(),
                requests: 1,
                input_tokens: 10,
                ..UsageHistoryModelTotalV1::default()
            }],
        },
        UsageHistoryRangeDayV1 {
            date: "2026-08-28".into(),
            total: Some(total.clone()),
            models: Vec::new(),
        },
    ]);
    let (rows, _) = draw(&model, 110, 20);
    assert!(
        rows.join("\n")
            .contains("no attributed rows in this range · first attributed date 2026-08-27")
    );

    let models = (0_u64..24)
        .map(|index| UsageHistoryModelTotalV1 {
            model: format!("model-{index:02}"),
            provider: "openai-oauth".into(),
            requests: 1,
            input_tokens: 24_000 - index * 1_000,
            ..UsageHistoryModelTotalV1::default()
        })
        .collect();
    model.usage.apply_history(vec![UsageHistoryRangeDayV1 {
        date: "2026-08-28".into(),
        total: Some(total),
        models,
    }]);
    let (rows, _) = draw(&model, 100, 14);
    assert!(
        rows.join("\n").contains("model-00"),
        "largest row starts visible"
    );
    assert!(
        model.usage.scroll_max.get() > 8,
        "the table owns a real viewport"
    );
    model.handle(key(KeyCode::PageDown));
    let (rows, _) = draw(&model, 100, 14);
    assert_eq!(model.usage.scroll.get(), 8);
    assert!(
        !rows.join("\n").contains("model-00"),
        "page-down moves the largest row above the viewport"
    );
    assert!(rows.join("\n").contains('⋮'), "the scroll gutter renders");
}

/// 954 heatmap laws on the History scope: an ABSENT day (`total: None`)
/// renders the faint `·`, a PRESENT ZERO day renders `▫` (a measured
/// zero is a fact), active days render `■`; the header stats fold only
/// present days; a held window under a typed error stays visible and is
/// never flattened into an empty grid.
///
/// MUTATION CHECK (executed): zero-fill absent days in the renderer
/// (treat `None` as `Some(0)`) — the `absent vs zero` assertion fails
/// because the absent cell renders `▫`; restore, green.
#[test]
fn history_scope_renders_absence_zero_and_activity_distinctly() {
    use haider_protocol::usage::{UsageHistoryDailyTotalV1, UsageHistoryRangeDayV1};
    let mut model = usage_model();
    model.handle(key(KeyCode::Char('s')));
    model.handle(key(KeyCode::Char('s')));
    model.handle(key(KeyCode::Char('s')));
    assert_eq!(model.usage.scope, UsageScope::History);

    let total = |tokens: u64| UsageHistoryDailyTotalV1 {
        sampled_slots: 4,
        requests: 2,
        input_tokens: tokens,
        ..Default::default()
    };
    model.usage.apply_history(vec![
        UsageHistoryRangeDayV1 {
            date: "2026-08-20".into(),
            total: Some(total(120_000)),
            models: Vec::new(),
        },
        UsageHistoryRangeDayV1 {
            date: "2026-08-21".into(),
            total: None,
            models: Vec::new(),
        },
        UsageHistoryRangeDayV1 {
            date: "2026-08-22".into(),
            total: Some(total(0)),
            models: Vec::new(),
        },
        UsageHistoryRangeDayV1 {
            date: "2026-08-23".into(),
            total: Some(total(40_000)),
            models: Vec::new(),
        },
    ]);
    let (rows, _) = draw(&model, 110, 30);
    let text = rows.join("\n");
    assert!(text.contains("Token activity"), "the header renders");
    assert!(
        text.contains("daily · weekly · cumulative — next"),
        "History keeps its own one-line legend beneath the shared scope strip"
    );
    assert!(
        text.contains("lifetime 160k"),
        "lifetime folds only present days: {text}"
    );
    assert!(text.contains("■"), "active days render squares");
    // Row-scoped assertions: the stats line also carries middots, so the
    // absence glyphs must be proven INSIDE the weekday grid rows.
    // 2026-08-21 was a Friday (absent) and 2026-08-22 a Saturday (zero).
    let grid_row = |name: &str| {
        rows.iter()
            .find(|row| row.trim_start().starts_with(name))
            .cloned()
            .unwrap_or_default()
    };
    assert!(
        grid_row("Fr").contains("·"),
        "the absent Friday renders the faint dot: {:?}",
        grid_row("Fr")
    );
    assert!(
        !grid_row("Fr").contains("▫"),
        "the absent Friday must NOT read as a measured zero"
    );
    assert!(
        grid_row("Sa").contains("▫"),
        "the zero Saturday renders the measured-zero glyph: {:?}",
        grid_row("Sa")
    );

    // A typed failure never flattens the held window.
    model.usage.history_failed("socket lost");
    let (rows, _) = draw(&model, 110, 30);
    let text = rows.join("\n");
    assert!(text.contains("history read failed"), "the error is typed");
    assert!(
        text.contains("Token activity"),
        "the held window stays visible under the error"
    );
}

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
        text.contains("▰▱▱▱▱▱▱▱▱▱"),
        "0.83 used renders the 1/10 remaining bar (draining semantics)"
    );
    assert!(text.contains("17% left"), "the remaining label renders");
    assert!(text.contains("resets in 2h 14m"), "the reset time renders");
    assert!(text.contains("seven_day"), "every window renders");
    assert!(text.contains("resets in 5d 3h"), "the weekly reset renders");
    assert!(
        text.contains("max_20x"),
        "the plan renders (unmasked — not sensitive)"
    );
    assert!(text.contains("oauth"), "the auth flavor renders");
    assert!(text.contains("plan"), "subscription usage is plan-covered");
    assert!(text.contains("3h 42m"), "the local duration renders");
    assert!(text.contains("+1240 −380 lines"), "LOC renders");
    assert!(text.contains("in 1.2M"), "token splits render");
}

#[test]
fn weekly_only_meter_row_pins_percent_label_and_reset() {
    let mut usage = report();
    usage.accounts[0].meter = AccountMeterStateV1::Metered {
        windows: vec![UsageWindowV1 {
            window: "weekly".into(),
            utilization: 0.08,
            resets_at_ms: Some(GENERATED_AT_MS + (3 * 24 + 4) * 60 * 60 * 1_000),
            label: None,
        }],
    };
    usage.accounts.truncate(1);
    let mut model = usage_model();
    model.usage.apply_report(usage);
    let (rows, _) = draw(&model, 110, 24);
    assert!(
        rows.join("\n")
            .contains("92% left (weekly) · resets in 3d 4h")
    );
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
        text.contains("meter unavailable · http 401"),
        "the typed reason renders honestly"
    );
    // The anthropic block now shows NO bar; openai is local-only; so the
    // whole report body must carry no meter glyph at all.
    assert!(
        !text.contains('▰') && !text.contains('▱'),
        "an unavailable meter never fabricates a bar"
    );

    model.usage.scope = UsageScope::Global;
    let (rows, _) = draw(&model, 110, 30);
    assert!(
        body_text(&rows).contains("meter unavailable · http 401"),
        "the compact global row must not swallow the typed reason"
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

/// OAuth is not an unknown price: it renders a labeled API-rate equivalent.
#[test]
fn part_a_oauth_usage_shows_labeled_api_rate_with_tokens_cache_and_hit_rate() {
    let mut model = usage_model();
    model.usage.filter = Some("anthropic-oauth".to_owned());
    let (rows, _) = draw(&model, 110, 30);
    let text = body_text(&rows);
    assert!(text.contains("in 1.2M"), "tokens remain visible: {text}");
    assert!(
        text.contains("read 800k"),
        "cache reads remain visible: {text}"
    );
    assert!(
        text.contains("80.00% hit"),
        "hit rate remains visible: {text}"
    );
    assert!(text.contains("plan"), "OAuth is plan-covered: {text}");
    assert!(
        text.contains("≈$4.12 API rate"),
        "OAuth shows the plan's API-rate equivalent: {text}"
    );
    assert!(
        !text.contains("est $4.12"),
        "OAuth is not real spend: {text}"
    );
}

/// Mixed device totals keep real spend and all-lane API equivalent separate.
#[test]
fn part_a_mixed_device_total_separates_metered_and_all_lane_api_rate() {
    let model = usage_model();
    let (rows, _) = draw(&model, 120, 40);
    let text = body_text(&rows);
    assert!(text.contains("est $0.57 (metered lanes)"), "{text}");
    assert!(text.contains("≈$4.69 API rate (all lanes)"), "{text}");
    assert!(!text.contains("est $4.69"), "{text}");
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

/// `f` is visible work in both ledger scopes: it refreshes the compact
/// provider snapshot and reconciled range together, immediately renders the
/// in-flight state, then leaves a typed reason if either read fails.
#[test]
fn history_and_models_f_refresh_has_immediate_feedback() {
    for scope in [UsageScope::History, UsageScope::Models] {
        let mut model = live_gated_model();
        model
            .daemon_features
            .insert(haider_rpc::FEATURE_USAGE_HISTORY_V1.to_owned());
        run_slash(&mut model, &format!("/usage {}", scope.name()));
        model.requests.clear();
        model.usage.fetching = false;
        model.usage.history_fetching = false;

        model.handle(key(KeyCode::Char('f')));
        assert!(model.usage.fetching, "{scope:?} provider read starts");
        assert!(model.usage.history_fetching, "{scope:?} ledger read starts");
        assert!(
            model
                .requests
                .iter()
                .any(|request| matches!(request, AppRequest::UsageRefresh))
        );
        assert!(
            model
                .requests
                .iter()
                .any(|request| matches!(request, AppRequest::UsageHistoryRefresh))
        );
        let (rows, _) = draw(&model, 100, 24);
        assert!(rows.join("\n").contains("fetching…"), "{scope:?}");

        model.usage.read_failed("http 503");
        model.usage.history_failed("ledger busy");
        let (rows, _) = draw(&model, 100, 24);
        let text = rows.join("\n");
        assert!(text.contains("usage read failed — http 503"), "{scope:?}");
        assert!(text.contains("read failed — ledger busy"), "{scope:?}");
    }
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
        haider_rpc::ResponseBody::UsageReport {
            report: report(),
            availability: None,
        },
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
