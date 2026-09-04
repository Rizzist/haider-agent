#![allow(clippy::expect_used)]

use std::future;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::ids::ArtifactRef;
use haider_protocol::item::ToolStatus;
use haider_protocol::provider::{Block, PrefixDigests, StreamEvent};
use haider_protocol::tool::{AttachmentBlock, ImageBlockRef, PdfDeliveryMode};
use reqwest::header::AUTHORIZATION;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::anthropic::{
    ANTHROPIC_COMPUTER_BETA_20250124, ANTHROPIC_COMPUTER_BETA_20251124, ANTHROPIC_FAST_BETA_VALUE,
    ANTHROPIC_OAUTH_BASE_URL, ANTHROPIC_OAUTH_BETA_HEADER, ANTHROPIC_OAUTH_BETA_VALUE,
    ANTHROPIC_OAUTH_SYSTEM_IDENTITY, AnthropicComputerToolVersion, AnthropicProvider,
    SseChunkSource, anthropic_computer_tool_version, read_error_body_bounded,
    replay_anthropic_native_computer_sse, replay_anthropic_sse, stream_sse_source,
};
use crate::origin::FixedDnsResolver;
use crate::{
    AnthropicCacheTtl, Message, PromptCacheMetadata, Provider as _, ProviderError,
    ProviderErrorKind, ResolvedAttachment, ToolDefinition, TurnRequest, UserCommandRecord,
    select_anthropic_cache_ttl,
};

struct HangingFixture {
    first_chunk: Option<Vec<u8>>,
}

struct StubFixedResolver {
    address: SocketAddr,
}

fn correlation_attempt() -> haider_protocol::cache::ProviderRequestAttemptV1 {
    haider_protocol::cache::ProviderRequestAttemptV1 {
        session_id: haider_protocol::ids::SessionId::new("session-anthropic"),
        run_id: haider_protocol::ids::RunId::new("run-anthropic"),
        turn_ordinal: 2,
        request_ordinal: 3,
        request_kind: haider_protocol::cache::ProviderRequestKind::Primary,
    }
}

#[tokio::test]
async fn anthropic_request_has_locked_correlation_headers_without_body_mutation() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-turn-correlation");
    vault.put(&alias, b"audit-key").expect("stores audit key");
    let provider = AnthropicProvider::new(
        vault.resolve(&alias).expect("resolves audit key"),
        "claude-audit",
    )
    .expect("Anthropic provider");
    let payload = serde_json::json!({"model":"claude-audit","messages":[]});
    let baseline = provider
        .request_body(payload.clone())
        .await
        .expect("baseline request");
    let attempt = correlation_attempt();
    let correlated = crate::scope_provider_request(
        attempt.clone(),
        crate::RequestMetadataBodySupport::Unsupported,
        provider.request_body(payload),
    )
    .await
    .expect("correlated request");
    assert_eq!(
        correlated.headers()[crate::HAIDER_TURN_HEADER],
        "session-anthropic/run-anthropic/2/3"
    );
    assert_eq!(
        correlated.headers()[crate::HAIDER_REQUEST_KIND_HEADER],
        "primary"
    );
    let baseline_body = baseline
        .body()
        .expect("baseline request body")
        .as_bytes()
        .expect("baseline request byte body");
    let correlated_body = correlated
        .body()
        .expect("correlated request body")
        .as_bytes()
        .expect("correlated request byte body");
    assert_eq!(baseline_body, correlated_body);
    let expected_body = correlated_body.to_vec();
    let ledger = crate::capture_in_fake_proxy_ledger(correlated).await;
    assert_eq!(
        ledger.headers.get("x-haider-turn").map(String::as_str),
        Some("session-anthropic/run-anthropic/2/3")
    );
    assert_eq!(
        ledger
            .headers
            .get("x-haider-request-kind")
            .map(String::as_str),
        Some("primary")
    );
    assert_eq!(ledger.body, expected_body);
}

#[async_trait]
impl FixedDnsResolver for StubFixedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![self.address])
    }
}

#[test]
fn anthropic_credential_client_ignores_inherited_proxy_environment() {
    const CHILD_MARKER: &str = "HAIDER_ANTHROPIC_PROXY_PIN_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let vault = MemoryVault::new();
        let alias = CredentialAlias::new("anthropic-proxy-audit");
        vault
            .put(&alias, b"anthropic-proxy-sentinel")
            .expect("store proxy audit secret");
        let credential = vault.resolve(&alias).expect("resolve proxy audit secret");
        let provider =
            AnthropicProvider::new(credential, "claude-audit").expect("Anthropic client");
        assert!(
            !provider.client_debug().contains("proxies"),
            "Anthropic credential-bearing client retained inherited proxy configuration"
        );
        let subscription_vault = MemoryVault::new();
        let subscription_alias = CredentialAlias::new("anthropic-subscription-proxy-audit");
        subscription_vault
            .put(
                &subscription_alias,
                b"anthropic-subscription-proxy-sentinel",
            )
            .expect("store subscription proxy audit secret");
        let subscription = AnthropicProvider::new_subscription(
            subscription_vault
                .resolve(&subscription_alias)
                .expect("resolve subscription proxy secret"),
            "claude-audit",
            ANTHROPIC_OAUTH_BASE_URL,
        )
        .expect("Anthropic subscription client");
        assert!(
            !subscription.client_debug().contains("proxies"),
            "Anthropic subscription client retained inherited proxy configuration"
        );
        return;
    }

    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .arg("anthropic_credential_client_ignores_inherited_proxy_environment")
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
        .expect("run isolated Anthropic proxy child");
    assert!(
        output.status.success(),
        "Anthropic proxy child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// MUTATION CHECK: restore x-api-key, drop the OAuth beta, change Bearer, or
/// accept a private subscription origin, or disconnect the pinned resolver.
/// The named assertions fail without sending a credential-bearing request.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn anthropic_oauth_subscription_is_bearer_beta_without_api_key_and_fixed_origin() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-oauth-request-audit");
    vault
        .put(&alias, b"ANTHROPIC_OAUTH_ACCESS_SENTINEL_591c")
        .expect("store OAuth access");
    let provider = AnthropicProvider::new_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve OAuth access"),
        "claude-audit",
        ANTHROPIC_OAUTH_BASE_URL,
        Arc::new(StubFixedResolver {
            address: SocketAddr::from(([93, 184, 216, 34], 443)),
        }),
    )
    .expect("Anthropic subscription provider");
    let request = provider
        .request_body(serde_json::json!({"model":"claude-audit"}))
        .await
        .expect("fixed request");
    assert_eq!(
        request.url().as_str(),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(
        request.headers().get(AUTHORIZATION).expect("Bearer header"),
        "Bearer ANTHROPIC_OAUTH_ACCESS_SENTINEL_591c"
    );
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("OAuth beta header"),
        ANTHROPIC_OAUTH_BETA_VALUE
    );
    assert_eq!(
        request
            .headers()
            .get("anthropic-version")
            .expect("Anthropic version"),
        "2023-06-01"
    );
    assert!(!request.headers().contains_key("x-api-key"));
    assert!(
        provider.stall_fixed_connection_resolution(),
        "subscription provider has a fixed-origin guard"
    );
    let execution = provider.execute_request_for_test(request);
    tokio::pin!(execution);
    let resolution_observed = async {
        while provider.fixed_connection_resolution_count() == Some(0) {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        result = &mut execution => {
            panic!("fixed connection resolver did not stall the request: {result:?}");
        }
        observed = tokio::time::timeout(Duration::from_secs(1), resolution_observed) => {
            observed.expect("reqwest must consume the pinned fixed resolver");
        }
    }
    assert_eq!(
        provider.fixed_connection_resolution_count(),
        Some(1),
        "one connection lookup must use the pinned fixed resolver"
    );

    let private_vault = MemoryVault::new();
    let private_alias = CredentialAlias::new("anthropic-private-origin-audit");
    private_vault
        .put(&private_alias, b"NEVER_SEND_ANTHROPIC_PRIVATE_37b1")
        .expect("store private-origin sentinel");
    let rejected = AnthropicProvider::new_subscription(
        private_vault
            .resolve(&private_alias)
            .expect("resolve private-origin sentinel"),
        "claude-audit",
        "http://169.254.169.254",
    )
    .expect_err("private fixed base must be rejected");
    assert_eq!(rejected.kind, ProviderErrorKind::InvalidRequest);

    for rebound_address in [
        SocketAddr::from(([127, 0, 0, 1], 443)),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443)),
        "[::ffff:127.0.0.1]:443"
            .parse()
            .expect("IPv4-mapped loopback address"),
        "100.100.100.200:443"
            .parse()
            .expect("RFC 6598 metadata address"),
        SocketAddr::from(([169, 254, 169, 254], 443)),
    ] {
        let rebound_vault = MemoryVault::new();
        let rebound_alias = CredentialAlias::new("anthropic-rebound-origin-audit");
        rebound_vault
            .put(&rebound_alias, b"NEVER_SEND_ANTHROPIC_REBOUND_c94d")
            .expect("store rebound sentinel");
        let rebound = AnthropicProvider::new_subscription_with_dns_resolver(
            rebound_vault
                .resolve(&rebound_alias)
                .expect("resolve rebound sentinel"),
            "claude-audit",
            ANTHROPIC_OAUTH_BASE_URL,
            Arc::new(StubFixedResolver {
                address: rebound_address,
            }),
        )
        .expect("construct fixed-host rebound audit");
        let rebound_error = rebound
            .request_body(serde_json::json!({"model":"claude-audit"}))
            .await
            .expect_err("loopback/private DNS answer must fail before bearer construction");
        assert_eq!(rebound_error.kind, ProviderErrorKind::InvalidRequest);
    }
}

