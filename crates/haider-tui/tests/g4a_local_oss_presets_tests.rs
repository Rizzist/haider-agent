#![allow(clippy::expect_used)]

//! G4a — the local OSS presets (Ollama, LM Studio): the SAME custom card
//! the HF/Zen/Go presets open, prefilled with the servers' default loopback
//! origins, but KEYLESS — the configure carries `auth_requirement: none`
//! and the commit skips the masked key card entirely, going straight to
//! model discovery. LK10 pins the card prefills, the key-card skip, the
//! wire auth requirement, the footer hints, and the "start the server"
//! row hint for an empty local inventory.

use haider_protocol::credential::AuthMethod;
use haider_tui::app::{
    AccountAddKind, AppModel, AppRequest, CustomField, Hit, RuntimeMode, Screen,
};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
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

fn keyless_summary(provider: &str, models: Vec<String>) -> haider_rpc::ProviderSummaryWire {
    let available = !models.is_empty();
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:11434/v1".to_owned()),
        response_open_timeout_ms: None,
        model_details: Vec::new(),
        models,
        auth_methods: Vec::new(),
        availability: if available {
            haider_rpc::ProviderAvailabilityWire::Available
        } else {
            haider_rpc::ProviderAvailabilityWire::Unavailable
        },
        availability_reason: (!available)
            .then(|| "provider model inventory is unavailable".to_owned()),
        default_model: None,
        enabled: true,
    }
}

fn render_text(model: &mut AppModel, width: u16, height: u16) -> (String, Vec<Hit>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| {
            hits = haider_tui::render::render(model, frame)
                .into_iter()
                .map(|(_, hit)| hit)
                .collect();
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
    (text, hits)
}

/// LAW (LK10 — ollama_preset_prefills_the_local_origin): `o` on /providers
/// opens the custom card prefilled with ollama's default compat origin
/// `http://127.0.0.1:11434/v1`, alias stem `ollama`, KEYLESS, focus on the
/// model field.
/// MUTATION CHECK: typo the port (1143) or drop the keyless flag. Expected
/// RUNTIME failure: the origin or keyless assertion below.
#[test]
fn ollama_preset_prefills_the_local_origin() {
    let mut model = live_model();
    model.handle(key(KeyCode::Char('o')));
    let card = model.custom_add.as_ref().expect("ollama preset card open");
    assert!(!card.edit);
    assert!(card.keyless, "the ollama preset is auth-None");
    assert!(card.name.starts_with("ollama"), "{}", card.name);
    assert_eq!(card.origin, "http://127.0.0.1:11434/v1");
    assert!(card.model.is_empty(), "the user names the pulled model");
    assert_eq!(card.focus, CustomField::Model);
}

/// LAW (LK10 — lmstudio_preset_prefills_the_local_origin): `l` opens the
/// same keyless card at LM Studio's default `http://127.0.0.1:1234/v1`,
/// alias stem `lmstudio`.
/// MUTATION CHECK: point the LM Studio preset at the ollama origin (a
/// plausible copy-paste). Expected RUNTIME failure: the origin assertion.
#[test]
fn lmstudio_preset_prefills_the_local_origin() {
    let mut model = live_model();
    model.handle(key(KeyCode::Char('l')));
    let card = model
        .custom_add
        .as_ref()
        .expect("lmstudio preset card open");
    assert!(!card.edit);
    assert!(card.keyless, "the LM Studio preset is auth-None");
    assert!(card.name.starts_with("lmstudio"), "{}", card.name);
    assert_eq!(card.origin, "http://127.0.0.1:1234/v1");
    assert_eq!(card.focus, CustomField::Model);
}

/// LAW (LK10 — keyless commit skips the key card): submitting a keyless
/// preset sends `auth_requirement: none` on the wire, and the committed
/// configure does NOT open the masked key card — it chains straight into
/// `provider.models_refresh` for the new provider.
/// MUTATION CHECK: drop the keyless branch from `custom_add_committed`.
/// Expected RUNTIME failure: the login-card-is-None assertion (the chain
/// opens the key card) and the missing refresh command.
#[test]
fn keyless_commit_skips_the_key_card_and_chains_discovery() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    model.handle(key(KeyCode::Char('o')));
    for c in "llama3.1:8b".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));

    // The submit rode the request queue with the keyless marker.
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider {
                command_id,
                keyless,
                origin,
                ..
            } => {
                assert!(*keyless, "the ollama preset submits keyless");
                assert_eq!(origin, "http://127.0.0.1:11434/v1");
                Some(command_id.clone())
            }
            _ => None,
        })
        .expect("configure issued");

    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id,
            provider: keyless_summary("ollama", Vec::new()),
            revision: 3,
        }),
        std::time::Instant::now(),
    );
    assert!(model.custom_add.is_none(), "the card closed on commit");
    assert!(
        model.login.is_none(),
        "NO key card for a keyless provider — there is no key to add"
    );
    let refresh_queued = model.requests.iter().any(|request| {
        matches!(request, AppRequest::ProviderModelsRefresh { provider } if provider == "ollama")
    }) || pass.commands.iter().any(|command| {
        matches!(command, LiveCommand::RefreshProviderModels { provider } if provider == "ollama")
    });
    assert!(refresh_queued, "commit chains straight into discovery");
    assert!(
        model
            .providers
            .message
            .as_deref()
            .is_some_and(|message| message.contains("keyless")),
        "the commit message names the keyless contract"
    );
}

