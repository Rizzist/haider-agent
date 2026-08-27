#![allow(clippy::expect_used)]

//! G4b — the enterprise provider cards (Azure OpenAI, Bedrock mantle,
//! Vertex): the SAME custom-card machinery reshaped per surface. Azure
//! derives `{endpoint}/openai/v1` and chains the key card; bedrock/vertex
//! re-configure their BUILTIN profiles (region / project+location), echo
//! the seeded model inventory, and chain the key card; the footer offers
//! the new keys and buttons; a daemon that does not list the builtin gets
//! a stale-daemon note instead of a card.
#![allow(clippy::expect_used)]

use haider_tui::app::{AccountAddKind, AppModel, CustomField, Hit, RuntimeMode, Screen};
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

fn enterprise_summary(
    provider: &str,
    endpoint: Option<&str>,
    models: &[&str],
    default_model: &str,
) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::AnthropicMessages,
        endpoint: endpoint.map(str::to_owned),
        response_open_timeout_ms: None,
        models: models.iter().map(|model| (*model).to_owned()).collect(),
        model_details: Vec::new(),
        auth_methods: vec![haider_protocol::credential::AuthMethod::ApiKey],
        availability: haider_rpc::ProviderAvailabilityWire::Unavailable,
        availability_reason: Some("provider has no credential".to_owned()),
        default_model: Some(default_model.to_owned()),
        enabled: true,
    }
}

const BEDROCK_SEEDS: [&str; 6] = [
    "anthropic.claude-fable-5",
    "anthropic.claude-opus-5",
    "anthropic.claude-opus-4-8",
    "anthropic.claude-opus-4-7",
    "anthropic.claude-sonnet-5",
    "anthropic.claude-haiku-4-5",
];

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

/// LAW (azure card): `a` opens the azure-shaped card, submit derives the
/// `/openai/v1` base and rides `provider.configure` with the
/// chat-completions family and the deployment name as inventory+default,
/// and the COMMIT chains straight into the masked key card (the api-key).
///
/// MUTATION CHECK: drop the `/openai/v1` derivation from `azure_v1_base`.
/// Expected RUNTIME failure: the origin equality below.
#[test]
fn azure_card_derives_the_v1_base_and_chains_the_key_card() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    model.handle(key(KeyCode::Char('a')));
    let card = model.custom_add.as_ref().expect("azure card open");
    assert_eq!(card.focus, CustomField::Origin);
    for c in "contoso.openai.azure.com".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Tab));
    for c in "my-gpt-deployment".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));

    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider {
                command_id,
                provider,
                origin,
                family,
                models,
                default_model,
                keyless,
                ..
            } => {
                assert_eq!(provider, "azure");
                assert_eq!(
                    origin, "https://contoso.openai.azure.com/openai/v1",
                    "the resource endpoint derives the v1 base"
                );
                assert_eq!(
                    *family,
                    haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
                );
                assert!(models.is_empty(), "azure rides the single-model shape");
                assert!(default_model.is_none());
                assert!(!keyless);
                Some(command_id.clone())
            }
            _ => None,
        })
        .expect("configure issued");

    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id,
            provider: enterprise_summary(
                "azure",
                Some("https://contoso.openai.azure.com/openai/v1"),
                &["my-gpt-deployment"],
                "my-gpt-deployment",
            ),
            revision: 2,
        }),
        std::time::Instant::now(),
    );
    assert!(model.custom_add.is_none(), "the card closed on commit");
    let login = model.login.as_ref().expect("key card chained");
    assert_eq!(login.provider, "azure");
    assert!(
        model
            .accounts
            .message
            .as_deref()
            .is_some_and(|message| message.contains("api-key")),
        "the commit copy names the api-key header contract"
    );
}