fn payload_provider(oauth: bool) -> AnthropicProvider {
    model_payload_provider(oauth, "claude-audit")
}

fn model_payload_provider(oauth: bool, model: &str) -> AnthropicProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-payload-audit");
    vault
        .put(&alias, b"anthropic-payload-audit-sentinel")
        .expect("store payload audit secret");
    let credential = vault.resolve(&alias).expect("resolve payload audit secret");
    if oauth {
        AnthropicProvider::new_subscription(credential, model, ANTHROPIC_OAUTH_BASE_URL)
            .expect("Anthropic subscription provider")
    } else {
        AnthropicProvider::new(credential, model).expect("Anthropic key provider")
    }
}

#[test]
fn usage_lane_dimensions_are_adapter_owned_and_api_keys_omit_speed() {
    let fast = model_payload_provider(true, "claude-opus-5")
        .with_effort(Some("xhigh".into()))
        .with_fast(true)
        .usage_lane_dimensions();
    assert_eq!(fast.api_family.as_deref(), Some("anthropic_messages"));
    assert_eq!(fast.effort.as_deref(), Some("xhigh"));
    assert_eq!(fast.speed.as_deref(), Some("fast"));

    let standard = model_payload_provider(true, "claude-opus-5").usage_lane_dimensions();
    assert_eq!(standard.speed.as_deref(), Some("standard"));

    let api_key = model_payload_provider(false, "claude-opus-5")
        .with_fast(true)
        .usage_lane_dimensions();
    assert!(api_key.speed.is_none());

    // MUTATION CHECK: returning the adapter default or inferring speed from
    // the fast flag alone fails the exact effort/standard/API-key assertions.
}

fn payload_request(system_prompt: Option<&str>) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text("Reply with exactly: payload-audit")],
        model: "claude-audit".into(),
        max_tokens: 30_000,
        system_prompt: system_prompt.map(str::to_owned),
        tools: vec![
            ToolDefinition {
                name: "fs_read".into(),
                description: "Read a UTF-8 file".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                }),
            },
            ToolDefinition {
                name: "fs_search".into(),
                description: "Search UTF-8 files".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "anyOf": [{"required": ["pattern"]}, {"required": ["query"]}],
                    "oneOf": [{"required": ["pattern"]}],
                    "allOf": [{"type": "object"}],
                    "properties": {
                        "pattern": {"type": "string"},
                        "query": {"type": "string"},
                        "mode": {"anyOf": [{"const": "literal"}, {"const": "simple"}]},
                    },
                }),
            },
        ],
        attachments: Vec::new(),
        cache_metadata: None,
    }
}

#[test]
fn user_command_record_reaches_anthropic_as_labeled_user_text() {
    let mut request = payload_request(None);
    request.messages = vec![Message::user_command(UserCommandRecord {
        call_id: "user-command-a".into(),
        command: "printf anthropic-user-command".into(),
        status: ToolStatus::Completed,
        exit_code: Some(0),
        output_preview: "[stdout]\nanthropic-user-command".into(),
        output_bytes: 22,
        output_truncated: true,
        output_lossy_utf8: false,
    })];

    let payload = payload_provider(false)
        .request_payload(&request)
        .expect("Anthropic user-command payload");
    assert_eq!(payload["messages"][0]["role"], "user");
    let text = payload["messages"][0]["content"][0]["text"]
        .as_str()
        .expect("Anthropic text block");
    assert!(text.contains("[user-initiated shell command]"));
    assert!(text.contains("origin: user_command"));
    assert!(text.contains("printf anthropic-user-command"));
    assert!(text.contains("anthropic-user-command"));
    assert!(text.contains("model-context output preview truncated"));
}

fn cache_metadata(provider: &str, stable_history_end: usize) -> PromptCacheMetadata {
    PromptCacheMetadata {
        stable_history_end,
        cacheable_history_end: None,
        current_user_start: stable_history_end,
        previous_stable_history_end: None,
        latest_compaction_summary_end: Some(1),
        prefix_digests: PrefixDigests {
            system: "system-digest".into(),
            tools: "tool-digest".into(),
            immutable_history: "history-digest".into(),
            model: "model-digest".into(),
            auth_mode: "auth-digest".into(),
            reasoning_settings: "reasoning-digest".into(),
        },
        cache_epoch: "epoch-a".into(),
        header_epoch: String::new(),
        compaction_epoch: "compaction-a".into(),
        provider: provider.into(),
        session_scope: "session-a".into(),
        cache_cohort: None,
        account_scope: Some("account-a".into()),
        stable_prefix_tokens: 8_192,
        expected_later_reads: 2,
        reuse_gap_ms: Some(30_000),
    }
}

fn cache_control_request() -> TurnRequest {
    TurnRequest {
        messages: vec![
            Message::user_text("Compacted history summary"),
            Message::user_text("prior question"),
            Message::assistant(vec![Block::Text {
                text: "prior answer".into(),
            }]),
            Message::user_text("current question"),
        ],
        model: "claude-audit".into(),
        max_tokens: 128,
        system_prompt: Some("Haider system".into()),
        tools: vec![ToolDefinition {
            name: "fs_read".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }],
        attachments: Vec::new(),
        cache_metadata: Some(cache_metadata("anthropic-oauth", 3)),
    }
}

fn strip_cache_control(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => values.iter_mut().for_each(strip_cache_control),
        serde_json::Value::Object(values) => {
            values.remove("cache_control");
            values.values_mut().for_each(strip_cache_control);
        }
        _ => {}
    }
}

/// CM2b — all four explicit Anthropic anchors are placed without moving or
/// decorating the OAuth identity block. The checked-in fixture is the exact
/// request body that crosses the adapter boundary.
///
/// MUTATION CHECK (executed): move any anchor by one message, annotate the
/// identity block, or drop tool/system/summary/history caching; the golden and
/// the explicit four-count assertion fail.
#[test]
fn cm2b_anthropic_four_breakpoints_oauth_identity_first_golden() {
    let payload = payload_provider(true)
        .with_prompt_caching_verified(true)
        .request_payload(&cache_control_request())
        .expect("cache-controlled OAuth payload");
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/anthropic/cache_control_request.json"
    ))
    .expect("Anthropic cache-control golden");
    assert_eq!(payload, expected);
    assert_eq!(
        payload["system"][0],
        serde_json::json!({"type": "text", "text": ANTHROPIC_OAUTH_SYSTEM_IDENTITY})
    );
    let cache_controls = payload.to_string().match_indices("cache_control").count();
    assert_eq!(
        cache_controls, 4,
        "exactly four explicit anchors: {payload}"
    );
}

