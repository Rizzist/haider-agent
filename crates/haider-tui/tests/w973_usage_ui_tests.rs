//! 973 usage UI: the `/usage` calendar made unambiguous for the owner.
//!
//! - accounts list — and the arrow keys step — in ONE on-screen order;
//! - reset instants read `local (UTC) · relative`, DST-safe per instant;
//! - the selected account's reset days are highlighted on the grid in a
//!   style distinct from today, other accounts' markers dim;
//! - the selected account opens a detail panel prepared for provider-
//!   reported extras (Haider Code allowance, reset history, banked resets)
//!   that says "not reported by provider" instead of inventing values;
//! - meter failures carry an honest, actionable hint.
//!
//! The fixture mirrors the owner's 972 screenshot (six accounts, three
//! providers, report order interleaving providers) with synthetic
//! identities; the owner zone is pinned as a fixed UTC+03:30 (the offset the
//! owner's own example implies) so frames are host-independent.
#![allow(clippy::expect_used)]

use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1, UsageWindowV1,
};
use haider_tui::app::{AppModel, AppRequest, Screen, UsageScope};
use haider_tui::theme::ThemeKey;
use haider_tui::usage_detail::{
    AccountResetExtras, AllowanceView, BankedResetAction, BankedResets, DetailTone, PastReset,
    ResetHistorySource, account_extras, banked_reset_unavailable_note, detail_rows,
    fmt_meter_reason_friendly,
};
use haider_tui::usage_time::{
    UsageTimeZone, civil_instant, fmt_relative, fmt_reset_instant, fmt_utc_offset,
    fmt_zone_caption, zoned_day,
};
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;

mod common;
mod tuivirt_common;
use common::{key, run_slash};
use tuivirt_common::{Snapshot, check_golden, draw, launcher_model};

// Wed 23 Sep 2026 17:00 UTC — the snapshot instant (`today`).
const SNAPSHOT_MS: u64 = 1_790_182_800_000;
const D_FIVE_HOUR_MS: u64 = 1_790_197_740_000; // Wed 23 Sep 21:09 UTC
const D_WEEKLY_MS: u64 = 1_790_355_540_000; // Fri 25 Sep 16:59 UTC
const E_FIVE_HOUR_MS: u64 = 1_790_216_100_000; // Thu 24 Sep 02:15 UTC
const E_WEEKLY_MS: u64 = 1_790_586_000_000; // Mon 28 Sep 09:00 UTC
const HC_WEEKLY_MS: u64 = 1_790_726_400_000; // Wed 30 Sep 00:00 UTC
/// The owner's example `20:29 local (16:59 UTC)` is UTC+03:30.
const OWNER_OFFSET: i32 = 3 * 3_600 + 30 * 60;

fn account(
    provider: &str,
    alias: &str,
    identity: &str,
    plan: Option<&str>,
    meter: AccountMeterStateV1,
) -> AccountUsageReportV1 {
    AccountUsageReportV1 {
        provider: provider.to_owned(),
        alias: CredentialAlias::new(alias),
        identity: Some(identity.to_owned()),
        plan: plan.map(str::to_owned),
        auth_method: AuthMethod::OAuth,
        meter,
        local: LocalUsageStatsV1::default(),
    }
}

fn window(name: &str, utilization: f64, resets_at_ms: Option<u64>) -> UsageWindowV1 {
    UsageWindowV1 {
        window: name.to_owned(),
        utilization,
        resets_at_ms,
        label: None,
    }
}

fn unavailable(reason: &str) -> AccountMeterStateV1 {
    AccountMeterStateV1::Unavailable {
        reason: reason.to_owned(),
    }
}

