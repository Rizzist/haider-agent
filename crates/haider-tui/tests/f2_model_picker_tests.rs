//! F2a — the full-screen `/model` picker: exact OAuth subscription pairs plus
//! one API row per model slug, with an exact-provider second stage;
//! receipted live selection renders the RESOLVED pair.
//!
//! The laws:
//! * bare `/model` opens a FULL-SCREEN picker with every OAuth pair and one
//!   collapsed API choice per slug; `/model <query>` pre-fills the search.
//! * KEY OWNERSHIP (heeded history): while open the picker owns every
//!   key — ⏎ acts on the HIGHLIGHTED row (never an exact-match jump), esc
//!   returns from providers before closing, and characters edit stage search.
//! * live sessions issue `session.select_model` and render the RESOLVED
//!   pair from the reply — never an echo of the request.
//! * typed refusals land INLINE; the row stays selectable for a retry;
//!   a refusal landing after the picker closed reaches the session view.
//! * unavailable providers render dimmed with their reason and REFUSE
//!   with it — never a silent failure.
//! * at the launcher a selection sets the default pair new sessions use.
#![allow(clippy::expect_used)]

use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::SessionId;
use haider_rpc::{ProviderAvailabilityWire, ProviderTrustWire};
use haider_tui::app::{AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::commands::{PaletteItem, palette_items};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, run_slash};

fn seeded_launcher() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = ["provider_models_v1", "session_model_select_v1"]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    model.daemon_version = Some("0.0.67".to_owned());
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model
}

fn sid() -> SessionId {
    SessionId::new("f2-picker-session")
}

fn seeded_session() -> AppModel {
    let mut model = seeded_launcher();
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(model.screen, Screen::Session);
    model.requests.clear();
    model
}

struct ApiProviderFixture<'a> {
    provider: &'a str,
    model: &'a str,
    available: bool,
    reason: Option<&'a str>,
    context_window: Option<u64>,
    fetched_at_ms: Option<u64>,
    lockdown: bool,
    is_default: bool,
}

fn add_api_provider(model: &mut AppModel, fixture: ApiProviderFixture<'_>) {
    let mut summary = model
        .providers
        .providers
        .iter()
        .find(|summary| summary.provider == "gemini")
        .expect("seeded API provider")
        .clone();
    summary.provider = fixture.provider.to_owned();
    summary.models = vec![fixture.model.to_owned()];
    let mut detail = summary.model_details[0].clone();
    detail.name = fixture.model.to_owned();
    detail.context_window = fixture.context_window;
    summary.model_details = vec![detail];
    summary.auth_methods = vec![AuthMethod::ApiKey];
    summary.availability = if fixture.available {
        ProviderAvailabilityWire::Available
    } else {
        ProviderAvailabilityWire::Unavailable
    };
    summary.availability_reason = fixture.reason.map(str::to_owned);
    summary.inventory_fetched_at_ms = fixture.fetched_at_ms;
    summary.trust = if fixture.lockdown {
        ProviderTrustWire::Lockdown
    } else {
        ProviderTrustWire::Full
    };
    summary.default_model = fixture.is_default.then(|| fixture.model.to_owned());
    model.providers.providers.push(summary);
}

fn draw_rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn pass(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    reply: Option<LiveReply>,
) -> Vec<LiveCommand> {
    live_pass(driver, model, reply, std::time::Instant::now()).commands
}

