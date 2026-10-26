#![allow(clippy::expect_used)]

//! C7: explicit child choices derive provenance from their own resolved row.
//! Kills `copy-parent-owner`, `drop-child-owner`, and `drop-settings-owner`.

use super::DelegationHandle;
use crate::accounts::{AccountsFacade, DeviceDiscoverySnapshot, ManagementSnapshot};
use crate::session_hub::{SessionHub, SessionHubConfig};
use haider_core::SqliteStoreHandle;
use haider_protocol::output_budget::SessionOutputBudgetSourceV1::{Derived, UserSet};
use haider_protocol::session::SessionMetadataV1;
use haider_rpc::{ModelDetailSourceWire, ProviderSummaryWire};
use haider_tools::SpawnSubagent;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

fn profile(provider: &str, model: &str, owner: &str) -> ProviderSummaryWire {
    serde_json::from_value(json!({
        "provider":provider, "api_family":"openai_chat_completions",
        "models":[model], "model_details":[{
            "name":model, "source":"remote", "max_output_tokens":2048
        }],
        "catalog":"unknown", "inventory":{"state":"static"},
        "inventory_authority":"advisory", "auth_methods":[],
        "availability":"available", "enabled":true, "trust":"full",
        "catalog_scope":{"key":owner}
    })).expect("synthetic catalog")
}

fn parent() -> SessionMetadataV1 {
    serde_json::from_value(json!({
        "cwd":"/tmp", "provider":"parent", "model":"parent-model",
        "max_tokens":1024, "max_tokens_source":{"kind":"user_set", "requested":1024},
        "created_at_ms":1, "model_catalog_key":"parent-owner"
    })).expect("synthetic parent")
}

async fn resolve(
    parent: &SessionMetadataV1,
    target: ProviderSummaryWire,
    model: Option<&str>,
    provider: Option<&str>,
    typed_workflow: bool,
) -> SessionMetadataV1 {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store, SessionHubConfig::default()).expect("hub");
    hub.install_creatable_providers(BTreeSet::from(["parent".into(), "child".into()]))
        .expect("creatable providers");
    let mut providers = vec![target];
    if providers[0].provider != "parent" {
        providers.push(profile("parent", "parent-model", "parent-owner"));
    }
    hub.install_accounts(AccountsFacade {
        login: None, oauth: None, snapshot: Arc::new(Mutex::new(Vec::new())),
        management: ManagementSnapshot::new(1, Vec::new(), providers),
        vault_supported: false, discovery_disabled: true,
        device_discovery: DeviceDiscoverySnapshot::new(true), vault: None,
        sources: Arc::new(Mutex::new(Vec::new())),
    }).expect("catalog");
    let mut request = json!({"task":"synthetic task", "prompt":"synthetic prompt"});
    if let Some(model) = model { request["model"] = json!(model); }
    if let Some(provider) = provider { request["provider"] = json!(provider); }
    if typed_workflow {
        request["agent_type"] = json!("reviewer");
        request["workflow"] = json!("implement_verify");
    }
    let request = SpawnSubagent::from_tool_args(request).expect("spawn arguments");
    DelegationHandle::new(hub).resolve_child_metadata(parent, &request)
        .expect("infrastructure").expect("selection")
}

#[tokio::test]
async fn c7_cross_provider_exact_and_alternate_coordinates_own_the_child() {
    for spelling in ["child-model", "CHILD_MODEL", "child.model", " child\u{a0}model "] {
        let child = resolve(&parent(), profile("child", "child-model", "child-owner"),
            Some(spelling), Some("child"), false).await;
        assert_eq!(child.model, "child-model");
        assert_eq!(child.model_catalog_key.as_deref(), Some("child-owner"), "{spelling}");
    }
}

#[tokio::test]
async fn c7_bare_selector_resolves_the_target_owner() {
    let child = resolve(&parent(), profile("child", "child-model", "child-owner"),
        Some("CHILD_MODEL"), None, false).await;
    assert_eq!(child.provider, "child");
    assert_eq!(child.model_catalog_key.as_deref(), Some("child-owner"));
}