/// REPORT order interleaves providers exactly like the owner's screenshot:
/// openai, anthropic, haider-code, anthropic, anthropic, openai.
fn owner_report() -> UsageReportV1 {
    UsageReportV1 {
        generated_at_ms: SNAPSHOT_MS,
        accounts: vec![
            account(
                "openai-oauth",
                "codex-fc1509ff98ec",
                "sam@example.ai",
                None,
                unavailable("credential_unavailable"),
            ),
            account(
                "anthropic-oauth",
                "anthropic-oauth",
                "claude@example.com",
                None,
                unavailable("http_status_429"),
            ),
            account(
                "haider-code",
                "haider-code-api",
                "hc@example.dev",
                None,
                AccountMeterStateV1::LocalOnly,
            ),
            account(
                "anthropic-oauth",
                "anthropic-oauth-2",
                "claude2@example.com",
                None,
                AccountMeterStateV1::Metered {
                    windows: vec![
                        window("five_hour", 0.4, Some(D_FIVE_HOUR_MS)),
                        window("seven_day", 0.2, Some(D_WEEKLY_MS)),
                    ],
                },
            ),
            account(
                "anthropic-oauth",
                "anthropic-oauth-3",
                "claude3@example.com",
                None,
                AccountMeterStateV1::Metered {
                    windows: vec![
                        window("five_hour", 0.7, Some(E_FIVE_HOUR_MS)),
                        window("seven_day", 0.5, Some(E_WEEKLY_MS)),
                    ],
                },
            ),
            account(
                "openai-oauth",
                "openai-oauth",
                "sam@example.ai",
                Some("pro"),
                unavailable("malformed_response"),
            ),
        ],
        orchestration: Default::default(),
    }
}

/// The on-screen order: provider groups first-seen, report order within.
const DISPLAY_ORDER: [&str; 6] = [
    "codex-fc1509ff98ec",
    "openai-oauth",
    "anthropic-oauth",
    "anthropic-oauth-2",
    "anthropic-oauth-3",
    "haider-code-api",
];

fn usage_model(report: UsageReportV1, scope: UsageScope, zone: UsageTimeZone) -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/usage");
    assert_eq!(model.screen, Screen::Usage);
    model.usage.apply_report(report);
    model.usage.time_zone = zone;
    model.usage.scope = scope;
    model
}

fn calendar(zone: UsageTimeZone) -> AppModel {
    usage_model(owner_report(), UsageScope::Calendar, zone)
}

fn selected_alias(model: &AppModel) -> String {
    let slot = model.usage.selected_slot().expect("a selection");
    model.usage.report.as_ref().expect("report").accounts[slot]
        .alias
        .as_str()
        .to_owned()
}

/// The account aliases in the order the FRAME lists them (marker rows of
/// the calendar list: `  > a  alias · …` / `    b  alias · …`).
fn calendar_list_order(frame: &Snapshot) -> Vec<String> {
    let start = frame.row_containing("RESET MARKERS").expect("marker list");
    frame.rows[start + 1..]
        .iter()
        .filter_map(|row| {
            let rest = row.strip_prefix("  ")?;
            let rest = rest
                .strip_prefix("> ")
                .or_else(|| rest.strip_prefix("  "))?;
            let (marker, rest) = rest.split_once("  ")?;
            (marker.len() == 1 && marker.chars().all(|c| c.is_ascii_lowercase()))
                .then(|| rest.split(" · ").next().unwrap_or_default().to_owned())
        })
        .collect()
}

/// The alias on the frame's `>` row of the calendar list.
fn calendar_selected_row(frame: &Snapshot) -> String {
    let start = frame.row_containing("RESET MARKERS").expect("marker list");
    frame.rows[start + 1..]
        .iter()
        .find_map(|row| {
            let rest = row.strip_prefix("  > ")?;
            let (_, rest) = rest.split_once("  ")?;
            Some(rest.split(" · ").next().unwrap_or_default().to_owned())
        })
        .expect("a selected row")
}

// ---- 1. time formatting ------------------------------------------------

#[test]
fn utc_offsets_format_with_sign_and_minutes() {
    assert_eq!(fmt_utc_offset(0), "UTC+00:00");
    assert_eq!(fmt_utc_offset(OWNER_OFFSET), "UTC+03:30");
    assert_eq!(fmt_utc_offset(-4 * 3_600), "UTC-04:00");
    assert_eq!(fmt_utc_offset(5 * 3_600 + 45 * 60), "UTC+05:45");
}