#[test]
fn prepared_anthropic_wire_bytes_match_legacy_final_render() {
    let provider = payload_provider(true).with_prompt_caching_verified(true);
    let request = cache_control_request();
    let legacy = provider
        .request_payload(&request)
        .expect("legacy cache-controlled Anthropic payload");
    let prepared = provider.prepare_turn(&request).expect("prepared turn");
    let mut borrowed_request = request.clone();
    let shared_tools = std::mem::take(&mut borrowed_request.tools);
    let borrowed =
        crate::Provider::prepare_turn_with_tools(&provider, &borrowed_request, &shared_tools)
            .expect("borrowed-tools prepared turn");
    assert_eq!(
        crate::serialize_prepared_json_body_ref(borrowed.wire.as_ref().expect("borrowed wire"))
            .expect("borrowed bytes"),
        crate::serialize_prepared_json_body_ref(prepared.wire.as_ref().expect("prepared wire"))
            .expect("prepared bytes"),
        "Arc-backed preparation must preserve exact Anthropic wire bytes"
    );
    assert_eq!(
        crate::serialize_prepared_json_body_ref(prepared.wire.as_ref().expect("prepared wire"))
            .expect("prepared bytes"),
        serde_json::to_vec(&legacy).expect("legacy bytes")
    );

    let mut owned_request = cache_control_request();
    let artifact = ArtifactRef::new("blake3:owned-attachment-move");
    owned_request
        .messages
        .last_mut()
        .expect("current user message")
        .blocks
        .push(Block::Attachment(AttachmentBlock::Image {
            artifact: artifact.clone(),
            mime: "image/png".into(),
            width: None,
            height: None,
        }));
    owned_request.attachments.push(ResolvedAttachment {
        artifact,
        data_base64: "A".repeat(1024 * 1024),
    });
    let legacy_owned = provider
        .request_payload(&owned_request)
        .expect("legacy attachment payload");
    let original_pointer = owned_request.attachments[0].data_base64.as_ptr();
    let owned_tools = std::mem::take(&mut owned_request.tools);
    let owned =
        crate::Provider::prepare_turn_with_tools_owned(&provider, &mut owned_request, &owned_tools)
            .expect("ownership-aware prepared turn");
    let owned_payload = &owned.wire.as_ref().expect("owned wire").payload;
    let prepared_data = owned_payload["messages"][3]["content"][1]["source"]["data"]
        .as_str()
        .expect("prepared attachment data");
    assert_eq!(prepared_data.as_ptr(), original_pointer);
    assert_eq!(
        crate::serialize_prepared_json_body_ref(owned.wire.as_ref().expect("owned wire"))
            .expect("owned prepared bytes"),
        serde_json::to_vec(&legacy_owned).expect("legacy owned bytes")
    );

    let mut rejected_request = cache_control_request();
    let rejected_artifact = ArtifactRef::new("blake3:rejected-attachment-move");
    rejected_request
        .messages
        .last_mut()
        .expect("current user message")
        .blocks
        .push(Block::Attachment(AttachmentBlock::Pdf {
            artifact: rejected_artifact.clone(),
            name: "oversized.pdf".into(),
            pages: 1,
            delivery: PdfDeliveryMode::NativeDocument,
        }));
    rejected_request.attachments.push(ResolvedAttachment {
        artifact: rejected_artifact,
        data_base64: "A".repeat(32 * 1024 * 1024),
    });
    let rejected_pointer = rejected_request.attachments[0].data_base64.as_ptr();
    let rejected_tools = std::mem::take(&mut rejected_request.tools);
    assert!(
        crate::Provider::prepare_turn_with_tools_owned(
            &provider,
            &mut rejected_request,
            &rejected_tools,
        )
        .is_none(),
        "oversized native PDF must retain the provider's existing rejection"
    );
    assert_eq!(
        rejected_request.attachments[0].data_base64.as_ptr(),
        rejected_pointer,
        "a rejected preparation must move the original base64 allocation back"
    );
    assert_eq!(
        rejected_request.attachments[0].data_base64.len(),
        32 * 1024 * 1024
    );
}

#[test]
fn api_key_system_only_marker_preserves_legacy_diagnostic_shape_semantics() {
    let provider = payload_provider(false).with_prompt_caching_verified(true);
    let mut request = cache_control_request();
    request.tools.clear();
    request.messages.clear();
    let metadata = request.cache_metadata.as_mut().expect("cache metadata");
    metadata.provider = crate::ANTHROPIC_PROVIDER_NAME.into();
    metadata.stable_history_end = 0;
    metadata.current_user_start = 0;
    metadata.latest_compaction_summary_end = None;
    let prepared = provider.prepare_turn(&request).expect("prepared turn");
    assert!(
        prepared.wire.as_ref().expect("prepared wire").payload["system"]
            .to_string()
            .contains("cache_control"),
        "fixture must exercise the legacy string-to-array system marker case"
    );
    assert!(
        !matches!(
            prepared.cache_control(),
            haider_protocol::provider::CacheControlObservationV1::Emitted { .. }
        ),
        "legacy structural comparison did not observe a key through the string-to-array system shape change"
    );
    assert!(
        prepared
            .provider_view()
            .expect("provider view")
            .ledger()
            .boundaries
            .is_empty(),
        "system-only API-key observation historically recorded no ledger breakpoint"
    );
}

/// CM2c — the longer TTL requires both a gap beyond five minutes and at
/// least two later reads; every uncertain/short/single-read case stays 5m.
///
/// MUTATION CHECK (executed): change `>` to `>=` or remove the read-count
/// conjunct; the boundary and one-read assertions fail.
#[test]
fn cm2c_anthropic_cache_ttl_requires_long_gap_and_two_reads() {
    assert_eq!(
        select_anthropic_cache_ttl(Some(300_001), 2),
        AnthropicCacheTtl::OneHour
    );
    for (gap, reads) in [
        (None, 2),
        (Some(300_000), 2),
        (Some(300_001), 1),
        (Some(10_000), 99),
    ] {
        assert_eq!(
            select_anthropic_cache_ttl(gap, reads),
            AnthropicCacheTtl::FiveMinutes
        );
    }
}

/// CM2f — consumer OAuth is deliberately unverified in production. Merely
/// attaching provider-neutral metadata must keep its CM1 full-history bytes.
#[test]
fn cm2f_unverified_anthropic_oauth_is_byte_exact_full_history() {
    let request = cache_control_request();
    let mut baseline = request.clone();
    baseline.cache_metadata = None;
    let provider = payload_provider(true);
    assert_eq!(
        provider
            .request_payload(&request)
            .expect("fallback payload"),
        provider.request_payload(&baseline).expect("CM1 payload")
    );

    let verified = payload_provider(true).with_prompt_caching_verified(true);
    let mut mismatched = request.clone();
    mismatched
        .cache_metadata
        .as_mut()
        .expect("metadata")
        .provider = "anthropic".into();
    assert_eq!(
        verified
            .request_payload(&mismatched)
            .expect("mismatch fallback"),
        verified.request_payload(&baseline).expect("CM1 payload")
    );

    let mut malformed = request;
    let metadata = malformed.cache_metadata.as_mut().expect("metadata");
    metadata.stable_history_end = 3;
    metadata.current_user_start = 2;
    assert_eq!(
        verified
            .request_payload(&malformed)
            .expect("boundary fallback"),
        verified.request_payload(&baseline).expect("CM1 payload")
    );
}

/// CM2g — cache metadata changes annotations only. After deleting those
/// ephemeral keys, the exact system, tools, message roles, text, and ordering
/// are identical to the unannotated request.
///
/// MUTATION CHECK (executed): truncate/reorder the stable messages while
/// annotating; stripping cache keys no longer recovers the baseline.
#[test]
fn cm2g_anthropic_annotations_do_not_change_model_visible_content() {
    let request = cache_control_request();
    let mut annotated = payload_provider(true)
        .with_prompt_caching_verified(true)
        .request_payload(&request)
        .expect("annotated payload");
    strip_cache_control(&mut annotated);
    let baseline = payload_provider(true)
        .request_payload(&request)
        .expect("unannotated payload");
    assert_eq!(annotated, baseline);
}

#[test]
fn cm2g_anthropic_api_key_system_text_and_signed_opaque_are_unchanged() {
    let provider = payload_provider(false);
    let mut request = cache_control_request();
    request.cache_metadata.as_mut().expect("metadata").provider = "anthropic".into();
    let signed = serde_json::json!({
        "type": "thinking",
        "thinking": "provider reasoning",
        "signature": "signed-provider-bytes"
    });
    request.messages[2].blocks.push(Block::ProviderOpaque {
        provider: "anthropic".into(),
        data: signed.clone().into(),
    });
    let annotated = provider
        .request_payload(&request)
        .expect("annotated API-key payload");
    let mut baseline_request = request;
    baseline_request.cache_metadata = None;
    let baseline = provider
        .request_payload(&baseline_request)
        .expect("baseline API-key payload");

    assert_eq!(annotated["system"][0]["text"], baseline["system"]);
    let mut stripped = annotated.clone();
    strip_cache_control(&mut stripped);
    assert_eq!(stripped["messages"], baseline["messages"]);
    assert_eq!(annotated["messages"][2]["content"][1], signed);
    assert!(
        annotated["messages"][2]["content"][1]
            .get("cache_control")
            .is_none(),
        "signed terminal blocks are never decorated"
    );
}

