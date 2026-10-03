#![allow(clippy::expect_used)]
//! Frozen 78a2b592 (pre-973 activity) effect-class decoder. Unknown authority
//! must fail typed decoding while the raw envelope remains lossless.
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

#[test]
fn old_effect_class_decoder_refuses_new_authority_and_raw_json_stays_lossless() {
    for class in [
        EffectClass::CheckpointRestore,
        EffectClass::ProviderTransport,
    ] {
        let current = serde_json::to_value(EffectIntent {
            effect: haider_protocol::ids::EffectId::new("synthetic-effect"),
            class,
            summary: "synthetic bookkeeping".into(),
            args_digest: "digest".into(),
            workspace_revision: None,
        })
        .expect("new intent");
        let error = serde_json::from_value::<LegacyEffectClass>(current["class"].clone())
            .expect_err("old decoder must reject new authority");
        assert!(error.is_data());
        assert!(error.to_string().contains("unknown variant"));
        let raw: haider_protocol::envelope::RawEnvelope = serde_json::from_value(serde_json::json!({
            "schema_version":1,"event_id":"event","seq":1,"session_id":"session","device_id":"device",
            "authority_epoch":0,"worker_generation":1,"committed_at_ms":1,
            "render":{"ui":false,"durable":true,"prompt":"omit"},
            "payload":{"type":"effect","phase":"intent","effect":current["effect"],
                "class":current["class"],"summary":current["summary"],"args_digest":current["args_digest"]}
        })).expect("raw reader is the compatibility boundary");
        let serialized = serde_json::to_vec(&raw).expect("preserve raw record");
        let reopened: haider_protocol::envelope::RawEnvelope =
            serde_json::from_slice(&serialized).expect("raw restart");
        assert_eq!(reopened.payload["class"], current["class"]);
        assert_eq!(
            serde_json::to_vec(&reopened).expect("stable raw bytes"),
            serialized
        );
    }
    for class in [
        EffectClass::FsWrite,
        EffectClass::ProcessExec,
        EffectClass::Network {
            host: "provider-request".into(),
        },
    ] {
        let bytes = serde_json::to_value(class).expect("old class");
        let legacy = serde_json::from_value::<LegacyEffectClass>(bytes.clone())
            .expect("existing classes stay accepted");
        assert_eq!(serde_json::to_value(legacy).expect("legacy bytes"), bytes);
    }
}
