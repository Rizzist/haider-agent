//! Dev tool (973 usage UI): run the REAL interactive TUI loop (the demo
//! runtime: real terminal, real key handling, real render) with a FIXTURE
//! `usage.report` pre-loaded, so the `/usage` calendar can be exercised by
//! hand or in tmux without live accounts:
//!
//! ```sh
//! TZ=Asia/Tehran cargo run -p haider-tui --example usage_calendar_fixture
//! # then type `/usage calendar` ⏎ and use ↑↓ ←→ / b / s
//! TZ=Asia/Tehran cargo run -p haider-tui --example usage_calendar_fixture -- --plain --down 3
//! ```
//!
//! The report is synthetic (identities are `example.*` addresses) and is
//! injected only here — the shipped binaries never load it. Local times
//! come from the host zone exactly as in the live TUI (`TZ` selects it).
#![allow(clippy::expect_used)]

use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1,
    UsageResetCreditV1, UsageResetCreditsV1, UsageResetHistoryEntryV1, UsageResetHistorySourceV1,
    UsageWindowV1,
};
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::theme::{ThemeChoice, ThemeKey};
use haider_tui::usage_time::UsageTimeZone;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        reset_credits: None,
        previous_resets: Vec::new(),
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
        retry_at_ms: None,
    };
    let mut report = UsageReportV1 {
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
    };
    report.accounts[0].reset_credits = Some(UsageResetCreditsV1 {
        available_count: 2,
        applicable_available_count: Some(1),
        total_earned_count: Some(3),
        used_count: Some(1),
        remaining_count: None,
        apply_supported: true,
        credits: vec![UsageResetCreditV1 {
            reset_type: "banked_reset".into(),
            status: "available".into(),
            title: Some("Synthetic earned reset".into()),
            description: Some("Synthetic provider detail for UI verification".into()),
            granted_at_ms: Some(1_790_179_200_000),
            expires_at_ms: Some(1_790_269_200_000),
            redeemed_at_ms: None,
        }],
    });
    report.accounts[0].previous_resets = vec![UsageResetHistoryEntryV1 {
        reset_at_ms: 1_790_096_400_000,
        source: UsageResetHistorySourceV1::Provider,
        window: None,
        kind: Some("banked_reset".into()),
        observed_at_ms: Some(1_790_182_800_000),
    }];
    report.accounts[0].meter = AccountMeterStateV1::Unavailable {
        reason: "http_status_429".into(),
        retry_at_ms: Some(1_790_186_400_000),
    };
    report
}

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

/// `--plain [--down N]`: open `/usage calendar`, step the selection N rows
/// with the real key handler, press `b`, and print the repo's plain oracle
/// (`run_demo_plain`, which appends the `/usage` plain grammar) instead of
/// starting the interactive loop.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let plain = args.iter().any(|arg| arg == "--plain");
    let downs = args
        .iter()
        .position(|arg| arg == "--down")
        .and_then(|at| args.get(at + 1))
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    let mut model = AppModel::new();
    model.identity.device = "fixture-box".to_owned();
    model.apply_theme_choice(ThemeChoice::Fixed(ThemeKey::Dark));
    model.usage.apply_report(fixture_report());
    model.usage.time_zone = UsageTimeZone::Host;
    if !plain {
        return haider_tui::runtime::run_demo(model, None).await;
    }
    // Leave the boot screen the way the runtime does, so the composer takes
    // the slash command.
    model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::HarnessStatus(haider_protocol::state::HarnessStatus::Ready),
    )));
    for c in "/usage calendar".chars() {
        key(&mut model, KeyCode::Char(c));
    }
    key(&mut model, KeyCode::Esc);
    key(&mut model, KeyCode::Enter);
    for _ in 0..downs {
        key(&mut model, KeyCode::Down);
    }
    if model.screen != haider_tui::app::Screen::Usage {
        eprintln!("usage_calendar_fixture: /usage did not open");
        std::process::exit(1);
    }
    key(&mut model, KeyCode::Char('b'));
    let banked_note = model.flash.clone();
    print!("{}", haider_tui::runtime::run_demo_plain(model));
    if let Some(note) = banked_note {
        println!("b pressed: {note}");
    }
    Ok(())
}