/// CM2a (final wire) — append-only history does not perturb Haider-owned
/// system/tool bytes or their canonical digests. A real owned-input mutation
/// changes the corresponding digest.
#[test]
fn cm2a_anthropic_final_wire_system_and_tool_digests_are_stable() {
    let provider = payload_provider(true).with_prompt_caching_verified(true);
    let first = cache_control_request();
    let first_digests = provider
        .rendered_cache_prefix_digests(&first)
        .expect("first rendered digests");
    let mut second = first.clone();
    second.messages.push(Message::assistant(vec![Block::Text {
        text: "current answer".into(),
    }]));
    second.messages.push(Message::user_text("next question"));
    let metadata = second.cache_metadata.as_mut().expect("cache metadata");
    metadata.stable_history_end = 5;
    metadata.current_user_start = 5;
    metadata.previous_stable_history_end = Some(3);
    let second_digests = provider
        .rendered_cache_prefix_digests(&second)
        .expect("second rendered digests");
    assert_eq!(first_digests.system, second_digests.system);
    assert_eq!(first_digests.tools, second_digests.tools);
    let second_prepared = provider
        .prepare_turn(&second)
        .expect("second prepared turn");
    assert_eq!(
        second_prepared.previous_immutable_history_digest(),
        Some(first_digests.immutable_history.as_str()),
        "the grown wire retains the old rendered history prefix"
    );
    assert_ne!(
        first_digests.immutable_history, second_digests.immutable_history,
        "the current moving history breakpoint advances"
    );

    let mut mutated = second;
    mutated.tools[0].description.push_str(" mutated");
    mutated.system_prompt = Some("mutated system".into());
    let mutated_digests = provider
        .rendered_cache_prefix_digests(&mutated)
        .expect("mutated rendered digests");
    assert_ne!(first_digests.system, mutated_digests.system);
    assert_ne!(first_digests.tools, mutated_digests.tools);
}

#[test]
fn cache_diagnostic_provider_prepare_added_cpu_cost_is_measured() {
    let provider = payload_provider(true).with_prompt_caching_verified(true);
    let request = cache_control_request();
    let samples = 5_000_usize;
    let mut baseline = Vec::with_capacity(samples);
    let mut prepared = Vec::with_capacity(samples);
    for sample in 0..samples {
        if sample % 2 == 0 {
            let started = std::time::Instant::now();
            std::hint::black_box(
                provider
                    .request_payload(std::hint::black_box(&request))
                    .expect("baseline renders"),
            );
            baseline.push(started.elapsed());
            let started = std::time::Instant::now();
            std::hint::black_box(
                provider
                    .prepare_turn(std::hint::black_box(&request))
                    .expect("diagnostic prepares"),
            );
            prepared.push(started.elapsed());
        } else {
            let started = std::time::Instant::now();
            std::hint::black_box(
                provider
                    .prepare_turn(std::hint::black_box(&request))
                    .expect("diagnostic prepares"),
            );
            prepared.push(started.elapsed());
            let started = std::time::Instant::now();
            std::hint::black_box(
                provider
                    .request_payload(std::hint::black_box(&request))
                    .expect("baseline renders"),
            );
            baseline.push(started.elapsed());
        }
    }
    baseline.sort_unstable();
    prepared.sort_unstable();
    let mean = |values: &[std::time::Duration]| {
        values
            .iter()
            .map(std::time::Duration::as_nanos)
            .sum::<u128>()
            / values.len() as u128
    };
    let baseline_mean = mean(&baseline);
    let prepared_mean = mean(&prepared);
    let baseline_p95 = baseline[baseline.len() * 95 / 100].as_nanos();
    let prepared_p95 = prepared[prepared.len() * 95 / 100].as_nanos();
    eprintln!(
        "cache diagnostic provider prepare: added_mean_ns={} added_p95_ns={} baseline_mean_ns={baseline_mean} prepared_mean_ns={prepared_mean} samples={samples}",
        prepared_mean.saturating_sub(baseline_mean),
        prepared_p95.saturating_sub(baseline_p95),
    );
}

/// v0.0.962 M5 allocation proof: the thread-local counting allocator samples
/// every live allocation made during preparation, including transient DOMs
/// and serializer buffers. The 1 MiB text makes fixed overhead negligible;
/// arena-backed CAS segments leave at most one transient rendered copy.
#[test]
fn single_render_prepare_peaks_at_one_transient_prompt_view() {
    let provider = payload_provider(true).with_prompt_caching_verified(true);
    let mut request = cache_control_request();
    request.messages[0] = Message::user_text("x".repeat(1024 * 1024));
    let (prepared, peak_bytes) = crate::measure_peak_test_allocation(|| {
        provider.prepare_turn(&request).expect("prepared turn")
    });
    assert!(
        prepared
            .wire
            .as_ref()
            .expect("prepared wire")
            .reply_bindings
            .iter()
            .any(|binding| binding.text.len() == 1024 * 1024),
        "the one-megabyte prompt must remain arena-backed"
    );
    assert!(
        prepared
            .provider_view_storage_blobs
            .iter()
            .any(haider_protocol::cache::ProviderViewBlobV1::is_segmented)
    );
    let allowed = 1024 * 1024 + 256 * 1024;
    assert!(
        peak_bytes <= allowed,
        "prepare peak {peak_bytes} exceeded one prompt view plus fixed overhead {allowed}"
    );
}

#[test]
fn single_render_cas_digests_match_legacy_whole_fragment_serialization() {
    let provider = payload_provider(true).with_prompt_caching_verified(true);
    let request = cache_control_request();
    let prepared = provider.prepare_turn(&request).expect("prepared turn");
    let mut neutral_request = request;
    neutral_request.cache_metadata = None;
    let neutral = provider
        .request_payload(&neutral_request)
        .expect("neutral payload");
    let legacy_digest = |value: Option<&serde_json::Value>| {
        blake3::hash(&serde_json::to_vec(&value).expect("legacy fragment serialization"))
            .to_hex()
            .to_string()
    };
    let messages = neutral["messages"].as_array().expect("messages array");
    assert_eq!(
        prepared.prefix_digests().system,
        legacy_digest(neutral.get("system"))
    );
    assert_eq!(
        prepared.prefix_digests().tools,
        legacy_digest(neutral.get("tools"))
    );
    assert_eq!(
        prepared.prefix_digests().immutable_history,
        blake3::hash(
            &serde_json::to_vec(&Some(&messages[..3])).expect("legacy history serialization")
        )
        .to_hex()
        .to_string()
    );
}

/// MUTATION CHECK: change or drop the identity text, merge it into the turn's
/// own system block, reorder the blocks, omit `system` on a promptless OAuth
/// turn, or leak the identity into api-key mode. Each mutation fails a named
/// assertion below. Live law: Anthropic rejects OAuth-subscription bodies
/// whose `system` does not open with the exact Claude Code identity block
/// (captured 2026-08-05: schema-valid identity-free OAuth turns were refused
/// with generic-"Error" responses on every attempt).
#[test]
fn oauth_payload_opens_system_with_claude_code_identity_block() {
    let oauth = payload_provider(true);
    let request = payload_request(Some("haider-system-v2\nYou are Haider Code."));
    let payload = oauth.request_payload(&request).expect("OAuth payload");

    let system = payload["system"]
        .as_array()
        .expect("OAuth system is an array of blocks");
    assert_eq!(system.len(), 2, "identity block plus the turn's own prompt");
    assert_eq!(
        system[0],
        serde_json::json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
        }),
        "first system block is exactly the Claude Code identity line"
    );
    assert_eq!(
        system[1],
        serde_json::json!({
            "type": "text",
            "text": "haider-system-v2\nYou are Haider Code.",
        }),
        "the turn's real system prompt rides as its own second block"
    );

    // A promptless OAuth turn still carries the identity block: omitting
    // `system` entirely is rejected by the same server-side validation.
    let bare = oauth
        .request_payload(&payload_request(None))
        .expect("promptless OAuth payload");
    assert_eq!(
        bare["system"],
        serde_json::json!([{
            "type": "text",
            "text": ANTHROPIC_OAUTH_SYSTEM_IDENTITY,
        }]),
        "promptless OAuth turns send exactly the identity block"
    );
}

/// MUTATION CHECK: prepend the identity to api-key bodies, turn the api-key
/// system into an array, or let the two modes drift anywhere outside
/// `system`. Each mutation fails a named assertion below.
#[test]
fn api_key_payload_keeps_plain_system_and_matches_oauth_outside_system() {
    let api_key = payload_provider(false);
    let oauth = payload_provider(true);
    let request = payload_request(Some("haider-system-v2\nYou are Haider Code."));

    let key_payload = api_key.request_payload(&request).expect("api-key payload");
    assert_eq!(
        key_payload["system"],
        serde_json::Value::String("haider-system-v2\nYou are Haider Code.".into()),
        "api-key mode sends the turn's system prompt as a plain string"
    );
    assert!(
        !key_payload.to_string().contains("Claude Code"),
        "api-key mode never carries the OAuth identity line"
    );

    let bare = api_key
        .request_payload(&payload_request(None))
        .expect("promptless api-key payload");
    assert!(
        bare.get("system").is_none(),
        "promptless api-key turns omit `system` entirely"
    );

    // Golden cross-mode law: the two bodies differ in `system` and nowhere else.
    let mut key_rest = key_payload;
    let mut oauth_rest = oauth.request_payload(&request).expect("OAuth payload");
    key_rest.as_object_mut().expect("object").remove("system");
    oauth_rest.as_object_mut().expect("object").remove("system");
    assert_eq!(
        key_rest, oauth_rest,
        "auth modes agree on every field except `system`"
    );
}

