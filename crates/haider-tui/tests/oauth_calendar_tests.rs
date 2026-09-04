//! v0.0.970 OAuth reset calendar: exact provider timestamps projected into
//! a deterministic UTC month grid. Text and theme-token styling are pinned
//! at the three owner widths in both light and dark themes.
#![allow(clippy::expect_used)]

use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1, UsageWindowV1,
};
use haider_provider::UsageMeterEndpoint;
use haider_tui::app::{AppModel, Screen, UsageScope};
use haider_tui::theme::ThemeKey;
use ratatui::crossterm::event::KeyCode;

mod common;
mod tuivirt_common;
use common::{key, run_slash};
use tuivirt_common::{SIZES, check_golden, draw, launcher_model};

// Friday 04 September 2026, 12:00 UTC. All reset literals below are the
// fixture's exact Unix-millisecond fields; render code must not add a cadence.
const GENERATED_AT_MS: u64 = 1_788_523_200_000;
const ANTHROPIC_FIVE_HOUR_MS: u64 = 1_788_540_120_000; // Fri 04 Sep 16:42 UTC
const ANTHROPIC_WEEKLY_MS: u64 = 1_789_028_100_000; // Thu 10 Sep 08:15 UTC
const OPENAI_FIVE_HOUR_MS: u64 = 1_788_575_400_000; // Sat 05 Sep 02:30 UTC
const OPENAI_WEEKLY_MS: u64 = 1_789_149_900_000; // Fri 11 Sep 18:05 UTC

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

fn calendar_report() -> UsageReportV1 {
    UsageReportV1 {
        generated_at_ms: GENERATED_AT_MS,
        accounts: vec![
            account(
                "anthropic-oauth",
                "anthropic",
                "max@example.com",
                Some("max_20x"),
                AccountMeterStateV1::Metered {
                    windows: vec![
                        UsageWindowV1 {
                            window: "five_hour".into(),
                            utilization: 0.60,
                            resets_at_ms: Some(ANTHROPIC_FIVE_HOUR_MS),
                            label: None,
                        },
                        UsageWindowV1 {
                            window: "seven_day".into(),
                            utilization: 0.12,
                            resets_at_ms: Some(ANTHROPIC_WEEKLY_MS),
                            label: None,
                        },
                    ],
                },
            ),
            account(
                "anthropic-oauth",
                "anthropic2",
                "work@corp.dev",
                None,
                AccountMeterStateV1::Unavailable {
                    reason: "credential_unavailable".into(),
                },
            ),
            account(
                "openai-oauth",
                "codex-work",
                "dev@corp.dev",
                Some("plus"),
                AccountMeterStateV1::Metered {
                    windows: vec![
                        // A named per-model 5h limit deliberately comes first.
                        // Calendar must use the unlabeled account window below.
                        UsageWindowV1 {
                            window: "5h".into(),
                            utilization: 0.91,
                            resets_at_ms: Some(ANTHROPIC_FIVE_HOUR_MS),
                            label: Some("gpt-5.6-sol".into()),
                        },
                        UsageWindowV1 {
                            window: "5h".into(),
                            utilization: 0.42,
                            resets_at_ms: Some(OPENAI_FIVE_HOUR_MS),
                            label: None,
                        },
                        UsageWindowV1 {
                            window: "weekly".into(),
                            utilization: 0.25,
                            resets_at_ms: Some(OPENAI_WEEKLY_MS),
                            label: None,
                        },
                    ],
                },
            ),
        ],
    }
}

fn calendar_model() -> AppModel {
    calendar_model_from_report(calendar_report())
}

fn calendar_model_from_report(report: UsageReportV1) -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/usage");
    assert_eq!(model.screen, Screen::Usage);
    model.usage.apply_report(report);
    model.handle(key(KeyCode::Char('s')));
    assert_eq!(model.usage.scope, UsageScope::Calendar);
    model
}

#[test]
fn one_s_opens_calendar_then_the_existing_scope_ring_continues() {
    let mut model = launcher_model();
    run_slash(&mut model, "/usage");
    assert_eq!(model.usage.scope, UsageScope::Accounts);
    for expected in [
        UsageScope::Calendar,
        UsageScope::Global,
        UsageScope::History,
        UsageScope::Models,
        UsageScope::Accounts,
    ] {
        model.handle(key(KeyCode::Char('s')));
        assert_eq!(model.usage.scope, expected);
    }
}

#[test]
fn calendar_uses_exact_fixture_resets_and_names_unknowns_without_guessing() {
    let model = calendar_model();
    let frame = draw(&model, 118, 36);
    let text = frame.rows.join("\n");
    assert!(text.contains("[04] today"), "report date is calendar today");
    assert!(
        text.contains("[04] a5"),
        "Anthropic five-hour day is marked"
    );
    assert!(text.contains("10  aW"), "Anthropic weekly day is marked");
    assert!(
        text.contains("05  c5"),
        "OpenAI base five-hour day is marked"
    );
    assert!(text.contains("11  cW"), "OpenAI weekly day is marked");
    assert!(text.contains("5h Fri 04 September 2026 · 16:42 UTC · five_hour"));
    assert!(text.contains("weekly Thu 10 September 2026 · 08:15 UTC · seven_day"));
    assert!(text.contains("5h Sat 05 September 2026 · 02:30 UTC · 5h"));
    assert!(text.contains("weekly Fri 11 September 2026 · 18:05 UTC · weekly"));
    assert!(
        text.contains(
            "5h reset unknown · weekly reset unknown · meter unavailable · credential unavailable"
        ),
        "unavailable means unknown, never an inferred cadence"
    );
    assert_eq!(
        text.matches("Fri 04 September 2026 · 16:42 UTC").count(),
        1,
        "the named OpenAI model window is not substituted for its base reset"
    );
}

