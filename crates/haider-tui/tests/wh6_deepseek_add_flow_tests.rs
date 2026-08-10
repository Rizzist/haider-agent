//! WH6 — the named DeepSeek add-card rides the existing masked U-wave
//! vault-stage/login transaction, then refreshes the authenticated catalog.
#![allow(clippy::expect_used)]

use haider_tui::app::{
    AccountAddKind, AppEvent, AppModel, Hit, LoginFocus, LoginStage, RuntimeMode, Screen,
};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn draw(model: &AppModel) -> (String, Vec<(ratatui::layout::Rect, Hit)>) {
    let backend = TestBackend::new(160, 44);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw DeepSeek flow");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    (text, hits)
}

/// MUTATION CHECK: remove the card/hit, route `d` to a custom provider,
/// bypass masked staging, change the provider id, or omit post-login live
/// discovery. Each step below observes that seam directly.
#[test]
fn wh6_deepseek_add_card_and_validate_flow() {
    let mut model: AppModel = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_version = Some("0.0.83".to_owned());
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    run_slash(&mut model, "/providers");
    model.requests.clear();

    let (providers_frame, hits) = draw(&model);
    assert!(providers_frame.contains("[+ DeepSeek (API)]"));
    assert!(
        hits.iter()
            .any(|(_, hit)| { matches!(hit, Hit::AccountAdd(AccountAddKind::DeepSeekApi)) })
    );

    key(&mut model, KeyCode::Char('d'));
    assert_eq!(model.screen, Screen::Accounts);
    let card = model.login.as_ref().expect("DeepSeek key card opens");
    assert_eq!(card.provider, "deepseek");
    assert_eq!(card.alias, "deepseek-api");
    assert_eq!(card.focus, LoginFocus::Key);

    let secret = "DEEPSEEK_TUI_SECRET_SENTINEL_521c";
    for character in secret.chars() {
        key(&mut model, KeyCode::Char(character));
    }
    let (masked_frame, _) = draw(&model);
    assert!(!masked_frame.contains(secret), "raw key must never render");
    key(&mut model, KeyCode::Enter);
    assert!(matches!(
        model.login.as_ref().expect("card remains").stage,
        LoginStage::Submitting
    ));

    let mut driver = LiveDriver::new("wh6");
    let now = std::time::Instant::now();
    let staged = live_pass(&mut driver, &mut model, None, now);
    let attempt = staged
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::Stage {
                provider,
                alias,
                attempt,
                ..
            } if provider == "deepseek" && alias.as_deref() == Some("deepseek-api") => {
                Some(*attempt)
            }
            _ => None,
        })
        .expect("DeepSeek key stages through the existing vault command");

    let login = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "deepseek-vault-reference".to_owned(),
            provider: "deepseek".to_owned(),
            alias: Some("deepseek-api".to_owned()),
            attempt,
        }),
        now,
    );
    let command_id = login
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::LoginApi {
                command_id,
                provider,
                alias,
                vault_reference,
                attempt: login_attempt,
            } if provider == "deepseek"
                && alias.as_deref() == Some("deepseek-api")
                && vault_reference == "deepseek-vault-reference"
                && *login_attempt == attempt =>
            {
                Some(command_id.clone())
            }
            _ => None,
        })
        .expect("DeepSeek login follows the owned stage");

    let committed = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::LoggedIn {
            command_id,
            identity: "deepseek api key".to_owned(),
        }),
        now,
    );
    assert!(matches!(
        &model.login.as_ref().expect("success card").stage,
        LoginStage::Done(identity) if identity == "deepseek api key"
    ));
    assert!(
        committed
            .commands
            .iter()
            .any(|command| matches!(command, LiveCommand::AccountList)),
        "committed vault+descriptor truth refreshes account rows"
    );
    assert!(
        committed.commands.iter().any(|command| matches!(
            command,
            LiveCommand::RefreshProviderModels { provider } if provider == "deepseek"
        )),
        "the validated key immediately probes the live DeepSeek /models catalog"
    );
}
