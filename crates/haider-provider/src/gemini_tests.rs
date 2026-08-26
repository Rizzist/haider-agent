#![allow(clippy::expect_used)]

use std::future;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::item::ToolStatus;
use haider_protocol::provider::{Block, FinishReason, PrefixDigests, StreamEvent};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::gemini::{
    GeminiCacheBackend, GeminiCacheRegistry, GeminiDecoder, GeminiProvider, GeminiRetryPolicy,
    GeminiSseChunkSource, gemini_request_json, parse_protobuf_duration_ms, replay_gemini_sse,
    stream_sse_source,
};
use crate::origin::FixedDnsResolver;
use crate::{
    GEMINI_PROVIDER_NAME, Message, PromptCacheMetadata, ProviderError, ProviderErrorKind,
    ToolDefinition, TurnRequest, UserCommandRecord,
};

struct StubFixedResolver {
    address: SocketAddr,
}

#[async_trait]
impl FixedDnsResolver for StubFixedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![self.address])
    }
}

fn provider_with_resolver(address: SocketAddr) -> GeminiProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("gemini-origin-audit");
    vault
        .put(&alias, b"GEMINI_API_KEY_SENTINEL_91a7")
        .expect("stores audit key");
    GeminiProvider::new_with_dns_resolver(
        vault.resolve(&alias).expect("resolves audit key"),
        "gemini-2.5-flash",
        Arc::new(StubFixedResolver { address }),
    )
    .expect("constructs Gemini provider")
}

#[test]
fn constructor_transport_config_disables_retries_and_pins_all_timeouts() {
    let config = GeminiProvider::transport_config();
    assert_eq!(config.retry_policy, GeminiRetryPolicy::Never);
    assert_eq!(config.connect_timeout, Duration::from_secs(10));
    assert_eq!(config.response_open_timeout, Duration::from_secs(30));
    assert_eq!(config.chunk_idle_timeout, Duration::from_secs(90));
}