/// LAW (bedrock card): `b` opens the region card prefilled from the LISTED
/// builtin profile, submit builds the mantle URL, states the
/// anthropic-messages family, ECHOES the seeded inventory + default, and
/// the commit chains the bearer-key card. A daemon that does not list
/// `bedrock` gets a stale-daemon note and NO card (both directions).
///
/// MUTATION CHECK: submit `card.origin` verbatim instead of
/// `bedrock_mantle_url(..)`. Expected RUNTIME failure: the mantle-URL
/// equality below.
#[test]
fn bedrock_card_builds_the_mantle_url_and_echoes_the_seeded_inventory() {
    // NO bedrock in the daemon's provider list: the card refuses honestly.
    let mut gated = live_model();
    gated.handle(key(KeyCode::Char('b')));
    assert!(gated.custom_add.is_none(), "no card without the builtin");
    assert!(
        gated.providers.message.is_some(),
        "a stale-daemon note shows"
    );

    let mut model = live_model();
    model.providers.apply_snapshot(
        vec![enterprise_summary(
            "bedrock",
            Some("https://bedrock-mantle.us-east-1.api.aws/anthropic"),
            &BEDROCK_SEEDS,
            "anthropic.claude-fable-5",
        )],
        1,
    );
    let mut driver = LiveDriver::new("test");
    model.handle(key(KeyCode::Char('b')));
    let card = model.custom_add.as_ref().expect("bedrock card open");
    assert_eq!(card.origin, "us-east-1", "region prefills from the profile");
    // us-east-1 -> us-east-2.
    model.handle(key(KeyCode::Backspace));
    model.handle(key(KeyCode::Char('2')));
    model.handle(key(KeyCode::Enter));

    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider {
                command_id,
                provider,
                origin,
                family,
                models,
                default_model,
                ..
            } => {
                assert_eq!(provider, "bedrock");
                assert_eq!(
                    origin, "https://bedrock-mantle.us-east-2.api.aws/anthropic",
                    "the region field builds the mantle URL"
                );
                assert_eq!(
                    *family,
                    haider_rpc::ProviderApiFamilyWire::AnthropicMessages
                );
                assert_eq!(
                    models,
                    &BEDROCK_SEEDS
                        .iter()
                        .map(|slug| (*slug).to_owned())
                        .collect::<Vec<_>>(),
                    "the seeded inventory is echoed, never wiped"
                );
                assert_eq!(default_model.as_deref(), Some("anthropic.claude-fable-5"));
                Some(command_id.clone())
            }
            _ => None,
        })
        .expect("configure issued");

    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id,
            provider: enterprise_summary(
                "bedrock",
                Some("https://bedrock-mantle.us-east-2.api.aws/anthropic"),
                &BEDROCK_SEEDS,
                "anthropic.claude-fable-5",
            ),
            revision: 2,
        }),
        std::time::Instant::now(),
    );
    let login = model.login.as_ref().expect("bearer-key card chained");
    assert_eq!(login.provider, "bedrock");
}

/// LAW (vertex card): `v` collects project + location (location defaults
/// `global`), submit builds the publishers-models URL, echoes the seeded
/// inventory, and the commit copy names the ~1h token / gcloud alternative
/// before the key card opens.
#[test]
fn vertex_card_collects_project_and_location() {
    let seeds = [
        "claude-fable-5",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-sonnet-4-5@20250929",
        "claude-haiku-4-5@20251001",
    ];
    let mut model = live_model();
    model.providers.apply_snapshot(
        vec![enterprise_summary("vertex", None, &seeds, "claude-fable-5")],
        1,
    );
    let mut driver = LiveDriver::new("test");
    model.handle(key(KeyCode::Char('v')));
    let card = model.custom_add.as_ref().expect("vertex card open");
    assert_eq!(card.extra, "global", "location prefills global");
    for c in "acme-ai".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));

    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider {
                command_id,
                provider,
                origin,
                models,
                ..
            } => {
                assert_eq!(provider, "vertex");
                assert_eq!(
                    origin,
                    "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models"
                );
                assert_eq!(models.len(), seeds.len());
                Some(command_id.clone())
            }
            _ => None,
        })
        .expect("configure issued");

    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id,
            provider: enterprise_summary(
                "vertex",
                Some(
                    "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
                ),
                &seeds,
                "claude-fable-5",
            ),
            revision: 2,
        }),
        std::time::Instant::now(),
    );
    let login = model.login.as_ref().expect("token card chained");
    assert_eq!(login.provider, "vertex");
    assert!(
        model
            .accounts
            .message
            .as_deref()
            .is_some_and(|message| message.contains("gcloud")),
        "the commit copy offers the gcloud auto-refresh alternative"
    );
}

/// LAW (surfaces): the /providers footer names the enterprise keys, the
/// add rows offer the three buttons (Custom still last), and `e` on a
/// bedrock row opens the REGION card (never the generic edit card, whose
/// identity family would be refused).
#[test]
fn enterprise_footer_buttons_and_edit_routing() {
    let mut model = live_model();
    model.providers.apply_snapshot(
        vec![enterprise_summary(
            "bedrock",
            Some("https://bedrock-mantle.eu-central-1.api.aws/anthropic"),
            &BEDROCK_SEEDS,
            "anthropic.claude-fable-5",
        )],
        1,
    );
    let (text, hits) = render_text(&mut model, 118, 44);
    assert!(
        text.contains("enterprise: a Azure · b Bedrock · v Vertex"),
        "the footer names the enterprise keys:\n{text}"
    );
    for expected in ["+ Azure OpenAI", "+ Bedrock (Claude)", "+ Vertex (Claude)"] {
        assert!(text.contains(expected), "button `{expected}` renders");
    }
    for kind in [
        AccountAddKind::AzureOpenAi,
        AccountAddKind::Bedrock,
        AccountAddKind::Vertex,
    ] {
        assert!(
            hits.contains(&Hit::AccountAdd(kind)),
            "button hit for {kind:?}"
        );
    }
    let custom_index = text.find("+ Custom (OpenAI-compatible)").expect("custom");
    let vertex_index = text.find("+ Vertex (Claude)").expect("vertex");
    assert!(
        vertex_index < custom_index,
        "Custom stays last (B6b edge rule)"
    );

    // `e` on the bedrock row routes to the region card, prefilled from the
    // stored endpoint.
    model.handle(key(KeyCode::Char('e')));
    let card = model.custom_add.as_ref().expect("bedrock edit card");
    assert_eq!(card.origin, "eu-central-1");
    assert_eq!(card.name, "bedrock");
}
