//! W5e-3: `/model`, `/provider`, `/account` argument slots fed from DAEMON
//! TRUTH — the discovered catalog (W5e-2) and the live account list — never
//! from a compile-time table.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppModel, RuntimeMode};
use haider_tui::commands::{PaletteItem, palette_items};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::runtime::live_pass;

mod common;
use common::{launcher_model, run_slash};

fn model_with_catalog() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = ["provider_models_v1", "provider_management_v1"]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    model.daemon_version = Some("0.0.19".to_owned());
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.identity.provider = "openai".to_owned();
    model
}

fn values(items: &[PaletteItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            PaletteItem::Arg { value, .. } => Some(value.clone()),
            PaletteItem::Cmd(_) | PaletteItem::Custom { .. } => None,
        })
        .collect()
}

/// MUTATION CHECK (W5e-3): make `dynamic_slots` return
/// `DynamicSlots::default()`. Expected runtime failure: `/model ` offers no
/// rows, so the assertions below find an empty candidate list — i.e. the
/// picker stops being fed by discovery.
/// Verified by revert on 2026-07-30.
#[test]
fn model_slot_offers_the_active_providers_discovered_models() {
    let model = model_with_catalog();
    let items = palette_items("model ", true, &model.dynamic_slots());
    let offered = values(&items);
    // Exactly the seeded openai inventory — the DISCOVERED list, in the
    // provider's own order.
    assert_eq!(offered, vec!["gpt-5.6", "gpt-5.6-codex", "o4-mini"]);
    // The provider's declared default is marked, not guessed.
    let default_desc = items
        .iter()
        .find_map(|item| match item {
            PaletteItem::Arg { value, desc, .. } if value == "gpt-5.6-codex" => Some(desc.clone()),
            _ => None,
        })
        .expect("codex row");
    assert!(
        default_desc.contains("default"),
        "the provider's own default must be marked: {default_desc:?}"
    );
}

/// The slot follows the ACTIVE provider — switching providers changes the
/// candidates, because they are that provider's discovered models.
#[test]
fn model_slot_follows_the_active_provider() {
    let mut model = model_with_catalog();
    model.identity.provider = "anthropic".to_owned();
    let offered = values(&palette_items("model ", true, &model.dynamic_slots()));
    assert_eq!(offered, vec!["claude-opus-5", "claude-sonnet-5"]);

    // The installed Gemini adapter (B6a) serves its discovered inventory.
    model.identity.provider = "gemini".to_owned();
    assert_eq!(
        values(&palette_items("model ", true, &model.dynamic_slots())),
        vec!["gemini-3"]
    );

    // A provider with NO discovered models offers nothing — never a guess
    // (kimi-oauth is signed out in the seed registry: empty inventory).
    model.identity.provider = "kimi-oauth".to_owned();
    assert!(
        values(&palette_items("model ", true, &model.dynamic_slots())).is_empty(),
        "an undiscovered provider must offer no models"
    );
}

#[test]
fn model_picker_aggregate_uses_freshest_available_inventory_age() {
    let mut model = model_with_catalog();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let now = u64::try_from(now).expect("clock fits u64");
    let gemini = model
        .providers
        .providers
        .iter_mut()
        .find(|summary| summary.provider == "gemini")
        .expect("gemini summary");
    gemini.inventory_fetched_at_ms = Some(now.saturating_sub(42_000));
    let mut mirror = gemini.clone();
    mirror.provider = "gemini-mirror".to_owned();
    mirror.inventory_fetched_at_ms = Some(now.saturating_sub(12_000));
    model.providers.providers.push(mirror);

    let aggregate = model
        .model_picker_filtered("gemini-3")
        .into_iter()
        .next()
        .expect("collapsed Gemini row");
    assert_eq!(aggregate.providers, ["gemini", "gemini-mirror"]);
    let age = aggregate.inventory_age_ms.expect("aggregate inventory age");
    assert!((12_000..=13_000).contains(&age));

    model.open_model_picker("gemini-3".to_owned());
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));
    let exact_ages: Vec<u64> = model
        .model_picker_rows()
        .into_iter()
        .filter_map(|row| row.inventory_age_ms)
        .collect();
    assert_eq!(exact_ages.len(), 2, "stage two retains exact ages");
    assert!((42_000..=43_000).contains(&exact_ages[0]));
    assert!((12_000..=13_000).contains(&exact_ages[1]));
}

