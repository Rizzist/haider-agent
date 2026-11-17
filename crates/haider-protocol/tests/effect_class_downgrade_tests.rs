#![allow(clippy::expect_used)]
//! Frozen wave-base 088e5a24 / 0.0.972 intent decoder: new writes must
//! remain decodable, while the current reader retains typed provenance.
use haider_protocol::effect::{EffectClass, EffectIntent};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum LegacyEffectClass {
    FsRead,
    FsWrite,
    ProcessExec,
    /// Execute a command on a saved remote SSH target. This remains distinct
    /// from local process authority and defaults to an explicit Ask.
    RemoteExecution,
    Network {
        host: String,
    },
    GitOp,
    AgentSpawn,
    /// Deliver untrusted input to another peer at a turn boundary.
    PeerMessage,
    CredentialAccess,
    /// Reserved (§9.3) — the computer-use pack lands post-0.1.
    GuiAct,
    /// Read pixels or the pointer location without actuating the desktop.
    ScreenObserve,
    /// Actuate the pointer, keyboard, scroll wheel, or an interactive wait.
    ScreenControl,
    /// Read pixels or structured accessibility state from an activated mobile capability.
    MobileObserve,
    /// Actuate an explicitly activated mobile capability.
    MobileControl,
    /// Read SMS data from an explicitly activated mobile capability.
    ReadSms,
}

// Copied from the wave base. No deny_unknown_fields: additive provenance is
// ignored by the released decoder, including during store startup replay.
#[derive(Debug, Serialize, Deserialize)]
struct LegacyEffectIntent {
    effect: haider_protocol::ids::EffectId,
    class: LegacyEffectClass,
    summary: String,
    args_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_revision: Option<haider_protocol::ids::WorkspaceRevision>,
}

fn intent(class: EffectClass) -> EffectIntent {
    EffectIntent {
        effect: haider_protocol::ids::EffectId::new("synthetic-effect"),
        class,
        summary: "checkpoint_restore provider_transport usage_reset_apply are only text here"
            .into(),
        args_digest: "digest".into(),
        workspace_revision: None,
    }
}

fn released_classes() -> [EffectClass; 16] {
    [
        EffectClass::FsRead,
        EffectClass::FsWrite,
        EffectClass::ProcessExec,
        EffectClass::RemoteExecution,
        EffectClass::Network {
            host: "provider-request".into(),
        },
        EffectClass::Network {
            host: "example.invalid".into(),
        },
        EffectClass::GitOp,
        EffectClass::AgentSpawn,
        EffectClass::PeerMessage,
        EffectClass::CredentialAccess,
        EffectClass::GuiAct,
        EffectClass::ScreenObserve,
        EffectClass::ScreenControl,
        EffectClass::MobileObserve,
        EffectClass::MobileControl,
        EffectClass::ReadSms,
    ]
}

#[test]
fn old_decoder_accepts_new_intents_and_current_reader_restores_origin() {
    for (class, carrier, origin) in [
        (
            EffectClass::CheckpointRestore,
            LegacyEffectClass::FsWrite,
            "checkpoint_restore",
        ),
        (
            EffectClass::ProviderTransport,
            LegacyEffectClass::Network {
                host: "provider-request".into(),
            },
            "provider_transport",
        ),
        (
            EffectClass::UsageResetApply,
            LegacyEffectClass::CredentialAccess,
            "usage_reset_apply",
        ),
    ] {
        for revision in [
            None,
            Some(haider_protocol::ids::WorkspaceRevision::new("7")),
        ] {
            let current = EffectIntent {
                workspace_revision: revision.clone(),
                ..intent(class.clone())
            };
            let value = serde_json::to_value(&current).expect("current intent");
            assert_eq!(value["effect_origin"], origin);
            let legacy: LegacyEffectIntent =
                serde_json::from_value(value.clone()).expect("released decoder");
            assert_eq!(legacy.class, carrier);
            assert_eq!(legacy.workspace_revision, revision);
            assert_eq!(
                serde_json::from_value::<EffectIntent>(value).expect("re-upgrade"),
                current
            );
            let bytes = rmp_serde::to_vec_named(&current).expect("msgpack write");
            let old: LegacyEffectIntent =
                rmp_serde::from_slice(&bytes).expect("old msgpack decoder");
            assert_eq!(old.class, carrier);
            assert_eq!(
                rmp_serde::from_slice::<EffectIntent>(&bytes).expect("new msgpack reader"),
                current
            );
        }
    }
}