#[tokio::test]
async fn c7_unlisted_child_does_not_retire_with_the_parent_account() {
    let child = resolve(&parent(), profile("parent", "parent-model", "next-account-owner"),
        Some("unlisted-child"), Some("parent"), false).await;
    assert_eq!(child.model, "unlisted-child");
    assert_eq!(child.model_catalog_key, None);
    assert_eq!(child.max_tokens, 1024);
}

#[tokio::test]
async fn c7_cross_provider_permanent_public_and_keyless_rows_have_no_parent_owner() {
    for source in [ModelDetailSourceWire::Configured, ModelDetailSourceWire::Static] {
        let mut target = profile("child", "child-model", "child-owner");
        target.model_details[0].source = Some(source);
        if source == ModelDetailSourceWire::Configured {
            target.configured_models.push("child-model".into());
        } else {
            target.static_models.push("child-model".into());
        }
        let child = resolve(&parent(), target, Some("CHILD_MODEL"), Some("child"), false).await;
        assert_eq!(child.model_catalog_key, None);
    }
    let mut target = profile("child", "child-model", "unused");
    target.catalog_scope = None;
    target.catalog = haider_rpc::ProviderCatalogKindWire::Public;
    let child = resolve(&parent(), target, Some("child-model"), Some("child"), false).await;
    assert_eq!(child.model_catalog_key, None);
}

#[tokio::test]
async fn c7_typed_workflow_uses_the_same_child_owner_rule() {
    let child = resolve(&parent(), profile("child", "child-model", "child-owner"),
        Some("CHILD_MODEL"), Some("child"), true).await;
    assert_eq!(child.model_catalog_key.as_deref(), Some("child-owner"));
}

#[tokio::test]
async fn c7_parent_settings_owner_is_replaced_by_target_settings_provenance() {
    for source in [Derived, UserSet { requested: 4096 }] {
        let mut parent = parent();
        parent.max_tokens_source = Some(source);
        let mut target = profile("child", "child-model", "child-owner");
        target.configured_models.push("child-model".into());
        let child = resolve(&parent, target, Some("CHILD_MODEL"), Some("child"), false).await;
        assert_eq!(child.model_catalog_key.as_deref(), Some("child-owner"));
        assert_eq!(child.max_tokens, 2048);
        assert_eq!(child.max_tokens_source, Some(source));
    }
}

#[tokio::test]
async fn c7_same_provider_permanent_unclamped_child_is_independent_of_account_switch() {
    for owner in ["parent-owner", "next-account-owner"] {
        let mut target = profile("parent", "permanent-child", owner);
        target.configured_models.push("permanent-child".into());
        let child = resolve(&parent(), target, Some("PERMANENT_CHILD"), Some("parent"), false).await;
        assert_eq!(child.model_catalog_key, None);
        assert_eq!(child.max_tokens, 1024);
    }
}

#[tokio::test]
async fn c7_implicit_inheritance_preserves_owner_coordinate_and_budget() {
    let mut parent = parent();
    parent.model = "PARENT_MODEL".into();
    parent.resolved_model = Some("parent-model".into());
    let child = resolve(&parent, profile("parent", "parent-model", "next-owner"),
        None, None, false).await;
    assert_eq!(child, parent);
}

#[tokio::test]
async fn c7_explicit_same_model_refreshes_owner_and_unowned_parent_uses_child_owner() {
    let child = resolve(&parent(), profile("parent", "parent-model", "next-owner"),
        Some("parent-model"), None, false).await;
    assert_eq!(child.model_catalog_key.as_deref(), Some("next-owner"));
    let mut parent = parent();
    parent.model_catalog_key = None;
    let child = resolve(&parent, profile("child", "child-model", "child-owner"),
        Some("child-model"), Some("child"), false).await;
    assert_eq!(child.model_catalog_key.as_deref(), Some("child-owner"));
}