#[test]
fn opening_model_picker_refreshes_a_stale_selected_provider() {
    let mut model = model_with_catalog();
    let openai = model
        .providers
        .providers
        .iter_mut()
        .find(|summary| summary.provider == "openai")
        .expect("openai summary");
    openai.inventory_fetched_at_ms = Some(1);
    let provider_snapshot = model.providers.providers.clone();
    let mut driver = LiveDriver::new("picker-refresh");

    run_slash(&mut model, "/model");
    let initial = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    assert!(initial.contains(&LiveCommand::ProviderList));
    let followups = driver.apply(
        &mut model,
        LiveReply::Providers {
            providers: provider_snapshot.clone(),
            revision: 2,
        },
    );
    assert!(followups.contains(&LiveCommand::RefreshProviderModels {
        provider: "openai".to_owned(),
    }));

    driver.apply(
        &mut model,
        LiveReply::ModelsRefreshFailed {
            provider: "openai".to_owned(),
            message: "temporary catalog failure".to_owned(),
        },
    );
    let retry = driver.apply(
        &mut model,
        LiveReply::Providers {
            providers: provider_snapshot,
            revision: 3,
        },
    );
    assert!(retry.contains(&LiveCommand::RefreshProviderModels {
        provider: "openai".to_owned(),
    }));
}

/// `/provider` lists the registry with honest health; `/account` lists live
/// aliases and marks the one in use.
#[test]
fn provider_and_account_slots_are_daemon_truth() {
    let model = model_with_catalog();
    let providers = values(&palette_items("provider ", true, &model.dynamic_slots()));
    assert!(providers.contains(&"openai".to_owned()));
    // B6a/B6k: provider.list now serves both new adapters — gemini as an
    // installed row, kimi-oauth as the signed-out (unavailable) one.
    assert!(providers.contains(&"gemini".to_owned()));
    assert!(
        providers.contains(&"kimi-oauth".to_owned()),
        "unavailable providers still listed"
    );

    let items = palette_items("account ", false, &model.dynamic_slots());
    let aliases = values(&items);
    assert!(aliases.contains(&"work-chatgpt".to_owned()));
    let in_use = items
        .iter()
        .find_map(|item| match item {
            PaletteItem::Arg { value, desc, .. } if value == "work-chatgpt" => Some(desc.clone()),
            _ => None,
        })
        .expect("row");
    assert!(
        in_use.contains("in use"),
        "the active alias is marked: {in_use:?}"
    );
    assert!(in_use.contains("oauth"), "auth label rides: {in_use:?}");
    assert!(
        in_use.contains("ChatGPT"),
        "the account identity rides the picker: {in_use:?}"
    );
    assert!(
        in_use.contains("added before 0.0.964"),
        "legacy creation absence stays explicit: {in_use:?}"
    );
}

/// Prefix filtering works on the dynamic rows like any slot.
#[test]
fn dynamic_slots_filter_by_prefix() {
    let model = model_with_catalog();
    let offered = values(&palette_items(
        "model gpt-5.6-c",
        true,
        &model.dynamic_slots(),
    ));
    assert_eq!(offered, vec!["gpt-5.6-codex"]);
}

/// Selecting a discovered model applies it; an undiscovered one cannot be
/// selected at all. F2a re-homes this law onto the full-screen picker:
/// `/model <query>` pre-fills the search, ⏎ selects the highlighted
/// DISCOVERED row, and a query matching nothing leaves everything as it
/// was (the picker stays open, honest and empty — never a silent accept).
#[test]
fn selecting_a_model_requires_it_to_be_discovered() {
    use ratatui::crossterm::event::KeyCode;
    let mut model = model_with_catalog();
    run_slash(&mut model, "/model o4-mini");
    let picker = model.model_picker.as_ref().expect("the picker opens");
    assert_eq!(picker.query, "o4-mini", "the query pre-fills the search");
    let rows = model.model_picker_filtered("o4-mini");
    assert_eq!(rows.len(), 1, "exactly the discovered pair matches");
    assert_eq!(rows[0].provider, "openai");
    model.handle(common::key(KeyCode::Enter));
    assert!(model.model_picker.is_none(), "selection closes the picker");
    assert_eq!(model.identity.model_short, "o4-mini");
    assert_eq!(model.identity.provider, "openai");

    run_slash(&mut model, "/model gpt-9-imaginary");
    assert!(
        model.model_picker_filtered("gpt-9-imaginary").is_empty(),
        "an undiscovered model matches no row"
    );
    model.handle(common::key(KeyCode::Enter));
    assert_eq!(
        model.identity.model_short, "o4-mini",
        "an undiscovered model must not be applied"
    );
    assert!(
        model.model_picker.is_some(),
        "nothing to select — the picker stays open instead of guessing"
    );
    model.handle(common::key(KeyCode::Esc));
    assert!(model.model_picker.is_none(), "esc closes without selecting");
}

/// With no catalog AND no daemon feature, `/model` names the stale daemon
/// rather than pretending there are no models (the W5e-1b law, applied here
/// before shipping).
#[test]
fn model_without_the_feature_names_the_stale_daemon() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_version = Some("0.0.13".to_owned());
    // No provider_models_v1 advertised, no catalog.
    run_slash(&mut model, "/model");
    let flash = model.flash.as_deref().unwrap_or_default();
    assert!(
        flash.contains("newer daemon") && flash.contains("0.0.13"),
        "must name the stale daemon: {flash:?}"
    );
}