#[test]
fn frozen_enum_rejects_direct_new_variants_but_all_released_classes_are_unchanged() {
    for class in [
        EffectClass::CheckpointRestore,
        EffectClass::ProviderTransport,
        EffectClass::UsageResetApply,
    ] {
        let direct = serde_json::to_value(&class).expect("direct new class");
        assert!(
            serde_json::from_value::<LegacyEffectClass>(direct.clone())
                .expect_err("strict enum")
                .to_string()
                .contains("unknown variant")
        );
        // Read journals made by earlier lane candidates, then normalize writes.
        let mut value = serde_json::to_value(intent(EffectClass::FsRead)).expect("fixture");
        value["class"] = direct;
        let read: EffectIntent = serde_json::from_value(value).expect("earlier candidate journal");
        assert_eq!(read.class, class);
        serde_json::from_value::<LegacyEffectIntent>(
            serde_json::to_value(read).expect("normalized write"),
        )
        .expect("released reader");
    }
    for class in released_classes() {
        let current = intent(class.clone());
        let value = serde_json::to_value(&current).expect("old intent");
        assert!(value.get("effect_origin").is_none());
        let legacy: LegacyEffectIntent =
            serde_json::from_value(value.clone()).expect("old decoder");
        assert_eq!(serde_json::to_value(legacy).expect("old bytes"), value);
        assert_eq!(
            serde_json::from_value::<EffectIntent>(value)
                .expect("current decoder")
                .class,
            class
        );
    }
}

#[test]
fn known_origins_require_matching_carriers_and_unknown_origins_never_gain_authority() {
    for (class, origin) in [
        (EffectClass::FsRead, "checkpoint_restore"),
        (EffectClass::ProcessExec, "checkpoint_restore"),
        (EffectClass::FsWrite, "provider_transport"),
        (
            EffectClass::Network {
                host: "example.invalid".into(),
            },
            "provider_transport",
        ),
    ] {
        let mut value = serde_json::to_value(intent(class)).expect("fixture");
        value["effect_origin"] = serde_json::json!(origin);
        assert!(
            serde_json::from_value::<EffectIntent>(value).is_err(),
            "origin={origin}"
        );
    }
    // Unknown origins do not confer restore authority. Retaining the released
    // carrier also lets this reader survive a later writer's additive origin.
    for origin in [
        "future_restore_authority",
        "CheckpointRestore",
        " checkpoint_restore",
        "checkpoint_restore ",
        "provider-transport",
        "UsageResetApply",
        " usage_reset_apply",
        "usage_reset_apply ",
        "usage-reset-apply",
        "",
    ] {
        for class in released_classes() {
            let mut value = serde_json::to_value(intent(class.clone())).expect("fixture");
            value["effect_origin"] = serde_json::json!(origin);
            let read: EffectIntent =
                serde_json::from_value(value.clone()).expect("unknown origin is additive");
            assert_eq!(read.class, class, "origin={origin}");
            serde_json::from_value::<LegacyEffectIntent>(value.clone()).expect("released reader");
            let bytes = rmp_serde::to_vec_named(&value).expect("unknown origin msgpack");
            assert_eq!(
                rmp_serde::from_slice::<EffectIntent>(&bytes)
                    .expect("unknown msgpack origin is additive")
                    .class,
                class
            );
        }
    }
}

#[test]
fn usage_reset_origin_requires_account_carrier_and_has_no_agent_activity() {
    assert!(
        haider_protocol::activity::ActivityKind::for_effect(&EffectClass::UsageResetApply)
            .is_none()
    );
    for class in released_classes() {
        let mut value = serde_json::to_value(intent(class.clone())).expect("fixture");
        value["effect_origin"] = serde_json::json!("usage_reset_apply");
        let read = serde_json::from_value::<EffectIntent>(value);
        if class == EffectClass::CredentialAccess {
            assert_eq!(
                read.expect("account carrier").class,
                EffectClass::UsageResetApply
            );
        } else {
            assert!(
                read.is_err(),
                "a weaker or unrelated carrier cannot confer reset authority: {class:?}"
            );
        }
    }
}

#[derive(Deserialize)]
struct LegacyManifestEffects {
    effects: Vec<LegacyEffectClass>,
}

#[derive(Deserialize)]
struct LegacyInventoryEntry {
    manifest: LegacyManifestEffects,
}

#[derive(Deserialize)]
struct LegacyGrant {
    class: LegacyEffectClass,
}

#[derive(Deserialize)]
struct LegacyInventory {
    tools: Vec<LegacyInventoryEntry>,
    remembered_grants: Vec<LegacyGrant>,
}

