#![allow(clippy::expect_used)]

use haider_provider::prompt_cache_fake;

use crate::usage_report::{ReqwestUsageMeterHttp, SessionFolder, UsageReportService};
use haider_accounts::{MemoryVault, Vault};
use haider_core::{
    HarnessActor, HarnessConfig, InteractionResolutionPolicy, SqliteStoreHandle, StoreHandle,
    SubmitTurn,
};
use haider_protocol::ids::{CredentialAlias, DeviceId, SessionId};
use haider_protocol::session::SessionInteractionModeV1;
use haider_protocol::state::RunState;
use haider_provider::{AnthropicProvider, ToolDefinition};
use prompt_cache_fake::RecordingCacheHttp;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn prompt_cache_http_actor_journal_usage_report_conserves_two_turn_and_tool_reads() {
    let before = run_fixture(false).await;
    let after = run_fixture(true).await;
    for (before, after) in before.iter().zip(&after) {
        assert_eq!(
            prompt_cache_fake::neutral(&before["body"]),
            prompt_cache_fake::neutral(&after["body"]),
            "cache controls cannot change logical model input"
        );
    }
}

async fn run_fixture(cache_on: bool) -> Vec<serde_json::Value> {
    let fake = RecordingCacheHttp::start(16, true).await;
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("SQLite journal");
    let session = SessionId::new("cache-fixture");
    let account = CredentialAlias::new("synthetic-cache-account");
    let vault = MemoryVault::new();
    vault
        .put(&account, b"synthetic-test-key")
        .expect("synthetic credential");
    let provider = Arc::new(
        AnthropicProvider::new(vault.resolve(&account).expect("handle"), "claude-fable-5-1")
            .expect("adapter")
            .with_account(account.clone())
            .with_api_url(&fake.url),
    );
    let mut config = HarnessConfig::for_session(
        session.clone(),
        DeviceId::new("cache-test"),
        0,
        store.worker_generation(),
    );
    config.model = "claude-fable-5-1".into();
    config.max_tokens = 512;
    config.system_prompt = Some("Synthetic stable policy. ".repeat(160));
    config.volatile_user_tail =
        Some("Request-local workflow observation, excluded from cache identity.".into());
    config.tools = vec![ToolDefinition {
        name: "request_input".into(),
        description: "Select the fixture default".into(),
        input_schema: json!({"type":"object"}),
    }];
    config.usage_account = Some(account.clone());
    // Controls-off is a paired synthetic fixture, not a historical candidate.
    config.usage_scope.provider = if cache_on {
        "anthropic"
    } else {
        "synthetic-controls-off"
    }
    .into();
    config.usage_scope.model = config.model.clone();
    config.usage_scope.auth_scope = "api_key".into();
    config.cached_input_is_subset = false;
    config.interaction_policy =
        InteractionResolutionPolicy::new(SessionInteractionModeV1::Autonomous);
    let (actor, handle) = HarnessActor::new(config, provider, Arc::new(store.clone()));
    let actor = tokio::spawn(actor.run());
    for turn in 0..8 {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            handle
                .submit_turn(SubmitTurn::new(format!("Synthetic accepted turn {turn}")))
                .await
                .expect("admitted")
                .wait()
                .await
                .expect("outcome")
        })
        .await
        .expect("bounded fixture");
        assert_eq!(
            outcome.state,
            RunState::Done,
            "cache_on={cache_on} turn={turn}: {outcome:?}"
        );
    }
    drop(handle);
    actor.await.expect("actor exits");
    fake.task.await.expect("fake exits");
    let records = fake.records.lock().expect("records").clone();
    assert_eq!(records.len(), 16);
    assert_eq!(records[0]["usage"]["read"], 0);
    if cache_on {
        assert!(records[0]["usage"]["write"].as_u64().expect("write") > 0);
        for record in &records[1..] {
            assert!(record["usage"]["read"].as_u64().expect("read") > 0);
        }
    } else {
        for record in &records {
            assert_eq!(record["usage"]["read"], 0);
            assert_eq!(record["usage"]["write"], 0);
        }
    }
    let events = store
        .read(&session, 0, usize::MAX)
        .await
        .expect("durable replay");
    let mut folder = SessionFolder::new("claude-fable-5-1");
    for event in &events {
        folder.push(event);
    }
    let stats = folder.finish();
    let cache = &stats.tokens[&account].cache;
    let sum = |key: &str| {
        records
            .iter()
            .map(|record| record["usage"][key].as_u64().expect("counter"))
            .sum::<u64>()
    };
    assert_eq!(cache.cache_read_tokens, sum("read"));
    assert_eq!(cache.cache_write_tokens, sum("write"));
    assert_eq!(cache.uncached_input_tokens, sum("uncached"));
    assert_eq!(cache.logical_input_tokens, sum("logical"));
    assert_eq!(cache.requests.len(), records.len());
    for (entry, record) in cache.requests.iter().zip(&records) {
        let usage = entry
            .request
            .normalized
            .as_ref()
            .expect("per-request normalization");
        assert_eq!(usage.logical_input, record["usage"]["logical"]);
        assert_eq!(usage.uncached_input, record["usage"]["uncached"]);
        assert_eq!(usage.cache_read_input, record["usage"]["read"]);
        assert_eq!(usage.cache_write_input, record["usage"]["write"]);
        assert_eq!(usage.cache_write_5m_input, record["usage"]["write_5m"]);
        assert_eq!(usage.cache_write_1h_input, record["usage"]["write_1h"]);
        assert_eq!(
            usage.cache_status,
            haider_protocol::provider::CacheStatAvailability::Present
        );
        assert_eq!(
            usage.cache_write_status,
            haider_protocol::provider::CacheStatAvailability::Present
        );
        let diagnostic = entry
            .request
            .cache
            .as_ref()
            .expect("per-request cache observation");
        assert_eq!(
            matches!(
                diagnostic.control,
                haider_protocol::provider::CacheControlObservationV1::Emitted { .. }
            ),
            cache_on
        );
    }
    let epochs: std::collections::BTreeSet<_> = cache
        .requests
        .iter()
        .filter_map(|entry| entry.scope.as_ref().map(|scope| &scope.cache_epoch))
        .collect();
    assert_eq!(
        epochs.len(),
        1,
        "accepted run IDs and the volatile tail cannot rotate cache identity"
    );
    // Exercise the actual usage.report service over the same SQLite journal;
    // an API-key descriptor has no remote subscription meter to poll.
    let descriptor = haider_protocol::credential::CredentialDescriptor {
        alias: account.clone(),
        provider: "anthropic".into(),
        base_url: None,
        auth_method: haider_protocol::credential::AuthMethod::ApiKey,
        identity: "synthetic@example.invalid".into(),
        status: haider_protocol::credential::CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    let report = UsageReportService::new(
        Arc::new(std::sync::Mutex::new(vec![descriptor])),
        None,
        Arc::new(ReqwestUsageMeterHttp::new()),
    )
    .report(&store)
    .await
    .expect("usage.report service");
    let reported = &report.accounts[0].local.cache;
    assert_eq!(reported.cache_read_tokens, cache.cache_read_tokens);
    assert_eq!(reported.cache_write_tokens, cache.cache_write_tokens);
    assert_eq!(reported.uncached_input_tokens, cache.uncached_input_tokens);
    assert_eq!(reported.requests.len(), cache.requests.len());
    assert_eq!(
        serde_json::to_value(&reported.requests).expect("report requests"),
        serde_json::to_value(&cache.requests).expect("journal requests"),
        "usage.report preserves every per-request observation"
    );
    if let Some(directory) = std::env::var_os("HAIDER_CACHE_EVIDENCE_DIR") {
        let directory = std::path::PathBuf::from(directory).join(if cache_on {
            "after"
        } else {
            "before-controls-off"
        });
        std::fs::create_dir_all(&directory).expect("evidence directory");
        std::fs::write(
            directory.join("synthetic-requests.json"),
            serde_json::to_vec_pretty(&records).expect("records JSON"),
        )
        .expect("save requests");
        std::fs::write(
            directory.join("synthetic-usage-report.json"),
            serde_json::to_vec_pretty(&report).expect("usage JSON"),
        )
        .expect("save report");
    }
    records
}
