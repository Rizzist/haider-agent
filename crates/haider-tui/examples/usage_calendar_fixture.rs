//! Dev tool (973 usage UI): run the REAL interactive TUI loop (the demo
//! runtime: real terminal, real key handling, real render) with a FIXTURE
//! `usage.report` pre-loaded, so the `/usage` calendar can be exercised by
//! hand or in tmux without live accounts:
//!
//! ```sh
//! TZ=Asia/Tehran cargo run -p haider-tui --example usage_calendar_fixture
//! # then type `/usage calendar` ⏎ and use ↑↓ ←→ / b / s
//! ```
//!
//! The report is synthetic (identities are `example.*` addresses) and is
//! injected only here — the shipped binaries never load it. Local times
//! come from the host zone exactly as in the live TUI (`TZ` selects it).
#![allow(clippy::expect_used)]

use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1, UsageWindowV1,
};
use haider_tui::app::AppModel;
use haider_tui::theme::{ThemeChoice, ThemeKey};
use haider_tui::usage_time::UsageTimeZone;

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

fn window(name: &str, utilization: f64, resets_at_ms: u64) -> UsageWindowV1 {
    UsageWindowV1 {
        window: name.to_owned(),
        utilization,
        resets_at_ms: Some(resets_at_ms),
        label: None,
    }
}

/// The owner's 972 screenshot shape: six accounts whose REPORT order
/// interleaves three providers. Snapshot: Wed 23 Sep 2026 17:00 UTC.
fn fixture_report() -> UsageReportV1 {
    let unavailable = |reason: &str| AccountMeterStateV1::Unavailable {
        reason: reason.to_owned(),
    };
    UsageReportV1 {
        generated_at_ms: 1_790_182_800_000,
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
                AccountMeterStateV1::Metered {
                    windows: vec![window("weekly", 0.38, 1_790_726_400_000)],
                },
            ),
            account(
                "anthropic-oauth",
                "anthropic-oauth-2",
                "claude2@example.com",
                None,
                AccountMeterStateV1::Metered {
                    windows: vec![
                        window("five_hour", 0.4, 1_790_197_740_000),
                        window("seven_day", 0.2, 1_790_355_540_000),
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
                        window("five_hour", 0.7, 1_790_216_100_000),
                        window("seven_day", 0.5, 1_790_586_000_000),
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

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut model = AppModel::new();
    model.identity.device = "fixture-box".to_owned();
    model.apply_theme_choice(ThemeChoice::Fixed(ThemeKey::Dark));
    model.usage.apply_report(fixture_report());
    model.usage.time_zone = UsageTimeZone::Host;
    haider_tui::runtime::run_demo(model, None).await
}