/// MUTATION CHECK: keep any of `oneOf`/`allOf`/`anyOf` at the top level of a
/// tool schema, or strip the nested `anyOf` too, and a named assertion below
/// fails. Live law: Anthropic's Messages API rejects top-level combinators in
/// custom tool schemas for both auth modes (captured 2026-08-05: HTTP 400
/// "tools.3.custom.input_schema: input_schema does not support oneOf, allOf,
/// or anyOf at the top level").
#[test]
fn tool_schemas_drop_top_level_combinators_for_both_auth_modes() {
    for oauth in [false, true] {
        let provider = payload_provider(oauth);
        let payload = provider
            .request_payload(&payload_request(Some("prompt")))
            .expect("payload");
        let tools = payload["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2, "both tools survive schema shaping");
        assert_eq!(
            tools[0]["input_schema"],
            payload_request(None).tools[0].input_schema,
            "combinator-free schemas pass through byte-identical (oauth={oauth})"
        );
        let search_schema = &tools[1]["input_schema"];
        for banned in ["oneOf", "allOf", "anyOf"] {
            assert!(
                search_schema.get(banned).is_none(),
                "top-level `{banned}` is dropped from the Anthropic body (oauth={oauth})"
            );
        }
        assert_eq!(
            search_schema["properties"]["mode"],
            serde_json::json!({"anyOf": [{"const": "literal"}, {"const": "simple"}]}),
            "nested combinators are preserved (oauth={oauth})"
        );
        assert_eq!(
            search_schema["additionalProperties"],
            serde_json::json!(false),
            "unrelated schema keys are untouched (oauth={oauth})"
        );
    }
}

fn computer_tool() -> ToolDefinition {
    ToolDefinition {
        name: "computer".into(),
        description: "Use the desktop".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"action": {"type": "string"}},
            "required": ["action"],
        }),
    }
}

#[test]
fn anthropic_native_computer_model_table_is_documented_and_fail_closed() {
    for model in [
        "claude-opus-5",
        "claude-sonnet-5-20260801",
        "anthropic.claude-opus-4-8",
        "claude-opus-4-7@20260801",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-opus-4-5",
    ] {
        assert_eq!(
            anthropic_computer_tool_version(model),
            Some(AnthropicComputerToolVersion::V20251124),
            "{model} uses the latest native dialect"
        );
    }
    for model in [
        "claude-sonnet-4-5",
        "claude-haiku-4-5@20251001",
        "anthropic.claude-opus-4-1",
        "claude-sonnet-4",
        "claude-opus-4-20250514",
    ] {
        assert_eq!(
            anthropic_computer_tool_version(model),
            Some(AnthropicComputerToolVersion::V20250124),
            "{model} uses the earlier native dialect"
        );
    }
    for model in [
        "claude-audit",
        "claude-fable-5",
        "claude-haiku-5",
        "claude-opus-5-1",
        "claude-sonnet-3-7",
    ] {
        assert_eq!(
            anthropic_computer_tool_version(model),
            None,
            "unknown/unsupported `{model}` stays generic"
        );
    }
}

#[test]
fn native_computer_advertisement_uses_latest_admitted_screenshot_and_replays_actions() {
    let screenshot = ImageBlockRef {
        artifact: ArtifactRef::new("blake3:anthropic-native-screen"),
        media_type: "image/png".into(),
        width: 1_600,
        height: 900,
        byte_len: 12,
    };
    let request = TurnRequest {
        messages: vec![
            Message::assistant(vec![Block::ToolCall {
                call_id: "toolu_screen".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "screenshot"}),
            }]),
            Message::tool_result_with_images(
                "toolu_screen",
                "screenshot captured (1600x900)",
                false,
                vec![screenshot.clone()],
            ),
            Message::assistant(vec![Block::ToolCall {
                call_id: "toolu_click".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "left_click", "x": 321, "y": 654}),
            }]),
        ],
        model: "claude-opus-5".into(),
        max_tokens: 128,
        system_prompt: None,
        tools: vec![computer_tool()],
        attachments: vec![ResolvedAttachment {
            artifact: screenshot.artifact,
            data_base64: "iVBORw0KGgo=".into(),
        }],
        cache_metadata: None,
    };
    let payload = model_payload_provider(false, "claude-opus-5")
        .request_payload(&request)
        .expect("native computer payload");

    assert_eq!(
        payload["tools"][0],
        serde_json::json!({
            "type": "computer_20251124",
            "name": "computer",
            "display_width_px": 1600,
            "display_height_px": 900,
            "display_number": 1,
        })
    );
    assert!(payload["tools"][0].get("description").is_none());
    assert!(payload["tools"][0].get("input_schema").is_none());
    assert_eq!(
        payload["messages"][2]["content"][0]["input"],
        serde_json::json!({"action": "left_click", "coordinate": [321, 654]}),
        "normalized assistant history replays in Anthropic's coordinate-array shape"
    );
    assert_eq!(
        payload["messages"][1]["content"][0]["content"][1]["type"], "image",
        "CU-1 screenshots remain native Anthropic tool_result image blocks"
    );
}

#[test]
fn native_computer_initial_size_is_xga_and_generic_fallback_is_unchanged() {
    let mut native = payload_request(None);
    native.model = "claude-sonnet-4-5".into();
    native.tools = vec![computer_tool()];
    let payload = model_payload_provider(false, "claude-sonnet-4-5")
        .request_payload(&native)
        .expect("earlier native payload");
    assert_eq!(
        payload["tools"][0],
        serde_json::json!({
            "type": "computer_20250124",
            "name": "computer",
            "display_width_px": 1024,
            "display_height_px": 768,
            "display_number": 1,
        })
    );

    let mut generic = payload_request(None);
    generic.tools = vec![computer_tool()];
    let payload = payload_provider(false)
        .request_payload(&generic)
        .expect("generic computer payload");
    assert_eq!(
        payload["tools"][0],
        serde_json::json!({
            "name": "computer",
            "description": "Use the desktop",
            "input_schema": {
                "type": "object",
                "properties": {"action": {"type": "string"}},
                "required": ["action"],
            },
        }),
        "a non-native model retains the pre-CU-5 custom-function bytes"
    );
}

#[test]
fn native_computer_sse_translates_action_envelope_before_dispatch() {
    let stream = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_scroll\",\"name\":\"computer\",\"input\":{}}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"action\\\":\\\"scroll\\\",\\\"coordinate\\\":[120,240],\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"scroll_direction\\\":\\\"down\\\",\\\"scroll_amount\\\":7}\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    let items = replay_anthropic_native_computer_sse(stream.as_bytes());
    assert_eq!(
        items.first(),
        Some(&Ok(StreamEvent::ToolCallStart {
            call_id: "toolu_scroll".into(),
            name: "computer".into(),
        }))
    );
    let args = items
        .iter()
        .find_map(|item| match item {
            Ok(StreamEvent::ToolCallArgsDelta { args_fragment, .. }) => {
                Some(serde_json::from_str::<serde_json::Value>(args_fragment).expect("args JSON"))
            }
            _ => None,
        })
        .expect("translated args delta");
    assert_eq!(
        args,
        serde_json::json!({
            "action": "scroll",
            "x": 120,
            "y": 240,
            "direction": "down",
            "amount": 7,
        })
    );
    assert!(items.iter().any(|item| matches!(
        item,
        Ok(StreamEvent::ToolCallEnd { call_id }) if call_id == "toolu_scroll"
    )));
}