fn inventory(classes: Vec<EffectClass>) -> haider_protocol::tool::ToolInventorySnapshot {
    use haider_protocol::tool::{
        DispatchMode, RememberedGrantScope, RememberedSessionGrant, ToolInventoryEntry,
        ToolInventorySnapshot, ToolManifest, ToolPermissionDefault,
    };
    ToolInventorySnapshot {
        tools: vec![ToolInventoryEntry {
            manifest: ToolManifest {
                name: "synthetic_inventory_only".into(),
                description: "Read-only serialization fixture; grants no authority".into(),
                effects: classes.clone(),
                dispatch: DispatchMode::Deferred,
                input_schema: serde_json::json!({"type":"object"}),
            },
            default: ToolPermissionDefault::Ask,
        }],
        remembered_grants: classes
            .into_iter()
            .map(|class| RememberedSessionGrant {
                class,
                scope: RememberedGrantScope::Class,
            })
            .collect(),
    }
}

#[test]
fn frozen_inventory_decoder_accepts_internal_classes_and_current_reader_restores_them() {
    use haider_protocol::tool::ToolInventorySnapshot;
    let mut classes = vec![
        EffectClass::UsageResetApply,
        EffectClass::CheckpointRestore,
        EffectClass::ProviderTransport,
    ];
    classes.extend(released_classes());
    let original = inventory(classes.clone());
    let value = serde_json::to_value(&original).expect("inventory json");
    assert_eq!(
        value["tools"][0]["manifest"]["effects"][0]["effect_origin"],
        "usage_reset_apply"
    );
    for (index, class) in classes.iter().enumerate().skip(3) {
        assert_eq!(
            value["tools"][0]["manifest"]["effects"][index],
            serde_json::to_value(class).expect("released class shape")
        );
    }
    let old: LegacyInventory =
        serde_json::from_value(value.clone()).expect("frozen direct JSON reader");
    let carriers = [
        LegacyEffectClass::CredentialAccess,
        LegacyEffectClass::FsWrite,
        LegacyEffectClass::Network {
            host: "provider-request".into(),
        },
    ];
    assert_eq!(old.tools[0].manifest.effects.len(), classes.len());
    assert_eq!(old.remembered_grants.len(), classes.len());
    for (index, expected) in carriers.iter().enumerate() {
        assert_eq!(&old.tools[0].manifest.effects[index], expected);
        assert_eq!(&old.remembered_grants[index].class, expected);
    }
    assert_eq!(
        serde_json::from_value::<ToolInventorySnapshot>(value).expect("current JSON reader"),
        original
    );
    let bytes = rmp_serde::to_vec_named(&original).expect("inventory MessagePack");
    let old: LegacyInventory =
        rmp_serde::from_slice(&bytes).expect("frozen direct MessagePack reader");
    assert_eq!(
        old.tools[0].manifest.effects[0],
        LegacyEffectClass::CredentialAccess
    );
    assert_eq!(
        old.remembered_grants[0].class,
        LegacyEffectClass::CredentialAccess
    );
    assert_eq!(
        rmp_serde::from_slice::<ToolInventorySnapshot>(&bytes).expect("current MessagePack reader"),
        original
    );
}

#[test]
fn inventory_origin_requires_its_released_carrier_and_aliases_confer_no_reset_class() {
    use haider_protocol::tool::ToolInventorySnapshot;
    let original =
        serde_json::to_value(inventory(vec![EffectClass::UsageResetApply])).expect("fixture");
    for path in ["/tools/0/manifest/effects/0", "/remembered_grants/0/class"] {
        let mut wrong = original.clone();
        wrong.pointer_mut(path).expect("class")["class"] = serde_json::json!("fs_read");
        assert!(
            serde_json::from_value::<ToolInventorySnapshot>(wrong).is_err(),
            "wrong carrier at {path}"
        );
        for origin in [
            "UsageResetApply",
            " usage_reset_apply",
            "usage_reset_apply ",
            "usage-reset-apply",
        ] {
            let mut unknown = original.clone();
            unknown.pointer_mut(path).expect("class")["effect_origin"] = serde_json::json!(origin);
            let read: ToolInventorySnapshot =
                serde_json::from_value(unknown).expect("unknown additive origin");
            let class = if path.starts_with("/tools/") {
                &read.tools[0].manifest.effects[0]
            } else {
                &read.remembered_grants[0].class
            };
            assert_eq!(class, &EffectClass::CredentialAccess);
        }
    }
}
