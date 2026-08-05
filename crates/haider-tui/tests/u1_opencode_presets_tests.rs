//! U1 — the OpenCode Zen/Go one-click presets: the SAME custom-provider
//! card the HuggingFace preset opens (W10b), prefilled with the researched
//! gateway origins. The daemon owns everything after ⏎ (provider.configure,
//! then the masked key login = Bearer OPENCODE_API_KEY); model inventory is
//! the unauthenticated `GET {origin}/models` discovery both gateways serve.
#![allow(clippy::expect_used)]

use haider_tui::app::{AccountAddKind, AppModel, CustomField, RuntimeMode, Screen};
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model};

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 1);
    model.screen = Screen::Providers;
    model
}

/// LAW (opencode_zen_preset_prefills_the_zen_gateway): `z` on /providers
/// opens the custom card prefilled with the Zen origin
/// `https://opencode.ai/zen/v1`, alias stem `opencode-zen`, focus on the
/// model field — the exact HuggingFace preset contract with Zen
/// coordinates.
/// MUTATION CHECK: drop the Zen preset arm or typo the origin. Expected
/// RUNTIME failure: `z` opens nothing, or the origin assertion below.
#[test]
fn opencode_zen_preset_prefills_the_zen_gateway() {
    let mut model = live_model();
    model.handle(key(KeyCode::Char('z')));
    let card = model.custom_add.as_ref().expect("zen preset card open");
    assert!(!card.edit);
    assert!(card.name.starts_with("opencode-zen"), "{}", card.name);
    assert_eq!(card.origin, "https://opencode.ai/zen/v1");
    assert!(card.model.is_empty(), "the user supplies the served model");
    assert_eq!(card.focus, CustomField::Model);
}

/// LAW (opencode_go_preset_prefills_the_go_gateway): `g` opens the same
/// card with the Go lane's own origin `https://opencode.ai/zen/go/v1` and
/// alias stem `opencode-go`.
/// MUTATION CHECK: point the Go preset at the Zen origin (a plausible
/// copy-paste). Expected RUNTIME failure: the origin assertion below.
#[test]
fn opencode_go_preset_prefills_the_go_gateway() {
    let mut model = live_model();
    model.handle(key(KeyCode::Char('g')));
    let card = model.custom_add.as_ref().expect("go preset card open");
    assert!(!card.edit);
    assert!(card.name.starts_with("opencode-go"), "{}", card.name);
    assert_eq!(card.origin, "https://opencode.ai/zen/go/v1");
    assert_eq!(card.focus, CustomField::Model);
}

/// LAW (presets_gate_on_the_daemon_configure_feature): without
/// `provider_configure_v1` a live TUI refuses the preset with the stale
/// daemon note instead of opening a card it cannot commit.
#[test]
fn presets_gate_on_the_daemon_configure_feature() {
    let mut model = live_model();
    model
        .daemon_features
        .remove(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1);
    model.handle(key(KeyCode::Char('z')));
    assert!(model.custom_add.is_none(), "no card without the feature");
    assert!(
        model.providers.message.is_some(),
        "the refusal is explained"
    );
}

/// LAW (accounts_add_rows_offer_both_presets): the /accounts add buttons
/// include `+ OpenCode Zen` and `+ OpenCode Go` (between HuggingFace and
/// Custom), and clicking each opens its preset card.
#[test]
fn accounts_add_rows_offer_both_presets() {
    use haider_tui::app::Hit;
    use haider_tui::render::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut model = live_model();
    model.screen = Screen::Accounts;
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| {
            hits = render(&model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    assert!(text.contains("+ OpenCode Zen"), "zen button rendered");
    assert!(text.contains("+ OpenCode Go"), "go button rendered");
    let kinds: Vec<AccountAddKind> = hits
        .iter()
        .filter_map(|(_, hit)| match hit {
            Hit::AccountAdd(kind) => Some(*kind),
            _ => None,
        })
        .collect();
    assert!(kinds.contains(&AccountAddKind::OpencodeZen));
    assert!(kinds.contains(&AccountAddKind::OpencodeGo));

    // Clicking dispatches the preset (value-carrying hit, same as HF).
    model.handle_hit(Hit::AccountAdd(AccountAddKind::OpencodeGo));
    let card = model.custom_add.as_ref().expect("clicked card open");
    assert_eq!(card.origin, "https://opencode.ai/zen/go/v1");
}