#[test]
fn native_computer_action_translation_covers_supported_anthropic_vocabulary() {
    let cases = [
        (
            serde_json::json!({"action": "screenshot"}),
            serde_json::json!({"action": "screenshot"}),
        ),
        (
            serde_json::json!({"action": "cursor_position"}),
            serde_json::json!({"action": "cursor_position"}),
        ),
        (
            serde_json::json!({"action": "left_click", "coordinate": [12, 34]}),
            serde_json::json!({"action": "left_click", "x": 12, "y": 34}),
        ),
        (
            serde_json::json!({"action": "right_click"}),
            serde_json::json!({"action": "right_click"}),
        ),
        (
            serde_json::json!({"action": "middle_click"}),
            serde_json::json!({"action": "middle_click"}),
        ),
        (
            serde_json::json!({"action": "double_click"}),
            serde_json::json!({"action": "double_click"}),
        ),
        (
            serde_json::json!({"action": "left_mouse_down"}),
            serde_json::json!({"action": "left_mouse_down"}),
        ),
        (
            serde_json::json!({"action": "left_mouse_up"}),
            serde_json::json!({"action": "left_mouse_up"}),
        ),
        (
            serde_json::json!({"action": "mouse_move", "coordinate": [55, 89]}),
            serde_json::json!({"action": "mouse_move", "x": 55, "y": 89}),
        ),
        (
            serde_json::json!({
                "action": "left_click_drag",
                "start_coordinate": [1, 2],
                "coordinate": [300, 400],
            }),
            serde_json::json!({
                "action": "left_click_drag",
                "from": {"x": 1, "y": 2},
                "to": {"x": 300, "y": 400},
            }),
        ),
        (
            serde_json::json!({"action": "type", "text": "hello"}),
            serde_json::json!({"action": "type", "text": "hello"}),
        ),
        (
            serde_json::json!({"action": "key", "text": "CMD+SHIFT+P"}),
            serde_json::json!({"action": "key", "keys": "CMD+SHIFT+P"}),
        ),
        (
            serde_json::json!({
                "action": "scroll",
                "coordinate": [101, 202],
                "scroll_direction": "up",
                "scroll_amount": 3,
            }),
            serde_json::json!({
                "action": "scroll",
                "x": 101,
                "y": 202,
                "direction": "up",
                "amount": 3,
            }),
        ),
        (
            serde_json::json!({"action": "wait", "duration": 1.25}),
            serde_json::json!({"action": "wait", "ms": 1250}),
        ),
    ];

    for (native, neutral) in cases {
        assert_eq!(
            crate::wire::anthropic_computer_input_to_neutral(&native)
                .expect("native action translates"),
            neutral
        );
        assert_eq!(
            crate::wire::anthropic_computer_input_from_neutral(&neutral)
                .expect("neutral action replays"),
            native
        );
    }
    for unsupported in [
        serde_json::json!({"action": "triple_click", "coordinate": [3, 4]}),
        serde_json::json!({"action": "hold_key", "text": "shift", "duration": 1}),
        serde_json::json!({"action": "zoom", "region": [0, 0, 10, 10]}),
        serde_json::json!({"action": "left_click", "coordinate": [3, 4], "key": "shift"}),
        serde_json::json!({"action": "right_click", "coordinate": [3, 4]}),
        serde_json::json!({
            "action": "scroll",
            "coordinate": [3, 4],
            "scroll_direction": "down",
            "scroll_amount": 2,
            "key": "shift",
        }),
        serde_json::json!({"action": "screenshot", "future_field": true}),
    ] {
        assert!(
            crate::wire::anthropic_computer_input_to_neutral(&unsupported).is_err(),
            "native-only or lossy action input must fail honestly: {unsupported}"
        );
    }
}

#[tokio::test]
async fn native_computer_beta_is_sent_on_bedrock_and_vertex_only_when_advertised() {
    let mut bedrock_turn = payload_request(None);
    bedrock_turn.model = "anthropic.claude-opus-5".into();
    bedrock_turn.tools = vec![computer_tool()];
    let bedrock = AnthropicProvider::new_endpoint(
        secret_credential("bedrock-native-computer", b"BEDROCK_NATIVE_COMPUTER"),
        "anthropic.claude-opus-5",
        "https://bedrock-mantle.us-east-1.api.aws/anthropic",
    )
    .expect("bedrock provider");
    let payload = bedrock
        .request_payload(&bedrock_turn)
        .expect("bedrock native payload");
    let request = bedrock
        .request_body(payload)
        .await
        .expect("bedrock request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ANTHROPIC_COMPUTER_BETA_20251124)
    );

    let mut vertex_turn = payload_request(None);
    vertex_turn.model = "claude-sonnet-4-5@20250929".into();
    vertex_turn.tools = vec![computer_tool()];
    let vertex = AnthropicProvider::new_vertex(
        secret_credential("vertex-native-computer", b"VERTEX_NATIVE_COMPUTER"),
        "claude-sonnet-4-5@20250929",
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
    )
    .expect("vertex provider");
    let payload = vertex
        .request_payload(&vertex_turn)
        .expect("vertex native payload");
    let request = vertex.request_body(payload).await.expect("vertex request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ANTHROPIC_COMPUTER_BETA_20250124)
    );

    let generic_payload = vertex
        .request_payload(&one_line_turn("claude-sonnet-4-5@20250929"))
        .expect("vertex generic payload");
    let generic_request = vertex
        .request_body(generic_payload)
        .await
        .expect("vertex generic request");
    assert!(
        !generic_request
            .headers()
            .contains_key(ANTHROPIC_OAUTH_BETA_HEADER),
        "generic Vertex request remains byte-identical"
    );
}

#[tokio::test]
async fn native_computer_beta_composes_with_fast_and_oauth_headers() {
    let mut turn = payload_request(None);
    turn.model = "claude-opus-5".into();
    turn.tools = vec![computer_tool()];

    let api_key = model_payload_provider(false, "claude-opus-5").with_fast(true);
    let payload = api_key.request_payload(&turn).expect("native fast payload");
    let request = api_key
        .request_body(payload)
        .await
        .expect("native fast request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("computer beta header")
            .to_str()
            .expect("ASCII beta header"),
        format!("{ANTHROPIC_FAST_BETA_VALUE},{ANTHROPIC_COMPUTER_BETA_20251124}")
    );

    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-native-computer-header-audit");
    vault
        .put(&alias, b"ANTHROPIC_NATIVE_COMPUTER_SENTINEL")
        .expect("store OAuth access");
    let oauth = AnthropicProvider::new_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve OAuth access"),
        "claude-opus-5",
        ANTHROPIC_OAUTH_BASE_URL,
        Arc::new(StubFixedResolver {
            address: SocketAddr::from(([93, 184, 216, 34], 443)),
        }),
    )
    .expect("OAuth native provider")
    .with_fast(true);
    let payload = oauth.request_payload(&turn).expect("OAuth native payload");
    let request = oauth
        .request_body(payload)
        .await
        .expect("OAuth native request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("composed OAuth computer beta header")
            .to_str()
            .expect("ASCII beta header"),
        format!(
            "{ANTHROPIC_OAUTH_BETA_VALUE},{ANTHROPIC_FAST_BETA_VALUE},{ANTHROPIC_COMPUTER_BETA_20251124}"
        )
    );
}

impl HangingFixture {
    fn new() -> Self {
        let fixture = include_bytes!("../tests/fixtures/anthropic/hanging_mid_turn.sse").as_slice();
        assert!(fixture.ends_with(b"\n"));
        assert!(!fixture.ends_with(b"\n\n"));
        let mut first_chunk = fixture.to_vec();
        first_chunk.push(b'\n');
        Self {
            first_chunk: Some(first_chunk),
        }
    }
}

impl SseChunkSource for HangingFixture {
    async fn next_chunk(
        &mut self,
        _route_gating: crate::RouteGating,
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
        HangingFixture::new(),
        None,
        sender,
        Duration::from_secs(90),
        Duration::from_secs(5 * 60),
        crate::RouteGating::Enabled,
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
        .expect("idle deadline emits one item")
        .expect_err("idle deadline is a typed error");
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(error.retryable);
    assert!(error.message.contains("90 seconds"));
    assert!(receiver.recv().await.is_none(), "failure is surfaced once");
    stream_task.await.expect("stream task exits after timeout");
}

/// LAW (LE3, anthropic half): the session effort rides `output_config.effort`
/// VERBATIM on BOTH auth modes; the body NEVER carries a `thinking` field
/// (`thinking.budget_tokens` 400s on 4.7+ and every 5-family model), and —
/// pinning brief decision 10 — never `temperature`/`top_p`/`top_k`. With no
/// effort the payload keeps its exact pre-G3 top-level key set.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): route the effort
/// through `thinking: {"budget_tokens": ...}` instead of `output_config`.
/// Expected runtime failure: the no-thinking-field and output_config
/// assertions below.
#[test]
fn effort_rides_output_config_and_never_thinking_or_sampling_params() {
    for oauth in [false, true] {
        let provider = payload_provider(oauth).with_effort(Some("xhigh".into()));
        let payload = provider
            .request_payload(&payload_request(Some("system prompt")))
            .expect("payload with effort");
        assert_eq!(
            payload["output_config"],
            serde_json::json!({"effort": "xhigh"}),
            "effort rides output_config (oauth={oauth}): {payload}"
        );
        let object = payload.as_object().expect("payload object");
        for forbidden in ["thinking", "temperature", "top_p", "top_k", "speed"] {
            assert!(
                !object.contains_key(forbidden),
                "`{forbidden}` must not ride an effort-only payload (oauth={oauth}): {payload}"
            );
        }

        // Without an effort the pre-G3 body shape is byte-stable: the exact
        // top-level key set, no output_config.
        let plain = payload_provider(oauth)
            .request_payload(&payload_request(Some("system prompt")))
            .expect("payload without effort");
        let mut keys: Vec<&str> = plain
            .as_object()
            .expect("plain object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "max_tokens",
                "messages",
                "model",
                "stream",
                "system",
                "tools"
            ],
            "the effortless payload keeps the pre-G3 key set (oauth={oauth})"
        );
    }
}