#[test]
fn gemini_credential_client_ignores_inherited_proxy_environment() {
    const CHILD_MARKER: &str = "HAIDER_GEMINI_PROXY_PIN_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let provider = provider_with_resolver(SocketAddr::from(([93, 184, 216, 34], 443)));
        assert!(
            !provider.client_debug().contains("proxies"),
            "Gemini credential-bearing client retained inherited proxy configuration"
        );
        return;
    }

    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .arg("gemini_credential_client_ignores_inherited_proxy_environment")
        .arg("--nocapture")
        .env(CHILD_MARKER, "1")
        .env("HTTP_PROXY", "http://127.0.0.1:18080")
        .env("HTTPS_PROXY", "http://127.0.0.1:18080")
        .env("ALL_PROXY", "http://127.0.0.1:18080")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("runs isolated Gemini proxy child");
    assert!(
        output.status.success(),
        "Gemini proxy child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// MUTATION CHECK: replace the API-key header with bearer auth, omit the
/// fixed `alt=sse` query, build the request before origin validation, or
/// disconnect reqwest from the pinned resolver. Each mutation changes an
/// assertion before a credential can reach a network peer.
#[tokio::test]
async fn x_goog_api_key_is_sensitive_and_request_consumes_fixed_origin_guard() {
    let provider = provider_with_resolver(SocketAddr::from(([93, 184, 216, 34], 443)));
    let request = provider
        .request(&serde_json::json!({"contents": []}))
        .await
        .expect("builds pinned request");
    assert_eq!(
        request.url().as_str(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    let api_key = request
        .headers()
        .get("x-goog-api-key")
        .expect("Gemini API-key header");
    assert_eq!(api_key, "GEMINI_API_KEY_SENTINEL_91a7");
    assert!(api_key.is_sensitive());
    assert!(
        !request
            .headers()
            .contains_key(reqwest::header::AUTHORIZATION)
    );

    provider.stall_fixed_connection_resolution();
    let execution = provider.execute_request_for_test(request);
    tokio::pin!(execution);
    let resolution_observed = async {
        while provider.fixed_connection_resolution_count() == 0 {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        result = &mut execution => panic!("fixed resolver did not stall request: {result:?}"),
        observed = tokio::time::timeout(Duration::from_secs(1), resolution_observed) => {
            observed.expect("reqwest consumes pinned Gemini resolver");
        }
    }
    assert_eq!(provider.fixed_connection_resolution_count(), 1);
}

#[tokio::test]
async fn private_or_special_dns_answers_fail_before_api_key_request_building() {
    for address in [
        SocketAddr::from(([127, 0, 0, 1], 443)),
        SocketAddr::from(([169, 254, 169, 254], 443)),
        "100.100.100.200:443".parse().expect("metadata address"),
        "[::ffff:127.0.0.1]:443".parse().expect("mapped loopback"),
    ] {
        let provider = provider_with_resolver(address);
        let error = provider
            .request(&serde_json::json!({"contents": []}))
            .await
            .expect_err("private resolution is rejected");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(!error.message.contains("GEMINI_API_KEY_SENTINEL_91a7"));
    }
}

#[test]
fn synthesized_call_ids_are_deterministic_and_accept_a_history_offset() {
    let bytes = b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"weather\",\"args\":{\"city\":\"Tehran\"}}}]},\"finishReason\":\"STOP\"}]}\n\n";
    let decode = |base| {
        let mut decoder = GeminiDecoder::new(None, base);
        let mut items = decoder.push(bytes);
        items.extend(decoder.finish());
        items
    };
    assert_eq!(decode(0), decode(0));
    assert!(matches!(
        &decode(0)[0],
        Ok(StreamEvent::ToolCallStart { call_id, .. })
            if call_id == "gemini-call-0000000000000000"
    ));
    assert!(matches!(
        &decode(7)[0],
        Ok(StreamEvent::ToolCallStart { call_id, .. })
            if call_id == "gemini-call-0000000000000007"
    ));
    assert!(matches!(
        decode(7).last(),
        Some(Ok(StreamEvent::Finish {
            reason: FinishReason::ToolUse
        }))
    ));
}

#[test]
fn retry_info_protobuf_durations_are_millisecond_exact_and_bounded() {
    assert_eq!(parse_protobuf_duration_ms("3s"), Some(3_000));
    assert_eq!(parse_protobuf_duration_ms("1.25s"), Some(1_250));
    assert_eq!(parse_protobuf_duration_ms("0.0075s"), Some(7));
    assert_eq!(parse_protobuf_duration_ms("bad"), None);
    assert_eq!(parse_protobuf_duration_ms("1.-2s"), None);
}

struct HangingFixture {
    first_chunk: Option<Vec<u8>>,
}

impl GeminiSseChunkSource for HangingFixture {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        if let Some(chunk) = self.first_chunk.take() {
            return Ok(Some(chunk));
        }
        future::pending().await
    }
}

#[tokio::test]
async fn hanging_mid_turn_fixture_times_out_only_the_idle_chunk_await() {
    tokio::time::pause();
    let (sender, mut receiver) = mpsc::channel(4);
    let stream_task = tokio::spawn(stream_sse_source(
        HangingFixture {
            first_chunk: Some(
                b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n"
                    .to_vec(),
            ),
        },
        None,
        0,
        sender,
        Duration::from_secs(90),
    ));
    assert_eq!(
        receiver.recv().await,
        Some(Ok(StreamEvent::TextDelta {
            text: "partial".into(),
        }))
    );
    tokio::time::advance(Duration::from_secs(89)).await;
    tokio::task::yield_now().await;
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    tokio::time::advance(Duration::from_secs(1)).await;
    let error = receiver
        .recv()
        .await
        .expect("idle timeout item")
        .expect_err("idle timeout is typed");
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(error.message.contains("90 seconds"));
    assert!(receiver.recv().await.is_none());
    stream_task.await.expect("stream task exits");
}

/// CM1b/CM1c — Gemini cachedContentTokenCount is a subset of prompt input,
/// while an omitted field is unavailable rather than a reported zero.
///
/// MUTATION CHECK (executed): keep promptTokenCount as uncached input or
/// default the missing counter to zero; the 28/3 and availability assertions
/// fail on the two captured responses.
#[test]
fn cm1b_cm1c_gemini_subset_and_missing_cache_telemetry() {
    use haider_protocol::provider::CacheStatAvailability;

    let usage_from = |bytes: &[u8]| {
        replay_gemini_sse(bytes)
            .into_iter()
            .find_map(|event| match event {
                Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
                _ => None,
            })
            .expect("Gemini usage")
    };
    let present = usage_from(include_bytes!("../tests/fixtures/gemini/combined.sse"));
    let present = present.normalized.expect("normalized present usage");
    assert_eq!(present.logical_input, 31);
    assert_eq!(present.uncached_input, 28);
    assert_eq!(present.cache_read_input, 3);
    assert_eq!(present.cache_status, CacheStatAvailability::Present);

    let missing = usage_from(include_bytes!(
        "../tests/fixtures/gemini/usage_metadata.sse"
    ));
    let missing = missing.normalized.expect("normalized missing usage");
    assert_eq!(missing.logical_input, 8);
    assert_eq!(missing.uncached_input, 8);
    assert_eq!(missing.cache_read_input, 0);
    assert_eq!(missing.cache_status, CacheStatAvailability::Unavailable);
}

#[derive(Debug, Default)]
struct RecordingCacheBackend {
    sequence: AtomicUsize,
    operations: StdMutex<Vec<String>>,
    create_payloads: StdMutex<Vec<serde_json::Value>>,
}

#[async_trait]
impl GeminiCacheBackend for RecordingCacheBackend {
    async fn create_cached_content(
        &self,
        payload: &serde_json::Value,
    ) -> Result<String, ProviderError> {
        let next = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        self.operations
            .lock()
            .expect("operations lock")
            .push(format!("create:{next}"));
        self.create_payloads
            .lock()
            .expect("payloads lock")
            .push(payload.clone());
        Ok(format!("cachedContents/mock-{next}"))
    }

    async fn delete_cached_content(&self, name: &str) -> Result<(), ProviderError> {
        self.operations
            .lock()
            .expect("operations lock")
            .push(format!("delete:{name}"));
        Ok(())
    }
}

fn gemini_cache_request(model: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![
            Message::user_text("stable question"),
            Message::assistant(vec![Block::Text {
                text: "stable answer".into(),
            }]),
            Message::user_text("volatile question"),
        ],
        model: model.into(),
        max_tokens: 256,
        system_prompt: Some("stable system".into()),
        tools: vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look something up".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }),
        }],
        attachments: Vec::new(),
        cache_metadata: Some(PromptCacheMetadata {
            stable_history_end: 2,
            cacheable_history_end: None,
            current_user_start: 2,
            previous_stable_history_end: None,
            latest_compaction_summary_end: None,
            prefix_digests: PrefixDigests {
                system: "system-a".into(),
                tools: "tools-a".into(),
                immutable_history: "history-a".into(),
                model: "model-a".into(),
                auth_mode: "auth-a".into(),
                reasoning_settings: "reasoning-a".into(),
            },
            cache_epoch: "epoch-a".into(),
            header_epoch: String::new(),
            compaction_epoch: "compaction-a".into(),
            provider: GEMINI_PROVIDER_NAME.into(),
            session_scope: "session-a".into(),
            account_scope: Some("account-a".into()),
            stable_prefix_tokens: 2_048,
            expected_later_reads: 2,
            reuse_gap_ms: None,
        }),
    }
}