/// MUTATION CHECK (F2a): route bare `/model` back to the old flash list.
/// Expected runtime failure: no picker opens and every visible-choice
/// assertion below fails.
#[test]
fn bare_model_opens_the_full_screen_picker_with_oauth_pairs_and_api_choices() {
    let mut model = seeded_launcher();
    run_slash(&mut model, "/model");
    assert!(model.model_picker.is_some(), "the picker opens");
    let rows = model.model_picker_rows();
    for (m, p) in [
        ("gpt-5.6", "openai"),
        ("gpt-5.6-codex", "openai"),
        ("claude-opus-5", "anthropic"),
        ("claude-sonnet-5", "anthropic"),
        ("gemini-3", "gemini"),
        ("deepseek-chat", "deepseek"),
        ("deepseek-reasoner", "deepseek"),
        ("qwen3-coder", "local"),
        ("llama-3-70b", "huggingface"),
    ] {
        assert!(
            rows.iter().any(|row| row.model == m && row.provider == p),
            "pair {m} × {p} must be offered"
        );
    }
    // The signed-out kimi builtin: one honest placeholder row.
    let kimi = rows
        .iter()
        .find(|row| row.provider == "kimi-oauth")
        .expect("kimi placeholder row");
    assert!(!kimi.selectable && !kimi.available);
    assert_eq!(
        kimi.reason.as_deref(),
        Some("provider model inventory is unavailable")
    );
    let deepseek = rows
        .iter()
        .find(|row| row.provider == "deepseek" && row.model == "deepseek-reasoner")
        .expect("DeepSeek pair row");
    assert!(
        deepseek.selectable && !deepseek.available,
        "discovered pairs stay visible/selectable while availability gates the switch"
    );
    assert_eq!(
        deepseek.reason.as_deref(),
        Some("provider has no credential")
    );
    // Full screen: title + search bar + rows render over the launcher.
    let rows_text = draw_rows(&model, 110, 30);
    let text = rows_text.join("\n");
    assert!(text.contains("MODELS"), "full-screen title");
    assert!(
        text.contains("OAuth subscriptions + one API choice per model"),
        "the title names the actual top-level grammar"
    );
    assert!(
        text.contains("choices ·"),
        "the count names visible choices"
    );
    assert!(!text.contains(" pairs ·"), "the stale pair count is gone");
    assert!(text.contains(" ❯ "), "search bar");
    assert!(text.contains("gpt-5.6"), "rows render");
    assert!(
        !text.contains("message haider"),
        "the picker COVERS the launcher composer"
    );
}

/// `/model <query>` pre-fills the search.
#[test]
fn model_query_prefills_the_search() {
    let mut model = seeded_launcher();
    run_slash(&mut model, "/model gemi");
    let picker = model.model_picker.as_ref().expect("picker opens");
    assert_eq!(picker.query, "gemi");
    let rows = model.model_picker_filtered("gemi");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "gemini-3");
}

/// HEEDED HISTORY: the palette's exact-match lead jump must NOT hijack ⏎
/// on `/model` — the fully-typed command shows the COMMAND row, ⏎ runs it
/// and the picker opens (the `/theme` law, applied to `/model`).
///
/// MUTATION CHECK (F2a): put "model" back in `has_arg_slots`. Expected
/// runtime failure: bare "model" jumps to arg rows, the label assertion
/// fails, and ⏎ fills a slot instead of opening the picker.
#[test]
fn exact_model_enter_opens_the_picker_not_an_arg_slot() {
    let mut model = seeded_launcher();
    let items = palette_items("model", false, &model.dynamic_slots());
    let labels: Vec<String> = items.iter().map(PaletteItem::label).collect();
    assert_eq!(labels, vec!["/model"], "the command row, not a slot hijack");
    for c in "/model".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        model.model_picker.is_some(),
        "⏎ on the exact command opens the picker"
    );
}

/// KEY-OWNERSHIP LAW: while open, characters edit the SEARCH (never the
/// composer), ⏎ over an empty filter selects nothing, and esc closes
/// without selecting.
#[test]
fn picker_owns_its_keys_while_open() {
    let mut model = seeded_session();
    let before_model = model.identity.model_short.clone();
    run_slash(&mut model, "/model");
    model.requests.clear();
    for c in "zzz-no-such".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(
        model.model_picker.as_ref().expect("open").query,
        "zzz-no-such"
    );
    assert_eq!(model.composer, "", "the composer never sees picker keys");
    model.handle(key(KeyCode::Enter));
    assert!(
        model.model_picker.is_some(),
        "⏎ over nothing selects nothing — the picker stays open"
    );
    assert!(model.requests.is_empty(), "and issues no request");
    model.handle(key(KeyCode::Esc));
    assert!(model.model_picker.is_none(), "esc closes");
    assert_eq!(
        model.identity.model_short, before_model,
        "esc selects NOTHING"
    );
}

