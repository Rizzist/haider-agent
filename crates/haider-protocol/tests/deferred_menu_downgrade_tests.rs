#![allow(clippy::expect_used)]
use haider_protocol::credential::UsageResetAccountBinding;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{CredentialAlias, MenuId};
use haider_protocol::menu::{Menu, MenuKind, MenuScope};
use serde::Deserialize;

// The wave-base Choice decoder ignores additive fields, but rejects a new
// internally tagged kind during startup's recovery scan.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReleasedKind {
    Choice,
    Question,
    Permission { effect_summary: String },
}

#[derive(Deserialize)]
struct ReleasedMenu {
    kind: ReleasedKind,
    blocking: bool,
}

fn menu(binding: Option<UsageResetAccountBinding>) -> Menu {
    Menu {
        id: MenuId::new("synthetic-pending-reset"),
        kind: MenuKind::DeferredApproval {
            approval_id: "synthetic-approval".into(),
            effect: EffectClass::UsageResetApply,
            account: Some(CredentialAlias::new("synthetic-alias")),
            binding,
        },
        title: "Spend one synthetic credit?".into(),
        body: Vec::new(),
        options: Vec::new(),
        blocking: false,
        scope: MenuScope::Session,
        origin: "deferred_approval_v1".into(),
        ttl_ms: Some(1234),
        timeout_option: None,
    }
}

#[test]
fn deferred_menu_writes_released_choice_and_preserves_typed_coordinates() {
    for binding in [
        None,
        Some(UsageResetAccountBinding {
            provider: "openai-oauth".into(),
            account_id: "synthetic-workspace".into(),
            incarnation: 17,
            subject_hash: Some("synthetic-subject".into()),
        }),
    ] {
        let original = menu(binding);
        let json = serde_json::to_vec(&original).expect("json write");
        let old: ReleasedMenu = serde_json::from_slice(&json).expect("released json reader");
        assert_eq!(old.kind, ReleasedKind::Choice);
        assert!(!old.blocking);
        assert_eq!(
            serde_json::from_slice::<Menu>(&json).expect("current json reader"),
            original
        );
        let msgpack = rmp_serde::to_vec_named(&original).expect("msgpack write");
        let old: ReleasedMenu = rmp_serde::from_slice(&msgpack).expect("released msgpack reader");
        assert_eq!(old.kind, ReleasedKind::Choice);
        assert!(!old.blocking);
        assert_eq!(
            rmp_serde::from_slice::<Menu>(&msgpack).expect("current msgpack reader"),
            original
        );
    }
}

#[test]
fn earlier_native_deferred_menus_remain_readable_and_normalize_on_write() {
    let original = menu(None);
    let mut value = serde_json::to_value(&original).expect("fixture");
    value["kind"] = serde_json::to_value(&original.kind).expect("earlier native kind");
    assert!(serde_json::from_value::<ReleasedMenu>(value.clone()).is_err());
    let current: Menu = serde_json::from_value(value).expect("earlier candidate remains readable");
    assert_eq!(current, original);
    let normalized = serde_json::to_value(current).expect("normalized write");
    assert_eq!(normalized["kind"]["kind"], "choice");
    assert!(serde_json::from_value::<ReleasedMenu>(normalized).is_ok());
}

#[test]
fn only_valid_choice_metadata_can_confer_deferred_approval() {
    let original = serde_json::to_value(menu(None)).expect("fixture");
    for kind in ["question", "permission", "secret", "file", "conflict"] {
        let mut value = original.clone();
        value["kind"]["kind"] = serde_json::json!(kind);
        value["kind"]["effect_summary"] = serde_json::json!("synthetic permission");
        assert!(
            serde_json::from_value::<Menu>(value).is_err(),
            "wrong carrier {kind}"
        );
    }
    for field in ["approval_id", "effect"] {
        let mut value = original.clone();
        value["kind"]["deferred_approval"]
            .as_object_mut()
            .expect("metadata")
            .remove(field);
        assert!(
            serde_json::from_value::<Menu>(value).is_err(),
            "missing {field}"
        );
    }
    for alias in [
        "DeferredApproval",
        " deferred_approval",
        "deferred_approval ",
        "deferred-approval",
    ] {
        let mut value = original.clone();
        let metadata = value["kind"]
            .as_object_mut()
            .expect("kind")
            .remove("deferred_approval")
            .expect("metadata");
        value["kind"][alias] = metadata;
        assert_eq!(
            serde_json::from_value::<Menu>(value)
                .expect("unknown additive field")
                .kind,
            MenuKind::Choice
        );
    }
}

#[test]
fn ordinary_menu_kinds_keep_their_released_encoding_and_semantics() {
    for kind in [
        MenuKind::Choice,
        MenuKind::Question,
        MenuKind::Permission {
            effect_summary: "synthetic write".into(),
            file_review: None,
        },
    ] {
        let mut original = menu(None);
        original.kind = kind.clone();
        original.blocking = true;
        let value = serde_json::to_value(&original).expect("fixture");
        assert_eq!(
            value["kind"],
            serde_json::to_value(&kind).expect("unchanged native shape")
        );
        let old: ReleasedMenu = serde_json::from_value(value.clone()).expect("released reader");
        assert!(old.blocking);
        match (&kind, &old.kind) {
            (MenuKind::Choice, ReleasedKind::Choice)
            | (MenuKind::Question, ReleasedKind::Question) => {}
            (
                MenuKind::Permission { effect_summary, .. },
                ReleasedKind::Permission {
                    effect_summary: released,
                },
            ) => assert_eq!(released, effect_summary),
            _ => panic!("ordinary menu changed its released semantics"),
        }
        assert_eq!(
            serde_json::from_value::<Menu>(value).expect("current reader"),
            original
        );
    }
}