#[test]
fn cache_diagnostic_gemini_hashes_current_wire_through_previous_history_length() {
    let provider = provider_with_resolver(SocketAddr::from(([127, 0, 0, 1], 443)));
    let first = gemini_cache_request("gemini-2.5-flash");
    let first_prepared = crate::Provider::prepare_turn(&provider, &first).expect("first prepared");

    let mut grown = first;
    grown.messages.extend([
        Message::assistant(vec![Block::Text {
            text: "current answer".into(),
        }]),
        Message::user_text("next question"),
    ]);
    let metadata = grown.cache_metadata.as_mut().expect("metadata");
    metadata.previous_stable_history_end = Some(2);
    metadata.stable_history_end = 4;
    metadata.current_user_start = 4;
    let grown_prepared = crate::Provider::prepare_turn(&provider, &grown).expect("grown prepared");

    assert_eq!(
        grown_prepared.previous_immutable_history_digest(),
        Some(first_prepared.prefix_digests().immutable_history.as_str()),
        "the old Gemini contents prefix remains hashable after history grows"
    );
    assert_ne!(
        grown_prepared.prefix_digests().immutable_history,
        first_prepared.prefix_digests().immutable_history
    );
}

#[test]
fn prepared_gemini_wire_bytes_match_legacy_final_render() {
    let provider = provider_with_resolver(SocketAddr::from(([127, 0, 0, 1], 443)));
    let request = gemini_cache_request("gemini-2.5-flash");
    let legacy = provider
        .request_payload(&request)
        .expect("legacy Gemini payload");
    let prepared = crate::Provider::prepare_turn(&provider, &request).expect("prepared Gemini");
    assert_eq!(
        serde_json::to_vec(&prepared.wire.as_ref().expect("prepared wire").payload)
            .expect("prepared Gemini bytes"),
        serde_json::to_vec(&legacy).expect("legacy Gemini bytes")
    );
}