/// LAW (LE4, wire half): fast mode is `speed: "fast"` in the body PLUS the
/// `fast-mode-2026-02-01` beta header — comma-joined AFTER the OAuth beta on
/// subscription requests, alone on api-key requests — and fast OFF keeps the
/// exact pre-G3 header value.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): replace the OAuth
/// comma-join with the fast beta ALONE. Expected runtime failure: the OAuth
/// header assertion below (and live, the subscription identity check 400s).
#[tokio::test]
async fn fast_mode_sets_speed_body_and_comma_joined_beta_header() {
    // Body: both auth modes carry the top-level speed field.
    for oauth in [false, true] {
        let payload = payload_provider(oauth)
            .with_fast(true)
            .request_payload(&payload_request(Some("system prompt")))
            .expect("fast payload");
        assert_eq!(
            payload["speed"], "fast",
            "fast rides the body (oauth={oauth}): {payload}"
        );
    }

    // Header, api-key mode: the fast beta alone.
    let api_key = payload_provider(false).with_fast(true);
    let request = api_key
        .request_body(serde_json::json!({"model": "claude-audit"}))
        .await
        .expect("api-key fast request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("fast beta header"),
        ANTHROPIC_FAST_BETA_VALUE
    );

    // Header, api-key mode, fast OFF: no beta header at all (pre-G3 shape).
    let request = payload_provider(false)
        .request_body(serde_json::json!({"model": "claude-audit"}))
        .await
        .expect("api-key standard request");
    assert!(request.headers().get(ANTHROPIC_OAUTH_BETA_HEADER).is_none());

    // Header, OAuth mode: ONE comma-joined value, subscription beta FIRST.
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-fast-header-audit");
    vault
        .put(&alias, b"ANTHROPIC_FAST_SENTINEL_77aa")
        .expect("store OAuth access");
    let oauth = AnthropicProvider::new_subscription_with_dns_resolver(
        vault.resolve(&alias).expect("resolve OAuth access"),
        "claude-audit",
        ANTHROPIC_OAUTH_BASE_URL,
        Arc::new(StubFixedResolver {
            address: SocketAddr::from(([93, 184, 216, 34], 443)),
        }),
    )
    .expect("Anthropic subscription provider")
    .with_fast(true);
    let request = oauth
        .request_body(serde_json::json!({"model": "claude-audit"}))
        .await
        .expect("oauth fast request");
    assert_eq!(
        request
            .headers()
            .get(ANTHROPIC_OAUTH_BETA_HEADER)
            .expect("oauth+fast beta header"),
        "oauth-2025-04-20,fast-mode-2026-02-01"
    );
    assert_eq!(
        request
            .headers()
            .get_all(ANTHROPIC_OAUTH_BETA_HEADER)
            .iter()
            .count(),
        1,
        "the betas comma-join into ONE header value"
    );
}

// ───────────────────────── G4b enterprise endpoints ─────────────────────────

fn secret_credential(alias: &str, secret: &[u8]) -> haider_accounts::SecretHandle {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new(alias);
    vault.put(&alias, secret).expect("store test secret");
    vault.resolve(&alias).expect("resolve test secret")
}

fn one_line_turn(model: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text("ping")],
        model: model.into(),
        max_tokens: 16,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    }
}

/// LAW (Q custom auth): a keyed custom Messages server receives exactly
/// `x-api-key`, while a no-auth server receives neither credential header.
/// Both retain the standard Anthropic version header and `/v1/messages` URL.
#[tokio::test]
async fn custom_anthropic_keyed_and_no_auth_headers_are_exact() {
    let keyed = AnthropicProvider::new_custom(
        secret_credential("custom-anthropic-keyed", b"custom-anthropic-secret"),
        "claude-local",
        "http://127.0.0.1:18181",
    )
    .expect("keyed custom Anthropic provider");
    let keyed_request = keyed
        .request_body(serde_json::json!({"model":"claude-local"}))
        .await
        .expect("keyed request");
    assert_eq!(keyed_request.url().path(), "/v1/messages");
    assert_eq!(
        keyed_request
            .headers()
            .get("x-api-key")
            .expect("custom key header"),
        "custom-anthropic-secret"
    );
    assert!(!keyed_request.headers().contains_key(AUTHORIZATION));
    assert_eq!(
        keyed_request
            .headers()
            .get("anthropic-version")
            .expect("standard version header"),
        "2023-06-01"
    );
    let escaped = keyed.with_api_url("http://169.254.169.254/v1/messages");
    let error = escaped
        .request_body(serde_json::json!({"model":"claude-local"}))
        .await
        .expect_err("custom key must not leave its pinned origin");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("left its pinned origin"));
    assert!(!error.message.contains("custom-anthropic-secret"));

    let no_auth = AnthropicProvider::new_custom_no_auth(
        secret_credential("custom-anthropic-no-auth", b"must-not-leak"),
        "claude-local",
        "http://127.0.0.1:18181/v1",
    )
    .expect("no-auth custom Anthropic provider");
    let no_auth_request = no_auth
        .request_body(serde_json::json!({"model":"claude-local"}))
        .await
        .expect("no-auth request");
    assert_eq!(no_auth_request.url().path(), "/v1/messages");
    assert!(!no_auth_request.headers().contains_key("x-api-key"));
    assert!(!no_auth_request.headers().contains_key(AUTHORIZATION));
    assert_eq!(
        no_auth_request
            .headers()
            .get("anthropic-version")
            .expect("standard version header"),
        "2023-06-01"
    );
}

/// CM1a — captured Anthropic usage keeps its separate read/write semantics,
/// including the provider's 5m/1h creation detail.
///
/// MUTATION CHECK (executed): map cache creation into reads, or omit the 1h
/// split; the unequal 100/70/30/10/20 assertions fail.
#[test]
fn cm1a_anthropic_separate_read_write_decode() {
    use haider_protocol::provider::{CacheStatAvailability, StreamEvent};

    let events = replay_anthropic_sse(include_bytes!(
        "../tests/fixtures/anthropic/cache_usage_split.sse"
    ));
    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(StreamEvent::UsageUpdate(usage)) => Some(usage),
            _ => None,
        })
        .expect("captured usage update");
    let normalized = usage.normalized.as_ref().expect("normalized usage");
    assert_eq!(normalized.logical_input, 200);
    assert_eq!(normalized.uncached_input, 130);
    assert_eq!(normalized.cache_read_input, 70);
    assert_eq!(normalized.cache_write_input, 30);
    assert_eq!(normalized.cache_write_5m_input, 10);
    assert_eq!(normalized.cache_write_1h_input, 20);
    assert_eq!(normalized.cache_status, CacheStatAvailability::Present);
    assert_eq!(
        normalized.cache_write_ttl_status,
        CacheStatAvailability::Present
    );
}

/// LAW (LB1 — the mantle golden): the Bedrock adapter POSTs
/// `{base}/v1/messages` with the bearer riding `x-api-key`, the standard
/// `anthropic-version: 2023-06-01` header, NO Authorization header, the
/// `anthropic.`-prefixed model IN THE BODY, and decodes a scripted standard
/// SSE stream — the mantle wire is the first-party Messages wire verbatim.
///
/// MUTATION CHECK: swap the mantle URL template, move the credential to
/// Authorization, drop the version header, or drop body.model. Expected
/// RUNTIME failure: the named equalities below.
#[tokio::test]
async fn lb1_bedrock_mantle_golden_url_headers_body_and_sse() {
    let provider = AnthropicProvider::new_endpoint(
        secret_credential("bedrock-mantle-audit", b"BEDROCK_BEARER_SENTINEL_44aa"),
        "anthropic.claude-opus-5",
        "https://bedrock-mantle.us-east-1.api.aws/anthropic",
    )
    .expect("bedrock mantle adapter");
    let payload = provider
        .request_payload(&one_line_turn("anthropic.claude-opus-5"))
        .expect("mantle payload");
    assert_eq!(
        payload.get("model").and_then(serde_json::Value::as_str),
        Some("anthropic.claude-opus-5"),
        "the mantle model rides IN THE BODY"
    );
    let request = provider
        .request_body(payload)
        .await
        .expect("mantle request");
    assert_eq!(
        request.url().as_str(),
        "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages"
    );
    assert_eq!(
        request
            .headers()
            .get("x-api-key")
            .expect("x-api-key bearer"),
        "BEDROCK_BEARER_SENTINEL_44aa"
    );
    assert_eq!(
        request
            .headers()
            .get("anthropic-version")
            .expect("standard version header"),
        "2023-06-01"
    );
    assert!(
        !request.headers().contains_key(AUTHORIZATION),
        "mantle bearer must never ride Authorization"
    );
    assert_eq!(
        provider.credential_surface(),
        crate::ProviderCredentialSurface::ApiKey,
        "the mantle surface is the EXACT x-api-key reuse (decision 5)"
    );
    // Scripted standard SSE decodes through the unmodified decoder.
    let stream = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"mantle ok\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    let items = crate::replay_anthropic_sse(stream.as_bytes());
    assert!(
        items.iter().any(|item| matches!(
            item,
            Ok(StreamEvent::TextDelta { text }) if text == "mantle ok"
        )),
        "standard SSE text decodes"
    );
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Ok(StreamEvent::Finish { .. }))),
        "standard SSE finish decodes"
    );
}