/// Live substring search: case-insensitive, over model, provider, and auth
/// flavor.
#[test]
fn search_matches_model_and_provider_case_insensitively() {
    let model = seeded_launcher();
    let by_model = model.model_picker_filtered("GPT");
    assert!(!by_model.is_empty());
    assert!(by_model.iter().all(|row| row.model.contains("gpt")));
    let by_provider = model.model_picker_filtered("ANTHROpic");
    assert_eq!(by_provider.len(), 2);
    assert!(by_provider.iter().all(|row| row.provider == "anthropic"));
    let by_oauth = model.model_picker_filtered("OAUTH");
    assert!(!by_oauth.is_empty());
    assert!(by_oauth.iter().all(|row| row.auth == "oauth"));
    let by_api = model.model_picker_filtered("api");
    assert!(!by_api.is_empty());
    assert!(by_api.iter().all(|row| row.auth == "api"));
    assert!(model.model_picker_filtered("zz-nothing").is_empty());
}

/// OAuth subscriptions keep their exact rows and order, while every API
/// provider for one slug is retained behind a single top-level row. Search
/// includes providers that are not the aggregate row's first display name.
#[test]
fn api_duplicates_collapse_without_merging_the_oauth_pair() {
    let mut model = seeded_launcher();
    let oauth_before: Vec<(String, String)> = model
        .model_picker_rows()
        .into_iter()
        .filter(|row| row.auth == "oauth")
        .map(|row| (row.provider, row.model))
        .collect();
    for provider in ["zai-api", "relay-api"] {
        add_api_provider(
            &mut model,
            ApiProviderFixture {
                provider,
                model: "gpt-5.6",
                available: true,
                reason: None,
                context_window: Some(128_000),
                fetched_at_ms: None,
                lockdown: false,
                is_default: false,
            },
        );
    }

    let rows = model.model_picker_rows();
    let same_slug: Vec<_> = rows.iter().filter(|row| row.model == "gpt-5.6").collect();
    assert_eq!(
        same_slug.len(),
        2,
        "one exact OAuth row plus one collapsed API row"
    );
    assert_eq!(same_slug[0].auth, "oauth");
    assert_eq!(same_slug[0].provider, "openai");
    assert_eq!(same_slug[1].auth, "api");
    assert_eq!(same_slug[1].providers, ["zai-api", "relay-api"]);
    let oauth_after: Vec<(String, String)> = rows
        .into_iter()
        .filter(|row| row.auth == "oauth")
        .map(|row| (row.provider, row.model))
        .collect();
    assert_eq!(oauth_after, oauth_before, "OAuth ordering is untouched");

    let by_nonleading_provider = model.model_picker_filtered("RELAY-api");
    assert_eq!(by_nonleading_provider.len(), 1);
    assert_eq!(by_nonleading_provider[0].model, "gpt-5.6");
    assert_eq!(
        by_nonleading_provider[0].providers,
        ["zai-api", "relay-api"]
    );

    model.identity.provider = "relay-api".to_owned();
    model.identity.model_short = "gpt-5.6".to_owned();
    model.open_model_picker("relay-api".to_owned());
    let text = draw_rows(&model, 160, 24).join("\n");
    assert!(
        text.contains("current · relay-api"),
        "the exact live API pair remains visible before drilling in: {text}"
    );
}

