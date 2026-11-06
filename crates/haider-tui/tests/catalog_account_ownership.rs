//! A provider management catalog belongs to the active account, not every pin.
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::{CredentialAlias, SessionId};
use haider_rpc::{
    ModelDetailSourceWire, ModelInventoryWire, ProviderApiFamilyWire, ProviderAvailabilityWire,
    ProviderCatalogKindWire, ProviderSummaryWire,
};
use haider_tui::app::{AccountRow, AppModel, RuntimeMode};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};

const PROVIDER: &str = "openai-oauth";
const MODEL: &str = "account-owned-model";

fn descriptors() -> Vec<CredentialDescriptor> {
    ["a", "b"]
        .into_iter()
        .map(|alias| CredentialDescriptor {
            incarnation: 0,
            credential_digest: None,

            alias: CredentialAlias::new(alias),
            provider: PROVIDER.into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: format!("fixture-{alias}"),
            status: CredentialStatus::Ok,
            active: alias == "a",
            label: None,
            account_identity: None,
            created_at_ms: None,
        })
        .collect()
}

fn fixture(pin: Option<&str>, catalog: ProviderCatalogKindWire) -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.accounts.apply_snapshot(
        descriptors()
            .iter()
            .map(AccountRow::from_descriptor)
            .collect(),
        Some(3),
    );
    let fetched_at_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("timestamp");
    // Decode the real wire shape so the exact tests also compile on aece0ad8.
    let summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider": PROVIDER,
        "api_family": ProviderApiFamilyWire::OpenAiResponses,
        "models": [MODEL],
        "model_details": [{
            "name": MODEL,
            "context_window": 8192,
            "max_output_tokens": 2048,
            "source": ModelDetailSourceWire::Remote,
            "supported_efforts": [],
            "supported_speeds": []
        }],
        "inventory": ModelInventoryWire::Fetched { fetched_at_ms },
        "catalog": catalog,
        "auth_methods": [AuthMethod::OAuth],
        "availability": ProviderAvailabilityWire::Available,
        "default_model": MODEL,
        "enabled": true
    }))
    .expect("catalog wire fixture");
    model.providers.apply_snapshot(vec![summary], 3);
    let session = SessionId::new("catalog-owner-session");
    model.upsert_live_session(&session);
    model.active_session = Some(session.clone());
    let row = model
        .sessions
        .iter_mut()
        .find(|row| row.id == session)
        .expect("session");
    row.account_provider = Some(PROVIDER.into());
    row.account_alias = pin.map(str::to_owned);
    model.identity.provider = PROVIDER.into();
    model.identity.model_short = MODEL.into();
    model.identity.context_window = 0;
    model.meter_epoch.admit(
        (PROVIDER.into(), MODEL.into()),
        Some(1),
        Some(4096),
        Some(true),
    );
    model
}

#[test]
fn catalog_owner_tui_pinned_window_excludes_other_account() {
    let mut model = fixture(Some("b"), ProviderCatalogKindWire::Authenticated);
    model.refresh_context_window();
    assert_eq!(
        model.identity.context_window, 0,
        "A's window cannot become the B-pinned session's declared window"
    );
}

#[test]
fn catalog_owner_tui_budget_projection_excludes_other_account() {
    let mut model = fixture(Some("b"), ProviderCatalogKindWire::Authenticated);
    model.refresh_context_window();
    assert!(
        model.context_meter().auto_compact_at.is_none(),
        "B's stored budget cannot project a threshold using A's output ceiling"
    );
    assert_eq!(model.meter_epoch.output_budget, Some(4096));
}

#[test]
fn catalog_owner_tui_replaced_account_waits_for_matching_snapshot() {
    let mut model = fixture(Some("a"), ProviderCatalogKindWire::Authenticated);
    model.accounts.rows[0].identity = "replacement-principal".into();
    model.accounts.revision = Some(4);
    model.refresh_context_window();
    assert_eq!(
        model.identity.context_window, 0,
        "a replacement account cannot reuse the previous revision's metadata"
    );
}

