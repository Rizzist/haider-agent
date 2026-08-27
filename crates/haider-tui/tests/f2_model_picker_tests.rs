//! F2a — the full-screen `/model` picker: every model × provider pair,
//! searchable; receipted live selection rendering the RESOLVED pair.
//!
//! The laws:
//! * bare `/model` opens a FULL-SCREEN picker with one row per model ×
//!   provider pair across every enabled provider; `/model <query>`
//!   pre-fills the search.
//! * KEY OWNERSHIP (heeded history): while open the picker owns every
//!   key — ⏎ selects the HIGHLIGHTED row (never an exact-match jump),
//!   esc closes without selecting, characters edit the search.
//! * live sessions issue `session.select_model` and render the RESOLVED
//!   pair from the reply — never an echo of the request.
//! * typed refusals land INLINE; the row stays selectable for a retry;
//!   a refusal landing after the picker closed reaches the session view.
//! * unavailable providers render dimmed with their reason and REFUSE
//!   with it — never a silent failure.
//! * at the launcher a selection sets the default pair new sessions use.
#![allow(clippy::expect_used)]

use haider_protocol::ids::SessionId;
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
/// Expected runtime failure: no picker opens and every pair assertion
/// below fails.
#[test]
fn bare_model_opens_the_full_screen_picker_with_every_pair() {
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

/// Live substring search: case-insensitive, over model AND provider.
#[test]
fn search_matches_model_and_provider_case_insensitively() {
    let model = seeded_launcher();
    let by_model = model.model_picker_filtered("GPT");
    assert!(!by_model.is_empty());
    assert!(by_model.iter().all(|row| row.model.contains("gpt")));
    let by_provider = model.model_picker_filtered("ANTHROpic");
    assert_eq!(by_provider.len(), 2);
    assert!(by_provider.iter().all(|row| row.provider == "anthropic"));
    assert!(model.model_picker_filtered("zz-nothing").is_empty());
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
    let commands = pass(&mut driver, &mut model, None);
    let command_id = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::SelectModel { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("attempt issued");
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
    assert!(model.model_picker.is_none());
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

/// A row CLICK selects exactly the pair the rect carried (value-carrying
/// hits — a stale map can never select a different row).
#[test]
fn clicking_a_row_selects_its_carried_pair() {
    let mut model = seeded_launcher();
    run_slash(&mut model, "/model");
    model.requests.clear();
    model.handle_hit(Hit::ModelPickerRow {
        provider: "gemini".to_owned(),
        model: "gemini-3".to_owned(),
    });
    assert!(model.model_picker.is_none(), "the click selects");
    assert_eq!(model.identity.provider, "gemini");
    assert_eq!(model.identity.model_short, "gemini-3");
}