/// A multi-provider API row opens a searchable provider stage. Its search is
/// fresh, and esc restores the parent query before a second esc closes.
#[test]
fn provider_stage_searches_live_and_escape_returns_before_closing() {
    let mut model = seeded_launcher();
    for provider in ["zai-api", "relay-api"] {
        add_api_provider(
            &mut model,
            ApiProviderFixture {
                provider,
                model: "shared-api-model",
                available: true,
                reason: None,
                context_window: Some(128_000),
                fetched_at_ms: None,
                lockdown: false,
                is_default: provider == "zai-api",
            },
        );
    }
    model.open_model_picker("relay-api".to_owned());
    model.handle(key(KeyCode::Enter));

    let picker = model
        .model_picker
        .as_ref()
        .expect("provider stage remains open");
    assert_eq!(
        picker
            .provider_stage
            .as_ref()
            .map(|stage| stage.model.as_str()),
        Some("shared-api-model")
    );
    assert_eq!(picker.query, "", "stage two starts with every provider");
    assert_eq!(
        model
            .model_picker_rows()
            .into_iter()
            .map(|row| row.provider)
            .collect::<Vec<_>>(),
        ["zai-api", "relay-api"],
        "provider order follows registry order"
    );
    let provider_text = draw_rows(&model, 120, 20).join("\n");
    assert!(provider_text.contains("PROVIDERS — shared-api-model"));
    assert!(provider_text.contains("API providers ·"));

    for c in "zai".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let filtered = model.model_picker_filtered("zai");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].provider, "zai-api");

    model.handle(key(KeyCode::Esc));
    let picker = model
        .model_picker
        .as_ref()
        .expect("first esc returns to models");
    assert!(picker.provider_stage.is_none());
    assert_eq!(picker.query, "relay-api", "the parent search is restored");
    model.handle(key(KeyCode::Esc));
    assert!(
        model.model_picker.is_none(),
        "second esc closes the overlay"
    );
}

/// Stage two uses the same remembered-top viewport authority as the model
/// list. Rendering a short window follows a deep provider selection, and esc
/// restores the parent's independent position.
#[test]
fn provider_stage_reuses_the_shared_follow_viewport_rule() {
    let mut model = seeded_launcher();
    for index in 0..14 {
        let provider = format!("viewport-{index:02}");
        add_api_provider(
            &mut model,
            ApiProviderFixture {
                provider: &provider,
                model: "viewport-model",
                available: true,
                reason: None,
                context_window: Some(128_000),
                fetched_at_ms: None,
                lockdown: false,
                is_default: false,
            },
        );
    }
    model.open_model_picker("viewport-model".to_owned());
    model.handle(key(KeyCode::Enter));
    for _ in 0..10 {
        model.handle(key(KeyCode::Down));
    }

    let text = draw_rows(&model, 120, 10).join("\n");
    let picker = model.model_picker.as_ref().expect("provider stage open");
    assert!(
        picker.scroll.get() > 0,
        "the shared viewport follows selection"
    );
    assert!(
        text.contains("viewport-10"),
        "the selected provider is visible"
    );

    model.handle(key(KeyCode::Esc));
    let picker = model.model_picker.as_ref().expect("parent list restored");
    assert!(picker.provider_stage.is_none());
    assert_eq!(
        picker.scroll.get(),
        0,
        "parent viewport is restored exactly"
    );
}

/// Aggregate facts are computed from usable providers: best available
/// context, freshest available inventory, explicit readiness/lockdown counts,
/// while stage two retains each provider's exact facts.
#[test]
fn api_aggregate_summarizes_divergent_provider_facts() {
    let mut model = seeded_launcher();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let now = u64::try_from(now).expect("clock fits u64");
    add_api_provider(
        &mut model,
        ApiProviderFixture {
            provider: "ready-api",
            model: "aggregate-model",
            available: true,
            reason: None,
            context_window: Some(128_000),
            fetched_at_ms: Some(now.saturating_sub(10_000)),
            lockdown: true,
            is_default: true,
        },
    );
    add_api_provider(
        &mut model,
        ApiProviderFixture {
            provider: "wide-ready-api",
            model: "aggregate-model",
            available: true,
            reason: None,
            context_window: Some(1_000_000),
            fetched_at_ms: Some(now.saturating_sub(1_000)),
            lockdown: false,
            is_default: false,
        },
    );
    add_api_provider(
        &mut model,
        ApiProviderFixture {
            provider: "newest-offline-api",
            model: "aggregate-model",
            available: false,
            reason: Some("provider has no credential"),
            context_window: Some(2_000_000),
            fetched_at_ms: Some(now.saturating_sub(100)),
            lockdown: false,
            is_default: false,
        },
    );

    let row = model
        .model_picker_filtered("aggregate-model")
        .into_iter()
        .next()
        .expect("collapsed API row");
    assert_eq!(
        row.providers,
        ["ready-api", "wide-ready-api", "newest-offline-api"]
    );
    assert_eq!(row.available_providers, 2);
    assert_eq!(row.lockdown_providers, 1);
    assert_eq!(row.default_providers, 1);
    assert_eq!(row.context_window, None);
    assert!(
        row.context_window_varies,
        "divergent usable limits are not collapsed into a misleading maximum"
    );
    assert!(
        (1_000..=2_000).contains(&row.inventory_age_ms.expect("fresh age")),
        "the newest unavailable provider does not supply display facts"
    );
    assert!(row.available && row.selectable);
    assert!(!row.lockdown, "lockdown is not falsely claimed for all");

    model.open_model_picker("aggregate-model".to_owned());
    let text = draw_rows(&model, 170, 20).join("\n");
    assert!(text.contains("2/3 available"));
    assert!(text.contains("1/3 lockdown"));
    assert!(text.contains("varies"));
    assert!(text.contains("freshest age"));
    model.handle(key(KeyCode::Enter));
    let exact = model.model_picker_rows();
    assert_eq!(exact.len(), 3);
    assert_eq!(exact[0].context_window, Some(128_000));
    assert_eq!(exact[1].context_window, Some(1_000_000));
    assert_eq!(exact[2].context_window, Some(2_000_000));
    assert!(exact[0].available && exact[1].available && !exact[2].available);
}