#[test]
fn catalog_control_tui_owned_account_keeps_declared_limits() {
    let mut model = fixture(Some("a"), ProviderCatalogKindWire::Authenticated);
    model.refresh_context_window();
    assert_eq!(model.identity.context_window, 8192);
    assert!(model.context_meter().auto_compact_at.is_some());
}

#[test]
fn catalog_control_tui_unpinned_session_uses_active_account() {
    let mut model = fixture(None, ProviderCatalogKindWire::Authenticated);
    model.refresh_context_window();
    assert_eq!(model.identity.context_window, 8192);
    assert!(model.context_meter().auto_compact_at.is_some());
}

#[test]
fn catalog_control_tui_public_limits_are_account_independent() {
    let mut model = fixture(Some("b"), ProviderCatalogKindWire::Public);
    model.accounts.revision = Some(4);
    model.refresh_context_window();
    assert_eq!(model.identity.context_window, 8192);
    assert!(model.context_meter().auto_compact_at.is_some());
}

#[test]
fn catalog_owner_tui_provider_first_refresh_recovers_owned_limits() {
    let mut model = fixture(Some("a"), ProviderCatalogKindWire::Authenticated);
    let mut driver = LiveDriver::new("catalog-owner-test");
    let summary = model.providers.providers[0].clone();
    let commands = driver.apply(
        &mut model,
        LiveReply::ProviderModelsRefreshed {
            provider: summary,
            revision: 4,
        },
    );
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, LiveCommand::AccountListAt { .. })),
        "a new catalog revision must request its owning account snapshot before a cache hit"
    );
    assert_eq!(model.identity.context_window, 0);
    driver.apply(
        &mut model,
        LiveReply::Accounts {
            descriptors: descriptors(),
            revision: Some(4),
            sources: Vec::new(),
        },
    );
    assert_eq!(
        model.identity.context_window, 8192,
        "the matched owning snapshot restores declared limits without another discovery"
    );
}

#[test]
fn catalog_owner_tui_accounts_first_refresh_recovers_owned_limits() {
    let mut model = fixture(Some("a"), ProviderCatalogKindWire::Authenticated);
    let mut driver = LiveDriver::new("catalog-owner-test");
    let summary = model.providers.providers[0].clone();
    let commands = driver.apply(
        &mut model,
        LiveReply::Accounts {
            descriptors: descriptors(),
            revision: Some(4),
            sources: Vec::new(),
        },
    );
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, LiveCommand::ProviderListAt { .. })),
        "a new account revision must request its owned catalog before a cache hit"
    );
    assert_eq!(model.identity.context_window, 0);
    driver.apply(
        &mut model,
        LiveReply::Providers {
            providers: vec![summary],
            revision: 4,
        },
    );
    assert_eq!(
        model.identity.context_window, 8192,
        "the matched catalog restores declared limits without another discovery"
    );
}

#[test]
fn catalog_control_tui_static_limits_are_account_independent() {
    let mut model = fixture(Some("b"), ProviderCatalogKindWire::Authenticated);
    model.providers.providers[0].inventory = ModelInventoryWire::Static;
    model.providers.providers[0].model_details[0].source = Some(ModelDetailSourceWire::Static);
    model.accounts.revision = Some(4);
    model.refresh_context_window();
    assert_eq!(
        model.identity.context_window, 8192,
        "maintained limits remain provider-wide across account revisions"
    );
    assert!(model.context_meter().auto_compact_at.is_some());
}

#[test]
fn catalog_control_tui_configured_limits_are_account_independent() {
    let mut model = fixture(Some("b"), ProviderCatalogKindWire::Custom);
    model.providers.providers[0].inventory = ModelInventoryWire::Configured;
    model.providers.providers[0].model_details[0].source = Some(ModelDetailSourceWire::Configured);
    model.accounts.revision = Some(4);
    model.refresh_context_window();
    assert_eq!(
        model.identity.context_window, 8192,
        "configured limits remain provider-wide across account revisions"
    );
    assert!(model.context_meter().auto_compact_at.is_some());
}