/// LAW (LK10 — footer hints updated): the /providers footer names the new
/// preset keys (`o` Ollama, `l` LM Studio) and the refresh key, and the
/// /accounts add rows offer both local preset buttons; clicking dispatches
/// the keyless card.
#[test]
fn footer_hints_and_add_buttons_offer_the_local_presets() {
    let mut model = live_model();
    let (text, _) = render_text(&mut model, 160, 42);
    assert!(text.contains("o Ollama"), "footer names the ollama key");
    assert!(
        text.contains("l LM Studio"),
        "footer names the lmstudio key"
    );
    assert!(text.contains("f refresh"), "footer names the refresh key");

    model.screen = Screen::Accounts;
    let (text, hits) = render_text(&mut model, 160, 42);
    assert!(text.contains("+ Ollama (local)"), "ollama button rendered");
    assert!(
        text.contains("+ LM Studio (local)"),
        "lmstudio button rendered"
    );
    let kinds: Vec<AccountAddKind> = hits
        .iter()
        .filter_map(|hit| match hit {
            Hit::AccountAdd(kind) => Some(*kind),
            _ => None,
        })
        .collect();
    assert!(kinds.contains(&AccountAddKind::Ollama));
    assert!(kinds.contains(&AccountAddKind::LmStudio));

    model.handle_hit(Hit::AccountAdd(AccountAddKind::LmStudio));
    let card = model.custom_add.as_ref().expect("clicked card open");
    assert_eq!(card.origin, "http://127.0.0.1:1234/v1");
    assert!(card.keyless);
}

/// LAW (LK10 — empty local discovery hint): a keyless custom provider whose
/// inventory is empty renders the actionable "start the server, then
/// refresh" row hint instead of the generic unavailable reason; a KEYED
/// custom keeps the daemon's own reason.
/// MUTATION CHECK: drop the keyless_local branch in `render_providers`.
/// Expected RUNTIME failure: the hint assertion below.
#[test]
fn empty_keyless_discovery_hints_start_the_server_then_refresh() {
    let mut model = live_model();
    let keyed = haider_rpc::ProviderSummaryWire {
        auth_methods: vec![AuthMethod::ApiKey],
        provider: "hf-proxy".to_owned(),
        ..keyless_summary("hf-proxy", Vec::new())
    };
    model
        .providers
        .apply_snapshot(vec![keyless_summary("ollama", Vec::new()), keyed], 2);
    let (text, _) = render_text(&mut model, 160, 42);
    assert!(
        text.contains("start the server, then refresh"),
        "keyless empty inventory carries the actionable hint"
    );
    assert!(
        text.contains("provider model inventory is unavailable"),
        "a keyed custom keeps the daemon's own reason"
    );

    // With models discovered, the keyless row is simply available.
    let mut model = live_model();
    model.providers.apply_snapshot(
        vec![keyless_summary("ollama", vec!["llama3.1:8b".to_owned()])],
        2,
    );
    let (text, _) = render_text(&mut model, 160, 42);
    assert!(!text.contains("start the server"));
    assert!(text.contains("available"));
}