#[test]
fn user_command_record_reaches_gemini_as_labeled_user_text() {
    let mut request = gemini_cache_request("gemini-2.5-flash");
    request.cache_metadata = None;
    request.messages = vec![Message::user_command(UserCommandRecord {
        call_id: "user-command-gemini".into(),
        command: "printf gemini-user-command".into(),
        status: ToolStatus::Completed,
        exit_code: Some(0),
        output_preview: "[stdout]\ngemini-user-command".into(),
        output_bytes: 19,
        output_truncated: false,
        output_lossy_utf8: false,
    })];

    let payload = gemini_request_json(&request, None, false).expect("Gemini user-command payload");
    assert_eq!(payload["contents"][0]["role"], "user");
    let text = payload["contents"][0]["parts"][0]["text"]
        .as_str()
        .expect("Gemini text part");
    assert!(text.contains("[user-initiated shell command]"));
    assert!(text.contains("origin: user_command"));
    assert!(text.contains("printf gemini-user-command"));
    assert!(text.contains("gemini-user-command"));
}

/// CM2e — an eligible epoch creates once, reuses the returned resource name,
/// and synchronously deletes the superseded resource before creating its
/// replacement.
///
/// MUTATION CHECK (executed): skip the delete or key the registry by turn;
/// the exact operation sequence and reused-name assertions fail.
#[tokio::test]
async fn cm2e_gemini_cached_content_create_reuse_and_delete_superseded() {
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let backend_trait: Arc<dyn GeminiCacheBackend> = backend.clone();
    let request = gemini_cache_request("gemini-2.5-flash");
    let full = gemini_request_json(&request, None, false).expect("full payload");

    let first = registry
        .prepare_generate_payload(&request, full.clone(), backend_trait.clone(), None, false)
        .await;
    assert_eq!(first["cachedContent"], "cachedContents/mock-1");

    let reused = registry
        .prepare_generate_payload(&request, full.clone(), backend_trait.clone(), None, false)
        .await;
    assert_eq!(reused["cachedContent"], "cachedContents/mock-1");

    let mut transitioned = request;
    let metadata = transitioned.cache_metadata.as_mut().expect("metadata");
    metadata.cache_epoch = "epoch-b".into();
    metadata.compaction_epoch = "compaction-b".into();
    let transitioned_full =
        gemini_request_json(&transitioned, None, false).expect("transitioned full payload");
    let replaced = registry
        .prepare_generate_payload(&transitioned, transitioned_full, backend_trait, None, false)
        .await;
    assert_eq!(replaced["cachedContent"], "cachedContents/mock-2");
    assert_eq!(
        *backend.operations.lock().expect("operations lock"),
        ["create:1", "delete:cachedContents/mock-1", "create:2"]
    );
}