#[test]
fn reset_instants_read_local_with_exact_utc_beside_them() {
    // The owner's own example, verbatim in shape.
    assert_eq!(
        fmt_reset_instant(D_WEEKLY_MS, UsageTimeZone::Fixed(OWNER_OFFSET), 2026),
        "Fri 25 Sep · 20:29 local (16:59 UTC)"
    );
    // Crossing midnight: the UTC side names its own date.
    assert_eq!(
        fmt_reset_instant(D_FIVE_HOUR_MS, UsageTimeZone::Fixed(OWNER_OFFSET), 2026),
        "Thu 24 Sep · 00:39 local (Wed 23 Sep 21:09 UTC)"
    );
    assert_eq!(
        fmt_reset_instant(E_FIVE_HOUR_MS, UsageTimeZone::Fixed(-4 * 3_600), 2026),
        "Wed 23 Sep · 22:15 local (Thu 24 Sep 02:15 UTC)"
    );
    // A zero offset is still a KNOWN local zone.
    assert_eq!(
        fmt_reset_instant(D_WEEKLY_MS, UsageTimeZone::Fixed(0), 2026),
        "Fri 25 Sep · 16:59 local = UTC"
    );
    // No local zone: UTC only, never a UTC labelled "local".
    assert_eq!(
        fmt_reset_instant(D_WEEKLY_MS, UsageTimeZone::UtcOnly, 2026),
        "Fri 25 Sep · 16:59 UTC"
    );
    // A different year is named.
    assert_eq!(
        fmt_reset_instant(D_WEEKLY_MS, UsageTimeZone::UtcOnly, 2025),
        "Fri 25 Sep 2026 · 16:59 UTC"
    );
}

/// America/New_York 2026: EDT (UTC-4) until Sun 01 Nov 06:00 UTC, EST
/// (UTC-5) after.
fn new_york_2026(unix_seconds: i64) -> Option<i32> {
    Some(if unix_seconds < 1_793_512_800 {
        -4 * 3_600
    } else {
        -5 * 3_600
    })
}

#[test]
fn local_times_resolve_the_offset_per_instant_across_a_dst_change() {
    let zone = UsageTimeZone::Rule(new_york_2026);
    // One minute before the fall-back: 01:59 EDT.
    assert_eq!(
        fmt_reset_instant(1_793_512_740_000, zone, 2026),
        "Sun 01 Nov · 01:59 local (05:59 UTC)"
    );
    // The fall-back instant itself: 01:00 EST, not 02:00 EDT.
    assert_eq!(
        fmt_reset_instant(1_793_512_800_000, zone, 2026),
        "Sun 01 Nov · 01:00 local (06:00 UTC)"
    );
    // A snapshot in EDT and a reset in EST: the RESET's own offset wins.
    assert_eq!(
        fmt_reset_instant(1_793_534_400_000, zone, 2026),
        "Sun 01 Nov · 07:00 local (12:00 UTC)"
    );
    assert_eq!(fmt_zone_caption(1_793_448_000_000, zone), "local UTC-04:00");
    assert_eq!(fmt_zone_caption(1_793_534_400_000, zone), "local UTC-05:00");
    // Calendar days are local days: 03:30 UTC Thu is still Wed in UTC-4.
    let wednesday = civil_instant(1_790_220_600_000, -4 * 3_600);
    assert_eq!((wednesday.date, wednesday.weekday), (23, 3));
    assert_eq!(
        zoned_day(1_790_220_600_000, UsageTimeZone::Fixed(-4 * 3_600)),
        wednesday.day
    );
    assert_eq!(
        zoned_day(1_790_220_600_000, UsageTimeZone::UtcOnly),
        wednesday.day + 1
    );
}

#[test]
fn an_unresolvable_local_zone_is_said_out_loud() {
    fn unresolvable(_: i64) -> Option<i32> {
        None
    }
    let zone = UsageTimeZone::Rule(unresolvable);
    assert_eq!(
        fmt_zone_caption(SNAPSHOT_MS, zone),
        "UTC (local zone unavailable)"
    );
    assert_eq!(
        fmt_reset_instant(D_WEEKLY_MS, zone, 2026),
        "Fri 25 Sep · 16:59 UTC"
    );
    assert_eq!(fmt_zone_caption(SNAPSHOT_MS, UsageTimeZone::UtcOnly), "UTC");
}

#[test]
fn relative_deltas_floor_and_name_the_direction() {
    assert_eq!(fmt_relative(SNAPSHOT_MS, D_WEEKLY_MS), "in 1d 23h");
    assert_eq!(fmt_relative(SNAPSHOT_MS, D_FIVE_HOUR_MS), "in 4h 9m");
    assert_eq!(fmt_relative(0, 12 * 60_000 + 59_999), "in 12m");
    assert_eq!(fmt_relative(0, 59_999), "due now");
    assert_eq!(fmt_relative(60_000, 0), "1m ago");
    assert_eq!(
        fmt_relative(SNAPSHOT_MS, SNAPSHOT_MS - 2 * 3_600_000 - 5 * 60_000),
        "2h 5m ago"
    );
    // 1d 3h 59m floors to the hour tier's `1d 3h`, never `1d 4h`.
    assert_eq!(fmt_relative(0, (27 * 60 + 59) * 60_000), "in 1d 3h");
}