/// An aggregate with no usable provider remains dim/unavailable and refuses
/// at the top level with every provider-qualified reason.
#[test]
fn all_unavailable_api_group_refuses_without_opening_provider_stage() {
    let mut model = seeded_session();
    for (provider, reason) in [
        ("offline-a", "credential missing"),
        ("offline-b", "endpoint unhealthy"),
    ] {
        add_api_provider(
            &mut model,
            ApiProviderFixture {
                provider,
                model: "offline-model",
                available: false,
                reason: Some(reason),
                context_window: Some(64_000),
                fetched_at_ms: None,
                lockdown: false,
                is_default: false,
            },
        );
    }
    model.open_model_picker("offline-model".to_owned());
    model.requests.clear();
    let row = model.model_picker_filtered("offline-model").remove(0);
    assert!(!row.available);
    assert_eq!(row.available_providers, 0);
    assert!(
        row.reason
            .as_deref()
            .is_some_and(|reason| { reason.contains("offline-a") && reason.contains("offline-b") }),
        "the aggregate retains both honest reasons"
    );

    model.handle(key(KeyCode::Enter));
    let picker = model
        .model_picker
        .as_ref()
        .expect("refusal keeps picker open");
    assert!(
        picker.provider_stage.is_none(),
        "an unusable group never opens an empty provider choice"
    );
    let error = picker.error.as_deref().expect("inline refusal");
    assert!(error.contains("offline-a") && error.contains("offline-b"));
    assert!(model.requests.is_empty(), "no selection RPC is issued");
}

/// Rows mark the session's CURRENT pair and each provider's default.
#[test]
fn rows_mark_the_current_pair_and_provider_defaults() {
    let mut model = seeded_launcher();
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "claude-opus-5".to_owned();
    let rows = model.model_picker_rows();
    let current: Vec<&str> = rows
        .iter()
        .filter(|row| row.is_current)
        .map(|row| row.model.as_str())
        .collect();
    assert_eq!(current, vec!["claude-opus-5"], "exactly one current pair");
    assert!(
        rows.iter()
            .any(|row| row.model == "gpt-5.6-codex" && row.is_default),
        "the provider's own default is marked"
    );
    run_slash(&mut model, "/model claude-opus");
    let text = draw_rows(&model, 110, 30).join("\n");
    assert!(text.contains("current"), "the current tag renders");
    assert!(text.contains('●'), "and its gutter dot");
}