#[tokio::test]
async fn cm2e_gemini_refreshes_when_cached_coverage_falls_below_eighty_percent() {
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let mut request = gemini_cache_request("gemini-2.5-flash");
    let initial = gemini_request_json(&request, None, false).expect("initial full payload");
    let first = registry
        .prepare_generate_payload(&request, initial, backend.clone(), None, false)
        .await;
    assert_eq!(first["cachedContent"], "cachedContents/mock-1");

    request.messages.push(Message::assistant(vec![Block::Text {
        text: "now-stable answer".into(),
    }]));
    request
        .messages
        .push(Message::user_text("new volatile question"));
    let metadata = request.cache_metadata.as_mut().expect("metadata");
    metadata.stable_history_end = 4;
    metadata.current_user_start = 4;
    metadata.stable_prefix_tokens = 2_500;
    let grown = gemini_request_json(&request, None, false).expect("grown full payload");
    let still_reused = registry
        .prepare_generate_payload(&request, grown.clone(), backend.clone(), None, false)
        .await;
    assert_eq!(still_reused["cachedContent"], "cachedContents/mock-1");

    request
        .cache_metadata
        .as_mut()
        .expect("metadata")
        .stable_prefix_tokens = 3_000;
    let refreshed = registry
        .prepare_generate_payload(&request, grown, backend.clone(), None, false)
        .await;
    assert_eq!(refreshed["cachedContent"], "cachedContents/mock-2");
    assert_eq!(
        *backend.operations.lock().expect("operations lock"),
        ["create:1", "delete:cachedContents/mock-1", "create:2"]
    );
    assert_eq!(
        backend.create_payloads.lock().expect("payloads lock")[1]["contents"]
            .as_array()
            .expect("refreshed cached contents")
            .len(),
        4
    );
}

/// Expiry is another supersession boundary: a dead resource name is deleted
/// and recreated instead of being sent forever after its one-hour TTL.
#[tokio::test(start_paused = true)]
async fn cm2e_gemini_expired_resource_is_deleted_and_recreated() {
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let request = gemini_cache_request("gemini-2.5-flash");
    let full = gemini_request_json(&request, None, false).expect("full payload");
    let first = registry
        .prepare_generate_payload(&request, full.clone(), backend.clone(), None, false)
        .await;
    assert_eq!(first["cachedContent"], "cachedContents/mock-1");

    tokio::time::advance(Duration::from_secs(3_601)).await;
    let recreated = registry
        .prepare_generate_payload(&request, full, backend.clone(), None, false)
        .await;
    assert_eq!(recreated["cachedContent"], "cachedContents/mock-2");
    assert_eq!(
        *backend.operations.lock().expect("operations lock"),
        ["create:1", "delete:cachedContents/mock-1", "create:2"]
    );
}

#[tokio::test]
async fn cm2e_gemini_switch_away_deletes_session_resource() {
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let request = gemini_cache_request("gemini-2.5-flash");
    let full = gemini_request_json(&request, None, false).expect("full payload");
    let _ = registry
        .prepare_generate_payload(&request, full, backend.clone(), None, false)
        .await;
    registry
        .delete_scope("session-a")
        .await
        .expect("switch-away delete");
    assert_eq!(
        *backend.operations.lock().expect("operations lock"),
        ["create:1", "delete:cachedContents/mock-1"]
    );
}

/// CM2g — the model-visible Gemini input is exactly the original full body
/// when the cached resource prefix and generate-call suffix are recombined.
#[tokio::test]
async fn cm2g_gemini_cached_prefix_plus_suffix_equals_full_model_input() {
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let request = gemini_cache_request("gemini-2.5-flash");
    let full = gemini_request_json(&request, None, false).expect("full payload");
    let generated = registry
        .prepare_generate_payload(&request, full.clone(), backend.clone(), None, false)
        .await;
    let created = backend.create_payloads.lock().expect("payloads lock")[0].clone();

    assert_eq!(created["systemInstruction"], full["system_instruction"]);
    assert_eq!(created["tools"], full["tools"]);
    let mut effective_contents = created["contents"]
        .as_array()
        .expect("cached contents")
        .clone();
    effective_contents.extend(
        generated["contents"]
            .as_array()
            .expect("volatile suffix")
            .iter()
            .cloned(),
    );
    assert_eq!(
        serde_json::Value::Array(effective_contents),
        full["contents"]
    );
    assert!(generated.get("system_instruction").is_none());
    assert!(generated.get("tools").is_none());
}