/// LAW (LB2 — endpoint pinning): `new_endpoint` accepts EXACTLY the mantle
/// URL shape and refuses everything else, so the bearer can never be aimed
/// at an arbitrary origin. Both directions: two valid regions construct;
/// the refusal matrix stays refused.
///
/// MUTATION CHECK: accept any https URL in
/// `validate_bedrock_mantle_base_url`. Expected RUNTIME failure: the
/// refusal matrix below constructs adapters.
#[test]
fn lb2_new_endpoint_refuses_non_mantle_url_shapes() {
    for accepted in [
        "https://bedrock-mantle.us-east-1.api.aws/anthropic",
        "https://bedrock-mantle.eu-central-1.api.aws/anthropic/",
    ] {
        AnthropicProvider::new_endpoint(
            secret_credential("bedrock-shape-audit", b"NEVER_SENT_SHAPE_AUDIT"),
            "anthropic.claude-opus-5",
            accepted,
        )
        .unwrap_or_else(|error| panic!("mantle shape `{accepted}` must construct: {error}"));
    }
    for refused in [
        "https://api.anthropic.com/v1/messages",
        "http://bedrock-mantle.us-east-1.api.aws/anthropic",
        "https://bedrock-mantle.us-east-1.api.aws.evil.example/anthropic",
        "https://bedrock-mantle.us-east-1.api.aws/anthropic/extra",
        "https://bedrock-mantle..api.aws/anthropic",
        "https://bedrock-mantle.Us-East-1.api.aws/anthropic",
        "https://bedrock-mantle.us.east/1.api.aws/anthropic",
        "https://bedrock-mantle.us-east-1.api.aws",
        "",
    ] {
        let error = AnthropicProvider::new_endpoint(
            secret_credential("bedrock-shape-audit", b"NEVER_SENT_SHAPE_AUDIT"),
            "anthropic.claude-opus-5",
            refused,
        )
        .err()
        .unwrap_or_else(|| panic!("non-mantle shape `{refused}` must be refused"));
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    }
}

/// LAW (LV1 — the vertex golden): the Vertex adapter POSTs
/// `{base}/{model}:streamRawPredict` (model IN THE URL), the body carries
/// `anthropic_version: "vertex-2023-10-16"` and NO `model` field, auth is a
/// plain `Authorization: Bearer`, and neither `x-api-key`, the OAuth beta,
/// nor the standard `anthropic-version` HEADER is sent.
///
/// MUTATION CHECK: keep `model` in the body, drop the `anthropic_version`
/// insert, or template the first-party URL. Expected RUNTIME failure: the
/// named equalities below.
#[tokio::test]
async fn lv1_vertex_golden_model_in_url_version_in_body_bearer_header() {
    let provider = AnthropicProvider::new_vertex(
        secret_credential("vertex-audit", b"VERTEX_GCP_TOKEN_SENTINEL_77cc"),
        "claude-sonnet-4-5@20250929",
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
    )
    .expect("vertex adapter");
    let payload = provider
        .request_payload(&one_line_turn("claude-sonnet-4-5@20250929"))
        .expect("vertex payload");
    assert!(
        payload.get("model").is_none(),
        "the vertex body must NOT carry a model field"
    );
    assert_eq!(
        payload
            .get("anthropic_version")
            .and_then(serde_json::Value::as_str),
        Some("vertex-2023-10-16"),
        "the vertex body versions through anthropic_version"
    );
    let request = provider
        .request_body(payload)
        .await
        .expect("vertex request");
    assert_eq!(
        request.url().as_str(),
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models/claude-sonnet-4-5@20250929:streamRawPredict",
        "the model rides IN THE URL"
    );
    assert_eq!(
        request.headers().get(AUTHORIZATION).expect("GCP bearer"),
        "Bearer VERTEX_GCP_TOKEN_SENTINEL_77cc"
    );
    assert!(!request.headers().contains_key("x-api-key"));
    assert!(!request.headers().contains_key(ANTHROPIC_OAUTH_BETA_HEADER));
    assert!(
        !request.headers().contains_key("anthropic-version"),
        "vertex versions through the BODY, never the header"
    );
    assert_eq!(
        provider.credential_surface(),
        crate::ProviderCredentialSurface::CloudBearer,
        "the vertex surface is CloudBearer (decision 5)"
    );
}

/// LAW (LV1, shape half): the Vertex base-URL validator accepts the global
/// and matching-regional templates and refuses host/path disagreement,
/// non-Google hosts, and http — both directions.
#[test]
fn vertex_base_url_shape_is_pinned_global_or_matching_regional() {
    use crate::anthropic::{validate_vertex_models_base_url, vertex_models_base_url};
    for accepted in [
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/us-east5/publishers/anthropic/models",
    ] {
        validate_vertex_models_base_url(accepted)
            .unwrap_or_else(|error| panic!("vertex shape `{accepted}` must pass: {error}"));
    }
    for refused in [
        "http://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/us-east5/publishers/anthropic/models",
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/eu-west4/publishers/anthropic/models",
        "https://evilaiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.example/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.com/v1/projects/acme.ai/locations/global/publishers/anthropic/models",
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/google/models",
        "",
    ] {
        assert!(
            validate_vertex_models_base_url(refused).is_err(),
            "vertex shape `{refused}` must be refused"
        );
    }
    // The card-side builder routes through the SAME validator.
    assert_eq!(
        vertex_models_base_url("acme-ai", "global").expect("global build"),
        "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models"
    );
    assert_eq!(
        vertex_models_base_url("acme-ai", "us-east5").expect("regional build"),
        "https://us-east5-aiplatform.googleapis.com/v1/projects/acme-ai/locations/us-east5/publishers/anthropic/models"
    );
    assert!(vertex_models_base_url("", "global").is_err());
}

/// LAW — Anthropic non-success bodies obey the 64 KiB ceiling exactly like
/// the OpenAI/Gemini adapters: bytes past the bound are never read into
/// memory, parsed, or logged. Pinned by the orchestrator after the
/// delete-the-truncation mutation SURVIVED the E2-E4 suite unpinned.
#[tokio::test]
async fn anthropic_error_body_read_is_bounded_to_64kib() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request_head = [0u8; 4096];
        let _ = socket.read(&mut request_head).await;
        let body = vec![b'x'; 64 * 1024 + 4096];
        let head = format!(
            "HTTP/1.1 529 Overloaded\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(&body).await;
        let _ = socket.shutdown().await;
    });

    let response = reqwest::Client::new()
        .get(format!("http://{address}/v1/messages"))
        .send()
        .await
        .expect("oversized error response");
    let body = read_error_body_bounded(response)
        .await
        .expect("bounded read succeeds");
    assert_eq!(
        body.len(),
        64 * 1024,
        "exactly the ceiling, never a byte beyond"
    );
}

/// MUTATION CHECK: propagate the diagnostic-body reset with `?` before
/// attaching the already-received status. Core would then mistake this
/// completed 503 response for route loss and enter WaitingForRoute.
#[tokio::test]
async fn completed_anthropic_5xx_with_reset_body_keeps_http_status_not_network_class() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request_head = [0u8; 4096];
        let _ = socket.read(&mut request_head).await;
        socket
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: 64\r\nx-request-id: req-503\r\nconnection: close\r\n\r\n",
            )
            .await
            .expect("write completed 503 headers");
        socket.shutdown().await.expect("truncate response body");
    });

    let provider = AnthropicProvider::new_custom(
        secret_credential("anthropic-503", b"fixture-secret"),
        "claude-local",
        &format!("http://{address}"),
    )
    .expect("loopback compatible provider");
    let error = provider
        .stream_turn(one_line_turn("claude-local"))
        .await
        .expect_err("completed HTTP 503 remains an error");

    assert_ne!(error.kind, ProviderErrorKind::NetworkUnavailable);
    assert_eq!(error.presentation.provider_http_status, Some(503));
    assert_eq!(
        error.presentation.provider_request_id.as_deref(),
        Some("req-503")
    );
}