// ---- 2. order: the list and the arrows agree ---------------------------

#[test]
fn display_order_groups_providers_and_keeps_report_order_within() {
    let model = calendar(UsageTimeZone::Fixed(OWNER_OFFSET));
    let report = model.usage.report.as_ref().expect("report");
    let aliases = model
        .usage
        .display_order()
        .into_iter()
        .map(|slot| report.accounts[slot].alias.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(aliases, DISPLAY_ORDER);
}

/// OWNER BUG (972): the arrows walked accounts in a different order from
/// the list. Every arrow now steps exactly one ON-SCREEN row: the order is
/// read back from the rendered frame at both owner sizes, not from the
/// model, so a renderer/reducer disagreement fails here.
///
/// MUTATION CHECK: render the calendar list in report order (sort the
/// display order by slot) — the frame order stops matching the step order.
#[test]
fn calendar_arrows_step_exactly_the_rendered_list_order() {
    for (width, height) in [(118, 36), (80, 24)] {
        for forward in [
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Tab,
            KeyCode::Char('>'),
        ] {
            let mut model = calendar(UsageTimeZone::Fixed(OWNER_OFFSET));
            // The list is taller than 24 rows: read the order unclipped.
            let listed = calendar_list_order(&draw(&model, width, 200));
            assert_eq!(listed, DISPLAY_ORDER, "{width}x{height}");
            let mut visited = vec![calendar_selected_row(&draw(&model, width, 200))];
            for _ in 1..listed.len() {
                model.handle(key(forward));
                // The small viewport follows the selection (the follow latch
                // is consumed by the first frame, so draw the owner size
                // first): the selected row is on screen after every step.
                let small = draw(&model, width, height);
                assert!(
                    small.rows.iter().any(|row| row.starts_with("  > ")),
                    "{forward:?} {width}x{height}: selected row scrolled out of view"
                );
                assert!(
                    small.contains("│ use banked reset"),
                    "{forward:?} {width}x{height}: the detail panel follows too"
                );
                visited.push(calendar_selected_row(&draw(&model, width, 200)));
            }
            assert_eq!(visited, listed, "{forward:?} at {width}x{height}");
            // Clamped: one more step stays on the last row.
            model.handle(key(forward));
            assert_eq!(selected_alias(&model), "haider-code-api");
        }
    }
}

#[test]
fn backward_arrows_retrace_the_same_rows_in_reverse() {
    for backward in [
        KeyCode::Up,
        KeyCode::Left,
        KeyCode::BackTab,
        KeyCode::Char('<'),
    ] {
        let mut model = calendar(UsageTimeZone::UtcOnly);
        for _ in 0..DISPLAY_ORDER.len() {
            model.handle(key(KeyCode::Down));
        }
        let mut visited = vec![selected_alias(&model)];
        for _ in 1..DISPLAY_ORDER.len() {
            model.handle(key(backward));
            visited.push(selected_alias(&model));
        }
        let mut reversed = DISPLAY_ORDER.map(str::to_owned).to_vec();
        reversed.reverse();
        assert_eq!(visited, reversed, "{backward:?}");
        model.handle(key(backward));
        assert_eq!(selected_alias(&model), DISPLAY_ORDER[0], "clamped at top");
    }
}

/// Global scope lists the same accounts in the same order; its `>` row is
/// the selection and the arrows step it one row at a time.
#[test]
fn global_arrows_step_the_rendered_rows_and_mark_the_selection() {
    let mut model = usage_model(
        owner_report(),
        UsageScope::Global,
        UsageTimeZone::Fixed(OWNER_OFFSET),
    );
    let selected_row = |model: &AppModel| {
        let frame = draw(model, 118, 36);
        let rows = frame
            .rows
            .iter()
            .filter(|row| {
                DISPLAY_ORDER
                    .iter()
                    .any(|alias| row.trim_start_matches(['>', ' ']).starts_with(alias))
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), DISPLAY_ORDER.len(), "{rows:#?}");
        let listed = rows
            .iter()
            .map(|row| {
                row.trim_start_matches(['>', ' '])
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(listed, DISPLAY_ORDER);
        let marked = rows
            .iter()
            .filter(|row| row.starts_with("> "))
            .collect::<Vec<_>>();
        assert_eq!(marked.len(), 1, "exactly one selected row: {rows:#?}");
        marked[0]
            .trim_start_matches(['>', ' '])
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let mut visited = vec![selected_row(&model)];
    for _ in 1..DISPLAY_ORDER.len() {
        model.handle(key(KeyCode::Down));
        visited.push(selected_row(&model));
    }
    assert_eq!(visited, DISPLAY_ORDER);
    model.handle(key(KeyCode::Left));
    assert_eq!(selected_row(&model), "anthropic-oauth-3");
}

/// One selection across scopes: stepping in the calendar selects the same
/// account the accounts scope then shows (its group + tab).
#[test]
fn the_selection_is_shared_with_the_accounts_scope() {
    let mut model = calendar(UsageTimeZone::UtcOnly);
    for _ in 0..3 {
        model.handle(key(KeyCode::Down));
    }
    assert_eq!(selected_alias(&model), "anthropic-oauth-2");
    model.usage.scope = UsageScope::Accounts;
    let frame = draw(&model, 118, 36);
    assert!(
        frame.contains("anthropic-oauth  2/3 ←→"),
        "accounts scope shows the anthropic group at tab 2/3"
    );
    // Accounts scope keeps its per-group ↑↓ / tab ←→ layout (it renders one
    // block per provider, so group-then-tab IS its on-screen order).
    model.handle(key(KeyCode::Down));
    assert_eq!(selected_alias(&model), "haider-code-api");
}

// ---- 3. highlight --------------------------------------------------------

fn cell_bg(frame: &Snapshot, needle: &str) -> Vec<Color> {
    let row = frame.row_containing(needle).expect(needle);
    let column = frame.rows[row].find(needle).expect(needle);
    let column = frame.rows[row][..column].chars().count();
    (column..column + needle.chars().count())
        .map(|x| frame.cells[row][x].1.bg)
        .collect()
}

fn cell_fg(frame: &Snapshot, row: usize, needle: &str) -> Color {
    let column = frame.rows[row].find(needle).expect(needle);
    let column = frame.rows[row][..column].chars().count();
    frame.cells[row][column].1.fg
}

/// The selected account's 5h and weekly days wear the filled `ok` badge,
/// today keeps the gold soft ground, other accounts' markers dim — and the
/// highlight follows the selection.
///
/// MUTATION CHECK: drop the `selected_day` branch in `grid_cell` — the
/// badge assertions fail.
#[test]
fn selected_account_reset_days_are_highlighted_distinct_from_today() {
    let theme = ThemeKey::Dark.theme();
    let ok: Color = theme.ok.into();
    let gold_soft: Color = theme.gold_soft.into();
    let dim: Color = theme.dim.into();
    let mut model = calendar(UsageTimeZone::UtcOnly);
    model.theme = ThemeKey::Dark;
    // Select anthropic-oauth-2 (d): 5h Wed 23 (today, UTC) · weekly Fri 25.
    for _ in 0..3 {
        model.handle(key(KeyCode::Down));
    }
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("[23] d5"), "{:#?}", frame.rows);
    assert!(frame.contains("24  e5"));
    assert!(frame.contains("25  dW"));
    assert!(frame.contains("28  eW"));
    assert!(
        cell_bg(&frame, "25  dW").iter().all(|bg| *bg == ok),
        "the selected weekly day is badged"
    );
    assert!(
        cell_bg(&frame, "[23] d5").iter().all(|bg| *bg == ok),
        "a selected reset on today still reads as the selection"
    );
    assert!(
        cell_bg(&frame, "28  eW").iter().all(|bg| *bg != ok),
        "another account's day is not badged"
    );
    let row = frame.row_containing("28  eW").expect("e weekly");
    assert_eq!(cell_fg(&frame, row, "eW"), dim, "others' markers dim");

    // Step to anthropic-oauth-3 (e): the badge moves; today returns to gold.
    model.handle(key(KeyCode::Down));
    let frame = draw(&model, 118, 36);
    assert!(cell_bg(&frame, "28  eW").iter().all(|bg| *bg == ok));
    assert!(cell_bg(&frame, "25  dW").iter().all(|bg| *bg != ok));
    assert!(
        // The grid cell (the legend reads `[23] today`): d's dimmed marker
        // rides today's ground.
        cell_bg(&frame, "[23] d5").iter().all(|bg| *bg == gold_soft),
        "today without a selected reset keeps its own ground"
    );
    // e's 5h is Thu 24 (UTC).
    assert!(cell_bg(&frame, "24  e5").iter().all(|bg| *bg == ok));
}

/// Local days: in the owner's UTC+03:30, d's 21:09 UTC five-hour reset is
/// Thu 24 00:39 local — the marker moves to the 24th.
#[test]
fn grid_days_are_local_days_when_the_zone_is_known() {
    let model = calendar(UsageTimeZone::Fixed(OWNER_OFFSET));
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("local UTC+03:30 calendar"));
    assert!(frame.contains("24  d5,e5"), "{:#?}", frame.rows);
    let utc = draw(&calendar(UsageTimeZone::UtcOnly), 118, 36);
    assert!(utc.contains("UTC reset calendar"));
    assert!(utc.contains("[23] d5"), "{:#?}", utc.rows);
    assert!(utc.contains("24  e5"));
}

// ---- 4. which account, and when -----------------------------------------

#[test]
fn rows_name_the_alias_local_time_utc_and_relative_delta() {
    let mut model = calendar(UsageTimeZone::Fixed(OWNER_OFFSET));
    for _ in 0..3 {
        model.handle(key(KeyCode::Down));
    }
    let frame = draw(&model, 118, 60);
    let text = frame.rows.join("\n");
    assert!(
        text.contains(
            "selected d anthropic-oauth-2 — 5h in 4h 9m (Thu 24 Sep 00:39 local) · weekly in 1d 23h (Fri 25 Sep 20:29 local)"
        ),
        "{text}"
    );
    assert!(text.contains("> d  anthropic-oauth-2 · anthropic-oauth"));
    assert!(
        text.contains("5h Thu 24 Sep · 00:39 local (Wed 23 Sep 21:09 UTC) · in 4h 9m · five_hour")
    );
    assert!(text.contains("weekly Fri 25 Sep · 20:29 local (16:59 UTC) · in 1d 23h · seven_day"));
    assert!(text.contains("5h Thu 24 Sep · 05:45 local (02:15 UTC) · in 9h 15m · five_hour"));
    // Unknown stays unknown, with the honest reason and a hint.
    assert!(text.contains(
        "5h reset unknown · weekly reset unknown · meter unavailable · http 429 — rate limited · f retries later"
    ));
    assert!(
        text.contains("5h reset unknown · weekly reset unknown · local only; no provider meter")
    );

    // 80 columns: the same facts, window names dropped.
    let narrow = draw(&model, 80, 60).rows.join("\n");
    assert!(narrow.contains("> d  anthropic-oauth-2 · anthropic-oauth"));
    assert!(narrow.contains("weekly Fri 25 Sep · 20:29 local (16:59 UTC) · in 1d 23h"));
    assert!(narrow.contains("local UTC+03:30 days · UTC shown too"));
}

// ---- 5. detail panel -----------------------------------------------------

#[test]
fn detail_panel_says_not_reported_and_disables_the_action() {
    let mut model = calendar(UsageTimeZone::Fixed(OWNER_OFFSET));
    for _ in 0..3 {
        model.handle(key(KeyCode::Down));
    }
    let text = draw(&model, 118, 60).rows.join("\n");
    assert!(text.contains("│ DETAIL anthropic-oauth-2 — provider-reported extras"));
    assert!(text.contains("│ previous resets  not reported by provider"));
    assert!(text.contains("│ banked resets    not reported by provider"));
    assert!(
        text.contains("│ use banked reset unavailable — provider offers no banked-reset operation")
    );
    assert_eq!(
        text.matches("│ DETAIL").count(),
        1,
        "only the selected account opens a panel"
    );
    assert!(
        !text.contains("weekly allowance"),
        "the allowance row is Haider Code's"
    );
}

#[test]
fn haider_code_allowance_is_not_reported_until_the_meter_publishes_it() {
    let mut model = calendar(UsageTimeZone::Fixed(OWNER_OFFSET));
    for _ in 0..5 {
        model.handle(key(KeyCode::Down));
    }
    assert_eq!(selected_alias(&model), "haider-code-api");
    let text = draw(&model, 118, 60).rows.join("\n");
    assert!(text.contains("│ weekly allowance not reported by provider"));
}

/// When the Haider Code meter publishes its account-wide weekly window, the
/// calendar plots it and the panel shows the real allowance — the same
/// window, nothing derived.
#[test]
fn a_published_haider_code_weekly_window_is_plotted_and_shown() {
    let mut report = owner_report();
    report.accounts[2].meter = AccountMeterStateV1::Metered {
        windows: vec![window("weekly", 0.38, Some(HC_WEEKLY_MS))],
    };
    let mut model = usage_model(
        report,
        UsageScope::Calendar,
        UsageTimeZone::Fixed(OWNER_OFFSET),
    );
    for _ in 0..5 {
        model.handle(key(KeyCode::Down));
    }
    let text = draw(&model, 118, 60).rows.join("\n");
    assert!(text.contains("30  fW"), "{text}");
    assert!(text.contains("5h reset unknown · exact window not published"));
    assert!(text.contains(
        "│ weekly allowance 62% left · resets Wed 30 Sep · 03:30 local (00:00 UTC) · in 6d 7h · weekly"
    ));
    // A LABELED weekly window is per-feature, never the account allowance.
    let mut labeled = owner_report();
    labeled.accounts[2].meter = AccountMeterStateV1::Metered {
        windows: vec![UsageWindowV1 {
            label: Some("model-x".into()),
            ..window("weekly", 0.38, Some(HC_WEEKLY_MS))
        }],
    };
    assert_eq!(account_extras(&labeled.accounts[2]).allowance, None);
}

/// Fixture-only: the panel renders every provider-reported extra the
/// backend follow-up will map — partial counters stay "not reported", a
/// journal-sourced history is labelled, and the enabled action names the
/// always-ask approval.
#[test]
fn detail_rows_render_reported_extras_without_filling_gaps() {
    let report = owner_report();
    let account = &report.accounts[3];
    let extras = AccountResetExtras {
        allowance: None,
        history: Some(vec![
            PastReset {
                window: "five_hour".into(),
                at_ms: SNAPSHOT_MS - 3 * 3_600_000,
                source: ResetHistorySource::Provider,
            },
            PastReset {
                window: "seven_day".into(),
                at_ms: 1_789_750_740_000,
                source: ResetHistorySource::DeviceJournal,
            },
            PastReset {
                window: "five_hour".into(),
                at_ms: SNAPSHOT_MS - 8 * 3_600_000,
                source: ResetHistorySource::Provider,
            },
            PastReset {
                window: "five_hour".into(),
                at_ms: SNAPSHOT_MS - 13 * 3_600_000,
                source: ResetHistorySource::Provider,
            },
        ]),
        banked: Some(BankedResets {
            banked: Some(2),
            used: Some(1),
            remaining: None,
        }),
        action: BankedResetAction::Available,
    };
    let rows = detail_rows(
        account,
        &extras,
        UsageTimeZone::Fixed(OWNER_OFFSET),
        SNAPSHOT_MS,
        2026,
    );
    let text = rows
        .iter()
        .map(|row| format!("{:<17}{}", row.label, row.text))
        .collect::<Vec<_>>();
    assert_eq!(
        text,
        vec![
            "previous resets  five_hour Wed 23 Sep · 17:30 local (14:00 UTC) · 3h 0m ago · provider",
            "                 five_hour Wed 23 Sep · 12:30 local (09:00 UTC) · 8h 0m ago · provider",
            "                 five_hour Wed 23 Sep · 07:30 local (04:00 UTC) · 13h 0m ago · provider",
            "                 +1 older",
            "banked resets    banked 2 · used 1 · remaining not reported",
            "use banked reset b · always asks for your approval before spending one",
        ]
    );
    assert_eq!(rows.last().map(|row| row.tone), Some(DetailTone::Action));

    let journal_only = AccountResetExtras {
        history: Some(vec![PastReset {
            window: "seven_day".into(),
            at_ms: 1_789_750_740_000,
            source: ResetHistorySource::DeviceJournal,
        }]),
        action: BankedResetAction::Unavailable {
            reason: "no banked resets remaining".into(),
        },
        ..AccountResetExtras::default()
    };
    let rows = detail_rows(
        account,
        &journal_only,
        UsageTimeZone::UtcOnly,
        SNAPSHOT_MS,
        2026,
    );
    assert!(rows[0].text.ends_with("· this device's meter journal"));
    assert_eq!(rows[1].text, "not reported by provider");
    assert_eq!(rows[2].text, "unavailable — no banked resets remaining");
    assert_eq!(rows[2].tone, DetailTone::Disabled);
    let empty = AccountResetExtras {
        history: Some(Vec::new()),
        ..AccountResetExtras::default()
    };
    let rows = detail_rows(account, &empty, UsageTimeZone::UtcOnly, SNAPSHOT_MS, 2026);
    assert_eq!(rows[0].text, "none reported", "a reported empty history");

    let allowance = AccountResetExtras {
        allowance: Some(AllowanceView {
            window: "weekly".into(),
            utilization: None,
            resets_at_ms: None,
        }),
        ..AccountResetExtras::default()
    };
    let rows = detail_rows(
        &report.accounts[2],
        &allowance,
        UsageTimeZone::UtcOnly,
        SNAPSHOT_MS,
        2026,
    );
    assert_eq!(
        rows[0].text,
        "remaining not reported · reset not reported · weekly"
    );
}

/// `b` never spends anything today: no provider operation is reported, so
/// it explains and queues no request.
#[test]
fn b_explains_the_unavailable_banked_reset_and_requests_nothing() {
    let mut model = calendar(UsageTimeZone::UtcOnly);
    model.handle(key(KeyCode::Down));
    model.requests.clear();
    model.handle(key(KeyCode::Char('b')));
    assert_eq!(
        model.flash.as_deref(),
        Some(
            "use banked reset: unavailable for openai-oauth — provider offers no banked-reset operation"
        )
    );
    assert!(
        !model
            .requests
            .iter()
            .any(|request| !matches!(request, AppRequest::UsageRefresh)),
        "no action request is queued"
    );
    assert_eq!(
        banked_reset_unavailable_note("x", &BankedResetAction::Available),
        None
    );
}

// ---- 6. friendlier meter failures ---------------------------------------

#[test]
fn meter_failures_keep_the_code_and_add_only_what_it_establishes() {
    for (reason, expected) in [
        (
            "credential_unavailable",
            "credential unavailable — could not load the login · try /login",
        ),
        (
            "credential_expired",
            "credential expired — re-login with /login",
        ),
        (
            "credential_revoked",
            "credential revoked — re-login with /login",
        ),
        (
            "credential_source_gone",
            "credential source gone — re-link or remove in /accounts",
        ),
        (
            "http_status_429",
            "http 429 — rate limited · f retries later",
        ),
        ("http_status_401", "http 401 — unauthorized · try /login"),
        ("http_status_503", "http 503 — provider error · f retries"),
        ("http_status_418", "http 418"),
        (
            "transport_timeout",
            "transport timeout — provider did not answer · f retries",
        ),
        (
            "malformed_response",
            "malformed response — meter reply not understood",
        ),
        ("provider_exposes_no_quota", "provider exposes no quota"),
    ] {
        assert_eq!(fmt_meter_reason_friendly(reason), expected, "{reason}");
    }
    assert!(
        !fmt_meter_reason_friendly("credential_unavailable").contains("expired"),
        "unavailable is never relabelled expired"
    );
}

// ---- 7. goldens ----------------------------------------------------------

/// Owner-shaped calendar with anthropic-oauth-2 selected, the owner's zone
/// pinned: the grid badge, the selection summary, local+UTC rows and the
/// detail panel, at the bench size and the small laptop split.
fn golden_model(theme: ThemeKey) -> AppModel {
    let mut model = calendar(UsageTimeZone::Fixed(OWNER_OFFSET));
    model.theme = theme;
    for _ in 0..3 {
        model.handle(key(KeyCode::Down));
    }
    model
}

/// Each size gets a fresh model so the follow latch armed by the last step
/// scrolls THAT frame (the selected account is on screen at 80x24 too).
#[test]
fn usage_calendar_973_dark_goldens() {
    for (width, height) in [(118, 36), (80, 24)] {
        let model = golden_model(ThemeKey::Dark);
        check_golden("w973_usage_calendar_dark", &draw(&model, width, height));
    }
}

#[test]
fn usage_calendar_973_light_goldens() {
    for (width, height) in [(118, 36), (80, 24)] {
        let model = golden_model(ThemeKey::Light);
        check_golden("w973_usage_calendar_light", &draw(&model, width, height));
    }
}

/// Review aid: `W973_DUMP=1 cargo test -p haider-tui --test
/// w973_usage_ui_tests dump -- --nocapture` prints the frames.
#[test]
fn dump_frames_for_review() {
    if std::env::var_os("W973_DUMP").is_none() {
        return;
    }
    for (width, height) in [(118, 36), (80, 24), (118, 60)] {
        let model = golden_model(ThemeKey::Dark);
        println!("──── {width}x{height} ────");
        for row in draw(&model, width, height).rows {
            println!("{}", row.trim_end());
        }
    }
    let global = usage_model(
        owner_report(),
        UsageScope::Global,
        UsageTimeZone::Fixed(OWNER_OFFSET),
    );
    println!("──── global 118x36 ────");
    for row in draw(&global, 118, 36).rows {
        println!("{}", row.trim_end());
    }
}