/// OWNER LAW: a custom passthrough id omitted by `/v1/models` remains visible
/// as the CURRENT pair, but it is typed/dimmed as unlisted and never inserted
/// into the provider's pickable inventory.
///
/// MUTATION CHECK: omit the advisory-current branch, mark it available, or
/// append it to `summary.models`. Expected RUNTIME failure: the exact row,
/// rendered `unlisted` note, or no-fabrication assertion fails.
#[test]
fn advisory_custom_current_model_is_visible_as_unlisted_not_available() {
    let mut model = seeded_launcher();
    let mut custom = model.providers.providers[0].clone();
    custom.provider = "bench-proxy".to_owned();
    custom.api_family = haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions;
    custom.models = vec!["catalog-model".to_owned()];
    custom.model_details.clear();
    custom.default_model = None;
    custom.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Advisory;
    model.providers.providers.push(custom);
    model.identity.provider = "bench-proxy".to_owned();
    model.identity.model_short = "passthrough-model".to_owned();

    let rows = model.model_picker_rows();
    let unlisted = rows
        .iter()
        .find(|row| row.provider == "bench-proxy" && row.model == "passthrough-model")
        .expect("current custom passthrough row remains visible");
    assert!(unlisted.is_current);
    assert!(!unlisted.available);
    assert!(!unlisted.selectable);
    assert_eq!(
        unlisted.reason.as_deref(),
        Some("unlisted by advisory provider catalog")
    );
    assert_eq!(
        model
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == "bench-proxy")
            .expect("custom summary")
            .models,
        ["catalog-model"]
    );

    model.open_model_picker("passthrough-model".to_owned());
    let text = draw_rows(&model, 110, 30).join("\n");
    assert!(text.contains("unlisted by advisory provider catalog"));
}

/// The auth flavor rides every row — oauth vs api is what gets metered.
#[test]
fn rows_carry_the_auth_flavor_and_context_window() {
    let model = seeded_launcher();
    let rows = model.model_picker_rows();
    let claude = rows
        .iter()
        .find(|row| row.model == "claude-opus-5")
        .expect("claude row");
    assert_eq!(
        claude.auth, "oauth",
        "anthropic's selected account is OAuth"
    );
    let gemini = rows
        .iter()
        .find(|row| row.model == "gemini-3")
        .expect("gemini row");
    assert_eq!(
        gemini.auth, "api",
        "gemini's selected account is an API key"
    );
    let kimi = rows
        .iter()
        .find(|row| row.provider == "kimi-oauth")
        .expect("kimi row");
    assert_eq!(kimi.auth, "oauth", "the provider key encodes the flavor");
    let deepseek = rows
        .iter()
        .find(|row| row.model == "deepseek-chat")
        .expect("DeepSeek row");
    assert_eq!(deepseek.auth, "api", "DeepSeek is a Bearer API-key pair");
}

/// MUTATION CHECK (F2a): make `apply_model_selected` keep the REQUESTED
/// pair instead of the reply's. Expected runtime failure: the resolved
/// provider below never lands and the no-echo assertion fails.
#[test]
fn live_selection_is_receipted_and_renders_the_resolved_pair() {
    let mut model = seeded_session();
    let mut driver = LiveDriver::new("test");
    run_slash(&mut model, "/model o4-mini");
    model.handle(key(KeyCode::Enter));
    let picker = model.model_picker.as_ref().expect("still open — pending");
    assert_eq!(
        picker.pending,
        Some(("openai".to_owned(), "o4-mini".to_owned())),
        "the request is marked pending, nothing applied optimistically"
    );
    assert_eq!(
        model.identity.model_short, "fable-5",
        "no optimism: the identity holds until the reply"
    );
    let commands = pass(&mut driver, &mut model, None);
    let (command_id, session, m, p) = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::SelectModel {
                command_id,
                session,
                model,
                provider,
                ..
            } => Some((
                command_id.clone(),
                session.clone(),
                model.clone(),
                provider.clone(),
            )),
            _ => None,
        })
        .expect("session.select_model issued");
    assert_eq!(session, sid());
    assert_eq!(m, "o4-mini");
    assert_eq!(p, "openai");

    // The daemon resolves the pair — deliberately NOT the request's
    // provider string, proving the render is truth, never an echo.
    let commands = pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ModelSelected {
            command_id,
            session: sid(),
            provider: "openai-oauth".to_owned(),
            model: "o4-mini".to_owned(),
            worker_generation: 7,
        }),
    );
    assert!(commands.is_empty());
    assert!(model.model_picker.is_none(), "the commit closes the picker");
    assert_eq!(model.identity.provider, "openai-oauth", "RESOLVED provider");
    assert_eq!(model.identity.model_short, "o4-mini");
    let flash = model.flash.as_deref().unwrap_or_default();
    assert!(
        flash.contains("o4-mini") && flash.contains("openai-oauth"),
        "the flash names the resolved pair: {flash:?}"
    );
}

