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
        summary: "checkpoint_restore provider_transport are only text here".into(),
        args_digest: "digest".into(),
        workspace_revision: None,
    }
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
    for class in [
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
    ] {
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
fn origin_is_typed_and_requires_its_matching_carrier() {
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
        (EffectClass::FsWrite, "future_restore_authority"),
        (EffectClass::FsWrite, "CheckpointRestore"),
        (EffectClass::FsWrite, " checkpoint_restore"),
    ] {
        let mut value = serde_json::to_value(intent(class)).expect("fixture");
        value["effect_origin"] = serde_json::json!(origin);
        assert!(
            serde_json::from_value::<EffectIntent>(value).is_err(),
            "origin={origin}"
        );
    }
}
