//! Offline client installation diagnostic, hosted here to keep core/image
//! validation out of the control executable. Never starts a profile daemon.
use haider_core::{HarnessActor, HarnessConfig, MemoryStore, StoreHandle, SubmitTurn};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use std::process::ExitCode;
use std::sync::Arc;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const CRATES: [&str; 10] = [
    haider_protocol::CRATE_NAME,
    haider_store::CRATE_NAME,
    haider_core::CRATE_NAME,
    haider_provider::CRATE_NAME,
    haider_tools::CRATE_NAME,
    haider_verify::CRATE_NAME,
    haider_accounts::CRATE_NAME,
    haider_rpc::CRATE_NAME,
    haider_client::CRATE_NAME,
    "haider-tui",
];

fn cli_config(worker_generation: u64, model: &str) -> HarnessConfig {
    let mut config = HarnessConfig::for_session(
        SessionId::new("cli-session"),
        DeviceId::new("cli-device"),
        1,
        worker_generation,
    );
    config.model = model.into();
    config
}
/// Offline, ephemeral, deterministic. Structured JSON on stdout.
pub(super) async fn self_test(payload: Option<std::path::PathBuf>) -> ExitCode {
    let payload_ok = payload
        .map_or_else(
            || {
                std::env::current_exe().map(|path| {
                    path.with_file_name(format!("haider-tui{}", std::env::consts::EXE_SUFFIX))
                })
            },
            Ok,
        )
        .and_then(|path| haider_client::payload_identity::verify_payload(&path))
        .is_ok();
    let mut checks: Vec<serde_json::Value> = CRATES
        .iter()
        .map(|name| serde_json::json!({"name": format!("link:{name}"), "ok": *name != "haider-tui" || payload_ok}))
        .collect();
    let fake_turn_ok = fake_turn_self_test().await;
    checks.push(serde_json::json!({
        "name": "fake-provider-turn",
        "ok": fake_turn_ok,
    }));
    let report = serde_json::json!({
        "schema": "haider.selftest.v0",
        "version": VERSION,
        "ok": fake_turn_ok && payload_ok,
        "checks": checks,
    });
    println!("{report}");
    if fake_turn_ok && payload_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn fake_turn_self_test() -> bool {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText { text: "ok".into() },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(cli_config(1, "fake-model"), provider, store.clone());
    let Ok(turn) = handle.submit_turn(SubmitTurn::new("self-test")).await else {
        return false;
    };
    let Ok(outcome) = turn.wait().await else {
        return false;
    };
    if outcome.state != RunState::Done || outcome.error.is_some() {
        return false;
    }
    StoreHandle::latest_seq(store.as_ref(), &SessionId::new("cli-session"))
        .await
        .is_ok_and(|sequence| sequence > 0)
}