/// Selecting in stage two uses the same exact pending tuple and receipted
/// reply authority as an OAuth row. The top-level aggregate is never sent.
#[test]
fn provider_stage_selection_preserves_pending_and_resolved_truth() {
    let mut model = seeded_session();
    for provider in ["stage-a", "stage-b"] {
        add_api_provider(
            &mut model,
            ApiProviderFixture {
                provider,
                model: "stage-model",
                available: true,
                reason: None,
                context_window: Some(256_000),
                fetched_at_ms: None,
                lockdown: false,
                is_default: false,
            },
        );
    }
    let mut driver = LiveDriver::new("provider-stage");
    run_slash(&mut model, "/model stage-model");
    model.handle(key(KeyCode::Enter));
    assert!(
        model
            .model_picker
            .as_ref()
            .is_some_and(|picker| picker.provider_stage.is_some())
    );
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));

    let picker = model
        .model_picker
        .as_ref()
        .expect("pending stage remains open");
    assert_eq!(
        picker.pending,
        Some(("stage-b".to_owned(), "stage-model".to_owned()))
    );
    assert_eq!(
        model.identity.model_short, "fable-5",
        "identity does not move optimistically"
    );
    let rendered = draw_rows(&model, 140, 20).join("\n");
    assert!(
        rendered.contains('…'),
        "the exact pending row pulses visibly"
    );

    let commands = pass(&mut driver, &mut model, None);
    let command_id = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::SelectModel {
                command_id,
                model,
                provider,
                ..
            } if model == "stage-model" && provider == "stage-b" => Some(command_id.clone()),
            _ => None,
        })
        .expect("exact provider selection issued");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ModelSelected {
            command_id,
            session: sid(),
            provider: "resolved-stage".to_owned(),
            model: "stage-model-v2".to_owned(),
            worker_generation: 9,
        }),
    );
    assert!(model.model_picker.is_none());
    assert_eq!(model.identity.provider, "resolved-stage");
    assert_eq!(model.identity.model_short, "stage-model-v2");
}

/// A typed refusal lands INLINE with the public code; the pending mark
/// clears and the row stays selectable — ⏎ again retries.
#[test]
fn typed_refusal_lands_inline_and_the_row_stays_selectable() {
    let mut model = seeded_session();
    let mut driver = LiveDriver::new("test");
    run_slash(&mut model, "/model claude-sonnet-5");
    model.handle(key(KeyCode::Enter));
    let commands = pass(&mut driver, &mut model, None);
    let command_id = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::SelectModel { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("first attempt issued");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id),
            code: "provider_unavailable".to_owned(),
            message: "provider `anthropic` is not creatable here".to_owned(),
            retryable: false,
            presentation: None,
        }),
    );
    let picker = model.model_picker.as_ref().expect("picker stays open");
    assert!(picker.pending.is_none(), "the pending mark clears");
    let error = picker.error.as_deref().expect("inline error");
    assert!(
        error.contains("provider_unavailable") && error.contains("anthropic"),
        "the public code and provider reach the user: {error:?}"
    );
    assert_eq!(
        model.identity.model_short, "fable-5",
        "a refusal moves nothing"
    );
    // The row is still there and still selectable — retry issues anew.
    model.handle(key(KeyCode::Enter));
    let commands = pass(&mut driver, &mut model, None);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, LiveCommand::SelectModel { .. })),
        "⏎ retries the same row"
    );
}