#[test]
fn calendar_projects_the_existing_provider_fixtures_exactly() {
    let openai = UsageMeterEndpoint::OpenAiOauth
        .parse(
            200,
            include_bytes!("../../haider-provider/tests/fixtures/usage/openai_wham_usage.json"),
        )
        .expect("OpenAI meter fixture parses");
    assert_eq!(openai.windows[0].resets_at_ms, Some(1_738_300_000_000));
    assert_eq!(openai.windows[1].resets_at_ms, Some(1_738_900_000_000));
    let openai_text = draw(
        &calendar_model_from_report(UsageReportV1 {
            generated_at_ms: 1_738_238_400_000,
            accounts: vec![account(
                "openai-oauth",
                "codex-fixture",
                "fixture@example.com",
                openai.plan.as_deref(),
                AccountMeterStateV1::Metered {
                    windows: openai.windows,
                },
            )],
        }),
        118,
        36,
    )
    .rows
    .join("\n");
    assert!(openai_text.contains("31  a5"));
    assert!(openai_text.contains("07  aW"));
    assert!(openai_text.contains("5h Fri 31 January 2025 · 05:06 UTC · 5h"));
    assert!(openai_text.contains("weekly Fri 07 February 2025 · 03:46 UTC · weekly"));

    let anthropic = UsageMeterEndpoint::AnthropicOauth
        .parse(
            200,
            include_bytes!(
                "../../haider-provider/tests/fixtures/usage/anthropic_oauth_usage_live.json"
            ),
        )
        .expect("Anthropic meter fixture parses");
    assert_eq!(anthropic.windows[0].resets_at_ms, Some(1_785_923_400_833));
    assert_eq!(anthropic.windows[1].resets_at_ms, Some(1_785_963_600_833));
    let anthropic_text = draw(
        &calendar_model_from_report(UsageReportV1 {
            generated_at_ms: 1_785_888_000_000,
            accounts: vec![account(
                "anthropic-oauth",
                "claude-fixture",
                "fixture@example.com",
                anthropic.plan.as_deref(),
                AccountMeterStateV1::Metered {
                    windows: anthropic.windows,
                },
            )],
        }),
        118,
        36,
    )
    .rows
    .join("\n");
    assert!(anthropic_text.contains("[05] a5,aW"));
    assert!(anthropic_text.contains("5h Wed 05 August 2026 · 09:50 UTC · five_hour"));
    assert!(anthropic_text.contains("weekly Wed 05 August 2026 · 21:00 UTC · seven_day"));
}

#[test]
fn calendar_spills_past_month_end_and_never_guesses_ambiguous_windows() {
    let mut report = calendar_report();
    report.generated_at_ms = 1_790_683_200_000; // Tue 29 Sep 2026 12:00 UTC
    let AccountMeterStateV1::Metered { windows } = &mut report.accounts[0].meter else {
        panic!("fixture account is metered");
    };
    windows[1].resets_at_ms = Some(1_791_876_300_000); // Tue 13 Oct 2026 07:25 UTC
    report.accounts.push(account(
        "openai-oauth",
        "ambiguous",
        "ambiguous@example.com",
        Some("plus"),
        AccountMeterStateV1::Metered {
            windows: vec![UsageWindowV1 {
                window: "5h".into(),
                utilization: 0.2,
                resets_at_ms: Some(OPENAI_FIVE_HOUR_MS),
                label: Some("model-a".into()),
            }],
        },
    ));
    let text = draw(&calendar_model_from_report(report), 118, 42)
        .rows
        .join("\n");
    assert!(text.contains("[29] today"));
    assert!(
        text.contains("13  aW"),
        "the Oct 13 weekly marker proves the grid reaches two weeks past Sep 29"
    );
    assert!(text.contains("ambiguous"));
    assert!(text.contains("5h reset unknown · exact window not published"));
}

#[test]
fn angle_keys_still_switch_accounts_while_the_calendar_lists_every_account() {
    let mut model = calendar_model();
    let before = draw(&model, 118, 36).rows.join("\n");
    assert!(before.contains("> a  anthropic"));
    assert!(before.contains("  b  anthropic2"));
    assert!(before.contains("  c  codex-work"));

    model.handle(key(KeyCode::Char('>')));
    let after = draw(&model, 118, 36).rows.join("\n");
    assert!(after.contains("> b  anthropic2"));
    model.handle(key(KeyCode::Char('<')));
    let restored = draw(&model, 118, 36).rows.join("\n");
    assert!(restored.contains("> a  anthropic"));
}

#[test]
fn direct_calendar_scope_keeps_the_provider_filter() {
    let mut model = launcher_model();
    run_slash(&mut model, "/usage calendar openai");
    model.usage.apply_report(calendar_report());
    assert_eq!(model.usage.scope, UsageScope::Calendar);
    assert_eq!(model.usage.filter.as_deref(), Some("openai"));
    let text = draw(&model, 118, 36).rows.join("\n");
    assert!(text.contains("codex-work"));
    assert!(!text.contains("anthropic2"));
}

#[test]
fn calendar_dark_goldens_at_owner_widths() {
    let mut model = calendar_model();
    model.theme = ThemeKey::Dark;
    for (width, height) in SIZES {
        check_golden("oauth_calendar_dark", &draw(&model, width, height));
    }
}

#[test]
fn calendar_light_goldens_at_owner_widths() {
    let mut model = calendar_model();
    model.theme = ThemeKey::Light;
    for (width, height) in SIZES {
        check_golden("oauth_calendar_light", &draw(&model, width, height));
    }
}