#[tokio::test]
async fn cm2g_gemini_cached_prefix_preserves_signed_parts_byte_exact() {
    let signed = serde_json::json!({
        "kind": "signed_part",
        "call_id": "gemini-call-0000000000000000",
        "part": {
            "functionCall": {"name": "lookup", "args": {"z": 1, "a": 2}},
            "thoughtSignature": "signed-provider-bytes"
        }
    });
    let mut request = gemini_cache_request("gemini-2.5-flash");
    request.messages = vec![
        Message::assistant(vec![
            Block::ProviderOpaque {
                provider: GEMINI_PROVIDER_NAME.into(),
                data: signed.clone(),
            },
            Block::ToolCall {
                call_id: "gemini-call-0000000000000000".into(),
                name: "lookup".into(),
                args: serde_json::json!({"z": 1, "a": 2}),
            },
        ]),
        Message::tool_result("gemini-call-0000000000000000", "result", false),
    ];
    let metadata = request.cache_metadata.as_mut().expect("metadata");
    metadata.stable_history_end = 1;
    metadata.current_user_start = 1;
    let full = gemini_request_json(&request, None, false).expect("signed full payload");
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let generated = registry
        .prepare_generate_payload(&request, full.clone(), backend.clone(), None, false)
        .await;
    let created = backend.create_payloads.lock().expect("payloads lock")[0].clone();
    assert_eq!(created["contents"][0]["parts"][0], signed["part"]);
    let mut recombined = created["contents"]
        .as_array()
        .expect("cached contents")
        .clone();
    recombined.extend(
        generated["contents"]
            .as_array()
            .expect("suffix")
            .iter()
            .cloned(),
    );
    assert_eq!(serde_json::Value::Array(recombined), full["contents"]);
}

/// CM2f — unknown Gemini models stay on implicit caching with the exact full
/// request and no resource lifecycle calls.
#[tokio::test]
async fn cm2f_unknown_gemini_model_is_byte_exact_implicit_cache_fallback() {
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let request = gemini_cache_request("gemini-3.6-pro");
    let full = gemini_request_json(&request, None, false).expect("full payload");
    let fallback = registry
        .prepare_generate_payload(&request, full.clone(), backend.clone(), None, false)
        .await;
    assert_eq!(fallback, full);
    assert!(
        backend
            .operations
            .lock()
            .expect("operations lock")
            .is_empty()
    );
}

/// The explicit-cache gate consumes the shared documented-minimum registry.
/// Similar but undocumented model names cannot inherit a threshold.
#[tokio::test]
async fn gemini_explicit_cache_gate_uses_documented_model_minimums() {
    let registry = GeminiCacheRegistry::default();
    let backend = Arc::new(RecordingCacheBackend::default());
    let mut request = gemini_cache_request("gemini-3.7-flash");
    request
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .stable_prefix_tokens = 4_095;
    let full = gemini_request_json(&request, None, false).expect("full payload");
    let below = registry
        .prepare_generate_payload(&request, full.clone(), backend.clone(), None, false)
        .await;
    assert_eq!(below, full);

    request
        .cache_metadata
        .as_mut()
        .expect("cache metadata")
        .stable_prefix_tokens = 4_096;
    let eligible = gemini_request_json(&request, None, false).expect("eligible payload");
    let cached = registry
        .prepare_generate_payload(&request, eligible, backend.clone(), None, false)
        .await;
    assert_eq!(cached["cachedContent"], "cachedContents/mock-1");

    let unknown = gemini_cache_request("gemini-2.5-flash-lite");
    let unknown_full = gemini_request_json(&unknown, None, false).expect("unknown full payload");
    let unknown_registry = GeminiCacheRegistry::default();
    let fallback = unknown_registry
        .prepare_generate_payload(&unknown, unknown_full.clone(), backend.clone(), None, false)
        .await;
    assert_eq!(fallback, unknown_full);
    assert_eq!(
        *backend.operations.lock().expect("operations lock"),
        ["create:1"]
    );
}