/// A refusal landing AFTER the picker closed still surfaces — as a
/// session-view error line, never a silent IDLE (F2e).
#[test]
fn refusal_after_close_reaches_the_session_view() {
    let mut model = seeded_session();
    let mut driver = LiveDriver::new("test");
    run_slash(&mut model, "/model gemini-3");
    model.handle(key(KeyCode::Enter));
    model.handle(key(KeyCode::Enter));
    let commands = pass(&mut driver, &mut model, None);
    let command_id = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::SelectModel { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("attempt issued");
    model.handle(key(KeyCode::Esc));
    assert!(model.model_picker.is_some(), "first esc returns to models");
    model.handle(key(KeyCode::Esc));
    assert!(model.model_picker.is_none());
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id),
            code: "model_unknown".to_owned(),
            message: "no such model in the discovered inventory".to_owned(),
            retryable: false,
            presentation: None,
        }),
    );
    let error = model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Error { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("the session view carries the failure");
    assert!(
        error.contains("model_unknown") && error.contains("gemini-3"),
        "the public reason names the refused pair: {error:?}"
    );
}

/// Unavailable providers' rows refuse with their REASON — dimmed, honest,
/// no request, no crash.
#[test]
fn unavailable_rows_refuse_with_their_reason() {
    let mut model = seeded_session();
    run_slash(&mut model, "/model kimi");
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    assert!(
        model.requests.is_empty(),
        "no request for an unavailable provider"
    );
    let picker = model.model_picker.as_ref().expect("open");
    let error = picker.error.as_deref().expect("the reason surfaces");
    assert!(
        error.contains("kimi-oauth") && error.contains("unavailable"),
        "the reason names the provider: {error:?}"
    );
    // And the reason renders dimmed on the row itself.
    let text = draw_rows(&model, 110, 30).join("\n");
    assert!(text.contains("provider model inventory is unavailable"));
}

/// At the launcher (no session) a selection sets the DEFAULT pair new
/// sessions use — locally, pinned, no session RPC.
#[test]
fn launcher_selection_sets_the_default_pair() {
    let mut model = seeded_launcher();
    run_slash(&mut model, "/model qwen3");
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    assert!(
        model
            .model_picker
            .as_ref()
            .is_some_and(|picker| picker.provider_stage.is_some()),
        "even a singleton API choice opens its exact-provider stage"
    );
    assert_eq!(
        model.identity.model_short, "fable-5",
        "opening providers does not select optimistically"
    );
    model.handle(key(KeyCode::Enter));
    assert!(
        model.model_picker.is_none(),
        "the exact provider selection closes the picker"
    );
    assert_eq!(model.identity.provider, "local");
    assert_eq!(model.identity.model_short, "qwen3-coder");
    assert!(model.identity_pinned, "an explicit choice pins");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SelectModel { .. })),
        "no session RPC without a session"
    );
}

/// Without the daemon feature, a live-session selection names the stale
/// daemon instead of pretending.
#[test]
fn selection_without_the_feature_names_the_stale_daemon() {
    let mut model = seeded_session();
    model.daemon_features.remove("session_model_select_v1");
    run_slash(&mut model, "/model claude-sonnet-5");
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    assert!(
        model.requests.is_empty(),
        "no request against an ungated daemon"
    );
    let error = model
        .model_picker
        .as_ref()
        .expect("open")
        .error
        .as_deref()
        .unwrap_or_default()
        .to_owned();
    assert!(
        error.contains("newer daemon"),
        "the stale daemon is named: {error:?}"
    );
}

/// A collapsed API row click opens its provider stage; the exact stage row
/// then selects the value carried by that row's rect.
#[test]
fn clicking_a_row_selects_its_carried_pair() {
    let mut model = seeded_launcher();
    run_slash(&mut model, "/model");
    model.requests.clear();
    model.handle_hit(Hit::ModelPickerRow {
        provider: "gemini".to_owned(),
        model: "gemini-3".to_owned(),
        api_group: true,
    });
    assert!(
        model
            .model_picker
            .as_ref()
            .is_some_and(|picker| picker.provider_stage.is_some()),
        "the collapsed API click opens providers"
    );
    model.handle_hit(Hit::ModelPickerRow {
        provider: "gemini".to_owned(),
        model: "gemini-3".to_owned(),
        api_group: false,
    });
    assert!(model.model_picker.is_none(), "the click selects");
    assert_eq!(model.identity.provider, "gemini");
    assert_eq!(model.identity.model_short, "gemini-3");
}
